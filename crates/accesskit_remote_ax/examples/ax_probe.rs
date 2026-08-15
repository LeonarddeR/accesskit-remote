//! Raw AX characterization probe: what each application actually publishes,
//! read straight off the API with no mirror in between.
//!
//! The AT-SPI work established every toolkit finding in `docs/next-steps.md`
//! with two instruments — `charprobe` for static shape and `busctl monitor`
//! for the live signal stream. This is the first of those. It exists to settle
//! questions *before* any mapping code is written, because each answer changes
//! what the mapping has to be.
//!
//! Usage:
//!   ax_probe                             summary of every application
//!   ax_probe --app Safari                dump matching applications' trees
//!   ax_probe --app Safari --attrs        ... with every attribute and action
//!   ax_probe --identity                  re-walk element identity survival
//!   ax_probe --opt-in                    what AXManualAccessibility changes
//!   ax_probe --menu-bar                  whether menu bars sit outside AXWindows
//!
//! Options: --depth N (default 6), --timeout MS (default 1000), --max N.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ax_probe runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    probe::run()
}

#[cfg(target_os = "macos")]
mod probe {
    use accesskit_remote_ax::attr;
    use accesskit_remote_ax::ax::{self, App};
    use accesskit_remote_ax::element::ElementKey;
    use accesskit_remote_ax::names::Names;
    use accesskit_remote_ax::opt_in::{self, OptIn};
    use accesskit_remote_ax::{trust, window_id};
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::CFRetained;
    use std::collections::{BTreeMap, HashSet};
    use std::time::{Duration, Instant};

    struct Options {
        app_filter: Option<String>,
        attrs: bool,
        identity: bool,
        opt_in: bool,
        menu_bar: bool,
        depth: usize,
        max_nodes: usize,
        timeout: f32,
    }

    pub fn run() {
        let options = parse_args();

        // The grant gates everything. Without it AX does not error loudly, it
        // just reports empty trees, so a probe that skipped this check would
        // look like a working tool reporting a broken desktop.
        println!("Accessibility grant: {}", if trust::is_trusted() { "yes" } else { "NO" });
        if !trust::is_trusted() {
            println!("\n{}", trust::untrusted_message());
            std::process::exit(2);
        }
        println!(
            "_AXUIElementGetWindow: {}",
            if window_id::is_available() { "available" } else { "UNAVAILABLE (native window ids will be None)" }
        );
        println!("messaging timeout: {:.0}ms\n", options.timeout * 1000.0);

        let names = Names::new();
        let mut apps = ax::running_apps();
        if let Some(filter) = &options.app_filter {
            let needle = filter.to_lowercase();
            apps.retain(|app| app.info.name.to_lowercase().contains(&needle));
        }
        if apps.is_empty() {
            println!("no matching applications");
            return;
        }
        for app in &apps {
            let _ = attr::set_timeout(&app.element, options.timeout);
        }

        if options.opt_in {
            report_opt_in(&apps, &names, &options);
        } else if options.identity {
            report_identity(&apps, &names, &options);
        } else if options.menu_bar {
            report_menu_bar(&apps, &names, &options);
        } else if options.app_filter.is_some() {
            for app in &apps {
                dump_app(app, &names, &options);
            }
        } else {
            summarize(&apps, &names, &options);
        }
    }

    // ------------------------------------------------------------- summary

    /// One line per application: how much tree it publishes and what it cost.
    /// The walk budget is the number every later phase is spent against, so it
    /// is measured from the first read, and against an *idle* app — a busy one
    /// inflates it many-fold, exactly as AT-SPI's did.
    fn summarize(apps: &[App], names: &Names, options: &Options) {
        println!(
            "{:<28} {:<34} {:>5} {:>7} {:>8} {:>8}  {}",
            "APPLICATION", "BUNDLE ID", "PID", "WINDOWS", "NODES", "WALK", "NOTE"
        );
        let mut total_nodes = 0usize;
        let mut total_time = Duration::ZERO;
        for app in apps {
            let started = Instant::now();
            let windows = ax::windows_of(app, names).unwrap_or_default();
            let mut nodes = 0;
            for window in &windows {
                nodes += walk(window.key.clone(), names, options).len();
            }
            let elapsed = started.elapsed();
            total_nodes += nodes;
            total_time += elapsed;

            // An app with windows but no contents is the Chromium signature.
            let note = if windows.is_empty() {
                "no windows"
            } else if nodes <= windows.len() {
                "EMPTY TREE (try --opt-in)"
            } else if window_id::is_available()
                && windows.iter().all(|w| w.native_window_id.is_none())
            {
                "no window ids"
            } else {
                ""
            };
            println!(
                "{:<28} {:<34} {:>5} {:>7} {:>8} {:>7.0?}  {}",
                truncate(&app.info.name, 28),
                truncate(app.info.app_id.as_deref().unwrap_or("-"), 34),
                app.pid,
                windows.len(),
                nodes,
                elapsed,
                note,
            );
        }
        println!(
            "\ntotal: {total_nodes} nodes across {} application(s) in {total_time:.0?}",
            apps.len()
        );
        println!(
            "depth cap {}, node cap {} per window — a capped walk under-reports",
            options.depth, options.max_nodes
        );
    }

    // ------------------------------------------------------------- identity

    /// **The measurement this phase exists for.**
    ///
    /// The delta architecture assumes a re-walk mostly re-encounters the *same*
    /// elements, so node ids stay stable and an unchanged node emits nothing.
    /// On AT-SPI that was guaranteed by stable object paths. On AX it is a
    /// property of the toolkit, and LibreOffice already proved on Linux that a
    /// toolkit may mint a fresh object per visit.
    ///
    /// A low survival ratio here means every re-walk is a full tree replacement
    /// and the whole delta path buys nothing — the signal to switch to a
    /// positional key before building anything on top.
    fn report_identity(apps: &[App], names: &Names, options: &Options) {
        println!("Re-walk element identity survival (CFEqual across two walks)\n");
        println!("{:<28} {:<26} {:>7} {:>7} {:>9}", "APPLICATION", "WINDOW", "WALK1", "WALK2", "SURVIVED");
        let mut worst = 100.0f64;
        for app in apps {
            let Ok(windows) = ax::windows_of(app, names) else {
                continue;
            };
            for window in windows {
                let first = walk(window.key.clone(), names, options);
                let second = walk(window.key.clone(), names, options);
                if first.is_empty() {
                    continue;
                }
                let seen: HashSet<&ElementKey> = first.iter().collect();
                let survived = second.iter().filter(|key| seen.contains(key)).count();
                let ratio = if second.is_empty() {
                    0.0
                } else {
                    survived as f64 * 100.0 / second.len() as f64
                };
                worst = worst.min(ratio);
                println!(
                    "{:<28} {:<26} {:>7} {:>7} {:>8.1}%",
                    truncate(&app.info.name, 28),
                    truncate(&window.title, 26),
                    first.len(),
                    second.len(),
                    ratio,
                );
            }
        }
        println!("\nworst survival: {worst:.1}%");
        println!(
            "Near 100% means AXUIElement identity is stable enough for id reuse and\n\
             minimal deltas. Substantially below that, a positional key is needed\n\
             instead — see `element::ElementKey`."
        );
    }

    // --------------------------------------------------------------- opt-in

    /// What `AXManualAccessibility` changes, and whether it disturbs the
    /// window layout. Both halves matter: the attribute is only worth setting
    /// if it reveals a tree, and only *safe* to set if the frames do not move.
    fn report_opt_in(apps: &[App], names: &Names, options: &Options) {
        println!("AXManualAccessibility: effect on tree size and window frames\n");
        for app in apps {
            let before = measure(app, names, options);
            let response = opt_in::request(&app.element, names);
            // Chromium builds its tree asynchronously after accepting.
            if response == OptIn::Accepted {
                std::thread::sleep(Duration::from_millis(750));
            }
            let after = measure(app, names, options);
            println!(
                "{:<28} {:<18} nodes {:>5} -> {:<5} frames {}",
                truncate(&app.info.name, 28),
                match response {
                    OptIn::Accepted => "accepted".to_owned(),
                    OptIn::NotApplicable => "not applicable".to_owned(),
                    OptIn::Failed(e) => format!("failed: {e}"),
                },
                before.0,
                after.0,
                if before.1 == after.1 { "unchanged" } else { "MOVED" },
            );
        }
        println!(
            "\n'MOVED' on any application would mean this opt-in is not the\n\
             side-effect-free one it is documented to be. AXEnhancedUserInterface\n\
             is the lever that does move windows, and is deliberately never set."
        );
    }

    /// Total node count and every window's frame, for before/after comparison.
    fn measure(app: &App, names: &Names, options: &Options) -> (usize, Vec<(i64, i64, i64, i64)>) {
        let windows = ax::windows_of(app, names).unwrap_or_default();
        let mut nodes = 0;
        let mut frames = Vec::new();
        for window in &windows {
            nodes += walk(window.key.clone(), names, options).len();
            let position = attr::value(window.key.element(), &names.position)
                .ok()
                .flatten()
                .and_then(|v| attr::as_point(&v));
            let size = attr::value(window.key.element(), &names.size)
                .ok()
                .flatten()
                .and_then(|v| attr::as_size(&v));
            if let (Some(p), Some(s)) = (position, size) {
                frames.push((p.x as i64, p.y as i64, s.width as i64, s.height as i64));
            }
        }
        (nodes, frames)
    }

    // ------------------------------------------------------------- menu bar

    /// Whether an application's menu bar is reachable from its windows.
    ///
    /// If it is not — if `AXMenuBar` is only a sibling of `AXWindows` on the
    /// application element — then a window-rooted walk skips every app's entire
    /// menu tree for free. On Linux the equivalent cost was real: LibreOffice
    /// published ~770 menu items per window.
    fn report_menu_bar(apps: &[App], names: &Names, options: &Options) {
        println!("Menu bars: present, and reachable from a window-rooted walk?\n");
        for app in apps {
            let menu_bar = attr::element(&app.element, &names.menu_bar).ok().flatten();
            let Some(menu_bar) = menu_bar else {
                println!("{:<28} no menu bar", truncate(&app.info.name, 28));
                continue;
            };
            let menu_key = ElementKey::new(app.pid, menu_bar);
            let menu_nodes = walk(menu_key.clone(), names, options).len();
            let windows = ax::windows_of(app, names).unwrap_or_default();
            let mut reachable = false;
            for window in &windows {
                if walk(window.key.clone(), names, options).contains(&menu_key) {
                    reachable = true;
                    break;
                }
            }
            println!(
                "{:<28} {:>5} menu nodes, {}",
                truncate(&app.info.name, 28),
                menu_nodes,
                if reachable {
                    "REACHABLE from a window (they cost us)"
                } else {
                    "not reachable from any window (free)"
                },
            );
        }
    }

    // ----------------------------------------------------------------- dump

    /// The full static shape of one application: role, subrole, and — with
    /// `--attrs` — every attribute name with its settable flag, plus actions.
    /// This is the AX ground truth the role and state maps get written against.
    fn dump_app(app: &App, names: &Names, options: &Options) {
        println!("=== {} ({}) pid {}", app.info.name, app.info.app_id.as_deref().unwrap_or("-"), app.pid);
        match opt_in::is_requested(&app.element, names) {
            Some(set) => println!("    Chromium-based; AXManualAccessibility = {set}"),
            None => println!("    native (no AXManualAccessibility)"),
        }
        let windows = match ax::windows_of(app, names) {
            Ok(windows) => windows,
            Err(error) => {
                println!("    windows unreadable: {error}");
                return;
            }
        };
        for window in windows {
            println!(
                "\n--- window {:?} id={:?} active={}",
                window.title, window.native_window_id, window.active
            );
            let started = Instant::now();
            let mut roles: BTreeMap<String, usize> = BTreeMap::new();
            dump_element(&window.key, names, options, 0, &mut roles);
            println!("    walked in {:.0?}", started.elapsed());
            println!("    roles: {}", format_histogram(&roles));
        }
    }

    fn dump_element(
        key: &ElementKey,
        names: &Names,
        options: &Options,
        depth: usize,
        roles: &mut BTreeMap<String, usize>,
    ) {
        if depth > options.depth {
            return;
        }
        let element = key.element();
        let role = read(element, &names.role).unwrap_or_else(|| "?".into());
        let subrole = read(element, &names.subrole);
        let title = read(element, &names.title);
        *roles.entry(role.clone()).or_default() += 1;

        let indent = "  ".repeat(depth + 1);
        let subrole = subrole.map(|s| format!("/{s}")).unwrap_or_default();
        let title = title
            .filter(|t| !t.is_empty())
            .map(|t| format!(" {:?}", truncate(&t, 48)))
            .unwrap_or_default();
        println!("{indent}{role}{subrole}{title}  [{key:?}]");

        if options.attrs {
            // The attribute list *is* the capability surface: AX has no
            // interface set to enumerate the way AT-SPI does.
            if let Ok(attributes) = attr::names(element) {
                for name in attributes {
                    let cf = objc2_core_foundation::CFString::from_str(&name);
                    let settable = attr::is_settable(element, &cf).unwrap_or(false);
                    let value = attr::value(element, &cf)
                        .ok()
                        .flatten()
                        .map(|v| describe(&v))
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{indent}    {}{name} = {}",
                        if settable { "w " } else { "  " },
                        truncate(&value, 60)
                    );
                }
            }
            if let Ok(actions) = attr::action_names(element) {
                if !actions.is_empty() {
                    println!("{indent}    actions: {}", actions.join(", "));
                }
            }
        }

        for child in attr::elements(element, &names.children).unwrap_or_default() {
            dump_element(&ElementKey::new(key.pid(), child), names, options, depth + 1, roles);
        }
    }

    // ----------------------------------------------------------------- util

    /// Breadth-first element collection, bounded in both depth and count so a
    /// pathological application cannot hang the probe.
    fn walk(root: ElementKey, names: &Names, options: &Options) -> Vec<ElementKey> {
        let mut out = Vec::new();
        let mut queue = std::collections::VecDeque::from([(root, 0usize)]);
        let mut seen: HashSet<ElementKey> = HashSet::new();
        while let Some((key, depth)) = queue.pop_front() {
            if out.len() >= options.max_nodes || !seen.insert(key.clone()) {
                continue;
            }
            out.push(key.clone());
            if depth >= options.depth {
                continue;
            }
            for child in attr::elements(key.element(), &names.children).unwrap_or_default() {
                queue.push_back((ElementKey::new(key.pid(), child), depth + 1));
            }
        }
        out
    }

    fn read(element: &AXUIElement, name: &objc2_core_foundation::CFString) -> Option<String> {
        attr::string(element, name).ok().flatten()
    }

    /// A one-line rendering of any CF value, for the attribute dump. Elements
    /// are never described via `CFCopyDescription` — that is IPC into the
    /// target app, and would deadlock against the app this probe is diagnosing.
    fn describe(value: &objc2_core_foundation::CFType) -> String {
        if let Some(text) = attr::as_string(value) {
            return format!("{text:?}");
        }
        if let Some(number) = attr::as_f64(value) {
            return number.to_string();
        }
        if let Some(point) = attr::as_point(value) {
            return format!("({}, {})", point.x, point.y);
        }
        if let Some(size) = attr::as_size(value) {
            return format!("{}x{}", size.width, size.height);
        }
        if let Some(flag) = attr::as_bool(value) {
            return flag.to_string();
        }
        let elements = attr::as_elements(value);
        if !elements.is_empty() {
            return format!("[{} element(s)]", elements.len());
        }
        "<opaque>".to_owned()
    }

    fn format_histogram(roles: &BTreeMap<String, usize>) -> String {
        let mut pairs: Vec<(&String, &usize)> = roles.iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1));
        pairs
            .iter()
            .take(12)
            .map(|(role, count)| format!("{role}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn truncate(text: &str, width: usize) -> String {
        if text.chars().count() <= width {
            return text.to_owned();
        }
        text.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    }

    fn parse_args() -> Options {
        let mut options = Options {
            app_filter: None,
            attrs: false,
            identity: false,
            opt_in: false,
            menu_bar: false,
            depth: 6,
            max_nodes: 5000,
            timeout: 1.0,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--app" => options.app_filter = args.next(),
                "--attrs" => options.attrs = true,
                "--identity" => options.identity = true,
                "--opt-in" => options.opt_in = true,
                "--menu-bar" => options.menu_bar = true,
                "--depth" => options.depth = args.next().and_then(|v| v.parse().ok()).unwrap_or(6),
                "--max" => {
                    options.max_nodes = args.next().and_then(|v| v.parse().ok()).unwrap_or(5000)
                }
                "--timeout" => {
                    let ms: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1000.0);
                    options.timeout = ms / 1000.0;
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    std::process::exit(2);
                }
            }
        }
        options
    }

    /// Unused import guard: `CFRetained` is named in signatures above.
    #[allow(dead_code)]
    fn _types(_: CFRetained<AXUIElement>) {}
}
