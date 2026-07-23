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
        println!(
            "  window {} | app {:?} | title {:?} | {} nodes",
            descriptor.id.0,
            descriptor.app.name,
            descriptor.title,
            update.nodes.len(),
        );
        for (id, node) in update.nodes.iter() {
            let text = node.value().or_else(|| node.label());
            let clickable = node.supports_action(accesskit::Action::Click);
            if text.is_some() || clickable {
                println!(
                    "      node {} {:?} click={} {:?}",
                    id.0,
                    node.role(),
                    clickable,
                    text,
                );
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
