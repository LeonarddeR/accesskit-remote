//! Proves passive event reflection closes the action/re-walk race: clicking
//! "New Tab" makes GTK create accessibles asynchronously, so the immediate
//! re-walk in `handle_action` sees the old tree, but the `children-changed`
//! event triggers a later re-walk that sees the new nodes. The test passes
//! only if a tree update *after* the first grows past the initial node count.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use std::time::{Duration, Instant};

    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, _focus) = source.initial_state();
    let (descriptor, update) = windows.first().expect("at least one window");
    let window = descriptor.id;
    let initial_nodes = update.nodes.len();
    println!("window {} starts with {initial_nodes} nodes", window.0);

    let target = update
        .nodes
        .iter()
        .find(|(_, node)| {
            node.label() == Some("New Tab") && node.supports_action(accesskit::Action::Click)
        })
        .map(|(id, _)| *id)
        .expect("a clickable 'New Tab' button");
    println!("clicking New Tab (node {})", target.0);
    source.perform(
        window,
        &accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node: target,
            data: None,
        },
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut updates = 0;
    let mut max_nodes = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            if let SourceEvent::TreeUpdate { window, update } = event {
                updates += 1;
                let nodes = update.nodes.len();
                max_nodes = max_nodes.max(nodes);
                println!("update #{updates} for window {}: {nodes} nodes", window.0);
            }
        }
    }

    if max_nodes > initial_nodes {
        println!("PASS: tree grew {initial_nodes} -> {max_nodes} after New Tab via passive reflection");
    } else {
        eprintln!("FAIL: {updates} update(s) seen, node count never grew past {initial_nodes}");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
