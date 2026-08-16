//! Checks that a screen point lands on the character that is drawn there.
//!
//! This is the instrument for the one text question the provider cannot answer
//! by looking at itself: per-character rectangles can be present, consistent and
//! completely wrong, and nothing on this side would notice. It walks real
//! windows, feeds the trees to the **real `accesskit_consumer`** — the same code
//! the Windows adapter runs behind UIA's `RangeFromPoint` — and asks it, for the
//! centre of every character rectangle, which character is there. The answer
//! must be the character whose rectangle it is.
//!
//! It exists because a UIA probe from Windows reported `RangeFromPoint` mapping
//! every point to the end of the document. That turned out to be a probe aimed
//! at the centre of a mostly empty text view, where end-of-document is the
//! correct answer — but telling that apart from a real coordinate-space defect
//! needed the consumer's own judgement, not reasoning about it. `--empty` prints
//! that case explicitly so the two are never confused again.
//!
//! Usage: hit_probe [--app <substr>] [--empty] [--verbose]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("hit_probe runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    use accesskit::{NodeId, Point, Role, TreeId};
    use accesskit_remote_ax::element::NodeIdMap;
    use accesskit_remote_ax::names::Names;
    use accesskit_remote_ax::{ax, trust, walk};
    use std::collections::HashMap;

    if !trust::is_trusted() {
        eprintln!("{}", trust::untrusted_message());
        std::process::exit(2);
    }

    let mut filter = None;
    let mut show_empty = false;
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app" => filter = args.next().map(|f| f.to_lowercase()),
            "--empty" => show_empty = true,
            "--verbose" => verbose = true,
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let names = Names::new();
    let mut apps = ax::running_apps();
    if let Some(filter) = &filter {
        apps.retain(|app| app.info.name.to_lowercase().contains(filter));
    }

    let (mut elements, mut probed, mut wrong, mut ungeometried) = (0usize, 0usize, 0usize, 0usize);
    for app in &apps {
        for window in ax::windows_of(app, &names).unwrap_or_default() {
            let nodes = walk::walk_window(window.key.clone(), &names);
            let mut ids = NodeIdMap::new();
            let Some(update) = walk::build_window_update(&nodes, &mut ids) else {
                continue;
            };

            // Keep the raw nodes: the consumer's run iterator is private, and
            // the rectangles to probe are the ones this crate emitted.
            let raw: HashMap<NodeId, accesskit::Node> =
                update.nodes.iter().map(|(id, node)| (*id, node.clone())).collect();
            let containers: Vec<NodeId> = update
                .nodes
                .iter()
                .filter(|(_, node)| {
                    node.children().iter().any(|child| {
                        raw.get(child).map(|c| c.role() == Role::TextRun).unwrap_or(false)
                    })
                })
                .map(|(id, _)| *id)
                .collect();
            if containers.is_empty() {
                continue;
            }

            let tree = accesskit_consumer::Tree::new(update, true);
            let state = tree.state();
            for container_id in containers {
                let Some(element) = state.node_by_tree_local_id(container_id, TreeId::ROOT) else {
                    continue;
                };
                if !element.supports_text_ranges() {
                    // A text-bearing node whose role the consumer will not
                    // answer range queries for is a mapping problem, not a
                    // geometry one — report it, but it is not this test.
                    if verbose {
                        println!(
                            "  {} | {:?}: {:?} carries runs but answers no ranges",
                            app.info.name,
                            window.title,
                            element.role()
                        );
                    }
                    continue;
                }
                elements += 1;

                let container = &raw[&container_id];
                let mut global = 0usize;
                let (mut here, mut bad) = (0usize, 0usize);
                let mut without_geometry = false;
                for run_id in container.children() {
                    let Some(run) = raw.get(run_id) else { continue };
                    let count = run.character_lengths().len();
                    let (Some(bounds), Some(positions), Some(widths)) =
                        (run.bounds(), run.character_positions(), run.character_widths())
                    else {
                        without_geometry = true;
                        global += count;
                        continue;
                    };
                    let y = (bounds.y0 + bounds.y1) / 2.0;
                    for (index, (position, width)) in
                        positions.iter().zip(widths.iter()).enumerate()
                    {
                        // A zero-width character — a newline's box — has no
                        // point that is inside it, and asking is meaningless.
                        if *width <= 0.0 {
                            continue;
                        }
                        let x = bounds.x0 + f64::from(*position) + f64::from(*width) / 2.0;
                        let landed =
                            element.text_position_at_point(Point::new(x, y)).to_global_usv_index();
                        here += 1;
                        if landed != global + index {
                            bad += 1;
                            if verbose && bad <= 5 {
                                println!(
                                    "    ({x:.1},{y:.1}) is character {} but resolved to {landed}",
                                    global + index
                                );
                            }
                        }
                    }
                    global += count;
                }
                probed += here;
                wrong += bad;
                if without_geometry {
                    ungeometried += 1;
                }

                if here > 0 && (bad > 0 || verbose) {
                    println!(
                        "  {} | {:?} | {:?}: {}/{} characters resolve correctly",
                        app.info.name,
                        window.title,
                        element.role(),
                        here - bad,
                        here,
                    );
                }
                if show_empty {
                    // The Windows finding, reproduced deliberately: a point in
                    // the element but past its last character.
                    if let Some(bounds) = container.bounds() {
                        let centre = Point::new(
                            (bounds.x0 + bounds.x1) / 2.0,
                            (bounds.y0 + bounds.y1) / 2.0,
                        );
                        let position = element.text_position_at_point(centre);
                        println!(
                            "  {} | {:?}: the element's centre resolves to offset {}{}",
                            app.info.name,
                            window.title,
                            position.to_global_usv_index(),
                            if position.is_document_end() { " (end of document)" } else { "" },
                        );
                    }
                }
            }
        }
    }

    println!(
        "\n{probed} character rectangle(s) across {elements} text element(s); \
         {ungeometried} element(s) carried text with no geometry at all"
    );
    if elements == 0 {
        println!("FAIL: no text element was found to probe — open a document and retry");
        std::process::exit(1);
    }
    if wrong == 0 {
        println!("PASS: every probed point resolves to the character drawn there");
    } else {
        println!("FAIL: {wrong} point(s) resolved to the wrong character");
        std::process::exit(1);
    }
}
