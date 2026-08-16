//! UIA hosting on windows that are already visible.
//!
//! `SubclassingAdapter` refuses visible windows, so this module hosts the
//! lower-level `accesskit_windows::Adapter` under a manual subclass: install
//! swaps the wndproc and parks the adapter state in a window property; the
//! wndproc answers `WM_GETOBJECT`, applies tree deltas posted from other
//! threads via a registered window message, and frees the state when the
//! window is destroyed. Serves RDP RAIL windows, which are visible before the
//! plugin ever sees them.
//!
//! It serves both arrangements. **One window per remote window** is RAIL:
//! [`install_visible_adapter`] binds a `RAIL_WINDOW` to one remote window, and
//! [`post_delta`] carries that window's tree. **One window for the whole
//! desktop** is a full-desktop session, where there is no per-window HWND to
//! bind: [`install_visible_desktop_adapter`] hosts every remote window as a
//! grafted subtree, [`post_desktop_delta`] names which window a delta belongs
//! to, and [`post_sync`] is the heartbeat that composition needs — an adapter
//! activates on demand, so the tree has to be rebuilt at a moment no client
//! event announces.
//!
//! The two differ only in what they park in the window property; the subclass,
//! the message plumbing and the teardown are one implementation deliberately,
//! because they are the parts that are hard to get right and impossible to test
//! off Windows.

use accesskit_remote::WindowId;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::transmute;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;
use tracing::{debug, warn};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWLP_WNDPROC, GetPropW, IsWindow, PostMessageW, RegisterWindowMessageW,
    RemovePropW, SetPropW, SetWindowLongPtrW, WM_ENTERMENULOOP, WM_ENTERSIZEMOVE, WM_EXITMENULOOP,
    WM_EXITSIZEMOVE, WM_GETOBJECT, WM_KILLFOCUS, WM_NCDESTROY, WM_SETFOCUS, WNDPROC,
};
use windows::core::w;

use crate::desktop::{desktop_parts, DesktopShared};
use crate::{ForwardActions, OutgoingAction, SharedClient, SnapshotActivation};

type LongPtr = isize;

const PROP_NAME: windows::core::PCWSTR = w!("AccessKitRemoteVisible");

/// The registered window message carrying a `Box<accesskit::TreeUpdate>`
/// pointer in `lParam`.
pub fn delta_message() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("AccessKitRemoteDelta")) })
}

/// The registered window message requesting the adapter detach itself on the
/// window's own thread.
pub fn detach_message() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("AccessKitRemoteDetach")) })
}

/// Ask a bound window to remove its adapter. Callable from any thread.
pub fn post_detach(hwnd: HWND) -> bool {
    unsafe { PostMessageW(Some(hwnd), detach_message(), WPARAM(0), LPARAM(0)) }.is_ok()
}

/// The registered window message carrying a host-focus flag (0/1) in `wParam`.
pub fn focus_message() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("AccessKitRemoteFocus")) })
}

/// Tell a bound window whether its remote window holds the session focus.
/// Callable from any thread; the state is applied on the window's own thread.
/// The consumer only raises UIA focus events while it believes the window is
/// host-focused, so remote focus must be driven through here for a RAIL window
/// that never receives its own `WM_SETFOCUS`.
pub fn post_focus(hwnd: HWND, is_focused: bool) -> bool {
    unsafe {
        PostMessageW(
            Some(hwnd),
            focus_message(),
            WPARAM(is_focused as usize),
            LPARAM(0),
        )
    }
    .is_ok()
}

/// The registered window message asking a desktop-hosted window to bring its
/// composed tree in line with the client.
pub fn sync_message() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("AccessKitRemoteSync")) })
}

/// Ask a window bound with [`install_visible_desktop_adapter`] to re-compose.
///
/// Callable from any thread. Post it on every client event **and** on a timer:
/// the adapter activates when an assistive technology asks, which no client
/// event announces, and the composition has to be rebuilt at that moment.
pub fn post_sync(hwnd: HWND) -> bool {
    unsafe { PostMessageW(Some(hwnd), sync_message(), WPARAM(0), LPARAM(0)) }.is_ok()
}

/// Post a tree delta for one remote window to a window bound with
/// [`install_visible_desktop_adapter`], which hosts many.
///
/// Follow it with [`post_sync`]: a window's first tree is what makes it
/// graftable, and until it is grafted its deltas are withheld.
pub fn post_desktop_delta(
    hwnd: HWND,
    window: WindowId,
    update: accesskit::TreeUpdate,
) -> bool {
    post_delta_for(hwnd, window.0 as usize, update)
}

/// Post a tree delta to a window bound with [`install_visible_adapter`].
/// Callable from any thread; the delta is applied on the window's own thread.
pub fn post_delta(hwnd: HWND, update: accesskit::TreeUpdate) -> bool {
    post_delta_for(hwnd, 0, update)
}

fn post_delta_for(hwnd: HWND, wparam: usize, update: accesskit::TreeUpdate) -> bool {
    let raw = Box::into_raw(Box::new(update));
    let posted = unsafe {
        PostMessageW(Some(hwnd), delta_message(), WPARAM(wparam), LPARAM(raw as isize))
    };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(raw) });
        return false;
    }
    true
}

struct VisibleState {
    adapter: accesskit_windows::Adapter,
    activation: BoxedActivation,
}

/// The activation handler is boxed so one wndproc serves both a single remote
/// window and a whole composed desktop.
struct BoxedActivation(Box<dyn accesskit::ActivationHandler>);

impl accesskit::ActivationHandler for BoxedActivation {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        self.0.request_initial_tree()
    }
}

struct VisibleImpl {
    state: RefCell<VisibleState>,
    prev_wnd_proc: WNDPROC,
    window_destroyed: Cell<bool>,
    /// Present only for a desktop-hosted window; `None` means this window
    /// carries exactly one remote window and needs no composition.
    desktop: Option<DesktopShared>,
}

extern "system" fn wnd_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handle = unsafe { GetPropW(window, PROP_NAME) };
    let impl_ptr = handle.0 as *mut VisibleImpl;
    if impl_ptr.is_null() {
        return LRESULT(0);
    }
    let r#impl = unsafe { &*impl_ptr };
    if message == delta_message() {
        let update = *unsafe { Box::from_raw(lparam.0 as *mut accesskit::TreeUpdate) };
        // In desktop mode the delta belongs to one of many remote windows, named
        // by wParam, and must be retagged into that window's subtree — or
        // withheld, if that window has not been grafted yet.
        let update = match &r#impl.desktop {
            Some(shared) => shared.retag(WindowId(wparam.0 as u64), update),
            None => Some(update),
        };
        if let Some(update) = update {
            r#impl.apply(update);
        }
        return LRESULT(0);
    }
    if message == sync_message() {
        if let Some(shared) = &r#impl.desktop {
            // Taken before touching the adapter: sync_updates locks the client,
            // and the adapter's activation handler locks it too.
            for update in shared.sync_updates() {
                r#impl.apply(update);
            }
        }
        return LRESULT(0);
    }
    if message == detach_message() {
        uninstall_visible_adapter(window);
        return LRESULT(0);
    }
    if message == focus_message() {
        r#impl.update_window_focus_state(wparam.0 != 0);
        return LRESULT(0);
    }
    match message {
        WM_GETOBJECT => {
            let mut state = r#impl.state.borrow_mut();
            let state_mut = &mut *state;
            if let Some(result) =
                state_mut
                    .adapter
                    .handle_wm_getobject(wparam, lparam, &mut state_mut.activation)
            {
                drop(state);
                return result.into();
            }
        }
        WM_SETFOCUS | WM_EXITMENULOOP | WM_EXITSIZEMOVE => {
            r#impl.update_window_focus_state(true);
        }
        WM_KILLFOCUS | WM_ENTERMENULOOP | WM_ENTERSIZEMOVE => {
            r#impl.update_window_focus_state(false);
        }
        WM_NCDESTROY => {
            r#impl.window_destroyed.set(true);
            let prev = r#impl.prev_wnd_proc;
            let result = unsafe { CallWindowProcW(prev, window, message, wparam, lparam) };
            unsafe {
                let _ = RemovePropW(window, PROP_NAME);
            }
            drop(unsafe { Box::from_raw(impl_ptr) });
            debug!("visible adapter state freed on WM_NCDESTROY for {window:?}");
            return result;
        }
        _ => (),
    }
    unsafe { CallWindowProcW(r#impl.prev_wnd_proc, window, message, wparam, lparam) }
}

impl VisibleImpl {
    /// Applies one update, holding the state borrow only across the update
    /// itself — raising events re-enters the wndproc.
    fn apply(&self, update: accesskit::TreeUpdate) {
        let mut state = self.state.borrow_mut();
        let events = state.adapter.update_if_active(|| update);
        drop(state);
        if let Some(events) = events {
            events.raise();
        }
    }

    fn update_window_focus_state(&self, is_focused: bool) {
        let mut state = self.state.borrow_mut();
        if let Some(events) = state.adapter.update_window_focus_state(is_focused) {
            drop(state);
            events.raise();
        }
    }
}

/// Host a UIA provider for `window` on `hwnd`, which may already be visible.
///
/// Must be called on the thread that owns `hwnd`. The initial tree is pulled
/// from the client store on UIA activation; live deltas arrive via
/// [`post_delta`]. The state frees itself when the window is destroyed;
/// call [`uninstall_visible_adapter`] (on the owning thread) to detach
/// earlier. Fails if the window already has a visible adapter installed.
pub fn install_visible_adapter(
    hwnd: HWND,
    window: WindowId,
    client: SharedClient,
    actions: Sender<OutgoingAction>,
    is_focused: bool,
) -> windows::core::Result<()> {
    if !unsafe { GetPropW(hwnd, PROP_NAME) }.0.is_null() {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_UNEXPECTED,
        ));
    }
    let action = ForwardActions { window, actions };
    let adapter = accesskit_windows::Adapter::new(hwnd, is_focused, action);
    let activation = BoxedActivation(Box::new(SnapshotActivation { window, client }));
    let r#impl = Box::new(VisibleImpl {
        state: RefCell::new(VisibleState { adapter, activation }),
        prev_wnd_proc: None,
        window_destroyed: Cell::new(false),
        desktop: None,
    });
    let impl_ptr = Box::into_raw(r#impl);
    subclass(hwnd, impl_ptr)?;
    debug!("visible adapter installed on {hwnd:?} for remote window {}", window.0);
    Ok(())
}

/// Host a UIA provider for the **whole remote desktop** on `hwnd`, which may
/// already be visible — the RDP client's session window, which shows a picture
/// of every remote window and has no per-window HWND to bind.
///
/// Every remote window becomes a grafted subtree of one composed tree. Deltas
/// arrive via [`post_desktop_delta`] and the composition is rebuilt by
/// [`post_sync`], which must also run on a timer because a UIA client can
/// activate the adapter at a moment nothing else announces.
///
/// Must be called on the thread that owns `hwnd`. `label` is what a reader
/// announces for the desktop as a whole — name the machine, not a window.
pub fn install_visible_desktop_adapter(
    hwnd: HWND,
    label: impl Into<String>,
    client: SharedClient,
    actions: Sender<OutgoingAction>,
) -> windows::core::Result<()> {
    if !unsafe { GetPropW(hwnd, PROP_NAME) }.0.is_null() {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_UNEXPECTED,
        ));
    }
    let parts = desktop_parts(label, client, actions);
    // The session window really does hold the host focus while the user is in
    // the session, and unlike a RAIL window it receives its own WM_SETFOCUS,
    // so the initial guess is corrected by the wndproc either way.
    let adapter = accesskit_windows::Adapter::new(hwnd, true, parts.action);
    let activation = BoxedActivation(Box::new(parts.activation));
    let r#impl = Box::new(VisibleImpl {
        state: RefCell::new(VisibleState { adapter, activation }),
        prev_wnd_proc: None,
        window_destroyed: Cell::new(false),
        desktop: Some(parts.shared),
    });
    let impl_ptr = Box::into_raw(r#impl);
    subclass(hwnd, impl_ptr)?;
    debug!("visible desktop adapter installed on {hwnd:?}");
    Ok(())
}

/// Parks the state on the window and swaps in our wndproc, undoing both if the
/// swap fails.
fn subclass(hwnd: HWND, impl_ptr: *mut VisibleImpl) -> windows::core::Result<()> {
    unsafe { SetPropW(hwnd, PROP_NAME, Some(HANDLE(impl_ptr as *mut c_void))) }?;
    let result = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wnd_proc as *const c_void as _) };
    if result == 0 {
        let err = windows::core::Error::from_thread();
        unsafe {
            let _ = RemovePropW(hwnd, PROP_NAME);
        }
        drop(unsafe { Box::from_raw(impl_ptr) });
        return Err(err);
    }
    unsafe { (*impl_ptr).prev_wnd_proc = transmute::<LongPtr, WNDPROC>(result) };
    Ok(())
}

/// Detach a visible adapter installed by [`install_visible_adapter`] before
/// the window is destroyed. Must be called on the thread that owns `hwnd`.
pub fn uninstall_visible_adapter(hwnd: HWND) {
    let handle = unsafe { GetPropW(hwnd, PROP_NAME) };
    let impl_ptr = handle.0 as *mut VisibleImpl;
    if impl_ptr.is_null() {
        return;
    }
    let r#impl = unsafe { &*impl_ptr };
    if !r#impl.window_destroyed.get() && unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        let restored = unsafe {
            SetWindowLongPtrW(
                hwnd,
                GWLP_WNDPROC,
                transmute::<WNDPROC, LongPtr>(r#impl.prev_wnd_proc),
            )
        };
        if restored == 0 {
            warn!("failed to restore wndproc for {hwnd:?}: {:?}", windows::core::Error::from_thread());
        }
    }
    unsafe {
        let _ = RemovePropW(hwnd, PROP_NAME);
    }
    drop(unsafe { Box::from_raw(impl_ptr) });
    debug!("visible adapter uninstalled from {hwnd:?}");
}
