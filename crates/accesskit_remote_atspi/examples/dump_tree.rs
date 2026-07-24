//! Connects to the AT-SPI bus, enumerates the initial snapshot, and prints
//! each discovered window with its tree size. A WSL-local smoke test for the
//! bus layer that needs no Windows bridge.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::TreeSource;

    let mut source = match accesskit_remote_atspi::AtspiSource::new() {
        Ok(source) => source,
        Err(e) => {
            eprintln!("AtspiSource::new failed: {e}");
            std::process::exit(1);
        }
    };
    let (windows, focus) = source.initial_state();
    println!("focused window: {focus:?}");
    println!("discovered {} window(s):", windows.len());
    for (descriptor, update) in &windows {
        let app = &descriptor.app;
        println!(
            "  window {} | app {:?} app_id={:?} pid={:?} toolkit={:?} {:?} | title {:?} | {} nodes",
            descriptor.id.0,
            app.name,
            app.app_id,
            app.pid,
            app.toolkit,
            app.toolkit_version,
            descriptor.title,
            update.nodes.len(),
        );
        // The consumer filters GenericContainer and TextRun out of the tree, so
        // everything else is what a UIA client actually sees. Watch this count
        // when broadening the role map: it is the tree-inflation metric.
        let mut by_role = std::collections::BTreeMap::<String, usize>::new();
        for (_, node) in update.nodes.iter() {
            *by_role.entry(format!("{:?}", node.role())).or_default() += 1;
        }
        let reaching = update
            .nodes
            .iter()
            .filter(|(_, node)| {
                !matches!(
                    node.role(),
                    accesskit::Role::GenericContainer | accesskit::Role::TextRun
                )
            })
            .count();
        println!(
            "      {reaching}/{} nodes reach the client tree | roles: {}",
            update.nodes.len(),
            by_role
                .iter()
                .map(|(role, count)| format!("{role}={count}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        for (id, node) in update.nodes.iter() {
            let text = node.value().or_else(|| node.label());
            let clickable = node.supports_action(accesskit::Action::Click);
            let is_run = node.role() == accesskit::Role::TextRun;
            let selection = node.text_selection().map(|s| {
                (
                    (s.anchor.node.0, s.anchor.character_index),
                    (s.focus.node.0, s.focus.character_index),
                )
            });
            let toggled = node.toggled();
            let expanded = node.is_expanded();
            let selected = node.is_selected();
            let has_popup = node.has_popup();
            let has_state =
                toggled.is_some() || expanded.is_some() || selected.is_some() || has_popup.is_some();
            if text.is_some() || clickable || is_run || selection.is_some() || has_state {
                let geom = node.bounds().map(|b| {
                    format!(
                        "({},{})-({},{}) pos={}",
                        b.x0,
                        b.y0,
                        b.x1,
                        b.y1,
                        node.character_positions().map_or(0, |p| p.len()),
                    )
                });
                let state = if has_state {
                    format!(
                        " tog={toggled:?} exp={expanded:?} sel={selected:?} pop={has_popup:?}"
                    )
                } else {
                    String::new()
                };
                println!(
                    "      node {} {:?} click={} runs={} sel={:?} geom={:?}{} {:?}",
                    id.0,
                    node.role(),
                    clickable,
                    node.children().len(),
                    selection,
                    geom,
                    state,
                    text,
                );
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
