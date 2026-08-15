//! `AXObserver` registration and the routing decision for what it delivers.
//!
//! This is the AX counterpart of the AT-SPI source's event subscription, and it
//! keeps that design's central insight — **structure, semantics and text are
//! three separate routes**, because conflating them is what makes a mirror
//! either slow or wrong:
//!
//! - a *structural* change invalidates the shape of a window, so it debounces
//!   into one re-walk ([`Route::Rewalk`]);
//! - a *semantic* change alters one node's properties only, so it rate-limits
//!   into a single-node refresh that may never change the tree's shape
//!   ([`Route::Refresh`]);
//! - *focus* moves without changing anything else, so it emits a focus-only
//!   delta with no reads at all ([`Route::Focus`]).
//!
//! Which macOS notification belongs to which route is a judgement that has to
//! be checked against real applications rather than assumed — `ax_events` is
//! the instrument for that, and it exists because macOS has no equivalent of
//! the `busctl monitor` that established the same table on Linux.
//!
//! Callbacks arrive on the thread whose run loop the observer's source was
//! added to, which is the AX worker thread. Nothing here is shared across
//! threads, and nothing locks.

use crate::attr::AxError;
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{CFRetained, CFRunLoop, CFString, kCFRunLoopDefaultMode};
use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

/// What a notification means for the mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Route {
    /// The window's shape changed: re-walk it, debounced.
    Rewalk,
    /// One node's properties changed: re-read that node, rate-limited.
    Refresh,
    /// Keyboard focus moved. Costs no reads — the focused element is carried
    /// on the notification itself.
    Focus,
    /// A window appeared or vanished: reconcile the window set.
    Lifecycle,
    /// Deliberately ignored.
    Ignore,
}

/// The notifications this crate registers for, and where each one routes.
///
/// Registration is not free — each is an IPC call per element — so this list
/// is the whole subscription and nothing else is asked for. `AXMoved` and
/// `AXResized` are deliberately absent: they fire on every drag and scroll,
/// and geometry rides the re-walk. The AT-SPI source excludes `BoundsChanged`
/// for exactly the same reason.
pub const SUBSCRIPTIONS: &[(&str, Route)] = &[
    // Window set.
    ("AXWindowCreated", Route::Lifecycle),
    ("AXUIElementDestroyed", Route::Lifecycle),
    ("AXDrawerCreated", Route::Lifecycle),
    ("AXSheetCreated", Route::Lifecycle),
    // Focus.
    ("AXFocusedUIElementChanged", Route::Focus),
    ("AXFocusedWindowChanged", Route::Focus),
    ("AXMainWindowChanged", Route::Focus),
    ("AXApplicationActivated", Route::Focus),
    ("AXApplicationDeactivated", Route::Focus),
    // Structure.
    ("AXCreated", Route::Rewalk),
    ("AXLayoutChanged", Route::Rewalk),
    ("AXRowCountChanged", Route::Rewalk),
    ("AXRowExpanded", Route::Rewalk),
    ("AXRowCollapsed", Route::Rewalk),
    ("AXMenuOpened", Route::Rewalk),
    ("AXMenuClosed", Route::Rewalk),
    // Semantics.
    ("AXValueChanged", Route::Refresh),
    ("AXTitleChanged", Route::Refresh),
    ("AXSelectedChildrenChanged", Route::Refresh),
    ("AXSelectedRowsChanged", Route::Refresh),
    ("AXSelectedCellsChanged", Route::Refresh),
    ("AXElementBusyChanged", Route::Refresh),
];

/// Where a notification routes, or [`Route::Ignore`] if it is not one of ours.
pub fn route(notification: &str) -> Route {
    SUBSCRIPTIONS
        .iter()
        .find(|(name, _)| *name == notification)
        .map(|(_, route)| *route)
        .unwrap_or(Route::Ignore)
}

/// Counts every callback entry, before any interpretation.
///
/// Diagnostic: "the observer never fires" and "the queue never receives" are
/// different faults with different causes, and only this tells them apart.
pub static CALLBACKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One delivered notification.
pub struct Notification {
    pub pid: i32,
    pub notification: String,
    pub element: CFRetained<AXUIElement>,
}

/// The queue a callback appends to, shared with the worker that drains it.
///
/// `Rc<RefCell<..>>` rather than `Arc<Mutex<..>>` on purpose: callbacks fire on
/// the run loop of the thread that registered them, which is the only thread
/// that ever touches this. Reaching for a lock would imply a sharing that does
/// not exist.
pub type Queue = Rc<RefCell<Vec<Notification>>>;

/// A live subscription to one application's notifications.
///
/// Dropping it removes the observer's source from the run loop, which is what
/// stops callbacks for an application that has quit.
pub struct AppObserver {
    observer: CFRetained<AXObserver>,
    pid: i32,
}

impl AppObserver {
    /// Registers for every notification in [`SUBSCRIPTIONS`] on `app`.
    ///
    /// Returns the observer plus the notifications the application refused.
    /// Refusals are expected and are not failures — most applications support
    /// only a subset, and *which* subset is itself a finding worth reporting.
    pub fn new(
        pid: i32,
        app: &AXUIElement,
        queue: &Queue,
    ) -> Result<(Self, Vec<String>), AxError> {
        let mut raw: *mut AXObserver = std::ptr::null_mut();
        // SAFETY: `callback` matches the AXObserverCallback signature, and
        // `raw` is a valid writable slot the callee fills on success.
        let error = unsafe {
            AXObserver::create(pid, Some(callback), std::ptr::NonNull::from(&mut raw))
        };
        if error != AXError::Success {
            return Err(AxError(error));
        }
        let Some(ptr) = std::ptr::NonNull::new(raw) else {
            return Err(AxError(AXError::Failure));
        };
        // SAFETY: AXObserverCreate follows the Create rule, handing back a +1
        // reference which CFRetained now owns.
        let observer = unsafe { CFRetained::from_raw(ptr) };

        // The queue pointer handed to every callback. The worker owns the `Rc`
        // and outlives every observer it created, so this stays valid for as
        // long as callbacks can fire.
        let refcon = Rc::as_ptr(queue) as *mut c_void;

        let mut declined = Vec::new();
        for (notification, _) in SUBSCRIPTIONS {
            let name = CFString::from_static_str(notification);
            // SAFETY: a live observer and application element, a valid name,
            // and a refcon that outlives the observer.
            let error = unsafe { observer.add_notification(app, &name, refcon) };
            if error != AXError::Success {
                declined.push((*notification).to_owned());
            }
        }

        // The *current* thread's run loop, never the main one: callbacks are
        // delivered to whichever loop holds the source, and this must be the
        // worker whose loop is actually being run. Registering on main would
        // mean an observer that never fires and a mirror that silently never
        // updates.
        let Some(run_loop) = CFRunLoop::current() else {
            return Err(AxError(AXError::Failure));
        };
        // SAFETY: the observer is live; the returned source is +0 and is
        // retained by the run loop when added.
        let source = unsafe { observer.run_loop_source() };
        // SAFETY: a live run loop and a live source.
        unsafe { run_loop.add_source(Some(&source), kCFRunLoopDefaultMode) };

        Ok((Self { observer, pid }, declined))
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The observer's run-loop source, for verifying attachment.
    pub fn run_loop_source(&self) -> CFRetained<objc2_core_foundation::CFRunLoopSource> {
        // SAFETY: the observer is live; AXObserverGetRunLoopSource is a Get-rule
        // accessor returning the same cached source each call.
        unsafe { self.observer.run_loop_source() }
    }
}

impl Drop for AppObserver {
    fn drop(&mut self) {
        // SAFETY: the observer is still live here; removing its source stops
        // further callbacks before the queue pointer can dangle.
        let source = unsafe { self.observer.run_loop_source() };
        if let Some(loop_) = CFRunLoop::current() {
            // SAFETY: a live run loop and a live source.
            unsafe { loop_.remove_source(Some(&source), kCFRunLoopDefaultMode) };
        }
    }
}

/// The C entry point every notification arrives through.
///
/// Must not panic across the FFI boundary and must not block: it runs on the
/// run loop that also services the walk, so anything expensive here stalls
/// every application's updates. It therefore only appends to the queue; all
/// interpretation happens in the worker's own loop.
unsafe extern "C-unwind" fn callback(
    _observer: std::ptr::NonNull<AXObserver>,
    element: std::ptr::NonNull<AXUIElement>,
    notification: std::ptr::NonNull<CFString>,
    refcon: *mut c_void,
) {
    // `AssertUnwindSafe` because the captured pointers are raw and the only
    // shared state is the queue, which is left consistent by any panic here:
    // the push either happened or it did not.
    CALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if refcon.is_null() {
            return;
        }
        // SAFETY: `refcon` is the `Rc<RefCell<Vec<_>>>` the worker created and
        // keeps alive for as long as any observer exists. Not reconstructed as
        // an `Rc`, which would take ownership of a borrowed count.
        let queue = unsafe { &*(refcon as *const RefCell<Vec<Notification>>) };
        // SAFETY: +0 references owned by the caller for the duration of the
        // callback, retained here so they can outlive it.
        let (element, name) = unsafe {
            (
                CFRetained::retain(element),
                CFRetained::retain(notification).to_string(),
            )
        };
        let mut pid: libc::pid_t = 0;
        // SAFETY: `pid` is a valid writable slot.
        let error = unsafe { element.pid(std::ptr::NonNull::from(&mut pid)) };
        if error != AXError::Success {
            return;
        }
        // A callback re-entering while the worker drains would panic on
        // `borrow_mut`; dropping the notification is far better than aborting
        // the process, and the periodic reconcile is the backstop.
        if let Ok(mut queue) = queue.try_borrow_mut() {
            queue.push(Notification {
                pid,
                notification: name,
                element,
            });
        }
    }));
    if result.is_err() {
        tracing::error!("panic in the AX observer callback");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_subscription_has_a_real_route() {
        for (name, expected) in SUBSCRIPTIONS {
            assert!(name.starts_with("AX"), "{name} is not an AX notification");
            assert_ne!(*expected, Route::Ignore, "{name} subscribes but routes nowhere");
            assert_eq!(route(name), *expected);
        }
    }

    #[test]
    fn an_unsubscribed_notification_is_ignored() {
        // Applications deliver notifications nobody asked for; each must cost
        // one comparison and nothing else.
        assert_eq!(route("AXMoved"), Route::Ignore);
        assert_eq!(route("AXResized"), Route::Ignore);
        assert_eq!(route("AXSomethingInvented"), Route::Ignore);
    }

    /// Geometry churn must not reach the mirror. `AXMoved`/`AXResized` fire on
    /// every drag and scroll; bounds ride the re-walk instead. The AT-SPI
    /// source excludes `BoundsChanged` for the same reason.
    #[test]
    fn geometry_notifications_are_not_subscribed() {
        for noisy in ["AXMoved", "AXResized", "AXWindowMoved", "AXWindowResized"] {
            assert!(
                !SUBSCRIPTIONS.iter().any(|(name, _)| *name == noisy),
                "{noisy} would fire on every drag"
            );
        }
    }

    #[test]
    fn structure_and_semantics_stay_separate() {
        // The routing split is the design's core claim; these are the cases
        // that would quietly collapse it.
        assert_eq!(route("AXLayoutChanged"), Route::Rewalk);
        assert_eq!(route("AXRowCountChanged"), Route::Rewalk, "rows change shape");
        assert_eq!(route("AXValueChanged"), Route::Refresh, "a value does not");
        assert_eq!(route("AXTitleChanged"), Route::Refresh);
        assert_eq!(route("AXFocusedUIElementChanged"), Route::Focus);
        assert_eq!(route("AXWindowCreated"), Route::Lifecycle);
    }

    #[test]
    fn no_notification_is_listed_twice() {
        let mut names: Vec<&str> = SUBSCRIPTIONS.iter().map(|(name, _)| *name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "a duplicate registration costs an extra IPC call");
    }
}
