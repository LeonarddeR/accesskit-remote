//! UIA host (Windows): exposes remote AccessKit trees to UI Automation.
//!
//! [`RemoteWindowBinding`] ties one remote window to one local HWND via
//! `accesskit_windows::SubclassingAdapter`: UIA activation pulls a full
//! snapshot from the shared [`ClientConnection`] store, live deltas are
//! applied with [`apply`](RemoteWindowBinding::apply), and UIA-initiated
//! actions are forwarded through an mpsc channel to whatever thread pumps
//! the connection. The same binding serves a standalone viewer window and
//! a RAIL window inside the RDP client.
#![cfg(target_os = "windows")]

use accesskit_remote::WindowId;
use accesskit_remote_client::ClientConnection;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub use accesskit_windows::HWND;

pub type SharedClient = Arc<Mutex<ClientConnection>>;

/// An action requested by a UIA client, tagged with the remote window it
/// targets; forward to [`ClientConnection::request_action`].
pub type OutgoingAction = (WindowId, accesskit::ActionRequest);

pub struct RemoteWindowBinding {
    adapter: accesskit_windows::SubclassingAdapter,
}

impl RemoteWindowBinding {
    /// Subclasses `hwnd` and starts answering `WM_GETOBJECT` for it.
    ///
    /// Must be called on the thread that owns `hwnd`, before the window is
    /// first shown. Dropping the binding removes the subclass.
    pub fn attach(
        hwnd: HWND,
        window: WindowId,
        client: SharedClient,
        actions: Sender<OutgoingAction>,
    ) -> Self {
        let activation = SnapshotActivation {
            window,
            client: client.clone(),
        };
        let action = ForwardActions { window, actions };
        Self {
            adapter: accesskit_windows::SubclassingAdapter::new(hwnd, activation, action),
        }
    }

    /// Applies a live tree delta. Must be called on the window's thread,
    /// without holding the shared client lock.
    pub fn apply(&mut self, update: accesskit::TreeUpdate) {
        if let Some(events) = self.adapter.update_if_active(|| update) {
            events.raise();
        }
    }
}

struct SnapshotActivation {
    window: WindowId,
    client: SharedClient,
}

impl accesskit::ActivationHandler for SnapshotActivation {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        self.client.lock().unwrap().snapshot(self.window)
    }
}

struct ForwardActions {
    window: WindowId,
    actions: Sender<OutgoingAction>,
}

impl accesskit::ActionHandler for ForwardActions {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        let _ = self.actions.send((self.window, request));
    }
}
