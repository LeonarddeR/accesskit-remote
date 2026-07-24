//! Drives node focus end to end: perform `Action::Focus` on a focusable node
//! (AT-SPI `grab_focus`), then wait for the focus-only delta that only a GTK
//! `state-changed:focused` event produces. Headless WSL has no window manager,
//! so GTK emits no focus events there; run under a desktop shell
//! (`WSL2_WESTON_SHELL_DESKTOP=1`).

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use std::time::{Duration, Instant};

    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, focus) = source.initial_state();
    println!("initial focused window: {focus:?}");

    let (window, target, label) = windows
        .iter()
        .flat_map(|(descriptor, update)| {
            update.nodes.iter().map(move |(id, node)| (descriptor.id, *id, node))
        })
        .filter(|(_, _, node)| node.supports_action(accesskit::Action::Focus))
        .max_by_key(|(_, _, node)| node.role() == accesskit::Role::Button)
        .map(|(window, id, node)| (window, id, node.label().map(str::to_owned)))
        .expect("a focusable node");
    println!("focusing node {} {:?} in window {}", target.0, label, window.0);

    source.perform(
        window,
        &accesskit::ActionRequest {
            action: accesskit::Action::Focus,
            target_tree: accesskit::TreeId::ROOT,
            target_node: target,
            data: None,
        },
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut focus_delta = false;
    let mut rewalk_seen = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            match event {
                SourceEvent::TreeUpdate { window, update } => {
                    let kind = if update.nodes.is_empty() { "focus-only" } else { "re-walk" };
                    println!(
                        "{kind} update for window {}: {} nodes, focus {}",
                        window.0,
                        update.nodes.len(),
                        update.focus.0
                    );
                    if update.nodes.is_empty() && update.focus == target {
                        focus_delta = true;
                    } else if !update.nodes.is_empty() {
                        rewalk_seen = true;
                    }
                }
                other => println!("event: {other:?}"),
            }
        }
    }

    if focus_delta {
        println!("PASS: grab_focus round-tripped to a focus-only delta on the target");
    } else if rewalk_seen {
        eprintln!(
            "FAIL: the action re-walk arrived but no focus-only delta did \
             (GTK emitted no state-changed:focused)"
        );
        std::process::exit(2);
    } else {
        eprintln!("FAIL: no event at all (grab_focus likely failed)");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
