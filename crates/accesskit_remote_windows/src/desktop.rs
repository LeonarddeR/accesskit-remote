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

    /// Everything to apply for one arriving delta, in order.
    ///
    /// **A delta may not overtake the snapshot it belongs to.** Activation
    /// resets the composition and syncs it, but hands the adapter only the root
    /// update — every window's subtree goes into `pending` for the next sync.
    /// In that gap the composition already believes each window is grafted, so
    /// `retag` happily tags a delta for a subtree the consumer has not been
    /// given a single node of. Applying it names children that do not exist,
    /// which is a panic inside the consumer, inside `wnd_proc`, which cannot
    /// unwind: the screen reader's process aborts.
    ///
    /// Measured, against a real client: a session that composed perfectly for
    /// seconds died the moment a UIA client first attached, with hundreds of
    /// dangling children carrying ordinary per-window ids. A viewer never saw
    /// it because it activates before any delta is in flight.
    ///
    /// So the owed snapshots go first, and the delta is retagged only
    /// afterwards — by which time the sync may have grafted its window anyway.
    pub(crate) fn delta_updates(&self, window: WindowId, update: TreeUpdate) -> Vec<TreeUpdate> {
        let owed = !self.pending.lock().unwrap().is_empty();
        let mut updates = if owed { self.sync_updates() } else { Vec::new() };
        updates.extend(self.retag(window, update));
        updates
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

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit::{ActivationHandler, Node, NodeId, Role, Tree, TreeId};
    use accesskit_remote::{AppInfo, Message, PeerRole, Session, SessionConfig};

    fn app() -> AppInfo {
        AppInfo {
            name: "test".into(),
            app_id: None,
            pid: None,
            toolkit: None,
            toolkit_version: None,
        }
    }

    fn tree_of(label: &str) -> TreeUpdate {
        let root = NodeId(1);
        let mut window = Node::new(Role::Window);
        window.set_label(label);
        window.set_children(vec![NodeId(2)]);
        let mut button = Node::new(Role::Button);
        button.set_label("press me");
        TreeUpdate {
            nodes: vec![(root, window), (NodeId(2), button)],
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        }
    }

    /// An established client holding one remote window with a tree.
    fn established() -> (SharedClient, Session) {
        let mut client = accesskit_remote_client::ClientConnection::new("test-client");
        let mut provider = Session::new(SessionConfig::new(PeerRole::Provider, "test-provider"));
        provider.handle_input(&client.take_output()).unwrap();
        client.handle_input(&provider.take_output()).unwrap();
        provider
            .send(&Message::WindowAdded {
                window: WindowId(1),
                title: "a window".into(),
                app: app(),
                native_window_id: None,
            })
            .unwrap();
        provider
            .send(&Message::TreeUpdate {
                window: WindowId(1),
                update: tree_of("a window"),
            })
            .unwrap();
        let out = provider.take_output();
        client.handle_input(&out).unwrap();
        (Arc::new(Mutex::new(client)), provider)
    }

    /// **The abort.** Activation returns the root tree and queues every
    /// window's subtree for the next sync — but marks those windows grafted
    /// immediately. A delta arriving in that gap is retagged into a subtree the
    /// consumer has not been given a single node of, and naming children that
    /// do not exist panics inside the consumer, inside `wnd_proc`, which cannot
    /// unwind: the process aborts rather than fails.
    ///
    /// Observed against a real client as hundreds of dangling children with
    /// ordinary per-window ids, seconds after a UIA client first attached.
    #[test]
    fn a_delta_never_overtakes_the_snapshot_it_belongs_to() {
        let (client, _provider) = established();
        let (actions, _rx) = std::sync::mpsc::channel();
        let mut parts = desktop_parts("desk", client, actions);

        // A reader attaches: the adapter takes the root tree, and the window's
        // own subtree is only queued.
        let root = parts.activation.request_initial_tree().unwrap();
        assert_eq!(root.tree_id, TreeId::ROOT, "activation hands back the desktop root");
        assert!(
            !parts.shared.pending.lock().unwrap().is_empty(),
            "the window's subtree is owed, not yet given",
        );

        // The composition nonetheless considers the window grafted, which is
        // exactly the trap: retagging alone would emit this delta on its own.
        assert!(
            parts.shared.retag(WindowId(1), tree_of("a window")).is_some(),
            "retag alone would hand the adapter a delta for an empty subtree",
        );

        let updates = parts.shared.delta_updates(WindowId(1), tree_of("a window"));
        let delta_tree = accesskit_remote_client::DesktopTree::tree_id(WindowId(1));
        let first_for_window = updates
            .iter()
            .position(|update| update.tree_id == delta_tree)
            .expect("the window's subtree is among the updates");
        assert!(
            updates[first_for_window].tree.is_some(),
            "the first thing the adapter sees for this subtree must be a snapshot, \
             not a delta — a snapshot is what carries tree data",
        );
        assert!(
            parts.shared.pending.lock().unwrap().is_empty(),
            "and nothing stays owed once a delta has forced the flush",
        );
    }
}
