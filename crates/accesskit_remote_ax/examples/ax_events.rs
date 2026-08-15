//! Raw `AXObserver` monitor: what notifications applications actually deliver.
//!
//! **The most important new instrument in this crate**, and the one with no
//! AT-SPI counterpart. On Linux, `busctl monitor` gave ground truth on which
//! signals a toolkit really emits — it is how "GTK4 emits no `window:create`"
//! and "GTK4 routes selection through state changes" were established, both of
//! which changed the mirror's design. macOS ships no such tool, so this is it.
//!
//! Registers every notification in `observe::SUBSCRIPTIONS` and prints each one
//! as it arrives, with its route, timestamp and source element. Two things to
//! watch for: notifications an application *refuses to register* (printed at
//! start-up), and storms — bursts that justify the debounce constants rather
//! than inheriting them from the AT-SPI source on faith.
//!
//! Usage: ax_events [--app <substr>] [--seconds N]
//!
//! Then go and use the application: type, click, switch windows, open a menu.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ax_events runs on macOS only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    use accesskit_remote_ax::attr;
    use accesskit_remote_ax::names::Names;
    use accesskit_remote_ax::observe::{self, AppObserver, Route};
    use accesskit_remote_ax::{ax, trust};
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    if !trust::is_trusted() {
        eprintln!("{}", trust::untrusted_message());
        std::process::exit(2);
    }

    let mut filter = None;
    let mut seconds = 30u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app" => filter = args.next().map(|f| f.to_lowercase()),
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(30),
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

    let queue = Rc::new(RefCell::new(Vec::new()));
    let mut observers = Vec::new();
    for app in &apps {
        match AppObserver::new(app.pid, &app.element, &queue) {
            Ok((observer, declined)) => {
                // Which notifications an application refuses is itself a
                // finding: it bounds what can ever be reflected live for it,
                // exactly as GTK4's missing window:create did on Linux.
                if declined.is_empty() {
                    println!("{:<26} registered all {}", app.info.name, observe::SUBSCRIPTIONS.len());
                } else {
                    println!(
                        "{:<26} refused {}/{}: {}",
                        app.info.name,
                        declined.len(),
                        observe::SUBSCRIPTIONS.len(),
                        declined.join(", ")
                    );
                }
                observers.push(observer);
            }
            Err(error) => println!("{:<26} observer failed: {error}", app.info.name),
        }
    }
    if observers.is_empty() {
        println!("no applications to observe");
        return;
    }
    // A registered observer whose source is not on *this* thread's run loop
    // delivers nothing, silently. Prove the attachment rather than assume it.
    if let Some(run_loop) = CFRunLoop::current() {
        for observer in &observers {
            let source = observer.run_loop_source();
            let attached =
                unsafe { run_loop.contains_source(Some(&source), kCFRunLoopDefaultMode) };
            if !attached {
                println!("WARNING: pid {} source is NOT on this run loop", observer.pid());
            }
        }
    }

    println!(
        "\nwatching {} application(s) for {seconds}s — go and use them\n",
        observers.len()
    );
    println!("{:<10} {:<10} {:<30} {:<22} {}", "TIME", "ROUTE", "NOTIFICATION", "ROLE", "NAME");

    let started = Instant::now();
    let deadline = Duration::from_secs(seconds);
    let mut counts: BTreeMap<(String, Route), usize> = BTreeMap::new();
    let mut total = 0usize;

    while started.elapsed() < deadline {
        // Same shape as the worker's loop: run the loop briefly, then drain.
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
        let drained: Vec<_> = queue.borrow_mut().drain(..).collect();
        for event in drained {
            let route = observe::route(&event.notification);
            *counts.entry((event.notification.clone(), route)).or_default() += 1;
            total += 1;
            let role = attr::string(&event.element, &names.role)
                .ok()
                .flatten()
                .unwrap_or_else(|| "-".into());
            let name = attr::string(&event.element, &names.title)
                .ok()
                .flatten()
                .filter(|t| !t.is_empty())
                .or_else(|| attr::string(&event.element, &names.description).ok().flatten())
                .unwrap_or_default();
            println!(
                "{:>8.2}s  {:<10} {:<30} {:<22} {}",
                started.elapsed().as_secs_f64(),
                format!("{route:?}"),
                truncate(&event.notification, 30),
                truncate(&role, 22),
                truncate(&name, 40),
            );
        }
    }

    let fired = observe::CALLBACKS.load(std::sync::atomic::Ordering::Relaxed);
    println!("\n{total} notification(s) queued; callback fired {fired} time(s) in {seconds}s");
    if total == 0 {
        println!(
            "Nothing arrived. Either the applications were idle, or they deliver \
             none of what we subscribe to — which is the finding."
        );
        return;
    }
    println!("\n{:<30} {:<10} {}", "NOTIFICATION", "ROUTE", "COUNT");
    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    for ((notification, route), count) in rows {
        println!("{:<30} {:<10} {count}", truncate(&notification, 30), format!("{route:?}"));
    }
    // Observers must be dropped on this thread: it owns the run loop holding
    // their sources.
    drop(observers);
}

#[cfg(target_os = "macos")]
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars().take(width - 1).collect::<String>() + "…"
}
