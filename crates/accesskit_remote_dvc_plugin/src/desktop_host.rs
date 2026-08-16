//! Hosting the whole remote desktop on the RDP client's session window.
//!
//! The counterpart of [`crate::rail`] for a full-desktop session: instead of
//! binding each remote window to its own `RAIL_WINDOW`, every remote window is
//! grafted into one composed tree hosted on the single window that shows the
//! remote machine.
//!
//! It borrows `rail`'s hard-won mechanism wholesale, because the constraint is
//! the same: **the adapter must be installed on the thread that owns the
//! window**, and that thread belongs to the RDP client, not to us. An in-context
//! `SetWinEventHook` delivers its callback synchronously on the owning thread,
//! so the install happens there; and since the session window may not emit an
//! event of its own for a long time, the same nudge `rail` uses forces one —
//! a same-title `SetWindowTextW` is serviced as `WM_SETTEXT` on the owning
//! thread, whose `DefWindowProc` raises `EVENT_OBJECT_NAMECHANGE` right there.

use accesskit_remote::WindowId;
use accesskit_remote_windows::{
    install_visible_desktop_adapter, post_desktop_delta, post_sync, OutgoingAction, SharedClient,
};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetWindowTextW, PostThreadMessageW, SetWindowTextW,
    TranslateMessage, EVENT_OBJECT_CREATE, EVENT_OBJECT_NAMECHANGE, MSG, OBJID_WINDOW,
    WINEVENT_INCONTEXT, WM_QUIT,
};
use windows::core::PCWSTR;

/// How often the composed tree is re-checked.
///
/// Not a poll for changes — those arrive as events — but the heartbeat the
/// composition needs: a screen reader activates the adapter at a moment no
/// client event announces, and the tree has to be rebuilt then. Idempotent, so
/// a tick with nothing to do costs a lock and a comparison.
const SYNC_INTERVAL: Duration = Duration::from_millis(250);

/// What the hook proc needs, reachable from it through a process global
/// because a `WinEventProc` takes no context of its own.
struct HostShared {
    client: SharedClient,
    actions: Sender<OutgoingAction>,
    label: String,
    /// The window we mean to host on, and whether we already did.
    target: AtomicIsize,
    installed: AtomicBool,
}

static SHARED: OnceLock<Mutex<Option<Arc<HostShared>>>> = OnceLock::new();

fn shared_slot() -> &'static Mutex<Option<Arc<HostShared>>> {
    SHARED.get_or_init(|| Mutex::new(None))
}

fn shared() -> Option<Arc<HostShared>> {
    shared_slot().lock().ok()?.clone()
}

/// Handle to the hook and heartbeat threads.
pub struct DesktopHost {
    hook_thread: u32,
    /// Behind mutexes so teardown works through the `Arc` the channel callback
    /// holds it by — the channel closing is what ends the session.
    hook_join: Mutex<Option<JoinHandle<()>>>,
    beat_stop: Arc<AtomicBool>,
    beat_join: Mutex<Option<JoinHandle<()>>>,
}

impl DesktopHost {
    /// The window the composed tree is hosted on, once it has been found.
    pub fn hwnd(&self) -> Option<HWND> {
        let raw = shared()?.target.load(Ordering::Acquire);
        (raw != 0).then(|| HWND(raw as *mut core::ffi::c_void))
    }

    /// Routes a live delta for one remote window to the host.
    pub fn delta(&self, window: WindowId, update: accesskit::TreeUpdate) {
        if let Some(hwnd) = self.hwnd() {
            post_desktop_delta(hwnd, window, update);
            // A window's first tree is what makes it graftable, so ask for a
            // re-compose immediately rather than waiting for the heartbeat.
            post_sync(hwnd);
        }
    }

    /// Asks the host to bring its tree in line with the client.
    pub fn sync(&self) {
        if let Some(hwnd) = self.hwnd() {
            post_sync(hwnd);
        }
    }

    /// Stops the heartbeat and the hook. Idempotent: the channel can close
    /// once, but nothing here minds being told twice.
    pub fn stop(&self) {
        self.beat_stop.store(true, Ordering::Release);
        let beat = self.beat_join.lock().unwrap().take();
        if let Some(join) = beat {
            let _ = join.join();
        }
        if let Ok(mut slot) = shared_slot().lock() {
            *slot = None;
        }
        let hook = self.hook_join.lock().unwrap().take();
        if let Some(join) = hook {
            unsafe {
                let _ = PostThreadMessageW(self.hook_thread, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = join.join();
        }
    }
}

/// Finds the session window and arranges for the adapter to be installed on
/// its owning thread.
pub fn start(
    client: SharedClient,
    actions: Sender<OutgoingAction>,
    label: impl Into<String>,
) -> DesktopHost {
    let target = crate::session_window::find();
    // Not named `shared`: that is the accessor the hook proc and the heartbeat
    // reach the same state through, and a local of that name shadows it.
    let state = Arc::new(HostShared {
        client,
        actions,
        label: label.into(),
        target: AtomicIsize::new(target.as_ref().map_or(0, |c| c.hwnd)),
        installed: AtomicBool::new(false),
    });
    *shared_slot().lock().unwrap() = Some(state);

    let (tid_tx, tid_rx) = std::sync::mpsc::channel();
    let hook_join = std::thread::spawn(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        let hook = unsafe {
            SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_NAMECHANGE,
                Some(crate::instance()),
                Some(win_event_proc),
                GetCurrentProcessId(),
                0,
                WINEVENT_INCONTEXT,
            )
        };
        if hook.is_invalid() {
            warn!("SetWinEventHook failed; the remote desktop will not be hosted");
        } else {
            info!("desktop WinEvent hook installed (thread {thread_id})");
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
        debug!("desktop hook thread exiting");
    });
    let hook_thread = tid_rx.recv().unwrap_or(0);

    // The session window already exists and may sit quiet indefinitely, so
    // make it speak: this is what actually triggers the install.
    nudge();

    let beat_stop = Arc::new(AtomicBool::new(false));
    let stop_flag = beat_stop.clone();
    let beat_join = std::thread::spawn(move || {
        while !stop_flag.load(Ordering::Acquire) {
            if let Some(shared) = shared() {
                if !shared.installed.load(Ordering::Acquire) {
                    // Still unhosted: keep prodding until the hook lands.
                    nudge();
                } else {
                    let raw = shared.target.load(Ordering::Acquire);
                    if raw != 0 {
                        post_sync(HWND(raw as *mut core::ffi::c_void));
                    }
                }
            }
            std::thread::sleep(SYNC_INTERVAL);
        }
        debug!("desktop heartbeat stopped");
    });

    DesktopHost {
        hook_thread,
        hook_join: Mutex::new(Some(hook_join)),
        beat_stop,
        beat_join: Mutex::new(Some(beat_join)),
    }
}

/// Forces the session window's owning thread to raise an in-range WinEvent, so
/// the in-context hook runs there.
///
/// Setting a window's text to what it already is changes nothing a user can
/// see, and `DefWindowProc` still raises `EVENT_OBJECT_NAMECHANGE`. Must be
/// called with no locks held: it blocks in a cross-thread send.
fn nudge() {
    let Some(shared) = shared() else { return };
    if shared.installed.load(Ordering::Acquire) {
        return;
    }
    let raw = shared.target.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let mut text = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut text) };
    // A window with no title still needs nudging; an empty string is a valid
    // same-title set.
    let text: Vec<u16> = text[..len.max(0) as usize]
        .iter()
        .copied()
        .chain(core::iter::once(0))
        .collect();
    let _ = unsafe { SetWindowTextW(hwnd, PCWSTR(text.as_ptr())) };
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    idobject: i32,
    idchild: i32,
    _ideventthread: u32,
    _dwmstime: u32,
) {
    if idobject != OBJID_WINDOW.0 || idchild != 0 || hwnd.is_invalid() {
        return;
    }
    let Some(shared) = shared() else { return };
    if shared.installed.load(Ordering::Acquire) {
        return;
    }
    if hwnd.0 as isize != shared.target.load(Ordering::Acquire) {
        return;
    }
    // Claim the install before doing it: this proc runs on the owning thread,
    // but events can arrive re-entrantly.
    if shared
        .installed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    match install_visible_desktop_adapter(
        hwnd,
        shared.label.clone(),
        shared.client.clone(),
        shared.actions.clone(),
    ) {
        Ok(()) => info!("remote desktop hosted on {hwnd:?}"),
        Err(e) => {
            warn!("could not host the remote desktop on {hwnd:?}: {e}");
            shared.installed.store(false, Ordering::Release);
        }
    }
}
