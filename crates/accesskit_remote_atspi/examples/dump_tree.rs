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
            "  window {} | app {:?} pid={:?} toolkit={:?} {:?} | title {:?} | {} nodes",
            descriptor.id.0,
            app.name,
            app.pid,
            app.toolkit,
            app.toolkit_version,
            descriptor.title,
            update.nodes.len(),
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
            if text.is_some() || clickable || is_run || selection.is_some() {
                println!(
                    "      node {} {:?} click={} runs={} sel={:?} {:?}",
                    id.0,
                    node.role(),
                    clickable,
                    node.children().len(),
                    selection,
                    text,
                );
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
