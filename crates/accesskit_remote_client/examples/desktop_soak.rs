//! Composes a whole desktop from a running provider and audits every update
//! the way `accesskit_consumer` does — but reporting instead of aborting.
//!
//! **Why this exists.** The consumer enforces its tree rules with `panic!`, and
//! in a platform adapter those fire inside `wnd_proc`, which cannot unwind: the
//! screen reader's process aborts. So the rules get discovered in the worst
//! possible place. Here they are checked in a plain loop where a violation is a
//! printed line naming the ids.
//!
//! **The knobs are the point.** A desktop that composes perfectly over a fast
//! socket panicked within seconds over an RDP dynamic virtual channel, where a
//! 2 MB initial tree arrives in 16 KiB pieces, over seconds, on a socket shared
//! with video — while the desktop underneath keeps changing. `--chunk` and
//! `--stall` reproduce that delivery against any provider, so the difference
//! between the two paths can be measured rather than argued about.
//!
//! Usage:
//!   desktop_soak --tcp [PORT] [--secs N] [--chunk BYTES] [--stall MS]

use accesskit::{NodeId, TreeUpdate};
use accesskit_remote_client::{ClientConnection, ClientEvent, DesktopTree};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = flag(&args, "--tcp").unwrap_or(4750);
    let secs = flag(&args, "--secs").unwrap_or(120);
    let chunk_size = flag(&args, "--chunk").unwrap_or(0) as usize;
    let stall = flag(&args, "--stall").unwrap_or(0);
    // Deliver every Nth pair of pieces in the wrong order. A socket cannot do
    // this; a callback-delivered transport with no sequence number can, and
    // the question is what the damage then looks like — a decode error, or a
    // plausible-looking tree missing its middle.
    let swap = flag(&args, "--swap").unwrap_or(0) as usize;

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port as u16))?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    println!(
        "connected on {port}; auditing for {secs}s (chunk={}, stall={stall}ms)",
        if chunk_size == 0 { "whole".into() } else { chunk_size.to_string() }
    );

    let mut client = ClientConnection::new("desktop_soak");
    let mut desktop = DesktopTree::new("soak");
    let mut audit = Audit::default();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut buf = vec![0u8; 65536];

    while Instant::now() < deadline && !client.is_closed() {
        let out = client.take_output();
        if !out.is_empty() {
            stream.write_all(&out)?;
        }
        let read = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        };

        // Feed it the way the transport under test would, not the way the
        // socket happened to deliver it.
        let mut pieces: Vec<&[u8]> = match chunk_size {
            0 => vec![&buf[..read]],
            n => buf[..read].chunks(n).collect(),
        };
        if swap > 0 {
            for i in (1..pieces.len()).step_by(swap.max(1)) {
                pieces.swap(i - 1, i);
            }
        }
        for piece in pieces {
            let events = client.handle_input(piece).map_err(std::io::Error::other)?;
            for event in events {
                match event {
                    ClientEvent::Connected => println!("established"),
                    ClientEvent::WindowAdded { window } | ClientEvent::WindowRemoved { window } => {
                        println!("window {} came or went; resyncing the desktop", window.0);
                        for update in desktop.sync(&mut client) {
                            audit.check("sync", &update);
                        }
                    }
                    ClientEvent::FocusChanged { .. } => {
                        for update in desktop.sync(&mut client) {
                            audit.check("focus sync", &update);
                        }
                    }
                    ClientEvent::TreeUpdated { window, update } => {
                        if let Some(update) = desktop.delta(window, update) {
                            audit.check("delta", &update);
                        }
                    }
                    ClientEvent::Closed { reason } => println!("provider closed: {reason}"),
                    _ => {}
                }
            }
            if stall > 0 {
                std::thread::sleep(Duration::from_millis(stall));
            }
        }
    }

    audit.report();
    if audit.violations == 0 {
        println!("PASS");
        Ok(())
    } else {
        println!("FAIL");
        std::process::exit(1);
    }
}

/// One tree as a consumer would hold it: nodes it can reach from the root, and
/// nothing else.
#[derive(Default)]
struct Held {
    children: std::collections::HashMap<NodeId, Vec<NodeId>>,
    root: Option<NodeId>,
}

/// Applies the consumer's structural rules to every update, and remembers what
/// a real consumer would be holding afterwards.
///
/// **The pruning is the whole point.** A consumer keeps only what the root can
/// reach and throws the rest away, so "the node was sent earlier" is not the
/// same as "the consumer still has it". An audit that merely accumulates ids
/// accepts updates that abort a screen reader; this one does not.
#[derive(Default)]
struct Audit {
    live: std::collections::HashMap<accesskit::TreeId, Held>,
    updates: usize,
    violations: usize,
    pruned: usize,
}

impl Audit {
    fn check(&mut self, what: &str, update: &TreeUpdate) {
        self.updates += 1;
        let held = self.live.entry(update.tree_id.clone()).or_default();
        let known: HashSet<NodeId> = held.children.keys().copied().collect();
        let arriving: HashSet<NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();

        // The rule that aborts a screen reader: a child named by an arriving
        // node must be in this update or already in the tree.
        let mut dangling = Vec::new();
        for (id, node) in &update.nodes {
            for child in node.children() {
                if !arriving.contains(child) && !known.contains(child) {
                    dangling.push((*id, *child));
                }
            }
        }
        if !dangling.is_empty() {
            self.violations += 1;
            let shown: Vec<String> = dangling
                .iter()
                .take(8)
                .map(|(parent, child)| format!("#{}→#{}", parent.0, child.0))
                .collect();
            println!(
                "VIOLATION on {what} #{}: {} dangling child ref(s): {}{}",
                self.updates,
                dangling.len(),
                shown.join(", "),
                if dangling.len() > 8 { ", …" } else { "" }
            );
        }

        // The focus rule: the focused node must be in the tree it names.
        if !arriving.contains(&update.focus) && !known.contains(&update.focus) {
            self.violations += 1;
            println!("VIOLATION on {what} #{}: focus #{} is in no tree", self.updates, update.focus.0);
        }

        // Apply, then keep only what the root reaches — the consumer's own rule.
        for (id, node) in &update.nodes {
            held.children.insert(*id, node.children().to_vec());
        }
        if let Some(tree) = update.tree.as_ref() {
            held.root = Some(tree.root);
        }
        if let Some(root) = held.root {
            let mut reachable = HashSet::new();
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                if !reachable.insert(id) {
                    continue;
                }
                if let Some(children) = held.children.get(&id) {
                    stack.extend(children.iter().copied());
                }
            }
            let pruned = held.children.len() - reachable.len().min(held.children.len());
            held.children.retain(|id, _| reachable.contains(id));
            if pruned > 0 {
                self.pruned += pruned;
            }
        }
    }

    fn report(&self) {
        let nodes: usize = self.live.values().map(|h| h.children.len()).sum();
        println!("{} node(s) pruned as unreachable along the way", self.pruned);
        println!(
            "{} update(s) audited across {} tree(s), {} node(s) held, {} violation(s)",
            self.updates,
            self.live.len(),
            nodes,
            self.violations
        );
    }
}

fn flag(args: &[String], name: &str) -> Option<u64> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1)?.parse().ok()
}
