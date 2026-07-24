//! Observes window lifecycle over the live AT-SPI mirror: prints the initial
//! toplevels, then every `WindowAdded` / `WindowRemoved` (and `TreeUpdate`)
//! the source emits as apps open and close windows. Drive it by launching or
//! closing another a11y-enabled toplevel while it runs.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use std::time::{Duration, Instant};

    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, focus) = source.initial_state();
    println!("initial: {} window(s), focus {:?}", windows.len(), focus.map(|w| w.0));
    for (descriptor, update) in &windows {
        println!(
            "  window {} \"{}\" ({} nodes)",
            descriptor.id.0,
            descriptor.title,
            update.nodes.len()
        );
    }

    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(25);
    println!("watching for {secs}s...");

    let mut added = 0;
    let mut removed = 0;
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            match event {
                SourceEvent::WindowAdded { descriptor, tree } => {
                    added += 1;
                    println!(
                        "+ WindowAdded {} \"{}\" ({} nodes)",
                        descriptor.id.0,
                        descriptor.title,
                        tree.nodes.len()
                    );
                }
                SourceEvent::WindowRemoved(window) => {
                    removed += 1;
                    println!("- WindowRemoved {}", window.0);
                }
                SourceEvent::TreeUpdate { window, update } => {
                    let ids: Vec<u64> = update.nodes.iter().map(|(id, _)| id.0).collect();
                    println!(
                        "  TreeUpdate {} ({} nodes, focus {}) ids={:?}",
                        window.0,
                        update.nodes.len(),
                        update.focus.0,
                        &ids[..ids.len().min(6)]
                    );
                }
                SourceEvent::FocusChanged(window) => {
                    println!("  FocusChanged {:?}", window.map(|w| w.0));
                }
            }
        }
    }
    println!("done: {added} added, {removed} removed");
}

#[cfg(not(target_os = "linux"))]
fn main() {}
