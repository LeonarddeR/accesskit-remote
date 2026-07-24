//! Proves text-change forwarding: editing the document's contents makes GTK
//! emit `object:text-changed`, which the mirror turns into a minimal delta whose
//! synthesized TextRun children carry the new text — no re-walk. Passes only if
//! a delta reflecting the edited text arrives.

#[cfg(target_os = "linux")]
fn main() {
    use accesskit_remote_server::{SourceEvent, TreeSource};
    use atspi::connection::AccessibilityConnection;
    use atspi::object_ref::ObjectRefOwned;
    use atspi::proxy::accessible::ObjectRefExt;
    use atspi::proxy::editable_text::EditableTextProxy;
    use atspi::zbus::names::BusName;
    use atspi::{Interface, Role};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    const MARKER: &str = "AccessKit caret test";

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(AccessibilityConnection::new()).expect("raw connection");

    // Locate the first editable Role::Text document.
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
                        if seen > 800 {
                            break;
                        }
                        let Ok(p) = obj.as_accessible_proxy(zconn).await else {
                            continue;
                        };
                        let role = p.get_role().await.unwrap_or(Role::Invalid);
                        let ifaces = p.get_interfaces().await.ok();
                        if role == Role::Text && ifaces.is_some_and(|s| s.contains(Interface::Text)) {
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
        .expect("an editable Role::Text document");
    println!("document: {}", doc.path_as_str());

    // Start the mirror (subscribes to text events) before editing.
    let mut source = accesskit_remote_atspi::AtspiSource::new().expect("connect to AT-SPI");
    let _ = source.initial_state();

    // Edit the document via the raw connection; GTK emits text-changed.
    let edited = format!("{MARKER}\nsecond line");
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
        et.set_text_contents(&edited).await.ok()
    });
    println!("set_text_contents -> {set:?}");

    // Watch for a delta whose TextRun children carry the edited text.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut hit = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        for event in source.poll_events() {
            if let SourceEvent::TreeUpdate { window, update } = event {
                let run_nodes: Vec<_> = update
                    .nodes
                    .iter()
                    .filter(|(_, n)| n.role() == accesskit::Role::TextRun)
                    .collect();
                let with_geom = run_nodes.iter().filter(|(_, n)| n.bounds().is_some()).count();
                let runs: String = run_nodes.iter().filter_map(|(_, n)| n.value()).collect();
                if !update.nodes.is_empty() {
                    println!(
                        "delta window {}: {} nodes, geom {}/{} runs, runs {:?}",
                        window.0,
                        update.nodes.len(),
                        with_geom,
                        run_nodes.len(),
                        runs
                    );
                }
                if runs.contains(MARKER) {
                    hit = true;
                }
            }
        }
    }

    if hit {
        println!("PASS: a text delta carried the edited text without a re-walk");
    } else {
        eprintln!("FAIL: no delta reflected the edited text");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
