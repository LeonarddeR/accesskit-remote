//! Drives a full provider/consumer conversation through the server and
//! client cores, byte stream in between.

use accesskit_remote::{AppInfo, WindowId};
use accesskit_remote_client::{ClientConnection, ClientEvent};
use accesskit_remote_server::{ServerConnection, ServerEvent, WindowDescriptor};
use std::collections::HashMap;

fn pump_to_client(server: &mut ServerConnection, client: &mut ClientConnection) -> Vec<ClientEvent> {
    let bytes = server.take_output();
    let mut events = Vec::new();
    for chunk in bytes.chunks(7) {
        events.extend(client.handle_input(chunk).unwrap());
    }
    events
}

fn pump_to_server(client: &mut ClientConnection, server: &mut ServerConnection) -> Vec<ServerEvent> {
    let bytes = client.take_output();
    let mut events = Vec::new();
    for chunk in bytes.chunks(7) {
        events.extend(server.handle_input(chunk).unwrap());
    }
    events
}

fn editor_tree() -> accesskit::TreeUpdate {
    let mut root = accesskit::Node::new(accesskit::Role::Window);
    root.set_label("New Document (Draft) - Text Editor");
    root.set_children(vec![accesskit::NodeId(1), accesskit::NodeId(2)]);
    let mut text = accesskit::Node::new(accesskit::Role::MultilineTextInput);
    text.set_label("Document");
    let mut button = accesskit::Node::new(accesskit::Role::Button);
    button.set_label("Open");
    accesskit::TreeUpdate {
        nodes: vec![
            (accesskit::NodeId(0), root),
            (accesskit::NodeId(1), text),
            (accesskit::NodeId(2), button),
        ],
        tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
        tree_id: accesskit::TreeId::ROOT,
        focus: accesskit::NodeId(1),
    }
}

fn nodes_by_id(update: &accesskit::TreeUpdate) -> HashMap<u64, serde_json::Value> {
    update
        .nodes
        .iter()
        .map(|(id, node)| (id.0, serde_json::to_value(node).unwrap()))
        .collect()
}

fn established_pair() -> (ServerConnection, ClientConnection) {
    let mut server = ServerConnection::new("provider");
    let mut client = ClientConnection::new("consumer");
    let events = pump_to_client(&mut server, &mut client);
    assert!(matches!(events[..], [ClientEvent::Connected]));
    let events = pump_to_server(&mut client, &mut server);
    assert!(matches!(events[..], [ServerEvent::Established]));
    (server, client)
}

#[test]
fn initial_sync_populates_client_stores() {
    let (mut server, mut client) = established_pair();
    let window = WindowId(5);
    server
        .sync_initial_state(
            vec![(
                WindowDescriptor {
                    id: window,
                    title: "New Document (Draft) - Text Editor".into(),
                    app: AppInfo {
                        name: "gnome-text-editor".into(),
                        app_id: Some("org.gnome.TextEditor".into()),
                        ..Default::default()
                    },
                    native_window_id: None,
                },
                editor_tree(),
            )],
            Some(window),
        )
        .unwrap();
    let events = pump_to_client(&mut server, &mut client);
    assert!(matches!(
        events[..],
        [
            ClientEvent::WindowAdded { .. },
            ClientEvent::TreeUpdated { .. },
            ClientEvent::FocusChanged { .. }
        ]
    ));

    let info = client.window_info(window).unwrap();
    assert_eq!(info.app.app_id.as_deref(), Some("org.gnome.TextEditor"));
    assert_eq!(client.focused_window(), Some(window));

    let snapshot = client.snapshot(window).unwrap();
    assert_eq!(nodes_by_id(&snapshot), nodes_by_id(&editor_tree()));
    assert_eq!(snapshot.focus, accesskit::NodeId(1));
}

#[test]
fn incremental_update_and_pruning() {
    let (mut server, mut client) = established_pair();
    let window = WindowId(5);
    server
        .sync_initial_state(vec![(
            WindowDescriptor {
                id: window,
                title: "t".into(),
                app: AppInfo::default(),
                native_window_id: None,
            },
            editor_tree(),
        )], None)
        .unwrap();
    pump_to_client(&mut server, &mut client);

    let mut root = accesskit::Node::new(accesskit::Role::Window);
    root.set_label("New Document (Draft) - Text Editor");
    root.set_children(vec![accesskit::NodeId(1)]);
    server
        .send_tree_update(
            window,
            accesskit::TreeUpdate {
                nodes: vec![(accesskit::NodeId(0), root)],
                tree: None,
                tree_id: accesskit::TreeId::ROOT,
                focus: accesskit::NodeId(1),
            },
        )
        .unwrap();
    let events = pump_to_client(&mut server, &mut client);
    assert!(matches!(events[..], [ClientEvent::TreeUpdated { .. }]));

    let snapshot = client.snapshot(window).unwrap();
    let ids: Vec<u64> = {
        let mut v: Vec<u64> = snapshot.nodes.iter().map(|(id, _)| id.0).collect();
        v.sort();
        v
    };
    assert_eq!(ids, [0, 1], "removed button node is pruned from snapshots");
}

#[test]
fn action_round_trip() {
    let (mut server, mut client) = established_pair();
    let window = WindowId(5);
    server
        .sync_initial_state(vec![(
            WindowDescriptor {
                id: window,
                title: "t".into(),
                app: AppInfo::default(),
                native_window_id: None,
            },
            editor_tree(),
        )], None)
        .unwrap();
    pump_to_client(&mut server, &mut client);

    client
        .request_action(
            window,
            accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: accesskit::NodeId(2),
                data: None,
            },
        )
        .unwrap();
    let events = pump_to_server(&mut client, &mut server);
    match &events[..] {
        [ServerEvent::Action { window: w, request }] => {
            assert_eq!(*w, window);
            assert_eq!(request.action, accesskit::Action::Click);
            assert_eq!(request.target_node, accesskit::NodeId(2));
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn window_removal_clears_focus_and_store() {
    let (mut server, mut client) = established_pair();
    let window = WindowId(5);
    server
        .sync_initial_state(
            vec![(
                WindowDescriptor {
                    id: window,
                    title: "t".into(),
                    app: AppInfo::default(),
                    native_window_id: None,
                },
                editor_tree(),
            )],
            Some(window),
        )
        .unwrap();
    pump_to_client(&mut server, &mut client);

    server.remove_window(window).unwrap();
    let events = pump_to_client(&mut server, &mut client);
    assert!(matches!(events[..], [ClientEvent::WindowRemoved { .. }]));
    assert_eq!(client.focused_window(), None);
    assert!(client.snapshot(window).is_none());
    assert!(client.window_info(window).is_none());
}

#[test]
fn close_propagates() {
    let (mut server, mut client) = established_pair();
    server.close("provider shutting down");
    let events = pump_to_client(&mut server, &mut client);
    match &events[..] {
        [ClientEvent::Closed { reason }] => assert_eq!(reason, "provider shutting down"),
        other => panic!("unexpected events: {other:?}"),
    }
    assert!(client.is_closed());
}
