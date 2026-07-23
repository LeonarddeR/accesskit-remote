//! The async AT-SPI bus layer: connect, discover toplevel frames, walk a
//! frame's subtree into [`MirrorNode`]s, and perform actions. All functions
//! run on the bridge thread's tokio runtime; they hold no long-lived state of
//! their own (the [`crate::source`] `Mirror` owns that).

use crate::mapping::MirrorNode;
use accesskit_remote::{AppInfo, WindowId};
use accesskit_remote_server::WindowDescriptor;
use atspi::connection::AccessibilityConnection;
use atspi::object_ref::ObjectRefOwned;
use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::application::ApplicationProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::zbus::fdo::DBusProxy;
use atspi::zbus::names::BusName;
use atspi::{Interface, Role, State, StateSet};
use std::collections::{HashMap, VecDeque};

/// Errors from the bridge are boxed so bus, session, and zbus error types can
/// all flow through a single `?`.
pub type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Caps a single window's walk so a pathological tree cannot stall the bridge.
const MAX_NODES_PER_WINDOW: usize = 5000;

/// A toplevel frame found during discovery, before an id is assigned.
pub struct DiscoveredWindow {
    pub root: ObjectRefOwned,
    pub descriptor: WindowDescriptor,
    pub active: bool,
}

/// Opens a connection to the accessibility bus.
pub async fn connect() -> BridgeResult<AccessibilityConnection> {
    Ok(AccessibilityConnection::new().await?)
}

/// Enumerates the desktop's applications and returns their visible toplevel
/// frames. Ids on the returned descriptors are placeholders for the caller.
pub async fn discover_windows(conn: &AccessibilityConnection) -> BridgeResult<Vec<DiscoveredWindow>> {
    let zconn = conn.connection();
    let registry = conn.root_accessible_on_registry().await?;
    let mut windows = Vec::new();
    for app_ref in registry.get_children().await? {
        if app_ref.is_null() {
            continue;
        }
        let Ok(app) = app_ref.as_accessible_proxy(zconn).await else {
            continue;
        };
        let app_name = app.name().await.unwrap_or_default();
        let app_info = read_app_info(zconn, &app_ref, app_name).await;
        let frames = app.get_children().await.unwrap_or_default();
        for frame_ref in frames {
            if frame_ref.is_null() {
                continue;
            }
            let Ok(frame) = frame_ref.as_accessible_proxy(zconn).await else {
                continue;
            };
            let role = frame.get_role().await.unwrap_or(Role::Invalid);
            if !matches!(role, Role::Frame | Role::Window | Role::Dialog) {
                continue;
            }
            let states = frame.get_state().await.unwrap_or_else(|_| StateSet::empty());
            if !states.contains(State::Showing) || !states.contains(State::Visible) {
                continue;
            }
            let title = frame.name().await.unwrap_or_default();
            windows.push(DiscoveredWindow {
                root: frame_ref,
                descriptor: WindowDescriptor {
                    id: WindowId(0),
                    title,
                    app: app_info.clone(),
                },
                active: states.contains(State::Active),
            });
        }
    }
    Ok(windows)
}

/// Reads an application's identity: toolkit name and version from its
/// `Application` interface, pid from the a11y bus's `org.freedesktop.DBus`.
/// Pieces that cannot be read are left `None`.
async fn read_app_info(
    zconn: &atspi::zbus::Connection,
    app_ref: &ObjectRefOwned,
    name: String,
) -> AppInfo {
    let mut info = AppInfo {
        name,
        ..AppInfo::default()
    };
    let bus_name: BusName = match app_ref.name() {
        Some(unique) => unique.clone().into(),
        None => return info,
    };
    let app = async {
        ApplicationProxy::builder(zconn)
            .destination(bus_name.clone())?
            .path(app_ref.path())?
            .build()
            .await
    }
    .await
    .ok();
    if let Some(app) = app {
        info.toolkit = non_empty(app.toolkit_name().await);
        info.toolkit_version = non_empty(app.version().await);
    }
    if let Ok(dbus) = DBusProxy::new(zconn).await {
        info.pid = dbus.get_connection_unix_process_id(bus_name).await.ok();
    }
    info
}

/// Maps a successful but empty property read to `None`.
fn non_empty<E>(result: Result<String, E>) -> Option<String> {
    result.ok().filter(|value| !value.is_empty())
}

/// Walks the subtree rooted at `root`, breadth first, into flat
/// [`MirrorNode`]s plus a path → object map for action routing.
pub async fn walk_window(
    conn: &AccessibilityConnection,
    root: &ObjectRefOwned,
) -> BridgeResult<(Vec<MirrorNode>, HashMap<String, ObjectRefOwned>)> {
    let zconn = conn.connection();
    let mut nodes = Vec::new();
    let mut objects: HashMap<String, ObjectRefOwned> = HashMap::new();
    let mut queue: VecDeque<ObjectRefOwned> = VecDeque::new();
    queue.push_back(root.clone());
    while let Some(obj) = queue.pop_front() {
        if nodes.len() >= MAX_NODES_PER_WINDOW {
            break;
        }
        let path = obj.path_as_str().to_owned();
        if objects.contains_key(&path) {
            continue;
        }
        let Ok(proxy) = obj.as_accessible_proxy(zconn).await else {
            continue;
        };
        let role = proxy.get_role().await.unwrap_or(Role::Invalid);
        let name = proxy.name().await.unwrap_or_default();
        let states = proxy.get_state().await.unwrap_or_else(|_| StateSet::empty());
        let actionable = proxy
            .get_interfaces()
            .await
            .map(|set| set.contains(Interface::Action))
            .unwrap_or(false);
        let mut children = Vec::new();
        for child in proxy.get_children().await.unwrap_or_default() {
            if child.is_null() {
                continue;
            }
            children.push(child.path_as_str().to_owned());
            queue.push_back(child);
        }
        nodes.push(MirrorNode {
            path: path.clone(),
            role,
            name,
            focusable: states.contains(State::Focusable),
            focused: states.contains(State::Focused),
            actionable,
            children,
        });
        objects.insert(path, obj);
    }
    Ok((nodes, objects))
}

/// Performs an AccessKit action on an AT-SPI object: `Click` invokes the
/// object's default action, `Focus` grabs focus. Other actions are ignored.
pub async fn perform(
    conn: &AccessibilityConnection,
    target: &ObjectRefOwned,
    action: accesskit::Action,
) -> BridgeResult<()> {
    let zconn = conn.connection();
    let name: BusName = target.name().ok_or("null action target")?.clone().into();
    let path = target.path().clone();
    match action {
        accesskit::Action::Click => {
            let proxy = ActionProxy::builder(zconn)
                .destination(name)?
                .path(path)?
                .build()
                .await?;
            proxy.do_action(0).await?;
        }
        accesskit::Action::Focus => {
            let proxy = ComponentProxy::builder(zconn)
                .destination(name)?
                .path(path)?
                .build()
                .await?;
            proxy.grab_focus().await?;
        }
        _ => {}
    }
    Ok(())
}
