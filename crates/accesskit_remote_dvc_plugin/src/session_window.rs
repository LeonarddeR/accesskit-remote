//! Finding the RDP client's session window — the one showing the remote
//! desktop.
//!
//! In the RAIL arrangement there is nothing to find: each remote window gets a
//! local `RAIL_WINDOW`, and `crate::rail` binds them one for one. A full-desktop
//! session has exactly one window showing a picture of the whole remote
//! machine, and the composed tree has to hang off it.
//!
//! **Which window that is depends on the client, and this cannot be tested
//! anywhere but on a real one.** mstsc nests several classes — a
//! `TscShellContainerClass` shell around `UIMainClass`, around
//! `UIContainerClass`, around the `IHWindowClass` that actually paints the
//! session — and the Windows App (msrdc) uses different ones again. So this does
//! not assert a class name and hope. It scores every top-level window in this
//! process, takes the most plausible, and **logs every candidate it considered
//! with its class and size**, so a run that picks the wrong one says exactly
//! what it saw and what else was available. Override with
//! `ACCESSKIT_SESSION_WINDOW_CLASS` when the log shows a better answer than the
//! heuristic found.
//!
//! The heuristic is deliberately dull: the largest visible top-level window
//! belonging to this process. A session window is the point of the application,
//! so it is the big one; toolbars, tooltips and the connection bar are not.

use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;

/// A top-level window this process owns.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub hwnd: isize,
    pub class: String,
    pub width: i32,
    pub height: i32,
    pub visible: bool,
}

impl Candidate {
    fn area(&self) -> i64 {
        i64::from(self.width.max(0)) * i64::from(self.height.max(0))
    }
}

/// The class name an operator pinned, if any.
fn override_class() -> Option<String> {
    std::env::var("ACCESSKIT_SESSION_WINDOW_CLASS")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Picks the session window from a set of candidates.
///
/// Pure, so the choice is testable without a desktop: the part that cannot be
/// tested is enumerating the windows, not deciding between them.
pub fn choose(candidates: &[Candidate], pinned: Option<&str>) -> Option<Candidate> {
    if let Some(class) = pinned {
        let chosen = candidates
            .iter()
            .find(|candidate| candidate.class.eq_ignore_ascii_case(class));
        if chosen.is_none() {
            warn!("no window of class {class:?} in this process; falling back to the largest");
        }
        if let Some(chosen) = chosen {
            return Some(chosen.clone());
        }
    }
    candidates
        .iter()
        .filter(|candidate| candidate.visible)
        // A session window has real extent. Anything smaller than this is a
        // connection bar, a tooltip or a message-only window.
        .filter(|candidate| candidate.width >= 320 && candidate.height >= 240)
        .max_by_key(|candidate| candidate.area())
        .cloned()
}

/// Every top-level window this process owns.
pub fn candidates() -> Vec<Candidate> {
    let mut found: Vec<Candidate> = Vec::new();
    let ptr = &mut found as *mut Vec<Candidate> as isize;
    // Errors here mean the enumeration was cut short, which leaves whatever was
    // collected — a worse choice, not a wrong one.
    let _ = unsafe { EnumWindows(Some(enum_proc), LPARAM(ptr)) };
    found
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != unsafe { GetCurrentProcessId() } {
        return true.into();
    }
    let mut class = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
    let mut rect = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    let candidate = Candidate {
        hwnd: hwnd.0 as isize,
        class,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
    };
    let found = unsafe { &mut *(lparam.0 as *mut Vec<Candidate>) };
    found.push(candidate);
    true.into()
}

/// Finds the session window, reporting what it considered.
///
/// Logs every candidate at `info` on the way — the one diagnostic that makes a
/// wrong pick fixable from a log file rather than a debugging session.
pub fn find() -> Option<Candidate> {
    let candidates = candidates();
    for candidate in &candidates {
        info!(
            "candidate window: class={:?} {}x{} visible={} hwnd={:#x}",
            candidate.class, candidate.width, candidate.height, candidate.visible, candidate.hwnd,
        );
    }
    let pinned = override_class();
    let chosen = choose(&candidates, pinned.as_deref());
    match &chosen {
        Some(candidate) => info!(
            "hosting the remote desktop on class={:?} {}x{} hwnd={:#x}",
            candidate.class, candidate.width, candidate.height, candidate.hwnd,
        ),
        None => warn!(
            "no window in this process looks like a session window ({} considered); \
             set ACCESSKIT_SESSION_WINDOW_CLASS to pin one",
            candidates.len(),
        ),
    }
    debug!("session window search complete");
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(class: &str, width: i32, height: i32, visible: bool) -> Candidate {
        Candidate {
            hwnd: 1,
            class: class.to_owned(),
            width,
            height,
            visible,
        }
    }

    /// The session view is the big one; the connection bar and the shell
    /// chrome around it are not.
    #[test]
    fn the_largest_visible_window_wins() {
        let candidates = vec![
            candidate("TscConnectionBarClass", 600, 30, true),
            candidate("IHWindowClass", 1920, 1080, true),
            candidate("Tooltip", 100, 40, true),
        ];
        assert_eq!(
            choose(&candidates, None).unwrap().class,
            "IHWindowClass",
        );
    }

    /// A hidden window is not what the user is looking at, however large.
    #[test]
    fn an_invisible_window_is_never_chosen() {
        let candidates = vec![
            candidate("Hidden", 3840, 2160, false),
            candidate("IHWindowClass", 1280, 720, true),
        ];
        assert_eq!(choose(&candidates, None).unwrap().class, "IHWindowClass");
    }

    /// Message-only and chrome windows are excluded by size, so a client whose
    /// session window is small still beats them — but a desktop with nothing
    /// big enough picks nothing rather than something wrong.
    #[test]
    fn nothing_plausible_means_nothing_chosen() {
        let candidates = vec![candidate("TscConnectionBarClass", 600, 30, true)];
        assert!(choose(&candidates, None).is_none());
    }

    /// The override is what a log-reader reaches for when the heuristic picked
    /// the wrong nested window.
    #[test]
    fn a_pinned_class_overrides_the_heuristic() {
        let candidates = vec![
            candidate("UIMainClass", 1920, 1080, true),
            candidate("IHWindowClass", 1900, 1000, true),
        ];
        assert_eq!(
            choose(&candidates, Some("IHWindowClass")).unwrap().class,
            "IHWindowClass",
            "the pin wins even though it is smaller",
        );
    }

    /// A pin that matches nothing must not leave the user with no tree at all.
    #[test]
    fn a_pin_that_matches_nothing_falls_back() {
        let candidates = vec![candidate("IHWindowClass", 1920, 1080, true)];
        assert_eq!(
            choose(&candidates, Some("NoSuchClass")).unwrap().class,
            "IHWindowClass",
        );
    }
}
