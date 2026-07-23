//! UIA hosting on windows that are already visible.
//!
//! `SubclassingAdapter` refuses visible windows, so this module hosts the
//! lower-level `accesskit_windows::Adapter` under a manual subclass: install
//! swaps the wndproc and parks the adapter state in a window property; the
//! wndproc answers `WM_GETOBJECT`, applies tree deltas posted from other
//! threads via a registered window message, and frees the state when the
//! window is destroyed. Serves RDP RAIL windows, which are visible before the
//! plugin ever sees them.

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

use crate::{ForwardActions, OutgoingAction, SharedClient, SnapshotActivation};

type LongPtr = isize;

const PROP_NAME: windows::core::PCWSTR = w!("AccessKitRemoteVisible");

/// The registered window message carrying a `Box<accesskit::TreeUpdate>`
/// pointer in `lParam`.
pub fn delta_message() -> u32 {
    static MSG: OnceLock<u32> = OnceLock::new();
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("AccessKitRemoteDelta")) })
}

/// Post a tree delta to a window bound with [`install_visible_adapter`].
/// Callable from any thread; the delta is applied on the window's own thread.
pub fn post_delta(hwnd: HWND, update: accesskit::TreeUpdate) -> bool {
    let raw = Box::into_raw(Box::new(update));
    let posted =
        unsafe { PostMessageW(Some(hwnd), delta_message(), WPARAM(0), LPARAM(raw as isize)) };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(raw) });
        return false;
    }
    true
}

struct VisibleState {
    adapter: accesskit_windows::Adapter,
    activation: SnapshotActivation,
}

struct VisibleImpl {
    state: RefCell<VisibleState>,
    prev_wnd_proc: WNDPROC,
    window_destroyed: Cell<bool>,
}

extern "system" fn wnd_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handle = unsafe { GetPropW(window, PROP_NAME) };
    let impl_ptr = handle.0 as *mut VisibleImpl;
    if impl_ptr.is_null() {
        return LRESULT(0);
    }
    let r#impl = unsafe { &*impl_ptr };
    if message == delta_message() {
        let update = unsafe { Box::from_raw(lparam.0 as *mut accesskit::TreeUpdate) };
        let mut state = r#impl.state.borrow_mut();
        let events = state.adapter.update_if_active(|| *update);
        drop(state);
        if let Some(events) = events {
            events.raise();
        }
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
    let activation = SnapshotActivation { window, client };
    let r#impl = Box::new(VisibleImpl {
        state: RefCell::new(VisibleState { adapter, activation }),
        prev_wnd_proc: None,
        window_destroyed: Cell::new(false),
    });
    let impl_ptr = Box::into_raw(r#impl);
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
    debug!("visible adapter installed on {hwnd:?} for remote window {}", window.0);
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
