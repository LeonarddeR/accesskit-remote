//! The message schema exchanged between provider and consumer.
//!
//! The provider (tree source, e.g. the Linux daemon) streams window
//! lifecycle, tree updates, and cross-window focus; the consumer (platform
//! host, e.g. the Windows DVC plugin) sends action requests back. Unknown
//! JSON fields are ignored on deserialization, so fields can be added
//! without a version bump; new message kinds require one.

use serde::{Deserialize, Serialize};

/// Identifies one toplevel window for the lifetime of a session.
///
/// Assigned by the provider; never reused within a session.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct WindowId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerRole {
    Provider,
    Consumer,
}

impl PeerRole {
    pub fn opposite(self) -> Self {
        match self {
            Self::Provider => Self::Consumer,
            Self::Consumer => Self::Provider,
        }
    }
}

/// Identity of the application owning a window, for association and
/// presentation on the consumer side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    /// Desktop-file style identifier, e.g. `org.gnome.TextEditor`.
    pub app_id: Option<String>,
    pub pid: Option<u32>,
    pub toolkit: Option<String>,
    pub toolkit_version: Option<String>,
}

/// The first message each peer sends on a new connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    /// Highest protocol version the peer supports. The effective session
    /// version is the minimum of both peers' values.
    pub version: u32,
    pub role: PeerRole,
    /// Codec names in preference order. The first entry in the provider's
    /// list that the consumer also supports wins.
    pub codecs: Vec<String>,
    /// Human-readable peer name, for diagnostics only.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c", rename_all = "camelCase")]
pub enum Message {
    Hello(Hello),
    Goodbye {
        reason: String,
    },
    WindowAdded {
        window: WindowId,
        title: String,
        app: AppInfo,
    },
    WindowRemoved {
        window: WindowId,
    },
    TreeUpdate {
        window: WindowId,
        update: accesskit::TreeUpdate,
    },
    /// Which toplevel has keyboard focus; `None` when no exported window
    /// is focused. Focus within a window travels in its tree updates.
    FocusChanged {
        window: Option<WindowId>,
    },
    Action {
        window: WindowId,
        request: accesskit::ActionRequest,
    },
    Ping {
        seq: u64,
    },
    Pong {
        seq: u64,
    },
}

impl Message {
    /// A short, stable name for the variant, for bounded diagnostics
    /// (a message's `Debug` can embed an entire tree).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::Goodbye { .. } => "goodbye",
            Self::WindowAdded { .. } => "windowAdded",
            Self::WindowRemoved { .. } => "windowRemoved",
            Self::TreeUpdate { .. } => "treeUpdate",
            Self::FocusChanged { .. } => "focusChanged",
            Self::Action { .. } => "action",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree_update() -> accesskit::TreeUpdate {
        let mut root = accesskit::Node::new(accesskit::Role::Window);
        root.set_label("Test Window");
        root.set_children(vec![accesskit::NodeId(1)]);
        let mut button = accesskit::Node::new(accesskit::Role::Button);
        button.set_label("OK");
        accesskit::TreeUpdate {
            nodes: vec![(accesskit::NodeId(0), root), (accesskit::NodeId(1), button)],
            tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
            tree_id: accesskit::TreeId::ROOT,
            focus: accesskit::NodeId(0),
        }
    }

    fn assert_json_round_trip(msg: &Message) {
        let first = serde_json::to_vec(msg).unwrap();
        let decoded: Message = serde_json::from_slice(&first).unwrap();
        let second = serde_json::to_vec(&decoded).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn round_trips_every_variant() {
        assert_json_round_trip(&Message::Hello(Hello {
            version: 1,
            role: PeerRole::Provider,
            codecs: vec!["json".into()],
            name: "test".into(),
        }));
        assert_json_round_trip(&Message::Goodbye {
            reason: "bye".into(),
        });
        assert_json_round_trip(&Message::WindowAdded {
            window: WindowId(7),
            title: "New Document".into(),
            app: AppInfo {
                name: "gnome-text-editor".into(),
                app_id: Some("org.gnome.TextEditor".into()),
                pid: Some(403),
                toolkit: Some("GTK".into()),
                toolkit_version: Some("4.18".into()),
            },
        });
        assert_json_round_trip(&Message::WindowRemoved { window: WindowId(7) });
        assert_json_round_trip(&Message::TreeUpdate {
            window: WindowId(7),
            update: sample_tree_update(),
        });
        assert_json_round_trip(&Message::FocusChanged {
            window: Some(WindowId(7)),
        });
        assert_json_round_trip(&Message::FocusChanged { window: None });
        assert_json_round_trip(&Message::Action {
            window: WindowId(7),
            request: accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: accesskit::NodeId(1),
                data: None,
            },
        });
        assert_json_round_trip(&Message::Ping { seq: 42 });
        assert_json_round_trip(&Message::Pong { seq: 42 });
    }

    #[test]
    fn ignores_unknown_fields() {
        let json = br#"{"t":"windowRemoved","c":{"window":3,"futureField":true}}"#;
        let msg: Message = serde_json::from_slice(json).unwrap();
        match msg {
            Message::WindowRemoved { window } => assert_eq!(window, WindowId(3)),
            other => panic!("unexpected message: {other:?}"),
        }
    }
}
