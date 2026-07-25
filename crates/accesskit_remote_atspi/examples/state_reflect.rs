//! Proves live state reflection: a widget's state change (a checkbox toggle, a
//! list selection, an expander) reaches the client as a minimal *semantic*
//! delta, without a full window re-walk.
//!
//! Run it, then change a widget's state in a mirrored app — e.g.
//! `xdotool key --window <id> space` on a focused gtk4-widget-factory checkbox,
//! or click one. PASS requires at least one small delta whose node's
//! `toggled`/`selected`/`expanded` actually changed, with no full re-walk of
//! that window within a second of it.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// The state a semantic refresh is expected to carry.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Semantics {
        toggled: Option<accesskit::Toggled>,
        selected: Option<bool>,
        expanded: Option<bool>,
        disabled: bool,
        busy: bool,
    }

    fn semantics(node: &accesskit::Node) -> Semantics {
        Semantics {
            toggled: node.toggled(),
            selected: node.is_selected(),
            expanded: node.is_expanded(),
            disabled: node.is_disabled(),
            busy: node.is_busy(),
        }
    }

    /// A delta bigger than this is not the per-node refresh under test.
    const MAX_DELTA_NODES: usize = 4;
    /// How close a full re-walk may be to a delta before it stops proving the
    /// delta did the work.
    const REWALK_PROXIMITY: Duration = Duration::from_secs(1);

    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, focus) = source.initial_state();
    println!("initial: {} window(s), focus {:?}", windows.len(), focus.map(|w| w.0));

    let mut known: HashMap<(u64, u64), Semantics> = HashMap::new();
    for (descriptor, update) in &windows {
        for (id, node) in &update.nodes {
            known.insert((descriptor.id.0, id.0), semantics(node));
        }
        println!(
            "  window {} \"{}\" ({} nodes)",
            descriptor.id.0,
            descriptor.title,
            update.nodes.len()
        );
    }
    let stateful = known.values().filter(|s| s.toggled.is_some()).count();
    println!("tracking {} node(s), {stateful} with a toggle state", known.len());

    let secs: u64 = std::env::args().nth(1).and_then(|arg| arg.parse().ok()).unwrap_or(25);
    println!("watching for {secs}s — change a widget's state now...");

    let mut updates = 0;
    let mut walks: Vec<(Instant, u64)> = Vec::new();
    let mut hits: Vec<(Instant, u64, usize, String)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            match event {
                SourceEvent::TreeUpdate { window, update } => {
                    updates += 1;
                    let now = Instant::now();
                    // Only a full window build carries a `tree`.
                    let full = update.tree.is_some();
                    if full {
                        walks.push((now, window.0));
                    }
                    let mut changes = Vec::new();
                    for (id, node) in &update.nodes {
                        let fresh = semantics(node);
                        let key = (window.0, id.0);
                        if let Some(old) = known.get(&key) {
                            if *old != fresh {
                                changes.push(format!("node {} {old:?} -> {fresh:?}", id.0));
                            }
                        }
                        known.insert(key, fresh);
                    }
                    println!(
                        "  TreeUpdate w{} ({} nodes, {}){}{}",
                        window.0,
                        update.nodes.len(),
                        if full { "full walk" } else { "delta" },
                        if changes.is_empty() { "" } else { " " },
                        changes.join("; "),
                    );
                    if !full && !changes.is_empty() && update.nodes.len() <= MAX_DELTA_NODES {
                        hits.push((now, window.0, update.nodes.len(), changes.join("; ")));
                    }
                }
                SourceEvent::WindowAdded { descriptor, tree } => {
                    println!("+ WindowAdded {} \"{}\"", descriptor.id.0, descriptor.title);
                    for (id, node) in &tree.nodes {
                        known.insert((descriptor.id.0, id.0), semantics(node));
                    }
                }
                SourceEvent::WindowRemoved(window) => println!("- WindowRemoved {}", window.0),
                SourceEvent::FocusChanged(window) => {
                    println!("  FocusChanged {:?}", window.map(|w| w.0))
                }
            }
        }
    }

    let near = |at: Instant, other: Instant| {
        let gap = if other > at { other - at } else { at - other };
        gap < REWALK_PROXIMITY
    };
    let clean: Vec<&(Instant, u64, usize, String)> = hits
        .iter()
        .filter(|(at, window, _, _)| {
            !walks.iter().any(|(walked, walked_window)| walked_window == window && near(*at, *walked))
        })
        .collect();

    println!("done: {updates} update(s), {} full walk(s), {} state delta(s)", walks.len(), hits.len());
    match clean.first() {
        Some((_, window, nodes, what)) => println!(
            "PASS: {} state delta(s) with no re-walk within {REWALK_PROXIMITY:?}; \
             first was w{window} in {nodes} node(s): {what}",
            clean.len()
        ),
        None if hits.is_empty() => {
            eprintln!("FAIL: no small delta carried a state change");
            std::process::exit(2);
        }
        None => {
            eprintln!(
                "FAIL: {} state delta(s) seen, but every one had a full re-walk within \
                 {REWALK_PROXIMITY:?} — the re-walk may be doing the work",
                hits.len()
            );
            std::process::exit(2);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
