//! Drives a provider and a consumer session against each other through an
//! in-memory "transport" that delivers bytes in small chunks, exercising
//! framing, handshake, and message flow end to end.

use accesskit_remote::{
    AppInfo, Codec, Message, PeerRole, Session, SessionConfig, SessionEvent, WindowId,
    PROTOCOL_VERSION,
};

fn pump(from: &mut Session, to: &mut Session, chunk_size: usize) -> Vec<SessionEvent> {
    let bytes = from.take_output();
    let mut events = Vec::new();
    for chunk in bytes.chunks(chunk_size.max(1)) {
        events.extend(to.handle_input(chunk).unwrap());
    }
    events
}

fn sample_tree_update() -> accesskit::TreeUpdate {
    let mut root = accesskit::Node::new(accesskit::Role::Window);
    root.set_label("New Document (Draft) - Text Editor");
    root.set_children(vec![accesskit::NodeId(1)]);
    let mut button = accesskit::Node::new(accesskit::Role::Button);
    button.set_label("Open");
    accesskit::TreeUpdate {
        nodes: vec![(accesskit::NodeId(0), root), (accesskit::NodeId(1), button)],
        tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
        tree_id: accesskit::TreeId::ROOT,
        focus: accesskit::NodeId(1),
    }
}

#[test]
fn full_conversation_over_tiny_chunks() {
    let mut provider = Session::new(SessionConfig::new(PeerRole::Provider, "accesskit_remoted"));
    let mut consumer = Session::new(SessionConfig::new(PeerRole::Consumer, "dvc-plugin"));

    let events = pump(&mut provider, &mut consumer, 3);
    assert!(matches!(
        events[..],
        [SessionEvent::Established { version: PROTOCOL_VERSION, codec: Codec::Json }]
    ));
    let events = pump(&mut consumer, &mut provider, 3);
    assert!(matches!(events[..], [SessionEvent::Established { .. }]));

    let window = WindowId(5);
    provider
        .send(&Message::WindowAdded {
            window,
            title: "New Document (Draft) - Text Editor".into(),
            app: AppInfo {
                name: "gnome-text-editor".into(),
                app_id: Some("org.gnome.TextEditor".into()),
                pid: Some(403),
                toolkit: Some("GTK".into()),
                toolkit_version: None,
            },
        })
        .unwrap();
    provider
        .send(&Message::TreeUpdate {
            window,
            update: sample_tree_update(),
        })
        .unwrap();
    provider
        .send(&Message::FocusChanged {
            window: Some(window),
        })
        .unwrap();

    let events = pump(&mut provider, &mut consumer, 3);
    assert_eq!(events.len(), 3);
    match &events[0] {
        SessionEvent::Message(Message::WindowAdded { window: w, title, app }) => {
            assert_eq!(*w, window);
            assert_eq!(title, "New Document (Draft) - Text Editor");
            assert_eq!(app.app_id.as_deref(), Some("org.gnome.TextEditor"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    match &events[1] {
        SessionEvent::Message(Message::TreeUpdate { window: w, update }) => {
            assert_eq!(*w, window);
            assert_eq!(update.nodes.len(), 2);
            assert_eq!(update.focus, accesskit::NodeId(1));
            assert!(update.tree.is_some());
        }
        other => panic!("unexpected event: {other:?}"),
    }
    match &events[2] {
        SessionEvent::Message(Message::FocusChanged { window: w }) => {
            assert_eq!(*w, Some(window));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    consumer
        .send(&Message::Action {
            window,
            request: accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: accesskit::NodeId(1),
                data: None,
            },
        })
        .unwrap();
    let events = pump(&mut consumer, &mut provider, 3);
    match &events[..] {
        [SessionEvent::Message(Message::Action { window: w, request })] => {
            assert_eq!(*w, window);
            assert_eq!(request.target_node, accesskit::NodeId(1));
        }
        other => panic!("unexpected events: {other:?}"),
    }

    provider.close("session over");
    let events = pump(&mut provider, &mut consumer, 3);
    match &events[..] {
        [SessionEvent::Closed { reason }] => assert_eq!(reason, "session over"),
        other => panic!("unexpected events: {other:?}"),
    }
}
