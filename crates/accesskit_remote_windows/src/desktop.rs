//! Hosting a whole remote desktop on one local window.
//!
//! [`RemoteWindowBinding`](crate::RemoteWindowBinding) gives one remote window
//! one HWND, which is what the RAIL arrangement provides. A full-desktop RDP
//! session provides one window showing a picture of everything, so this module
//! puts the entire desktop behind it — every remote window as a grafted
//! subtree, composed by [`DesktopTree`].
//!
//! Two hosts use it, for the same reason there are two hosts for a single
//! window: [`DesktopBinding`] subclasses a window the caller is about to show
//! (the viewer), while
//! [`install_visible_desktop_adapter`](crate::install_visible_desktop_adapter)
//! hosts one on a window that already exists and belongs to someone else (the
//! RDP client's session window). Both drive the composition through
//! [`DesktopParts`], so there is one set of rules to get right.
//!
//! The awkward part is that a platform adapter activates *on demand*: it holds
//! no tree until an assistive technology asks, and everything pushed before
//! that is discarded. A composed tree must therefore be rebuilt from scratch at
//! activation, and its subtrees pushed immediately afterwards — but
//! `request_initial_tree` may return only one update. So activation resets the
//! composition, returns the root tree, and queues the rest.
//!
//! That makes syncing a heartbeat rather than a reaction: run it on every
//! client event *and* on a timer. It is idempotent and returns nothing to do
//! when nothing changed, so the timer costs a lock and a comparison — and
//! without it, a reader that activates on a quiet desktop would be left looking
//! at empty windows until something moved.

use crate::{OutgoingAction, SharedClient, HWND};
use accesskit::TreeUpdate;
use accesskit_remote::WindowId;
use accesskit_remote_client::DesktopTree;
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub(crate) type Pending = Arc<Mutex<VecDeque<TreeUpdate>>>;

/// Everything a host needs to serve a composed desktop, built together so the
/// activation handler and the sync path share one composition.
pub(crate) struct DesktopParts {
    pub(crate) activation: DesktopActivation,
    pub(crate) action: RouteActions,
    pub(crate) shared: DesktopShared,
}

/// The half a host keeps after handing the handlers to an adapter.
pub(crate) struct DesktopShared {
    pub(crate) desktop: Arc<Mutex<DesktopTree>>,
    pub(crate) client: SharedClient,
    pub(crate) pending: Pending,
}

pub(crate) fn desktop_parts(
    label: impl Into<String>,
    client: SharedClient,
    actions: Sender<OutgoingAction>,
) -> DesktopParts {
    let desktop = Arc::new(Mutex::new(DesktopTree::new(label)));
    let pending: Pending = Arc::new(Mutex::new(VecDeque::new()));
    DesktopParts {
        activation: DesktopActivation {
            desktop: desktop.clone(),
            client: client.clone(),
            pending: pending.clone(),
        },
        action: RouteActions {
            desktop: desktop.clone(),
            actions,
        },
        shared: DesktopShared {
            desktop,
            client,
            pending,
        },
    }
}

impl DesktopShared {
    /// The updates that bring the hosted tree in line with the client, in the
    /// order they must be applied.
    ///
    /// Must not be called while holding the client lock — it takes it.
    pub(crate) fn sync_updates(&self) -> Vec<TreeUpdate> {
        let mut queued: Vec<TreeUpdate> = self.pending.lock().unwrap().drain(..).collect();
        let mut client = self.client.lock().unwrap();
        let mut desktop = self.desktop.lock().unwrap();
        queued.extend(desktop.sync(&mut client));
        queued
    }

    /// Retags a live delta into its window's subtree, or `None` when that
    /// window has no subtree yet — pushing a delta first would panic inside the
    /// consumer, and the snapshot the next sync takes carries the same content.
    pub(crate) fn retag(&self, window: WindowId, update: TreeUpdate) -> Option<TreeUpdate> {
        self.desktop.lock().unwrap().delta(window, update)
    }
}

/// One local window carrying every remote window, subclassed before it is
/// shown.
///
/// For a window the caller owns and has not yet displayed — the viewer's. A
/// window that already exists needs
/// [`install_visible_desktop_adapter`](crate::install_visible_desktop_adapter)
/// instead, because `SubclassingAdapter` refuses visible windows.
pub struct DesktopBinding {
    adapter: accesskit_windows::SubclassingAdapter,
    shared: DesktopShared,
}

impl DesktopBinding {
    /// Subclasses `hwnd` and starts answering `WM_GETOBJECT` for the whole
    /// desktop.
    ///
    /// Must be called on the thread that owns `hwnd`, before the window is
    /// first shown. `label` is what a reader announces for the desktop itself —
    /// name the machine, not a window.
    pub fn attach(
        hwnd: HWND,
        label: impl Into<String>,
        client: SharedClient,
        actions: Sender<OutgoingAction>,
    ) -> Self {
        let parts = desktop_parts(label, client, actions);
        Self {
            adapter: accesskit_windows::SubclassingAdapter::new(
                hwnd,
                parts.activation,
                parts.action,
            ),
            shared: parts.shared,
        }
    }

    /// Brings the hosted tree in line with the client. Cheap when nothing
    /// changed; safe to call from a timer.
    ///
    /// Must be called on the window's thread, and not while holding the shared
    /// client lock.
    pub fn sync(&mut self) {
        for update in self.shared.sync_updates() {
            self.push(update);
        }
    }

    /// Applies a live delta for one remote window. Follow it with
    /// [`sync`](Self::sync): a window's first tree is what makes it graftable.
    pub fn delta(&mut self, window: WindowId, update: TreeUpdate) {
        if let Some(update) = self.shared.retag(window, update) {
            self.push(update);
        }
    }

    fn push(&mut self, update: TreeUpdate) {
        if let Some(events) = self.adapter.update_if_active(|| update) {
            events.raise();
        }
    }
}

pub(crate) struct DesktopActivation {
    desktop: Arc<Mutex<DesktopTree>>,
    client: SharedClient,
    pending: Pending,
}

impl accesskit::ActivationHandler for DesktopActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        let mut client = self.client.lock().unwrap();
        let mut desktop = self.desktop.lock().unwrap();
        // The adapter is starting a fresh tree, so anything this composition
        // believes it already pushed is gone.
        desktop.reset();
        let mut updates = desktop.sync(&mut client).into_iter();
        let root = updates.next()?;
        // Only one update may be returned; the subtrees follow on the next
        // sync, which is why syncing must also run on a timer.
        self.pending.lock().unwrap().extend(updates);
        Some(root)
    }
}

pub(crate) struct RouteActions {
    desktop: Arc<Mutex<DesktopTree>>,
    actions: Sender<OutgoingAction>,
}

impl accesskit::ActionHandler for RouteActions {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        // The subtree the request names *is* the window it belongs to. A
        // request against the desktop root itself belongs to no window and is
        // dropped.
        let window = self.desktop.lock().unwrap().window_for(request.target_tree);
        if let Some(window) = window {
            let _ = self.actions.send((window, request));
        }
    }
}
