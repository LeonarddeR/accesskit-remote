//! Walks every window on the desktop into real `accesskit::TreeUpdate`s and
//! reports what they cost and what a consumer would see.
//!
//! The port of `accesskit_remote_atspi`'s `dump_tree`, and it measures the same
//! two things. **Walk cost** is the budget every later phase is spent against —
//! time it against an idle application, since AX reads serialise on the target
//! app's main thread. **Post-filter node count** is the tree-inflation metric:
//! `accesskit_consumer::common_filter` drops exactly `GenericContainer` and
//! `TextRun`, so it is what a screen reader actually encounters, and it is the
//! number to watch whenever the role map is broadened.
//!
//! Usage: dump_tree [--app <substr>] [--tree]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("dump_tree runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    use accesskit_remote_ax::element::NodeIdMap;
    use accesskit_remote_ax::names::Names;
    use accesskit_remote_ax::{ax, trust, walk};
    use std::collections::BTreeMap;
    use std::time::Instant;

    if !trust::is_trusted() {
        eprintln!("{}", trust::untrusted_message());
        std::process::exit(2);
    }

    let mut filter = None;
    let mut show_tree = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app" => filter = args.next().map(|f| f.to_lowercase()),
            "--tree" => show_tree = true,
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let names = Names::new();
    // Start the clock where the first read happens, as the AT-SPI instrument
    // does: enumeration is part of the cost, not a preamble to it.
    let started = Instant::now();
    let mut apps = ax::running_apps();
    if let Some(filter) = &filter {
        apps.retain(|app| app.info.name.to_lowercase().contains(filter));
    }

    let (mut total_nodes, mut total_visible, mut windows_seen) = (0usize, 0usize, 0usize);
    for app in &apps {
        for window in ax::windows_of(app, &names).unwrap_or_default() {
            let walk_started = Instant::now();
            let nodes = walk::walk_window(window.key.clone(), &names);
            let mut ids = NodeIdMap::new();
            let Some(update) = walk::build_window_update(&nodes, &mut ids) else {
                continue;
            };
            let elapsed = walk_started.elapsed();
            windows_seen += 1;

            let mut by_role: BTreeMap<String, usize> = BTreeMap::new();
            for (_, node) in &update.nodes {
                *by_role.entry(format!("{:?}", node.role())).or_default() += 1;
            }
            let visible = update
                .nodes
                .iter()
                .filter(|(_, node)| {
                    !matches!(
                        node.role(),
                        accesskit::Role::GenericContainer | accesskit::Role::TextRun
                    )
                })
                .count();
            total_nodes += update.nodes.len();
            total_visible += visible;

            println!(
                "\n{} | {} | {:?}{} | id={:?}{}",
                app.info.name,
                app.info.app_id.as_deref().unwrap_or("-"),
                window.title,
                window.subrole.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default(),
                window.native_window_id,
                if window.active { " | FOCUSED" } else { "" },
            );
            println!(
                "  {} nodes ({visible} reach the consumer) in {elapsed:.0?}, focus {:?}",
                update.nodes.len(),
                update.focus,
            );
            let mut roles: Vec<_> = by_role.iter().collect();
            roles.sort_by(|a, b| b.1.cmp(a.1));
            println!(
                "  roles: {}",
                roles
                    .iter()
                    .map(|(role, count)| format!("{role}={count}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if show_tree {
                print_tree(&update);
            }
        }
    }

    println!(
        "\ntotal: {total_nodes} nodes ({total_visible} visible) across {windows_seen} window(s) \
         of {} application(s) in {:.0?}",
        apps.len(),
        started.elapsed(),
    );
}

/// Prints the update as a tree, in the shape the consumer will hold it.
#[cfg(target_os = "macos")]
fn print_tree(update: &accesskit::TreeUpdate) {
    use std::collections::HashMap;
    let nodes: HashMap<accesskit::NodeId, &accesskit::Node> =
        update.nodes.iter().map(|(id, node)| (*id, node)).collect();
    let Some(root) = update.tree.as_ref().map(|tree| tree.root) else {
        return;
    };
    let mut stack = vec![(root, 1usize)];
    while let Some((id, depth)) = stack.pop() {
        let Some(node) = nodes.get(&id) else {
            continue;
        };
        let name = node
            .label()
            .or_else(|| node.value())
            .map(|text| format!(" {:?}", truncate(text, 40)))
            .unwrap_or_default();
        println!(
            "{}{:?}{name}{}{}",
            "  ".repeat(depth),
            node.role(),
            if node.is_disabled() { " [disabled]" } else { "" },
            if id == update.focus { " <- focus" } else { "" },
        );
        for child in node.children().iter().rev() {
            stack.push((*child, depth + 1));
        }
    }
}

#[cfg(target_os = "macos")]
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars().take(width - 1).collect::<String>() + "…"
}
