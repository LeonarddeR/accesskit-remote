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
//! `--validate` checks the invariants `accesskit_consumer` enforces before it
//! will accept an update. It panics rather than complains when they are
//! violated, taking the whole consumer process with it, so checking here is the
//! difference between a diagnosable provider bug and a crashed screen reader.
//!
//! Usage: dump_tree [--app <substr>] [--tree] [--validate]

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
    let mut validate = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app" => filter = args.next().map(|f| f.to_lowercase()),
            "--tree" => show_tree = true,
            "--validate" => validate = true,
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
    let mut invalid = 0usize;
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
            if validate {
                let problems = validate_update(&update);
                if problems.is_empty() {
                    println!("  validate: OK");
                } else {
                    for problem in &problems {
                        println!("  INVALID: {problem}");
                    }
                    invalid += problems.len();
                }
            }
        }
    }

    println!(
        "\ntotal: {total_nodes} nodes ({total_visible} visible) across {windows_seen} window(s) \
         of {} application(s) in {:.0?}",
        apps.len(),
        started.elapsed(),
    );
    if validate {
        if invalid == 0 {
            println!("every tree satisfies the consumer's structural invariants");
        } else {
            println!("{invalid} structural problem(s) — a consumer would panic on these");
            std::process::exit(1);
        }
    }
}

/// The invariants `accesskit_consumer` asserts when it accepts an update.
///
/// Violating any of them aborts the consumer process rather than returning an
/// error, so a provider that can produce one has a crash, not a glitch. Both
/// checked here were observed live before being fixed.
#[cfg(target_os = "macos")]
fn validate_update(update: &accesskit::TreeUpdate) -> Vec<String> {
    use std::collections::{HashMap, HashSet};
    let mut problems = Vec::new();
    let present: HashSet<accesskit::NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();

    // "TreeUpdate includes duplicate child": one node named by two parents, or
    // twice by one.
    let mut claimed: HashMap<accesskit::NodeId, Vec<accesskit::NodeId>> = HashMap::new();
    for (parent, node) in &update.nodes {
        let mut seen_here = HashSet::new();
        for child in node.children() {
            if !seen_here.insert(*child) {
                problems.push(format!("node {} lists child {} twice", parent.0, child.0));
            }
            claimed.entry(*child).or_default().push(*parent);
        }
    }
    for (child, parents) in &claimed {
        if parents.len() > 1 {
            let names: Vec<String> = parents.iter().map(|p| p.0.to_string()).collect();
            problems.push(format!("child {} claimed by parents [{}]", child.0, names.join(", ")));
        }
    }

    // "children ids which are neither in the current tree nor the ID of
    // another node from the update".
    for (parent, node) in &update.nodes {
        for child in node.children() {
            if !present.contains(child) {
                problems.push(format!(
                    "node {} names child {}, which is not in the update",
                    parent.0, child.0
                ));
            }
        }
    }

    if let Some(tree) = &update.tree {
        if !present.contains(&tree.root) {
            problems.push(format!("root {} is not in the update", tree.root.0));
        }
    }
    if !present.contains(&update.focus) {
        problems.push(format!("focus {} is not in the update", update.focus.0));
    }
    problems
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
        // The actions a node *declares* are what a consumer can offer: the
        // Windows adapter gates InvokePattern on Action::Click, and a mirror
        // that executes actions while declaring none is unpressable.
        let actions: Vec<String> = [
            accesskit::Action::Click,
            accesskit::Action::Focus,
            accesskit::Action::Expand,
            accesskit::Action::Increment,
            accesskit::Action::SetValue,
        ]
        .iter()
        .filter(|a| node.supports_action(**a))
        .map(|a| format!("{a:?}"))
        .collect();
        println!(
            "{}{:?}{name}{}{}{}",
            "  ".repeat(depth),
            node.role(),
            if actions.is_empty() { String::new() } else { format!(" <{}>", actions.join(",")) },
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
