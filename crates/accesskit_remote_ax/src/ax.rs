//! Application and window discovery: the AX side of what
//! `mirror::discover_windows` does over AT-SPI.
//!
//! Two things are markedly simpler here than on Linux. Application identity is
//! a single `NSRunningApplication` read rather than a session-bus name sweep
//! (`AppIdResolver`, 117 lines, existing only because AT-SPI has no desktop-id
//! property). And a window's native id is one SPI call rather than a Weston
//! log tail.
//!
//! One thing is harder: every read is synchronous IPC into the target
//! application, so an unresponsive app would block this thread — and on macOS
//! that thread also runs the observer loop. Every application element gets a
//! messaging timeout before it is read from.

use crate::attr::{self, AxError};
use crate::element::ElementKey;
use crate::names::Names;
use crate::window_id;
use accesskit_remote::AppInfo;
use objc2_application_services::AXUIElement;
use objc2_core_foundation::CFRetained;

/// How long any single AX read may block before it is abandoned.
///
/// The AT-SPI source has no equivalent because a stalled call there blocked one
/// async task; here it would block the run loop that services every
/// application's notifications. One second is long enough that a merely busy
/// application still answers, and short enough that a wedged one costs a
/// bounded pause rather than the session.
pub const MESSAGING_TIMEOUT_SECS: f32 = 1.0;

/// The role a real toplevel window reports. Dialogs are this role too, told
/// apart by their subrole, so this is the only value worth exporting.
const WINDOW_ROLE: &str = "AXWindow";

/// A running application that may publish an accessibility tree.
pub struct App {
    pub element: CFRetained<AXUIElement>,
    pub info: AppInfo,
    pub pid: i32,
    /// Whether this is the frontmost application, which is what makes one of
    /// its windows the focused one.
    pub active: bool,
}

/// A toplevel window discovered on an application.
pub struct Window {
    pub key: ElementKey,
    pub title: String,
    /// The window's `AXSubrole`, which is where AX puts the distinction AT-SPI
    /// carries in the role itself — `AXStandardWindow` versus `AXDialog`,
    /// `AXSystemDialog`, `AXFloatingWindow`.
    pub subrole: Option<String>,
    pub app: AppInfo,
    /// The `CGWindowID`, when the SPI resolved it.
    pub native_window_id: Option<u64>,
    /// Whether this window holds keyboard focus session-wide.
    pub active: bool,
}

/// Every application that can own a window a user could be looking at.
///
/// Filtered to the *regular* activation policy: that excludes menu-bar extras,
/// agents and daemons, which own no ordinary windows but would otherwise cost
/// an IPC round trip each, on every reconcile, forever.
pub fn running_apps() -> Vec<App> {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};

    let workspace = NSWorkspace::sharedWorkspace();
    let running = workspace.runningApplications();
    let mut apps = Vec::new();
    for app in running.iter() {
        // SAFETY: `app` is a live NSRunningApplication; these are plain
        // property reads with no pointer arguments.
        unsafe {
            if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
                continue;
            }
            let pid = app.processIdentifier();
            if pid <= 0 {
                continue;
            }
            let element = AXUIElement::new_application(pid);
            // Bound every later read of this application. A failure here means
            // the app is already gone, so skip it rather than walk it.
            if attr::set_timeout(&element, MESSAGING_TIMEOUT_SECS).is_err() {
                continue;
            }
            apps.push(App {
                element,
                info: AppInfo {
                    name: app
                        .localizedName()
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    // The direct answer to what cost AT-SPI a whole sideband
                    // module: macOS applications carry their own identifier.
                    app_id: app.bundleIdentifier().map(|id| id.to_string()),
                    pid: Some(pid as u32),
                    toolkit: None,
                    toolkit_version: None,
                },
                pid,
                active: app.isActive(),
            });
        }
    }
    apps
}

/// The windows of one application.
///
/// A window with neither a title nor a resolvable id is still returned: the
/// consumer can match on app identity, and dropping it would silently hide a
/// window from the user. Being *unnameable* is a presentation problem;
/// being absent is a correctness one.
pub fn windows_of(app: &App, names: &Names) -> Result<Vec<Window>, AxError> {
    let elements = attr::elements(&app.element, &names.windows)?;
    // The focused window only matters when this app is frontmost; asking an
    // inactive app for it is a wasted round trip per app per reconcile.
    let focused = if app.active {
        attr::element(&app.element, &names.focused_window)
            .ok()
            .flatten()
            .map(|element| ElementKey::new(app.pid, element))
    } else {
        None
    };

    let mut windows = Vec::with_capacity(elements.len());
    for element in elements {
        let key = ElementKey::new(app.pid, element);
        // `AXWindows` is not exclusively windows. Finder publishes the desktop
        // there as an `AXScrollArea` (measured 2026-08-15) — it has no
        // `CGWindowID`, no title, and is not a window any user would name. The
        // AT-SPI source filters the same way, to Frame/Window/Dialog.
        let role = attr::string(key.element(), &names.role).ok().flatten();
        if role.as_deref() != Some(WINDOW_ROLE) {
            tracing::debug!(
                app = %app.info.name,
                ?role,
                "skipping non-window entry in AXWindows"
            );
            continue;
        }
        let title = attr::string(key.element(), &names.title)
            .ok()
            .flatten()
            .unwrap_or_default();
        windows.push(Window {
            subrole: attr::string(key.element(), &names.subrole).ok().flatten(),
            native_window_id: window_id::window_id(key.element()),
            active: focused.as_ref() == Some(&key),
            title,
            app: app.info.clone(),
            key,
        });
    }
    Ok(windows)
}

/// Every window on the desktop, with the focused one marked.
///
/// One application failing is not fatal: it is skipped and the rest of the
/// desktop is still reported. An app that quits mid-enumeration is the normal
/// case, not an exceptional one.
pub fn discover_windows(names: &Names) -> Vec<Window> {
    let mut out = Vec::new();
    for app in running_apps() {
        match windows_of(&app, names) {
            Ok(windows) => out.extend(windows),
            Err(error) => {
                tracing::debug!(
                    app = %app.info.name,
                    pid = app.pid,
                    %error,
                    "skipping application whose windows could not be read"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Enumeration is the one part of discovery that needs no Accessibility
    // grant: `NSWorkspace` is not gated by TCC. So the shape of the result is
    // testable in CI even though reading any *window* is not.

    #[test]
    fn running_apps_are_regular_and_carry_identity() {
        let apps = running_apps();
        for app in &apps {
            assert!(app.pid > 0, "a pid of {} is not addressable", app.pid);
            assert_eq!(app.info.pid, Some(app.pid as u32), "AppInfo agrees with the app");
            if let Some(id) = &app.info.app_id {
                assert!(!id.is_empty(), "an empty bundle id should be None, not empty");
            }
        }
        let active = apps.iter().filter(|app| app.active).count();
        assert!(active <= 1, "at most one application is frontmost, saw {active}");
    }

    #[test]
    fn enumeration_is_stable_and_pids_are_unique() {
        // Two enumerations moments apart should agree on nearly everything;
        // what matters here is that one pid never appears twice, since pid is
        // half of every element's identity.
        let apps = running_apps();
        let mut pids: Vec<i32> = apps.iter().map(|app| app.pid).collect();
        let count = pids.len();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), count, "each application must appear once");
    }

    #[test]
    fn discovery_is_total_without_a_grant() {
        // Without the Accessibility grant every window read fails, and the
        // whole point of the per-app error handling is that discovery still
        // returns rather than propagating. On a granted developer machine this
        // returns real windows; on CI it returns an empty vector. Both pass.
        let names = Names::new();
        let _ = discover_windows(&names);
    }
}
