//! The async AT-SPI bus layer: connect, discover toplevel frames, walk a
//! frame's subtree into [`MirrorNode`]s, and perform actions. All functions
//! run on the bridge thread's tokio runtime; they hold no long-lived state of
//! their own (the [`crate::source`] `Mirror` owns that).

use crate::app_id::AppIdResolver;
use crate::mapping::{
    clamp_text, has_text_caret, node_states, reads_text_runs, CharExtent, MirrorNode, TextState,
    MAX_GEOMETRY_CHARS, MAX_TEXT_CHARS,
};
use accesskit_remote::{AppInfo, WindowId};
use accesskit_remote_server::WindowDescriptor;
use atspi::connection::AccessibilityConnection;
use atspi::object_ref::ObjectRefOwned;
use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::application::ApplicationProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::selection::SelectionProxy;
use atspi::proxy::text::TextProxy;
use atspi::zbus::fdo::{DBusProxy, PropertiesProxy};
use atspi::zbus::names::{BusName, InterfaceName};
use atspi::zbus::proxy::CacheProperties;
use atspi::{CoordType, Interface, InterfaceSet, Role, State, StateSet};
use std::collections::{HashMap, HashSet, VecDeque};

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
pub async fn discover_windows(
    conn: &AccessibilityConnection,
    app_ids: &mut AppIdResolver,
) -> BridgeResult<Vec<DiscoveredWindow>> {
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
        let app_info = read_app_info(zconn, &app_ref, app_name, app_ids).await;
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
/// `Application` interface, pid from the a11y bus's `org.freedesktop.DBus`,
/// and the application id from the session-bus name that pid owns. Pieces
/// that cannot be read are left `None`.
async fn read_app_info(
    zconn: &atspi::zbus::Connection,
    app_ref: &ObjectRefOwned,
    name: String,
    app_ids: &mut AppIdResolver,
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
    if let Some(pid) = info.pid {
        info.app_id = app_ids.app_id_for_pid(pid).await;
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
        let Some((node, child_refs)) = read_node(zconn, &obj, true).await else {
            continue;
        };
        queue.extend(child_refs);
        nodes.push(node);
        objects.insert(path, obj);
    }
    Ok((nodes, objects))
}

/// Reads one AT-SPI object into a [`MirrorNode`] plus its non-null child
/// refs. `None` only when the object's proxy cannot be built; individual
/// property failures degrade to the same defaults the walk has always used.
pub(crate) async fn read_node(
    zconn: &atspi::zbus::Connection,
    obj: &ObjectRefOwned,
    with_text: bool,
) -> Option<(MirrorNode, Vec<ObjectRefOwned>)> {
    let proxy = obj.as_accessible_proxy(zconn).await.ok()?;
    // One pipelined batch. The app still services these serially on its main
    // loop, but issuing them together removes the client-side round-trip
    // stacking that dominated the per-node cost.
    let (role, identity, states, interfaces, child_refs) = tokio::join!(
        proxy.get_role(),
        read_identity(zconn, obj),
        proxy.get_state(),
        proxy.get_interfaces(),
        proxy.get_children(),
    );
    let role = role.unwrap_or(Role::Invalid);
    let (name, description) = identity;
    // A failed interface read degrades to "advertises nothing", so every
    // interface-gated read below is skipped rather than attempted blind.
    let interfaces = interfaces.unwrap_or_else(|_| InterfaceSet::empty());
    let mut children = Vec::new();
    let mut refs = Vec::new();
    for child in child_refs.unwrap_or_default() {
        if child.is_null() {
            continue;
        }
        children.push(child.path_as_str().to_owned());
        refs.push(child);
    }
    let wants_text =
        with_text && reads_text_runs(role, !children.is_empty()) && interfaces.contains(Interface::Text);
    let (text, bounds) = tokio::join!(
        async {
            if wants_text {
                read_text_state(zconn, obj, has_text_caret(role), true).await
            } else {
                None
            }
        },
        async {
            if interfaces.contains(Interface::Component) {
                read_component_extents(zconn, obj).await
            } else {
                None
            }
        },
    );
    let node = MirrorNode {
        path: obj.path_as_str().to_owned(),
        role,
        name,
        description,
        interfaces,
        states: node_states(states.unwrap_or_else(|_| StateSet::empty())),
        children,
        text,
        bounds,
    };
    Some((node, refs))
}

/// Reads the `Accessible` interface's `Name` and `Description` in a single
/// round trip via `Properties.GetAll`, rather than one call each. Verified
/// supported by both GTK4's own bridge and at-spi2-atk. Any failure degrades to
/// empty strings, matching how the walk treats every other unreadable property.
async fn read_identity(zconn: &atspi::zbus::Connection, obj: &ObjectRefOwned) -> (String, String) {
    async fn inner(
        zconn: &atspi::zbus::Connection,
        obj: &ObjectRefOwned,
    ) -> BridgeResult<(String, String)> {
        let name: BusName = obj.name().ok_or("null identity target")?.clone().into();
        let proxy = PropertiesProxy::builder(zconn)
            .destination(name)?
            .path(obj.path().clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let props = proxy
            .get_all(InterfaceName::try_from("org.a11y.atspi.Accessible")?)
            .await?;
        let read = |key: &str| -> String {
            props
                .get(key)
                .cloned()
                .and_then(|value| String::try_from(value).ok())
                .unwrap_or_default()
        };
        Ok((read("Name"), read("Description")))
    }
    inner(zconn, obj).await.unwrap_or_default()
}

/// Bounds the parent climb from an unwalked descendant to a known ancestor.
pub(crate) const MAX_SPLICE_HOPS: usize = 16;

/// Reads `descendant`, then climbs `Accessible.Parent` until a path in
/// `known` is reached, reading each object on the way. Returns the chain
/// ancestor-first (`chain[0]` is the known ancestor's fresh read, the last
/// element the descendant), or `None` when a read fails, a parent is null,
/// or the hop budget runs out.
pub(crate) async fn read_chain_to_known(
    conn: &AccessibilityConnection,
    descendant: &ObjectRefOwned,
    known: &HashSet<String>,
    max_hops: usize,
) -> Option<Vec<(MirrorNode, ObjectRefOwned)>> {
    let zconn = conn.connection();
    let mut chain: Vec<(MirrorNode, ObjectRefOwned)> = Vec::new();
    let mut current = descendant.clone();
    for _ in 0..max_hops {
        let (node, _) = read_node(zconn, &current, true).await?;
        let reached_known = known.contains(&node.path);
        chain.push((node, current.clone()));
        if reached_known {
            chain.reverse();
            return Some(chain);
        }
        let proxy = current.as_accessible_proxy(zconn).await.ok()?;
        let parent = proxy.parent().await.ok()?;
        if parent.is_null() {
            return None;
        }
        current = parent;
    }
    None
}

/// Reads the AT-SPI `Text` interface of `obj`: its text (capped at
/// [`MAX_TEXT_CHARS`] code points), when `with_caret` its caret offset and
/// first selection, and when `with_geometry` its per-character window-relative
/// extents (only up to [`MAX_GEOMETRY_CHARS`] code points — one bus call per
/// character). All offsets are Unicode scalar value (code point) indices.
/// Returns `None` if the text itself cannot be read; caret, selection, and
/// extents degrade to `None` individually.
pub async fn read_text_state(
    zconn: &atspi::zbus::Connection,
    obj: &ObjectRefOwned,
    with_caret: bool,
    with_geometry: bool,
) -> Option<TextState> {
    let name: BusName = obj.name()?.clone().into();
    let path = obj.path().clone();
    let proxy = TextProxy::builder(zconn)
        .destination(name)
        .ok()?
        .path(path)
        .ok()?
        // zbus caches properties lazily by default, which costs an AddMatch and
        // a GetAll on first property access and a RemoveMatch on drop -- pure
        // overhead for a proxy built to serve one node and dropped.
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()?;

    let count = proxy.character_count().await.ok()?.max(0);
    let capped = count.min(MAX_TEXT_CHARS as i32);
    let raw = proxy.get_text(0, capped).await.ok()?;
    let text = clamp_text(&raw).to_owned();
    let len = text.chars().count();

    let (caret, selection) = if with_caret {
        let caret = proxy
            .caret_offset()
            .await
            .ok()
            .filter(|&offset| offset >= 0)
            .map(|offset| (offset as usize).min(len));
        (caret, read_first_selection(&proxy, len).await)
    } else {
        (None, None)
    };

    let extents = if with_geometry && len <= MAX_GEOMETRY_CHARS {
        read_char_extents(&proxy, len).await
    } else {
        None
    };

    Some(TextState { text, caret, selection, extents })
}

/// Reads one window-relative extent per code point in `[0, len)`. Any single
/// failure returns `None` — geometry is all-or-nothing per node, so the
/// mapping never emits partial arrays.
async fn read_char_extents(proxy: &TextProxy<'_>, len: usize) -> Option<Vec<CharExtent>> {
    let mut extents = Vec::with_capacity(len);
    for offset in 0..len {
        let (x, y, width, height) = proxy
            .get_character_extents(offset as i32, CoordType::Window)
            .await
            .ok()?;
        extents.push(CharExtent { x, y, width, height });
    }
    Some(extents)
}

/// Reads an object's own window-relative extents off its `Component`
/// interface; `None` on any failure.
async fn read_component_extents(
    zconn: &atspi::zbus::Connection,
    obj: &ObjectRefOwned,
) -> Option<CharExtent> {
    let name: BusName = obj.name()?.clone().into();
    let path = obj.path().clone();
    let proxy = ComponentProxy::builder(zconn)
        .destination(name)
        .ok()?
        .path(path)
        .ok()?
        // zbus caches properties lazily by default, which costs an AddMatch and
        // a GetAll on first property access and a RemoveMatch on drop -- pure
        // overhead for a proxy built to serve one node and dropped.
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()?;
    let (x, y, width, height) = proxy.get_extents(CoordType::Window).await.ok()?;
    Some(CharExtent { x, y, width, height })
}

/// Reads the first AT-SPI text selection as a normalized `(start, end)` with
/// `start < end`; `None` when there is no non-degenerate selection.
async fn read_first_selection(proxy: &TextProxy<'_>, len: usize) -> Option<(usize, usize)> {
    if proxy.get_n_selections().await.ok()? <= 0 {
        return None;
    }
    let (start, end) = proxy.get_selection(0).await.ok()?;
    if start < 0 || end < 0 {
        return None;
    }
    let start = (start as usize).min(len);
    let end = (end as usize).min(len);
    let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
    (lo != hi).then_some((lo, hi))
}

/// The outcome of reading a container's selected children.
pub enum SelectedChildren {
    /// The object paths of the currently selected children.
    Paths(Vec<String>),
    /// More children are selected than the caller's cap allows.
    TooMany,
    /// The object exposes no readable `Selection` interface.
    Unavailable,
}

/// Reads the object paths of `container`'s selected children off its AT-SPI
/// `Selection` interface, at most `cap` of them.
pub async fn read_selected_children(
    conn: &AccessibilityConnection,
    container: &ObjectRefOwned,
    cap: usize,
) -> SelectedChildren {
    async fn inner(
        conn: &AccessibilityConnection,
        container: &ObjectRefOwned,
        cap: usize,
    ) -> BridgeResult<SelectedChildren> {
        let name: BusName = container.name().ok_or("null selection container")?.clone().into();
        let proxy = SelectionProxy::builder(conn.connection())
            .destination(name)?
            .path(container.path().clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let count = proxy.n_selected_children().await?;
        if count <= 0 {
            return Ok(SelectedChildren::Paths(Vec::new()));
        }
        if count as usize > cap {
            return Ok(SelectedChildren::TooMany);
        }
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let child = proxy.get_selected_child(index).await?;
            if child.is_null() {
                continue;
            }
            paths.push(child.path_as_str().to_owned());
        }
        Ok(SelectedChildren::Paths(paths))
    }
    inner(conn, container, cap)
        .await
        .unwrap_or(SelectedChildren::Unavailable)
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

/// Sets the caret or selection on a text object via its AT-SPI `Text` interface.
/// A collapsed range (`anchor == focus`) moves the caret; a real range sets
/// selection slot 0 with `start < end`. Offsets are code-point indices.
pub async fn set_text_selection(
    conn: &AccessibilityConnection,
    target: &ObjectRefOwned,
    anchor: usize,
    focus: usize,
) -> BridgeResult<()> {
    let zconn = conn.connection();
    let name: BusName = target.name().ok_or("null text selection target")?.clone().into();
    let path = target.path().clone();
    let proxy = TextProxy::builder(zconn)
        .destination(name)?
        .path(path)?
        .build()
        .await?;
    if anchor == focus {
        proxy.set_caret_offset(focus as i32).await?;
    } else {
        let (start, end) = (anchor.min(focus), anchor.max(focus));
        proxy.set_selection(0, start as i32, end as i32).await?;
    }
    Ok(())
}
