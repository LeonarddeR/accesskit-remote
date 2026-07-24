//! RAIL window discovery and UIA attach.
//!
//! An in-context `SetWinEventHook` over this process delivers object events
//! synchronously on the thread that owns the emitting window — for
//! `RAIL_WINDOW` HWNDs, the msrdc UI thread where the adapter must live. The
//! hook proc filters to unattached RAIL toplevels, matches them against the
//! remote window list by normalized title, and installs the visible-window
//! adapter right there on the owning thread. A dedicated thread holds the
//! hook registration and pumps messages; the hook dies with that thread and
//! `UnhookWinEvent` must run on it.

use accesskit_remote::WindowId;
use accesskit_remote_windows::{OutgoingAction, SharedClient, install_visible_adapter};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_CREATE, EVENT_OBJECT_NAMECHANGE, GetClassNameW, GetMessageW,
    GetPropW, GetWindowTextW, GetWindowThreadProcessId, MSG, OBJID_WINDOW, PostThreadMessageW,
    TranslateMessage, WINEVENT_INCONTEXT, WM_QUIT,
};
use windows::core::w;

use crate::association::{RailWindow, match_window};

const RAIL_CLASS: &str = "RAIL_WINDOW";

/// State shared between the pump thread (registry updates, delta routing) and
/// the hook proc (attach decisions), reachable from the hook via a process
/// global.
pub struct RailShared {
    pub client: SharedClient,
    pub actions: Sender<OutgoingAction>,
    pub distro: Option<String>,
    pub registry: Mutex<Registry>,
}

/// Known remote windows and which RAIL HWNDs they are bound to.
#[derive(Default)]
pub struct Registry {
    client_windows: HashMap<u64, ClientWindow>,
    bound: HashMap<u64, isize>,
    attached: HashSet<isize>,
    focused: Option<u64>,
}

struct ClientWindow {
    title: String,
    app_id: Option<String>,
}

/// The host-focus posts to make after a remote focus change: clear the window
/// that lost focus, set the one that gained it. `None` entries are unbound
/// windows with no HWND to post to.
#[derive(Debug, PartialEq, Eq)]
pub struct FocusTransition {
    pub unfocus: Option<isize>,
    pub focus: Option<isize>,
}

impl Registry {
    pub fn window_added(&mut self, id: WindowId, title: String, app_id: Option<String>) {
        self.client_windows.insert(id.0, ClientWindow { title, app_id });
    }

    /// Forgets a remote window; returns the HWND it was bound to, if any.
    pub fn window_removed(&mut self, id: WindowId) -> Option<isize> {
        self.client_windows.remove(&id.0);
        if self.focused == Some(id.0) {
            self.focused = None;
        }
        let hwnd = self.bound.remove(&id.0);
        if let Some(h) = hwnd {
            self.attached.remove(&h);
        }
        hwnd
    }

    pub fn bound_hwnd(&self, id: WindowId) -> Option<isize> {
        self.bound.get(&id.0).copied()
    }

    /// The remote window currently holding session focus, if known.
    pub fn focused(&self) -> Option<WindowId> {
        self.focused.map(WindowId)
    }

    /// Records the new session focus and returns the HWNDs to update: the
    /// previously-focused window is cleared and the newly-focused one is set.
    /// A repeat of the current focus is a no-op.
    pub fn focus_changed(&mut self, window: Option<WindowId>) -> FocusTransition {
        let new = window.map(|w| w.0);
        if new == self.focused {
            return FocusTransition { unfocus: None, focus: None };
        }
        let unfocus = self.focused.and_then(|id| self.bound.get(&id).copied());
        let focus = new.and_then(|id| self.bound.get(&id).copied());
        self.focused = new;
        FocusTransition { unfocus, focus }
    }

    fn unbound_windows(&self) -> Vec<(WindowId, accesskit_remote_client::WindowInfo)> {
        self.client_windows
            .iter()
            .filter(|(id, _)| !self.bound.contains_key(*id))
            .map(|(id, w)| {
                (
                    WindowId(*id),
                    accesskit_remote_client::WindowInfo {
                        title: w.title.clone(),
                        app: accesskit_remote::AppInfo {
                            name: String::new(),
                            app_id: w.app_id.clone(),
                            pid: None,
                            toolkit: None,
                            toolkit_version: None,
                        },
                    },
                )
            })
            .collect()
    }
}

static SHARED: Mutex<Option<Arc<RailShared>>> = Mutex::new(None);

fn shared() -> Option<Arc<RailShared>> {
    SHARED.lock().unwrap().clone()
}

/// The default WSL distro name, for anchored title-suffix stripping.
pub fn default_distro() -> Option<String> {
    let lxss = windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Lxss")
        .ok()?;
    let default = lxss.get_string("DefaultDistribution").ok()?;
    lxss.open(&default).ok()?.get_string("DistributionName").ok()
}

/// Handle to the hook thread; `stop` unregisters everything.
pub struct RailHook {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl RailHook {
    pub fn stop(mut self) {
        *SHARED.lock().unwrap() = None;
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Publish `shared` for the hook proc and start the hook thread.
pub fn start(shared_state: Arc<RailShared>) -> RailHook {
    *SHARED.lock().unwrap() = Some(shared_state);
    let (tid_tx, tid_rx) = std::sync::mpsc::channel();
    let join = std::thread::spawn(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        let hmodule = crate::instance();
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_NAMECHANGE,
                Some(hmodule),
                Some(win_event_proc),
                GetCurrentProcessId(),
                0,
                WINEVENT_INCONTEXT,
            )
        };
        if hook.is_invalid() {
            warn!("SetWinEventHook failed; RAIL windows will not be attached");
        } else {
            info!("RAIL WinEvent hook installed (thread {thread_id})");
        }
        let _ = tid_tx.send(thread_id);
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if !hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(hook);
            }
        }
        debug!("RAIL hook thread exiting");
    });
    let thread_id = tid_rx.recv().unwrap_or(0);
    RailHook { thread_id, join: Some(join) }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    _ideventthread: u32,
    _dwmstime: u32,
) {
    if idobject != OBJID_WINDOW.0 || idchild != 0 || hwnd.is_invalid() {
        return;
    }
    let Some(shared) = shared() else {
        return;
    };
    if !is_rail_window(hwnd) {
        return;
    }
    try_attach(&shared, hwnd, event);
}

fn is_rail_window(hwnd: HWND) -> bool {
    let mut class = [0u16; 32];
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    len > 0 && String::from_utf16_lossy(&class[..len as usize]) == RAIL_CLASS
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

fn server_window_id(hwnd: HWND) -> u64 {
    unsafe { GetPropW(hwnd, w!("WslgServerWindowId")) }.0 as u64
}

fn try_attach(shared: &Arc<RailShared>, hwnd: HWND, event: u32) {
    let hwnd_key = hwnd.0 as isize;
    let mut registry = shared.registry.lock().unwrap();
    if registry.attached.contains(&hwnd_key) {
        return;
    }
    let candidates = registry.unbound_windows();
    if candidates.is_empty() {
        return;
    }
    let title = window_title(hwnd);
    if title.is_empty() {
        return;
    }
    // The adapter install swaps the wndproc, which is only safe on the
    // window's owning thread; an in-context hook normally guarantees that.
    let owning_thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
    let current_thread = unsafe { GetCurrentThreadId() };
    if owning_thread != current_thread {
        warn!(
            "hook proc for {hwnd:?} ran on thread {current_thread}, owner is {owning_thread} \
             (event {event:#06x}); skipping install"
        );
        return;
    }
    let rail = RailWindow {
        server_window_id: server_window_id(hwnd),
        title,
        app_user_model_id: None,
    };
    let distro = shared.distro.as_deref().unwrap_or("");
    let Some(window) = match_window(&rail, distro, &candidates) else {
        return;
    };
    info!(
        "attaching remote window {} to RAIL hwnd {hwnd:?} (server id {:#x}, title {:?}, \
         event {event:#06x}, owning thread {owning_thread}, current thread {current_thread})",
        window.0, rail.server_window_id, rail.title
    );
    // Seed host-focus from local focus or a remote FocusChanged that arrived
    // before this window attached.
    let is_focused = unsafe { GetFocus() } == hwnd || registry.focused() == Some(window);
    match install_visible_adapter(
        hwnd,
        window,
        shared.client.clone(),
        shared.actions.clone(),
        is_focused,
    ) {
        Ok(()) => {
            registry.bound.insert(window.0, hwnd_key);
            registry.attached.insert(hwnd_key);
        }
        Err(e) => warn!("install_visible_adapter failed for {hwnd:?}: {e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::default()
    }

    #[test]
    fn focus_to_unbound_window_records_but_posts_nothing() {
        let mut r = registry();
        r.window_added(WindowId(1), "t".into(), None);
        assert_eq!(
            r.focus_changed(Some(WindowId(1))),
            FocusTransition { unfocus: None, focus: None }
        );
        assert_eq!(r.focused(), Some(WindowId(1)));
    }

    #[test]
    fn focus_across_bound_windows_clears_old_and_sets_new() {
        let mut r = registry();
        r.bound.insert(1, 0x111);
        r.bound.insert(2, 0x222);
        assert_eq!(
            r.focus_changed(Some(WindowId(1))),
            FocusTransition { unfocus: None, focus: Some(0x111) }
        );
        assert_eq!(
            r.focus_changed(Some(WindowId(2))),
            FocusTransition { unfocus: Some(0x111), focus: Some(0x222) }
        );
    }

    #[test]
    fn repeating_the_current_focus_is_a_no_op() {
        let mut r = registry();
        r.bound.insert(1, 0x111);
        r.focus_changed(Some(WindowId(1)));
        assert_eq!(
            r.focus_changed(Some(WindowId(1))),
            FocusTransition { unfocus: None, focus: None }
        );
    }

    #[test]
    fn focus_none_unfocuses_the_previous_window_only() {
        let mut r = registry();
        r.bound.insert(1, 0x111);
        r.focus_changed(Some(WindowId(1)));
        assert_eq!(
            r.focus_changed(None),
            FocusTransition { unfocus: Some(0x111), focus: None }
        );
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn removing_the_focused_window_clears_focus_so_next_focus_has_no_stale_unfocus() {
        let mut r = registry();
        r.window_added(WindowId(1), "a".into(), None);
        r.bound.insert(1, 0x111);
        r.attached.insert(0x111);
        r.focus_changed(Some(WindowId(1)));
        r.window_removed(WindowId(1));
        assert_eq!(r.focused(), None);
        r.bound.insert(2, 0x222);
        assert_eq!(
            r.focus_changed(Some(WindowId(2))),
            FocusTransition { unfocus: None, focus: Some(0x222) }
        );
    }
}
