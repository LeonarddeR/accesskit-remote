//! Watches the source's live output: what `AxSource` emits as you use the Mac.
//!
//! The `passive_reflect` analogue. It drives `AxSource` directly rather than
//! through a socket, so what it shows is exactly what the daemon would put on
//! the wire — no transport in the way to confuse a missing update with a
//! dropped one.
//!
//! What to watch for: that a burst of activity collapses into a *few* updates
//! rather than dozens (the debounce working), and that an idle desktop emits
//! nothing at all (no polling, no churn).
//!
//! Usage: reflect [--seconds N]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("reflect runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    use accesskit_remote_ax::AxSource;
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use std::time::{Duration, Instant};

    let mut seconds = 20u64;
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(20),
            "--verbose" => verbose = true,
            _ => {}
        }
    }

    let started = Instant::now();
    let mut source = match AxSource::new() {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let (windows, focus) = source.initial_state();
    println!(
        "initial: {} window(s) in {:.0?}, focused {:?}\n",
        windows.len(),
        started.elapsed(),
        focus
    );
    for (descriptor, update) in &windows {
        println!(
            "  window {} {:?} — {} nodes",
            descriptor.id.0,
            descriptor.title,
            update.nodes.len()
        );
    }

    println!("\nwatching for {seconds}s — go and use the Mac\n");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (mut updates, mut nodes_sent) = (0usize, 0usize);
    while Instant::now() < deadline {
        for event in source.poll_events() {
            let elapsed = started.elapsed().as_secs_f64();
            match event {
                SourceEvent::TreeUpdate { window, update } => {
                    updates += 1;
                    nodes_sent += update.nodes.len();
                    println!(
                        "{elapsed:>8.2}s  TreeUpdate   window {} — {} nodes, focus {:?}",
                        window.0,
                        update.nodes.len(),
                        update.focus
                    );
                    // What is actually churning. A delta that keeps arriving on
                    // an idle desktop is a bug somewhere, and the roles of the
                    // nodes in it are the fastest way to find where.
                    if verbose {
                        let mut by_role: std::collections::BTreeMap<String, usize> =
                            Default::default();
                        for (_, node) in &update.nodes {
                            *by_role.entry(format!("{:?}", node.role())).or_default() += 1;
                        }
                        let mut rows: Vec<_> = by_role.into_iter().collect();
                        rows.sort_by(|a, b| b.1.cmp(&a.1));
                        println!(
                            "            changed: {}",
                            rows.iter()
                                .map(|(role, count)| format!("{role}={count}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                    }
                }
                SourceEvent::WindowAdded { descriptor, tree } => println!(
                    "{elapsed:>8.2}s  WindowAdded  window {} {:?} — {} nodes",
                    descriptor.id.0,
                    descriptor.title,
                    tree.nodes.len()
                ),
                SourceEvent::WindowRemoved(id) => {
                    println!("{elapsed:>8.2}s  WindowRemoved window {}", id.0)
                }
                SourceEvent::FocusChanged(window) => {
                    println!("{elapsed:>8.2}s  FocusChanged {window:?}")
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    println!("\n{updates} update(s), {nodes_sent} node(s) sent in {seconds}s");
    if updates == 0 {
        println!("Nothing changed — which for an idle desktop is the correct output.");
    }
}
