//! Drives the caret end to end: perform `Action::SetTextSelection` with a
//! collapsed range (AT-SPI `set_caret_offset`), then wait for a delta whose
//! `TextSelection` lands on the requested position. GTK returns `NotSupported`
//! for the caret write on headless WSL; run under a desktop shell
//! (`WSL2_WESTON_SHELL_DESKTOP=1`) to characterize whether a window manager
//! changes that.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use atspi::connection::AccessibilityConnection;
    use atspi::object_ref::ObjectRefOwned;
    use atspi::proxy::accessible::ObjectRefExt;
    use atspi::proxy::editable_text::EditableTextProxy;
    use atspi::zbus::names::BusName;
    use atspi::Interface;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(AccessibilityConnection::new()).expect("raw connection");

    // Locate the first editable text object by interface, not role:
    // LibreOffice's editable body is a `paragraph`/`document text`, not
    // `Role::Text` as in gnome-text-editor.
    let doc: ObjectRefOwned = rt
        .block_on(async {
            let zconn = conn.connection();
            let registry = conn.root_accessible_on_registry().await.ok()?;
            for app_ref in registry.get_children().await.unwrap_or_default() {
                if app_ref.is_null() {
                    continue;
                }
                let Ok(app) = app_ref.as_accessible_proxy(zconn).await else {
                    continue;
                };
                for frame_ref in app.get_children().await.unwrap_or_default() {
                    if frame_ref.is_null() {
                        continue;
                    }
                    let mut queue: VecDeque<ObjectRefOwned> = VecDeque::new();
                    queue.push_back(frame_ref);
                    let mut seen = 0;
                    while let Some(obj) = queue.pop_front() {
                        seen += 1;
                        if seen > 4000 {
                            break;
                        }
                        let Ok(p) = obj.as_accessible_proxy(zconn).await else {
                            continue;
                        };
                        let ifaces = p.get_interfaces().await.ok();
                        if ifaces.is_some_and(|s| {
                            s.contains(Interface::Text) && s.contains(Interface::EditableText)
                        }) {
                            return Some(obj.clone());
                        }
                        for c in p.get_children().await.unwrap_or_default() {
                            if !c.is_null() {
                                queue.push_back(c);
                            }
                        }
                    }
                }
            }
            None
        })
        .expect("an editable text object");
    println!("document: {}", doc.path_as_str());

    // Seed known multi-line text so the caret has room to move.
    let name: BusName = doc.name().expect("named doc").clone().into();
    let path = doc.path().clone();
    let set = rt.block_on(async {
        let et = EditableTextProxy::builder(conn.connection())
            .destination(name)
            .ok()?
            .path(path)
            .ok()?
            .build()
            .await
            .ok()?;
        et.set_text_contents("AccessKit caret drive\nsecond line").await.ok()
    });
    println!("set_text_contents -> {set:?}");
    std::thread::sleep(Duration::from_millis(500));

    // Snapshot through the mirror; find the text container and its first run.
    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let (windows, _focus) = source.initial_state();
    let (window, container, selection) = windows
        .iter()
        .flat_map(|(descriptor, update)| {
            update.nodes.iter().map(move |(id, node)| (descriptor.id, *id, node))
        })
        .find_map(|(window, id, node)| {
            node.text_selection().map(|selection| (window, id, selection))
        })
        .expect("a node carrying a TextSelection");
    let nodes: std::collections::HashMap<_, _> = windows
        .iter()
        .flat_map(|(_, update)| update.nodes.iter())
        .map(|(id, node)| (*id, node))
        .collect();
    let run = *nodes[&container]
        .children()
        .iter()
        .find(|id| nodes[id].role() == accesskit::Role::TextRun)
        .expect("a TextRun child");
    let run_chars = nodes[&run].value().map_or(0, |v| v.chars().count());
    println!(
        "container {} in window {}: initial selection {:?}, run {} ({} chars)",
        container.0, window.0, selection, run.0, run_chars
    );

    let index = if selection.focus.node == run && selection.focus.character_index == 3 { 5 } else { 3 };
    assert!(run_chars > index, "run too short to move the caret into");
    let position = accesskit::TextPosition { node: run, character_index: index };
    let requested = accesskit::TextSelection { anchor: position, focus: position };
    println!("requesting collapsed caret at run {} index {index}", run.0);

    source.perform(
        window,
        &accesskit::ActionRequest {
            action: accesskit::Action::SetTextSelection,
            target_tree: accesskit::TreeId::ROOT,
            target_node: container,
            data: Some(accesskit::ActionData::SetTextSelection(requested)),
        },
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut hit = false;
    let mut update_seen = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            if let SourceEvent::TreeUpdate { window, update } = event {
                update_seen = true;
                let observed = update
                    .nodes
                    .iter()
                    .find(|(id, _)| *id == container)
                    .and_then(|(_, node)| node.text_selection());
                println!(
                    "update for window {}: {} nodes, container selection {:?}",
                    window.0,
                    update.nodes.len(),
                    observed
                );
                if observed == Some(&requested) {
                    hit = true;
                }
            }
        }
    }

    if hit {
        println!("PASS: set_caret_offset round-tripped to a delta with the requested caret");
    } else if update_seen {
        eprintln!(
            "FAIL: updates arrived but the caret never moved \
             (the AT-SPI caret write failed or was ineffective; ambient \
             re-walks can mask the no-event signal)"
        );
        std::process::exit(2);
    } else {
        eprintln!(
            "FAIL: no event at all (the AT-SPI caret write failed — NotSupported?)"
        );
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
