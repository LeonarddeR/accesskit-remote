//! Exercises the full action round trip against a live AT-SPI app: enumerate,
//! click a node, and print every tree update the click produces with its shape
//! and latency. A WSL-local smoke test for `perform` -> bridge ->
//! `poll_events`, needing no Windows bridge.
//!
//! Pass a label substring to choose the target (`click_probe checkbutton`);
//! without one it prefers "New Tab", else the first clickable node. The shape
//! column is what distinguishes an immediate post-action re-walk from a
//! toolkit-event-driven delta.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use std::time::{Duration, Instant};

    // The bridge reports a declined or unsupported AT-SPI call through
    // `tracing::warn`, which is otherwise invisible in an example.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .init();

    let wanted = std::env::args().nth(1);
    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, _focus) = source.initial_state();
    let (descriptor, update) = windows.first().expect("at least one window");
    let window = descriptor.id;
    println!("window {} has {} nodes", window.0, update.nodes.len());

    let clickable = |node: &accesskit::Node| node.supports_action(accesskit::Action::Click);
    let (target, label) = update
        .nodes
        .iter()
        .find(|(_, node)| match &wanted {
            Some(wanted) => clickable(node) && node.label().is_some_and(|l| l.contains(wanted)),
            None => clickable(node) && node.label() == Some("New Tab"),
        })
        .or_else(|| update.nodes.iter().find(|(_, node)| clickable(node)))
        .map(|(id, node)| (*id, node.label().map(str::to_owned)))
        .expect("a clickable node");
    println!("clicking node {} {:?}", target.0, label);

    let clicked = Instant::now();
    source.perform(
        window,
        &accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node: target,
            data: None,
        },
    );

    let mut updates = 0;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            match event {
                SourceEvent::TreeUpdate { window, update } => {
                    updates += 1;
                    println!(
                        "  +{:>5}ms TreeUpdate w{} ({} nodes, {})",
                        clicked.elapsed().as_millis(),
                        window.0,
                        update.nodes.len(),
                        if update.tree.is_some() { "full walk" } else { "delta" },
                    );
                }
                other => println!("  +{:>5}ms {other:?}", clicked.elapsed().as_millis()),
            }
        }
    }
    if updates == 0 {
        eprintln!("NO EVENT: action round trip produced no TreeUpdate");
        std::process::exit(2);
    }
    println!("done: {updates} update(s)");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
