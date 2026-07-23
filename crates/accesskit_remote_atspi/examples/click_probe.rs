//! Exercises the full action round trip against a live AT-SPI app: enumerate,
//! click a button, and wait for the resulting tree update. A WSL-local smoke
//! test for `perform` -> bridge -> `poll_events`, needing no Windows bridge.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use std::time::Duration;

    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, _focus) = source.initial_state();
    let (descriptor, update) = windows.first().expect("at least one window");
    let window = descriptor.id;
    println!("window {} has {} nodes", window.0, update.nodes.len());

    let (target, label) = update
        .nodes
        .iter()
        .find(|(_, node)| {
            node.label() == Some("New Tab") && node.supports_action(accesskit::Action::Click)
        })
        .or_else(|| {
            update
                .nodes
                .iter()
                .find(|(_, node)| node.supports_action(accesskit::Action::Click))
        })
        .map(|(id, node)| (*id, node.label().map(str::to_owned)))
        .expect("a clickable node");
    println!("clicking node {} {:?}", target.0, label);

    source.perform(
        window,
        &accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node: target,
            data: None,
        },
    );

    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            match event {
                SourceEvent::TreeUpdate { window, update } => {
                    println!("TreeUpdate for window {} -> {} nodes", window.0, update.nodes.len());
                    return;
                }
                other => println!("event: {other:?}"),
            }
        }
    }
    eprintln!("NO EVENT: action round trip produced no TreeUpdate");
    std::process::exit(2);
}

#[cfg(not(target_os = "linux"))]
fn main() {}
