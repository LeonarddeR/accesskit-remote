//! Drives real controls through the whole action path and reports what worked.
//!
//! The `action_drive` analogue. It goes through `AxSource::perform`, so what it
//! exercises is exactly what a UIA gesture from the consumer would exercise —
//! node id resolution, role lookup, planning, and execution — rather than
//! calling AX directly and proving something easier.
//!
//! Each target is driven and then re-read, so the report says whether the
//! application actually *changed*, not merely whether a call returned success.
//! That distinction is the whole point: AT-SPI taught that a toolkit will
//! happily accept a call and do nothing.
//!
//! **This clicks real controls in real applications.** It therefore plans only
//! by default and needs `--drive` to actually act — pointing an indiscriminate
//! version of this at System Settings would toggle the machine's actual
//! settings. Prefer `--target` to name one control.
//!
//! Usage: action_drive [--app <substr>] [--target <substr>] [--drive]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("action_drive runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    use accesskit::{Action, Role};
    use accesskit_remote_ax::drive::{plan_action, ActionContext};
    use accesskit_remote_ax::names::Names;
    use accesskit_remote_ax::{attr, ax, trust, walk};

    if !trust::is_trusted() {
        eprintln!("{}", trust::untrusted_message());
        std::process::exit(2);
    }

    let mut filter = None;
    // Planning is the default; acting is opt-in. This tool activates controls
    // in whatever application it is pointed at, and the cost of a stray run
    // against System Settings is the user's actual configuration.
    let mut drive = false;
    let mut target: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app" => filter = args.next().map(|f| f.to_lowercase()),
            "--drive" => drive = true,
            "--target" => target = args.next().map(|t| t.to_lowercase()),
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

    println!(
        "{:<22} {:<20} {:<26} {:<10} {:<28} {}",
        "APPLICATION", "ROLE", "NAME", "ACTION", "PLAN", "RESULT"
    );

    let (mut planned, mut performed, mut changed) = (0usize, 0usize, 0usize);
    for app in &apps {
        for window in ax::windows_of(app, &names).unwrap_or_default() {
            for node in walk::walk_window(window.key.clone(), &names) {
                let role = node.accesskit_role();
                // Only the roles a reader would actually activate, and only
                // named ones, so the report is legible and the driving is not
                // indiscriminate.
                let action = match role {
                    Role::Button | Role::CheckBox | Role::RadioButton | Role::Switch => {
                        Action::Click
                    }
                    Role::Slider => Action::Increment,
                    _ => continue,
                };
                if node.name().is_empty() {
                    continue;
                }
                if let Some(target) = &target {
                    if !node.name().to_lowercase().contains(target) {
                        continue;
                    }
                }

                let element = node.key.element();
                let context = ActionContext {
                    role,
                    actions: attr::action_names(element).unwrap_or_default(),
                    settable: settable_of(element, &names),
                    value: attr::value(element, &names.value)
                        .ok()
                        .flatten()
                        .as_deref()
                        .and_then(attr::as_f64),
                };
                let plan = plan_action(&context, action, None);
                if plan.is_empty() {
                    println!(
                        "{:<22} {:<20} {:<26} {:<10} {:<28} {}",
                        truncate(&app.info.name, 22),
                        format!("{role:?}"),
                        truncate(node.name(), 26),
                        format!("{action:?}"),
                        "-",
                        "no route",
                    );
                    continue;
                }
                planned += 1;
                let plan_text = format!("{:?}", plan.first().unwrap());
                if !drive {
                    println!(
                        "{:<22} {:<20} {:<26} {:<10} {:<28} {}",
                        truncate(&app.info.name, 22),
                        format!("{role:?}"),
                        truncate(node.name(), 26),
                        format!("{action:?}"),
                        truncate(&plan_text, 28),
                        "planned only (pass --drive to act)",
                    );
                    continue;
                }

                // The *window's* state, not the element's. Pressing a
                // calculator digit does not change the button — it changes the
                // display — so fingerprinting the target alone reports "no
                // change" for actions that plainly worked.
                let before = window_fingerprint(&window.key, &names);
                let request = accesskit::ActionRequest {
                    action,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: accesskit::NodeId(0),
                    data: None,
                };
                let done = ax::perform(&node.key, &request, role, &names);
                std::thread::sleep(std::time::Duration::from_millis(250));
                let after = window_fingerprint(&window.key, &names);

                let result = match (&done, before == after) {
                    (Some(_), false) => {
                        performed += 1;
                        changed += 1;
                        "CHANGED"
                    }
                    (Some(_), true) => {
                        performed += 1;
                        "accepted, no visible change"
                    }
                    (None, _) => "every route refused",
                };
                println!(
                    "{:<22} {:<20} {:<26} {:<10} {:<28} {}",
                    truncate(&app.info.name, 22),
                    format!("{role:?}"),
                    truncate(node.name(), 26),
                    format!("{action:?}"),
                    truncate(&plan_text, 28),
                    result,
                );
            }
        }
    }

    println!("\n{planned} planned, {performed} accepted, {changed} produced a visible change");
    if planned > 0 && changed == 0 && drive {
        println!(
            "Nothing changed. Either these controls are inert, or the plan is \
             being accepted and doing nothing — which AT-SPI showed is a real \
             toolkit behaviour, not a hypothetical."
        );
    }
}

/// Every value in the window, as a before/after fingerprint.
///
/// An action's effect usually lands somewhere other than the control that was
/// activated — a calculator digit changes the display, a checkbox may reveal a
/// pane — so the honest question is whether *the window* changed.
#[cfg(target_os = "macos")]
fn window_fingerprint(
    root: &accesskit_remote_ax::element::ElementKey,
    names: &accesskit_remote_ax::names::Names,
) -> Vec<String> {
    use accesskit_remote_ax::{attr, walk};
    walk::walk_window(root.clone(), names)
        .iter()
        .map(|node| {
            let value = node
                .value
                .as_deref()
                .and_then(|v| {
                    attr::as_string(v).or_else(|| attr::as_f64(v).map(|n| n.to_string()))
                })
                .unwrap_or_default();
            format!("{}|{}|{value}", node.role, node.name())
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn settable_of(
    element: &objc2_application_services::AXUIElement,
    names: &accesskit_remote_ax::names::Names,
) -> Vec<accesskit_remote_ax::drive::Settable> {
    use accesskit_remote_ax::attr;
    use accesskit_remote_ax::drive::Settable;
    let mut out = Vec::new();
    for (what, name) in [
        (Settable::Value, &names.value),
        (Settable::Focused, &names.focused),
        (Settable::Selected, &names.selected),
    ] {
        if attr::is_settable(element, name).unwrap_or(false) {
            out.push(what);
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars().take(width - 1).collect::<String>() + "…"
}
