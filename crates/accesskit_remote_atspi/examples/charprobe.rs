//! Raw AT-SPI characterization probe. Walks the tree of every application whose
//! name contains the `argv[1]` filter (case-insensitive; empty matches all) and
//! prints each node's role, name, the states the mapping cares about, its
//! interface set, and its action names when the Action interface is present.
//! Reads straight off the bus with no mirror in between, so it shows the
//! ground-truth AT-SPI shape of menus, combo boxes, and their items. Every
//! D-Bus call is time-bounded so one unresponsive app cannot wedge the walk.

#[cfg(target_os = "linux")]
fn main() {
    use atspi::connection::AccessibilityConnection;
    use atspi::object_ref::ObjectRefOwned;
    use atspi::proxy::accessible::ObjectRefExt;
    use atspi::proxy::action::ActionProxy;
    use atspi::zbus::names::BusName;
    use atspi::{Interface, Role, State, StateSet};
    use std::collections::VecDeque;
    use std::time::Duration;
    use tokio::time::timeout;

    const CALL: Duration = Duration::from_secs(3);

    async fn action_names(
        zconn: &atspi::zbus::Connection,
        obj: &ObjectRefOwned,
    ) -> Option<Vec<String>> {
        let name: BusName = obj.name()?.clone().into();
        let build = ActionProxy::builder(zconn)
            .destination(name)
            .ok()?
            .path(obj.path().clone())
            .ok()?
            .build();
        let proxy = timeout(CALL, build).await.ok()?.ok()?;
        let acts = timeout(CALL, proxy.get_actions()).await.ok()?.ok()?;
        Some(acts.into_iter().map(|a| a.name).collect())
    }

    async fn app_display_name(
        zconn: &atspi::zbus::Connection,
        app_ref: &ObjectRefOwned,
    ) -> Option<String> {
        let app = timeout(CALL, app_ref.as_accessible_proxy(zconn)).await.ok()?.ok()?;
        timeout(CALL, app.name()).await.ok()?.ok()
    }

    async fn do_action(
        zconn: &atspi::zbus::Connection,
        obj: &ObjectRefOwned,
        index: i32,
    ) -> bool {
        let Some(name) = obj.name() else { return false };
        let bus: BusName = name.clone().into();
        let Ok(b1) = ActionProxy::builder(zconn).destination(bus) else { return false };
        let Ok(b2) = b1.path(obj.path().clone()) else { return false };
        let Ok(Ok(proxy)) = timeout(CALL, b2.build()).await else { return false };
        matches!(timeout(CALL, proxy.do_action(index)).await, Ok(Ok(true)))
    }

    /// BFS the app subtree; on the first node whose action-name list contains
    /// `substr`, invoke that action and return true.
    async fn find_and_do(
        zconn: &atspi::zbus::Connection,
        app_ref: ObjectRefOwned,
        substr: &str,
    ) -> bool {
        let mut queue: VecDeque<ObjectRefOwned> = VecDeque::new();
        queue.push_back(app_ref);
        let mut seen = 0usize;
        while let Some(obj) = queue.pop_front() {
            seen += 1;
            if seen > 4000 {
                break;
            }
            let Ok(Ok(proxy)) = timeout(CALL, obj.as_accessible_proxy(zconn)).await else {
                continue;
            };
            let ifaces = timeout(CALL, proxy.get_interfaces()).await.ok().and_then(|r| r.ok());
            if ifaces.as_ref().is_some_and(|s| s.contains(Interface::Action)) {
                if let Some(acts) = action_names(zconn, &obj).await {
                    if let Some(i) = acts.iter().position(|a| a.to_lowercase().contains(substr)) {
                        println!("--> do_action {:?} on {:?}", acts[i], obj.path_as_str());
                        let ok = do_action(zconn, &obj, i as i32).await;
                        println!("--> do_action returned {ok}");
                        return true;
                    }
                }
            }
            let children = timeout(CALL, proxy.get_children())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            for child in children {
                if !child.is_null() {
                    queue.push_back(child);
                }
            }
        }
        false
    }

    async fn walk_app(zconn: &atspi::zbus::Connection, app_ref: ObjectRefOwned) {
        let mut queue: VecDeque<(ObjectRefOwned, usize)> = VecDeque::new();
        queue.push_back((app_ref, 0));
        let mut seen = 0usize;
        while let Some((obj, depth)) = queue.pop_front() {
            seen += 1;
            if seen > 4000 {
                println!("... (capped at 4000 nodes)");
                break;
            }
            let Ok(Ok(proxy)) = timeout(CALL, obj.as_accessible_proxy(zconn)).await else {
                continue;
            };
            let role = timeout(CALL, proxy.get_role())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(Role::Invalid);
            let name = timeout(CALL, proxy.name())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let states = timeout(CALL, proxy.get_state())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_else(StateSet::empty);
            let ifaces = timeout(CALL, proxy.get_interfaces()).await.ok().and_then(|r| r.ok());
            let mut sflags: Vec<&str> = Vec::new();
            for (label, st) in [
                ("focusable", State::Focusable),
                ("focused", State::Focused),
                ("selectable", State::Selectable),
                ("selected", State::Selected),
                ("expandable", State::Expandable),
                ("expanded", State::Expanded),
                ("collapsed", State::Collapsed),
                ("checkable", State::Checkable),
                ("checked", State::Checked),
                ("indeterminate", State::Indeterminate),
                ("pressed", State::Pressed),
                ("has_popup", State::HasPopup),
                ("showing", State::Showing),
                ("sensitive", State::Sensitive),
                ("enabled", State::Enabled),
                ("editable", State::Editable),
                ("read_only", State::ReadOnly),
                ("required", State::Required),
                ("invalid_entry", State::InvalidEntry),
                ("modal", State::Modal),
                ("multiselectable", State::Multiselectable),
                ("busy", State::Busy),
                ("horizontal", State::Horizontal),
                ("vertical", State::Vertical),
            ] {
                if states.contains(st) {
                    sflags.push(label);
                }
            }
            let actions = if ifaces.as_ref().is_some_and(|s| s.contains(Interface::Action)) {
                action_names(zconn, &obj).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            let indent = "  ".repeat(depth);
            println!(
                "{indent}{role:?} {name:?} [{}] ifaces={ifaces:?} actions={actions:?}",
                sflags.join(","),
            );
            let children = timeout(CALL, proxy.get_children())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            for child in children {
                if !child.is_null() {
                    queue.push_back((child, depth + 1));
                }
            }
        }
    }

    let args: Vec<String> = std::env::args().collect();
    let filter = args.get(1).cloned().unwrap_or_default().to_lowercase();
    let open = args
        .iter()
        .position(|a| a == "--open")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.to_lowercase());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = rt.block_on(AccessibilityConnection::new()).expect("raw connection");

    rt.block_on(async {
        let zconn = conn.connection();
        let registry = conn.root_accessible_on_registry().await.expect("registry root");

        if let Some(sub) = &open {
            let children = timeout(Duration::from_secs(5), registry.get_children())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            for app_ref in children {
                if app_ref.is_null() {
                    continue;
                }
                let Some(app_name) = app_display_name(zconn, &app_ref).await else {
                    continue;
                };
                if !filter.is_empty() && !app_name.to_lowercase().contains(&filter) {
                    continue;
                }
                if find_and_do(zconn, app_ref, sub).await {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(900)).await;
        }

        let children = timeout(Duration::from_secs(5), registry.get_children())
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        for app_ref in children {
            if app_ref.is_null() {
                continue;
            }
            let Some(app_name) = app_display_name(zconn, &app_ref).await else {
                continue;
            };
            if !filter.is_empty() && !app_name.to_lowercase().contains(&filter) {
                continue;
            }
            println!("== app {app_name:?} ==");
            let _ = timeout(Duration::from_secs(60), walk_app(zconn, app_ref)).await;
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {}
