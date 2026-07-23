//! Server core: drives the provider end of a connection.
//!
//! [`ServerConnection`] wraps one session with a consumer. The tree source
//! (e.g. the AT-SPI mirror) owns the authoritative window and tree state;
//! the caller pushes that state in through [`announce_window`],
//! [`send_tree_update`], [`send_focus`], and [`remove_window`], and reacts
//! to [`ServerEvent`]s coming back — most importantly
//! [`ServerEvent::Established`] (answer with
//! [`sync_initial_state`](ServerConnection::sync_initial_state)) and
//! [`ServerEvent::Action`] (perform on the source).
//!
//! [`announce_window`]: ServerConnection::announce_window
//! [`send_tree_update`]: ServerConnection::send_tree_update
//! [`send_focus`]: ServerConnection::send_focus
//! [`remove_window`]: ServerConnection::remove_window

use accesskit_remote::{
    AppInfo, Message, PeerRole, Session, SessionConfig, SessionError, SessionEvent, WindowId,
};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowDescriptor {
    pub id: WindowId,
    pub title: String,
    pub app: AppInfo,
}

#[derive(Debug)]
pub enum ServerEvent {
    Established,
    Action {
        window: WindowId,
        request: accesskit::ActionRequest,
    },
    Pong {
        seq: u64,
    },
    Closed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    Session(SessionError),
    UnknownWindow(WindowId),
    DuplicateWindow(WindowId),
    UnexpectedMessage(String),
}

impl core::fmt::Display for ServerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Session(e) => write!(f, "{e}"),
            Self::UnknownWindow(id) => write!(f, "unknown window {}", id.0),
            Self::DuplicateWindow(id) => write!(f, "window {} already announced", id.0),
            Self::UnexpectedMessage(what) => write!(f, "unexpected message from consumer: {what}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<SessionError> for ServerError {
    fn from(e: SessionError) -> Self {
        Self::Session(e)
    }
}

#[derive(Debug)]
pub struct ServerConnection {
    session: Session,
    announced: HashSet<WindowId>,
}

impl ServerConnection {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            session: Session::new(SessionConfig::new(PeerRole::Provider, name)),
            announced: HashSet::new(),
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

    pub fn handle_input(&mut self, chunk: &[u8]) -> Result<Vec<ServerEvent>, ServerError> {
        let events = self.session.handle_input(chunk)?;
        let mut out = Vec::new();
        for event in events {
            match event {
                SessionEvent::Established { .. } => out.push(ServerEvent::Established),
                SessionEvent::Closed { reason } => out.push(ServerEvent::Closed { reason }),
                SessionEvent::Message(Message::Action { window, request }) => {
                    out.push(ServerEvent::Action { window, request });
                }
                SessionEvent::Message(Message::Pong { seq }) => {
                    out.push(ServerEvent::Pong { seq });
                }
                SessionEvent::Message(other) => {
                    let error = ServerError::UnexpectedMessage(format!("{other:?}"));
                    self.session.close(error.to_string());
                    return Err(error);
                }
            }
        }
        Ok(out)
    }

    /// Announces existing windows, their full trees, and the focused window
    /// to a freshly established consumer.
    pub fn sync_initial_state(
        &mut self,
        windows: Vec<(WindowDescriptor, accesskit::TreeUpdate)>,
        focus: Option<WindowId>,
    ) -> Result<(), ServerError> {
        for (descriptor, tree) in windows {
            let id = descriptor.id;
            self.announce_window(&descriptor)?;
            self.send_tree_update(id, tree)?;
        }
        self.send_focus(focus)
    }

    pub fn announce_window(&mut self, descriptor: &WindowDescriptor) -> Result<(), ServerError> {
        if !self.announced.insert(descriptor.id) {
            return Err(ServerError::DuplicateWindow(descriptor.id));
        }
        self.session.send(&Message::WindowAdded {
            window: descriptor.id,
            title: descriptor.title.clone(),
            app: descriptor.app.clone(),
        })?;
        Ok(())
    }

    pub fn send_tree_update(
        &mut self,
        window: WindowId,
        update: accesskit::TreeUpdate,
    ) -> Result<(), ServerError> {
        if !self.announced.contains(&window) {
            return Err(ServerError::UnknownWindow(window));
        }
        self.session.send(&Message::TreeUpdate { window, update })?;
        Ok(())
    }

    pub fn send_focus(&mut self, window: Option<WindowId>) -> Result<(), ServerError> {
        if let Some(id) = window {
            if !self.announced.contains(&id) {
                return Err(ServerError::UnknownWindow(id));
            }
        }
        self.session.send(&Message::FocusChanged { window })?;
        Ok(())
    }

    pub fn remove_window(&mut self, window: WindowId) -> Result<(), ServerError> {
        if !self.announced.remove(&window) {
            return Err(ServerError::UnknownWindow(window));
        }
        self.session.send(&Message::WindowRemoved { window })?;
        Ok(())
    }

    pub fn send_ping(&mut self, seq: u64) -> Result<(), ServerError> {
        self.session.send(&Message::Ping { seq })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit_remote::Codec;

    fn established_pair() -> (ServerConnection, Session) {
        let mut server = ServerConnection::new("test-server");
        let mut consumer = Session::new(SessionConfig::new(PeerRole::Consumer, "test-consumer"));
        consumer.handle_input(&server.take_output()).unwrap();
        let events = server.handle_input(&consumer.take_output()).unwrap();
        assert!(matches!(events[..], [ServerEvent::Established]));
        (server, consumer)
    }

    fn descriptor(id: u64) -> WindowDescriptor {
        WindowDescriptor {
            id: WindowId(id),
            title: format!("window {id}"),
            app: AppInfo::default(),
        }
    }

    fn empty_tree() -> accesskit::TreeUpdate {
        accesskit::TreeUpdate {
            nodes: vec![(
                accesskit::NodeId(0),
                accesskit::Node::new(accesskit::Role::Window),
            )],
            tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
            tree_id: accesskit::TreeId::ROOT,
            focus: accesskit::NodeId(0),
        }
    }

    #[test]
    fn initial_sync_orders_windows_before_focus() {
        let (mut server, mut consumer) = established_pair();
        server
            .sync_initial_state(
                vec![(descriptor(1), empty_tree()), (descriptor(2), empty_tree())],
                Some(WindowId(2)),
            )
            .unwrap();
        let events = consumer.handle_input(&server.take_output()).unwrap();
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                SessionEvent::Message(Message::WindowAdded { .. }) => "added",
                SessionEvent::Message(Message::TreeUpdate { .. }) => "tree",
                SessionEvent::Message(Message::FocusChanged { .. }) => "focus",
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["added", "tree", "added", "tree", "focus"]);
    }

    #[test]
    fn action_from_consumer_surfaces() {
        let (mut server, mut consumer) = established_pair();
        consumer
            .send(&Message::Action {
                window: WindowId(3),
                request: accesskit::ActionRequest {
                    action: accesskit::Action::Focus,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: accesskit::NodeId(9),
                    data: None,
                },
            })
            .unwrap();
        let events = server.handle_input(&consumer.take_output()).unwrap();
        match &events[..] {
            [ServerEvent::Action { window, request }] => {
                assert_eq!(*window, WindowId(3));
                assert_eq!(request.target_node, accesskit::NodeId(9));
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn guards_unknown_and_duplicate_windows() {
        let (mut server, _consumer) = established_pair();
        assert_eq!(
            server.send_tree_update(WindowId(9), empty_tree()).unwrap_err(),
            ServerError::UnknownWindow(WindowId(9))
        );
        server.announce_window(&descriptor(1)).unwrap();
        assert_eq!(
            server.announce_window(&descriptor(1)).unwrap_err(),
            ServerError::DuplicateWindow(WindowId(1))
        );
        assert_eq!(
            server.send_focus(Some(WindowId(9))).unwrap_err(),
            ServerError::UnknownWindow(WindowId(9))
        );
        server.remove_window(WindowId(1)).unwrap();
        assert_eq!(
            server.remove_window(WindowId(1)).unwrap_err(),
            ServerError::UnknownWindow(WindowId(1))
        );
    }

    #[test]
    fn provider_only_message_from_consumer_closes() {
        let (mut server, mut consumer) = established_pair();
        consumer
            .send(&Message::WindowRemoved { window: WindowId(1) })
            .unwrap();
        assert!(matches!(
            server.handle_input(&consumer.take_output()),
            Err(ServerError::UnexpectedMessage(_))
        ));
        assert!(server.is_closed());
    }

    #[test]
    fn handshake_uses_json_codec() {
        let mut server = ServerConnection::new("s");
        let mut consumer = Session::new(SessionConfig::new(PeerRole::Consumer, "c"));
        let events = consumer.handle_input(&server.take_output()).unwrap();
        assert!(matches!(
            events[..],
            [SessionEvent::Established { codec: Codec::Json, .. }]
        ));
    }
}
