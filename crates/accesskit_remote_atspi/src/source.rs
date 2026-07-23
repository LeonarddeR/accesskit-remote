//! [`AtspiSource`]: a synchronous [`TreeSource`] backed by an async AT-SPI
//! bridge. A dedicated thread runs a tokio runtime that owns the connection
//! and the [`Mirror`] state; the sync side talks to it over channels —
//! actions in via a tokio channel, tree events out via a std channel.

use crate::focus::FocusTracker;
use crate::mapping::{build_window_update, focus_update, NodeIdMap};
use crate::mirror::{self, BridgeResult};
use crate::reconcile::{reconcile_windows, WindowKey};
use accesskit_remote::WindowId;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use atspi::connection::AccessibilityConnection;
use atspi::events::object::{ChildrenChangedEvent, StateChangedEvent};
use atspi::events::{Event, EventProperties, ObjectEvents, WindowEvents};
use atspi::object_ref::ObjectRefOwned;
use atspi::State;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::StreamExt;

type Snapshot = (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>);

/// An action to perform, routed from the sync side to the bridge thread.
struct PerformMsg {
    window: WindowId,
    action: accesskit::Action,
    node: accesskit::NodeId,
}

/// A [`TreeSource`] that mirrors the live AT-SPI accessibility tree.
pub struct AtspiSource {
    events: std_mpsc::Receiver<SourceEvent>,
    actions: tokio_mpsc::UnboundedSender<PerformMsg>,
    initial: Snapshot,
    _thread: JoinHandle<()>,
}

impl AtspiSource {
    /// Connects to the accessibility bus and performs the initial enumeration,
    /// blocking until the first snapshot is ready. The bridge thread then
    /// stays alive to service actions.
    pub fn new() -> BridgeResult<Self> {
        let (events_tx, events_rx) = std_mpsc::channel();
        let (actions_tx, actions_rx) = tokio_mpsc::unbounded_channel();
        let (init_tx, init_rx) = std_mpsc::channel();

        let thread = std::thread::Builder::new()
            .name("accesskit-atspi".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _ = init_tx.send(Err(format!("tokio runtime: {e}")));
                        return;
                    }
                };
                runtime.block_on(bridge_main(events_tx, actions_rx, init_tx));
            })?;

        let initial = init_rx
            .recv()
            .map_err(|_| "atspi bridge exited before initial sync")??;
        Ok(Self {
            events: events_rx,
            actions: actions_tx,
            initial,
            _thread: thread,
        })
    }
}

impl TreeSource for AtspiSource {
    fn initial_state(
        &mut self,
    ) -> (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>) {
        self.initial.clone()
    }

    fn perform(&mut self, window: WindowId, request: &accesskit::ActionRequest) {
        let _ = self.actions.send(PerformMsg {
            window,
            action: request.action,
            node: request.target_node,
        });
    }

    fn poll_events(&mut self) -> Vec<SourceEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }
}

/// The bridge thread's async entry point: connect, enumerate once, hand the
/// snapshot back, then service actions until the sync side drops the sender.
async fn bridge_main(
    events_tx: std_mpsc::Sender<SourceEvent>,
    mut actions_rx: tokio_mpsc::UnboundedReceiver<PerformMsg>,
    init_tx: std_mpsc::Sender<Result<Snapshot, String>>,
) {
    let conn = match mirror::connect().await {
        Ok(conn) => conn,
        Err(e) => {
            let _ = init_tx.send(Err(format!("connect: {e}")));
            return;
        }
    };
    // A dedicated connection carries the event stream. Its MessageStream must
    // not share a connection with method calls: a full event broadcast would
    // stall that connection's socket reader and deadlock in-flight replies.
    let event_conn = match mirror::connect().await {
        Ok(conn) => conn,
        Err(e) => {
            let _ = init_tx.send(Err(format!("connect events: {e}")));
            return;
        }
    };
    if let Err(e) = register_events(&event_conn).await {
        let _ = init_tx.send(Err(format!("register events: {e}")));
        return;
    }
    let mut mirror = Mirror::new();
    let snapshot = match mirror.enumerate(&conn).await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            let _ = init_tx.send(Err(format!("enumerate: {e}")));
            return;
        }
    };
    if init_tx.send(Ok(snapshot)).is_err() {
        return;
    }

    let events = event_conn.event_stream();
    tokio::pin!(events);
    'pump: loop {
        tokio::select! {
            action = actions_rx.recv() => match action {
                Some(msg) => {
                    if let Some(event) = mirror.handle_action(&conn, msg).await {
                        if events_tx.send(event).is_err() {
                            break 'pump;
                        }
                    }
                }
                None => break 'pump,
            },
            item = events.next() => match item {
                Some(Ok(event)) => {
                    for source_event in mirror.handle_atspi_event(&conn, event).await {
                        if events_tx.send(source_event).is_err() {
                            break 'pump;
                        }
                    }
                }
                Some(Err(_)) => {}
                None => break 'pump,
            },
        }
    }
}

/// Subscribes to the AT-SPI events that drive passive tree reflection:
/// structural `children-changed`, window lifecycle/activation, and
/// `state-changed` (filtered to `:focused` in the handler). The coarse
/// `state-changed` rule also delivers unrelated state changes; the handler
/// discards them in O(1) and never re-walks on them.
async fn register_events(conn: &AccessibilityConnection) -> BridgeResult<()> {
    conn.register_event::<ChildrenChangedEvent>().await?;
    conn.register_event::<WindowEvents>().await?;
    conn.register_event::<StateChangedEvent>().await?;
    Ok(())
}

/// Authoritative mirror state: one [`WindowState`] per discovered toplevel
/// frame, keyed so re-walks reuse stable node ids. The connection is owned by
/// the bridge and passed in, so its event stream can borrow it alongside.
struct Mirror {
    windows: Vec<WindowState>,
    next_id: u64,
    focus: FocusTracker,
}

struct WindowState {
    id: WindowId,
    root: ObjectRefOwned,
    ids: NodeIdMap,
    objects: HashMap<accesskit::NodeId, ObjectRefOwned>,
    /// The node this window last reported as focused, kept live so partial
    /// (focus-only, and later caret) deltas can carry a non-stale `focus`.
    focus: accesskit::NodeId,
}

impl Mirror {
    fn new() -> Self {
        Self {
            windows: Vec::new(),
            next_id: 1,
            focus: FocusTracker::default(),
        }
    }

    /// Discovers every visible frame, walks each into a tree, and returns the
    /// initial snapshot (windows with full trees, plus the focused window).
    async fn enumerate(&mut self, conn: &AccessibilityConnection) -> BridgeResult<Snapshot> {
        let discovered = mirror::discover_windows(conn).await?;
        let mut out = Vec::new();
        let mut focus = None;
        for window in discovered {
            let active = window.active;
            if let Some((descriptor, update)) = self.add_discovered(conn, window).await? {
                if active {
                    focus = Some(descriptor.id);
                }
                out.push((descriptor, update));
            }
        }
        if focus.is_none() {
            focus = self.windows.first().map(|w| w.id);
        }
        self.focus = FocusTracker::new(focus);
        Ok((out, focus))
    }

    /// Walks a freshly discovered frame, allocates its window id, records its
    /// state, and returns the descriptor plus initial tree. Returns `Ok(None)`
    /// — tracking nothing — when the frame walks empty, so a not-yet-ready
    /// window is retried on the next event rather than announced broken.
    async fn add_discovered(
        &mut self,
        conn: &AccessibilityConnection,
        window: mirror::DiscoveredWindow,
    ) -> BridgeResult<Option<(WindowDescriptor, accesskit::TreeUpdate)>> {
        let (nodes, objects_by_path) = mirror::walk_window(conn, &window.root).await?;
        if nodes.is_empty() {
            return Ok(None);
        }
        let id = WindowId(self.next_id);
        self.next_id += 1;
        let mut descriptor = window.descriptor;
        descriptor.id = id;
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes, &mut ids);
        let objects = index_objects(&nodes, &ids, &objects_by_path);
        self.windows.push(WindowState {
            id,
            root: window.root,
            ids,
            objects,
            focus: update.focus,
        });
        Ok(Some((descriptor, update)))
    }

    /// Performs an action on its target object, then re-walks that window and
    /// returns the resulting full-tree update.
    async fn handle_action(
        &mut self,
        conn: &AccessibilityConnection,
        msg: PerformMsg,
    ) -> Option<SourceEvent> {
        let target = {
            let window = self.windows.iter().find(|w| w.id == msg.window)?;
            window.objects.get(&msg.node)?.clone()
        };
        mirror::perform(conn, &target, msg.action).await.ok()?;
        self.rewalk(conn, msg.window).await
    }

    /// Reflects an AT-SPI event. A toplevel add/remove (see
    /// [`is_window_lifecycle_event`]) triggers a full [`reconcile`](Self::reconcile).
    /// A `state-changed:focused` gain emits a node-level focus delta without a
    /// re-walk (see [`handle_focus_change`](Self::handle_focus_change)).
    /// Otherwise the event re-walks the affected window(s): a deep
    /// `children-changed` is matched to its window by sender (app); an
    /// activate/deactivate is matched to the frame by sender and path and also
    /// advances window-level focus.
    async fn handle_atspi_event(
        &mut self,
        conn: &AccessibilityConnection,
        event: Event,
    ) -> Vec<SourceEvent> {
        if is_window_lifecycle_event(&event) {
            return self.reconcile(conn).await;
        }
        if let Event::Object(ObjectEvents::StateChanged(ev)) = &event {
            if ev.state == State::Focused {
                let enabled = ev.enabled;
                return self.handle_focus_change(conn, &event, enabled).await;
            }
        }
        let activation = matches!(&event, Event::Window(WindowEvents::Activate(_)));
        let deactivation = matches!(&event, Event::Window(WindowEvents::Deactivate(_)));
        let targets: Vec<WindowId> = match &event {
            Event::Object(ObjectEvents::ChildrenChanged(_)) => {
                let sender = event.sender();
                self.windows
                    .iter()
                    .filter(|w| w.root.name().is_some_and(|n| n.as_str() == sender.as_str()))
                    .map(|w| w.id)
                    .collect()
            }
            Event::Window(WindowEvents::Activate(_) | WindowEvents::Deactivate(_)) => {
                let sender = event.sender();
                let path = event.path();
                self.windows
                    .iter()
                    .filter(|w| {
                        w.root.path_as_str() == path.as_str()
                            && w.root.name().is_some_and(|n| n.as_str() == sender.as_str())
                    })
                    .map(|w| w.id)
                    .collect()
            }
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for window in &targets {
            if let Some(source_event) = self.rewalk(conn, *window).await {
                out.push(source_event);
            }
        }
        // Window-level focus follows activation, emitted after the re-walk's
        // update so the client has the fresh tree before focus moves to it.
        for window in &targets {
            let change = if activation {
                self.focus.focus(*window)
            } else if deactivation {
                self.focus.deactivate(*window)
            } else {
                None
            };
            if let Some(change) = change {
                out.push(SourceEvent::FocusChanged(change));
            }
        }
        out
    }

    /// Reflects an AT-SPI focus state change. A focus *gain* resolves the
    /// emitting object to its window and node and emits a focus-only
    /// `TreeUpdate` (no re-walk), plus a window-level `FocusChanged` when the
    /// focused window changed. A focus *loss* (`enabled == false`) is ignored:
    /// `TreeUpdate.focus` is mandatory, so "nothing focused" is expressed only
    /// at window level (via deactivate) and by the consumer's host-focus gating.
    /// When the object has not been walked yet, the owning app's windows are
    /// re-walked so a fresh tree carries `State::Focused`.
    async fn handle_focus_change(
        &mut self,
        conn: &AccessibilityConnection,
        event: &Event,
        enabled: bool,
    ) -> Vec<SourceEvent> {
        if !enabled {
            return Vec::new();
        }
        let sender = event.sender();
        let path = event.path();
        if let Some((window, node)) =
            resolve_focus_target(&self.windows, sender.as_str(), path.as_str())
        {
            if let Some(state) = self.windows.iter_mut().find(|w| w.id == window) {
                state.focus = node;
            }
            let mut out = vec![SourceEvent::TreeUpdate {
                window,
                update: focus_update(node),
            }];
            if let Some(change) = self.focus.focus(window) {
                out.push(SourceEvent::FocusChanged(change));
            }
            return out;
        }
        let targets: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|w| w.root.name().is_some_and(|n| n.as_str() == sender.as_str()))
            .map(|w| w.id)
            .collect();
        let mut out = Vec::new();
        for window in targets {
            if let Some(source_event) = self.rewalk(conn, window).await {
                out.push(source_event);
            }
        }
        out
    }

    /// Re-walks one window and rebuilds its tree, reusing its stable id map.
    async fn rewalk(
        &mut self,
        conn: &AccessibilityConnection,
        window: WindowId,
    ) -> Option<SourceEvent> {
        let index = self.windows.iter().position(|w| w.id == window)?;
        let root = self.windows[index].root.clone();
        let (nodes, objects_by_path) = mirror::walk_window(conn, &root).await.ok()?;
        if nodes.is_empty() {
            return None;
        }
        let state = &mut self.windows[index];
        let update = build_window_update(&nodes, &mut state.ids);
        state.objects = index_objects(&nodes, &state.ids, &objects_by_path);
        state.focus = update.focus;
        Some(SourceEvent::TreeUpdate { window, update })
    }

    /// Reconciles the tracked window set against a fresh discovery: drops and
    /// announces vanished toplevels, walks and announces newly visible ones.
    /// A removed window is dropped from the focus tracker; the client nulls its
    /// own focus reference on `WindowRemoved`, so no `FocusChanged` is emitted.
    async fn reconcile(&mut self, conn: &AccessibilityConnection) -> Vec<SourceEvent> {
        let discovered = match mirror::discover_windows(conn).await {
            Ok(discovered) => discovered,
            Err(_) => return Vec::new(),
        };
        let tracked: Vec<WindowKey> = self.windows.iter().map(|w| window_key(&w.root)).collect();
        let fresh: Vec<WindowKey> = discovered.iter().map(|w| window_key(&w.root)).collect();
        let diff = reconcile_windows(&tracked, &fresh);

        let mut out = Vec::new();
        // Resolve removal ids before mutating so the indices do not shift.
        let removed: Vec<WindowId> = diff.removed.iter().map(|&i| self.windows[i].id).collect();
        self.windows.retain(|w| !removed.contains(&w.id));
        for &id in &removed {
            self.focus.remove(id);
        }
        out.extend(removed.into_iter().map(SourceEvent::WindowRemoved));

        let added: HashSet<usize> = diff.added.into_iter().collect();
        for (index, window) in discovered.into_iter().enumerate() {
            if !added.contains(&index) {
                continue;
            }
            if let Ok(Some((descriptor, tree))) = self.add_discovered(conn, window).await {
                out.push(SourceEvent::WindowAdded { descriptor, tree });
            }
        }
        out
    }
}

/// The reconcile identity of a toplevel frame: its application's unique bus
/// name plus the frame's object path.
fn window_key(root: &ObjectRefOwned) -> WindowKey {
    WindowKey {
        bus_name: root.name().map(|name| name.as_str().to_owned()).unwrap_or_default(),
        path: root.path_as_str().to_owned(),
    }
}

/// The AT-SPI root object path. Each application exposes its root accessible
/// here and the desktop registry exposes the application list here, so a
/// `children-changed` at this path is a toplevel add or an application removal.
const ATSPI_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

/// Whether an event signals that a toplevel window was added or removed, as
/// opposed to a change within an existing window. GTK4 does not emit
/// `window:create`/`window:destroy` in this environment; it reports toplevel
/// lifecycle as `children-changed` on [`ATSPI_ROOT_PATH`]. The window
/// create/destroy variants are honored too for toolkits that do emit them.
fn is_window_lifecycle_event(event: &Event) -> bool {
    match event {
        Event::Window(WindowEvents::Create(_) | WindowEvents::Destroy(_)) => true,
        Event::Object(ObjectEvents::ChildrenChanged(_)) => {
            event.path().as_str() == ATSPI_ROOT_PATH
        }
        _ => false,
    }
}

/// Resolves a focus event's sender bus name and object path to the window and
/// node it belongs to. The node must exist in the window's *current* tree
/// (`objects`, rebuilt on every walk) — not merely its append-only id map — so
/// a node pruned since it was first seen is never targeted; an unknown focus id
/// is fatal to a consumer applying the update. Sender plus path also pins the
/// correct window when one app owns several.
fn resolve_focus_target(
    windows: &[WindowState],
    sender: &str,
    path: &str,
) -> Option<(WindowId, accesskit::NodeId)> {
    windows.iter().find_map(|w| {
        if !w.root.name().is_some_and(|n| n.as_str() == sender) {
            return None;
        }
        let id = w.ids.get(path)?;
        w.objects.contains_key(&id).then_some((w.id, id))
    })
}

/// Builds the node-id → object map used to route actions back to AT-SPI.
fn index_objects(
    nodes: &[crate::mapping::MirrorNode],
    ids: &NodeIdMap,
    objects_by_path: &HashMap<String, ObjectRefOwned>,
) -> HashMap<accesskit::NodeId, ObjectRefOwned> {
    let mut objects = HashMap::new();
    for node in nodes {
        if let (Some(id), Some(object)) = (ids.get(&node.path), objects_by_path.get(&node.path)) {
            objects.insert(id, object.clone());
        }
    }
    objects
}

#[cfg(test)]
mod tests {
    use super::*;
    use atspi::object_ref::ObjectRef;
    use atspi::zbus::names::UniqueName;
    use atspi::zbus::zvariant::ObjectPath;

    /// Builds a one-node `WindowState`: the window root plus a single node whose
    /// id is allocated in `ids` and, when `walked`, present in `objects`.
    fn window_state(
        id: u64,
        sender: &'static str,
        root_path: &'static str,
        node_path: &'static str,
        walked: bool,
    ) -> WindowState {
        let root = ObjectRef::new_owned(
            UniqueName::from_static_str_unchecked(sender),
            ObjectPath::from_static_str_unchecked(root_path),
        );
        let mut ids = NodeIdMap::new();
        let node_id = ids.id_for(node_path);
        let mut objects = HashMap::new();
        if walked {
            objects.insert(
                node_id,
                ObjectRef::new_owned(
                    UniqueName::from_static_str_unchecked(sender),
                    ObjectPath::from_static_str_unchecked(node_path),
                ),
            );
        }
        WindowState {
            id: WindowId(id),
            root,
            ids,
            objects,
            focus: node_id,
        }
    }

    #[test]
    fn resolves_by_sender_and_path() {
        let windows = vec![window_state(1, ":1.1", "/win/1", "/win/1/node", true)];
        let node = windows[0].ids.get("/win/1/node").unwrap();
        assert_eq!(
            resolve_focus_target(&windows, ":1.1", "/win/1/node"),
            Some((WindowId(1), node))
        );
    }

    #[test]
    fn same_path_under_a_different_sender_does_not_match() {
        let windows = vec![window_state(1, ":1.1", "/win/1", "/shared/node", true)];
        assert_eq!(resolve_focus_target(&windows, ":1.2", "/shared/node"), None);
    }

    #[test]
    fn multi_window_same_app_resolves_to_the_owning_window() {
        let windows = vec![
            window_state(1, ":1.1", "/win/1", "/win/1/node", true),
            window_state(2, ":1.1", "/win/2", "/win/2/node", true),
        ];
        let node2 = windows[1].ids.get("/win/2/node").unwrap();
        assert_eq!(
            resolve_focus_target(&windows, ":1.1", "/win/2/node"),
            Some((WindowId(2), node2))
        );
    }

    #[test]
    fn path_absent_from_current_objects_does_not_resolve() {
        let windows = vec![window_state(1, ":1.1", "/win/1", "/win/1/gone", false)];
        assert_eq!(resolve_focus_target(&windows, ":1.1", "/win/1/gone"), None);
    }

    #[test]
    fn unknown_path_does_not_resolve() {
        let windows = vec![window_state(1, ":1.1", "/win/1", "/win/1/node", true)];
        assert_eq!(resolve_focus_target(&windows, ":1.1", "/win/1/other"), None);
    }
}
