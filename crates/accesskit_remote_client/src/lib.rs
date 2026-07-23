//! Client core: drives the consumer end of a connection.
//!
//! [`ClientConnection`] receives the provider's stream, maintains a tree
//! store per window, and emits [`ClientEvent`]s. Live tree deltas are
//! surfaced as they arrive for hosts with an attached platform adapter;
//! [`snapshot`](ClientConnection::snapshot) reconstructs a full
//! `TreeUpdate` for adapters that attach late (e.g. after window
//! association completes on the platform side).

use accesskit_remote::{
    AppInfo, Message, PeerRole, Session, SessionConfig, SessionError, SessionEvent, WindowId,
};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub title: String,
    pub app: AppInfo,
}

#[derive(Debug)]
pub enum ClientEvent {
    Connected,
    WindowAdded {
        window: WindowId,
    },
    WindowRemoved {
        window: WindowId,
    },
    /// A live tree delta, already applied to the store.
    TreeUpdated {
        window: WindowId,
        update: accesskit::TreeUpdate,
    },
    FocusChanged {
        window: Option<WindowId>,
    },
    Pong {
        seq: u64,
    },
    Closed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    Session(SessionError),
    UnknownWindow(WindowId),
    DuplicateWindow(WindowId),
    UnexpectedMessage(String),
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Session(e) => write!(f, "{e}"),
            Self::UnknownWindow(id) => write!(f, "unknown window {}", id.0),
            Self::DuplicateWindow(id) => write!(f, "window {} already added", id.0),
            Self::UnexpectedMessage(what) => write!(f, "unexpected message from provider: {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<SessionError> for ClientError {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}

#[derive(Debug)]
struct TreeStore {
    nodes: HashMap<accesskit::NodeId, accesskit::Node>,
    tree: Option<accesskit::Tree>,
    tree_id: accesskit::TreeId,
    focus: accesskit::NodeId,
}

impl TreeStore {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            tree: None,
            tree_id: accesskit::TreeId::ROOT,
            focus: accesskit::NodeId(0),
        }
    }

    fn apply(&mut self, update: &accesskit::TreeUpdate) {
        for (id, node) in &update.nodes {
            self.nodes.insert(*id, node.clone());
        }
        if let Some(tree) = &update.tree {
            self.tree = Some(tree.clone());
        }
        self.tree_id = update.tree_id;
        self.focus = update.focus;
    }

    /// Rebuilds a full `TreeUpdate` from the nodes reachable from the root;
    /// nodes orphaned by child-list removals are excluded (and pruned).
    fn snapshot(&mut self) -> Option<accesskit::TreeUpdate> {
        let tree = self.tree.clone()?;
        let mut nodes = Vec::new();
        let mut reachable = std::collections::HashSet::new();
        let mut stack = vec![tree.root];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(node) = self.nodes.get(&id) {
                stack.extend(node.children().iter().copied());
                nodes.push((id, node.clone()));
            }
        }
        self.nodes.retain(|id, _| reachable.contains(id));
        Some(accesskit::TreeUpdate {
            nodes,
            tree: Some(tree),
            tree_id: self.tree_id,
            focus: self.focus,
        })
    }
}

#[derive(Debug)]
struct WindowEntry {
    info: WindowInfo,
    store: TreeStore,
}

#[derive(Debug)]
pub struct ClientConnection {
    session: Session,
    windows: HashMap<WindowId, WindowEntry>,
    focus: Option<WindowId>,
}

impl ClientConnection {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            session: Session::new(SessionConfig::new(PeerRole::Consumer, name)),
            windows: HashMap::new(),
            focus: None,
        }
    }

    pub fn is_established(&self) -> bool {
        self.session.is_established()
    }

    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        self.session.take_output()
    }

    pub fn close(&mut self, reason: impl Into<String>) {
        self.session.close(reason);
    }

    pub fn window_info(&self, window: WindowId) -> Option<&WindowInfo> {
        self.windows.get(&window).map(|e| &e.info)
    }

    pub fn windows(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.windows.keys().copied()
    }

    pub fn focused_window(&self) -> Option<WindowId> {
        self.focus
    }

    /// Full tree for a window, for late-attaching platform adapters.
    pub fn snapshot(&mut self, window: WindowId) -> Option<accesskit::TreeUpdate> {
        self.windows.get_mut(&window)?.store.snapshot()
    }

    pub fn request_action(
        &mut self,
        window: WindowId,
        request: accesskit::ActionRequest,
    ) -> Result<(), ClientError> {
        if !self.windows.contains_key(&window) {
            return Err(ClientError::UnknownWindow(window));
        }
        self.session.send(&Message::Action { window, request })?;
        Ok(())
    }

    pub fn handle_input(&mut self, chunk: &[u8]) -> Result<Vec<ClientEvent>, ClientError> {
        let events = self.session.handle_input(chunk)?;
        let mut out = Vec::new();
        for event in events {
            match event {
                SessionEvent::Established { .. } => out.push(ClientEvent::Connected),
                SessionEvent::Closed { reason } => out.push(ClientEvent::Closed { reason }),
                SessionEvent::Message(msg) => match msg {
                    Message::WindowAdded { window, title, app } => {
                        let entry = WindowEntry {
                            info: WindowInfo { title, app },
                            store: TreeStore::new(),
                        };
                        if self.windows.insert(window, entry).is_some() {
                            return self.fail(ClientError::DuplicateWindow(window));
                        }
                        out.push(ClientEvent::WindowAdded { window });
                    }
                    Message::WindowRemoved { window } => {
                        if self.windows.remove(&window).is_none() {
                            return self.fail(ClientError::UnknownWindow(window));
                        }
                        if self.focus == Some(window) {
                            self.focus = None;
                        }
                        out.push(ClientEvent::WindowRemoved { window });
                    }
                    Message::TreeUpdate { window, update } => {
                        let Some(entry) = self.windows.get_mut(&window) else {
                            return self.fail(ClientError::UnknownWindow(window));
                        };
                        entry.store.apply(&update);
                        out.push(ClientEvent::TreeUpdated { window, update });
                    }
                    Message::FocusChanged { window } => {
                        if let Some(id) = window {
                            if !self.windows.contains_key(&id) {
                                return self.fail(ClientError::UnknownWindow(id));
                            }
                        }
                        self.focus = window;
                        out.push(ClientEvent::FocusChanged { window });
                    }
                    Message::Pong { seq } => out.push(ClientEvent::Pong { seq }),
                    other => {
                        return self.fail(ClientError::UnexpectedMessage(format!("{other:?}")));
                    }
                },
            }
        }
        Ok(out)
    }

    fn fail(&mut self, error: ClientError) -> Result<Vec<ClientEvent>, ClientError> {
        self.session.close(error.to_string());
        Err(error)
    }
}
