//! Proves passive event reflection closes the action/re-walk race: clicking
//! "New Tab" makes GTK create accessibles asynchronously, so the immediate
//! re-walk in `handle_action` sees a stale mid-transition tree, but the
//! `children-changed` events trigger later re-walks that see the new nodes.
//! The test passes only if a tree update *after* the immediate post-action
//! one grows past that first update's node count.

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
    let mut first_nodes = None;
    let mut later_max = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            if let SourceEvent::TreeUpdate { window, update } = event {
                updates += 1;
                let nodes = update.nodes.len();
                match first_nodes {
                    None => first_nodes = Some(nodes),
                    Some(_) => later_max = later_max.max(nodes),
                }
                println!("update #{updates} for window {}: {nodes} nodes", window.0);
            }
        }
    }

    let first_nodes = first_nodes.unwrap_or(initial_nodes);
    if later_max > first_nodes {
        println!(
            "PASS: {updates} update(s); a passive re-walk grew {first_nodes} -> {later_max} \
             past the immediate post-action walk"
        );
    } else {
        eprintln!(
            "FAIL: {updates} update(s) seen, no update after the first grew past {first_nodes}"
        );
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
