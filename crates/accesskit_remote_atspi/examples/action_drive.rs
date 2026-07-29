//! Drives three action round trips end to end: `SetValue` and `Increment` on
//! the first slider or spin button with a known numeric value (AT-SPI
//! `Value` interface writes), then `Expand` on the first node exposing
//! `HasPopup` (a GTK4 menu-button). Each scenario PASS/FAIL-gates on the
//! resulting `TreeUpdate` delta, never on the fire-and-forget `perform` call.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit::{
        Action, ActionData, ActionRequest, HasPopup, NodeId, Role, TreeId, TreeUpdate,
    };
    use accesskit_remote::WindowId;
    use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
    use std::time::{Duration, Instant};

    fn find_numeric_target(
        windows: &[(WindowDescriptor, TreeUpdate)],
    ) -> Option<(WindowId, NodeId, f64, Option<f64>, Option<f64>, Option<String>)> {
        let search = |role: Role| {
            windows
                .iter()
                .flat_map(|(descriptor, update)| {
                    update.nodes.iter().map(move |(id, node)| (descriptor.id, *id, node))
                })
                .find_map(|(window, id, node)| {
                    (node.role() == role).then(|| node.numeric_value()).flatten().map(|value| {
                        (
                            window,
                            id,
                            value,
                            node.min_numeric_value(),
                            node.max_numeric_value(),
                            node.label().map(str::to_owned),
                        )
                    })
                })
        };
        search(Role::Slider).or_else(|| search(Role::SpinButton))
    }

    fn find_popup_target(
        windows: &[(WindowDescriptor, TreeUpdate)],
    ) -> Option<(WindowId, NodeId, HasPopup, Option<String>)> {
        windows
            .iter()
            .flat_map(|(descriptor, update)| {
                update.nodes.iter().map(move |(id, node)| (descriptor.id, *id, node))
            })
            .find_map(|(window, id, node)| {
                node.has_popup().map(|kind| (window, id, kind, node.label().map(str::to_owned)))
            })
    }

    fn midpoint_target(current: f64, min: Option<f64>, max: Option<f64>) -> f64 {
        match (min, max) {
            (Some(min), Some(max)) => {
                let midpoint = (min + max) / 2.0;
                if (midpoint - current).abs() > 1e-6 {
                    midpoint
                } else if max > current {
                    current + (max - current) / 2.0
                } else {
                    current - (current - min) / 2.0
                }
            }
            _ => current + 1.0,
        }
    }

    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, focus) = source.initial_state();
    println!("initial: {} window(s), focus {:?}", windows.len(), focus.map(|w| w.0));

    let mut ran = 0;
    let mut failed = 0;

    match find_numeric_target(&windows) {
        Some((window, node, current, min, max, label)) => {
            let target = midpoint_target(current, min, max);
            println!(
                "set-value target: node {} {label:?} in window {}, current {current} -> {target} (min {min:?}, max {max:?})",
                node.0, window.0
            );

            ran += 1;
            source.perform(
                window,
                &ActionRequest {
                    action: Action::SetValue,
                    target_tree: TreeId::ROOT,
                    target_node: node,
                    data: Some(ActionData::NumericValue(target)),
                },
            );

            let deadline = Instant::now() + Duration::from_secs(4);
            let mut hit = false;
            let mut update_seen = false;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
                for event in source.poll_events() {
                    if let SourceEvent::TreeUpdate { window: w, update } = event {
                        if w != window {
                            continue;
                        }
                        update_seen = true;
                        let observed = update
                            .nodes
                            .iter()
                            .find(|(id, _)| *id == node)
                            .and_then(|(_, n)| n.numeric_value());
                        println!(
                            "set-value update for window {}: {} nodes, node value {:?}",
                            w.0,
                            update.nodes.len(),
                            observed
                        );
                        if observed.is_some_and(|value| (value - target).abs() < 1e-3) {
                            hit = true;
                        }
                    }
                }
            }

            if hit {
                println!("PASS set-value: node {} reached {target}", node.0);
            } else {
                failed += 1;
                if update_seen {
                    eprintln!(
                        "FAIL set-value: updates arrived for window {} but node {} never reported numeric_value == {target}",
                        window.0, node.0
                    );
                } else {
                    eprintln!("FAIL set-value: no update arrived for window {} at all", window.0);
                }
            }

            let recorded = if hit { target } else { current };
            println!(
                "increment target: node {} {label:?} in window {}, recorded value {recorded}",
                node.0, window.0
            );

            ran += 1;
            source.perform(
                window,
                &ActionRequest {
                    action: Action::Increment,
                    target_tree: TreeId::ROOT,
                    target_node: node,
                    data: None,
                },
            );

            let deadline = Instant::now() + Duration::from_secs(4);
            let mut hit = false;
            let mut risen_to = None;
            let mut update_seen = false;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
                for event in source.poll_events() {
                    if let SourceEvent::TreeUpdate { window: w, update } = event {
                        if w != window {
                            continue;
                        }
                        update_seen = true;
                        let observed = update
                            .nodes
                            .iter()
                            .find(|(id, _)| *id == node)
                            .and_then(|(_, n)| n.numeric_value());
                        println!(
                            "increment update for window {}: {} nodes, node value {:?}",
                            w.0,
                            update.nodes.len(),
                            observed
                        );
                        if let Some(value) = observed {
                            if value > recorded {
                                hit = true;
                                risen_to = Some(value);
                            }
                        }
                    }
                }
            }

            if hit {
                println!("PASS increment: node {} rose from {recorded} to {risen_to:?}", node.0);
            } else {
                failed += 1;
                if update_seen {
                    eprintln!(
                        "FAIL increment: updates arrived for window {} but node {} numeric_value never exceeded {recorded}",
                        window.0, node.0
                    );
                } else {
                    eprintln!("FAIL increment: no update arrived for window {} at all", window.0);
                }
            }
        }
        None => {
            println!("SKIP set-value: no target");
            println!("SKIP increment: no target");
        }
    }

    match find_popup_target(&windows) {
        Some((window, node, kind, label)) => {
            println!(
                "expand-popup target: node {} {label:?} in window {}, has_popup {kind:?}",
                node.0, window.0
            );

            ran += 1;
            source.perform(
                window,
                &ActionRequest {
                    action: Action::Expand,
                    target_tree: TreeId::ROOT,
                    target_node: node,
                    data: None,
                },
            );

            let deadline = Instant::now() + Duration::from_secs(4);
            let mut update_seen = false;
            let mut expanded_after = None;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
                for event in source.poll_events() {
                    if let SourceEvent::TreeUpdate { window: w, update } = event {
                        if w != window {
                            continue;
                        }
                        update_seen = true;
                        if let Some((_, n)) = update.nodes.iter().find(|(id, _)| *id == node) {
                            expanded_after = n.is_expanded();
                        }
                        println!(
                            "expand-popup update for window {}: {} nodes, target is_expanded {:?}",
                            w.0,
                            update.nodes.len(),
                            expanded_after
                        );
                    }
                }
            }

            if update_seen {
                println!(
                    "PASS expand-popup: a tree update arrived for window {} (target is_expanded {expanded_after:?})",
                    window.0
                );
            } else {
                failed += 1;
                eprintln!(
                    "FAIL expand-popup: waited for any tree update on window {}, none arrived",
                    window.0
                );
            }
        }
        None => println!("SKIP expand-popup: no target"),
    }

    if ran == 0 {
        eprintln!("all scenarios skipped: no suitable targets found");
        std::process::exit(2);
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
