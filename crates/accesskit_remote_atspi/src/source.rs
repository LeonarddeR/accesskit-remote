//! [`AtspiSource`]: a synchronous [`TreeSource`] backed by an async AT-SPI
//! bridge. A dedicated thread runs a tokio runtime that owns the connection
//! and the [`Mirror`] state; the sync side talks to it over channels —
//! actions in via a tokio channel, tree events out via a std channel.

use crate::mapping::{build_window_update, NodeIdMap};
use crate::mirror::{self, BridgeResult};
use accesskit_remote::WindowId;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use atspi::connection::AccessibilityConnection;
use atspi::object_ref::ObjectRefOwned;
use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use tokio::sync::mpsc as tokio_mpsc;

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
    let mut mirror = Mirror::new(conn);
    let snapshot = match mirror.enumerate().await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            let _ = init_tx.send(Err(format!("enumerate: {e}")));
            return;
        }
    };
    if init_tx.send(Ok(snapshot)).is_err() {
        return;
    }

    while let Some(msg) = actions_rx.recv().await {
        if let Some(event) = mirror.handle_action(msg).await {
            if events_tx.send(event).is_err() {
                break;
            }
        }
    }
}

/// Authoritative mirror state: the connection plus one [`WindowState`] per
/// discovered toplevel frame, keyed so re-walks reuse stable node ids.
struct Mirror {
    conn: AccessibilityConnection,
    windows: Vec<WindowState>,
    next_id: u64,
}

struct WindowState {
    id: WindowId,
    root: ObjectRefOwned,
    ids: NodeIdMap,
    objects: HashMap<accesskit::NodeId, ObjectRefOwned>,
}

impl Mirror {
    fn new(conn: AccessibilityConnection) -> Self {
        Self {
            conn,
            windows: Vec::new(),
            next_id: 1,
        }
    }

    /// Discovers every visible frame, walks each into a tree, and returns the
    /// initial snapshot (windows with full trees, plus the focused window).
    async fn enumerate(&mut self) -> BridgeResult<Snapshot> {
        let discovered = mirror::discover_windows(&self.conn).await?;
        let mut out = Vec::new();
        let mut focus = None;
        for mut window in discovered {
            let id = WindowId(self.next_id);
            self.next_id += 1;
            window.descriptor.id = id;

            let (nodes, objects_by_path) = mirror::walk_window(&self.conn, &window.root).await?;
            if nodes.is_empty() {
                continue;
            }
            let mut ids = NodeIdMap::new();
            let update = build_window_update(&nodes, &mut ids);
            let objects = index_objects(&nodes, &ids, &objects_by_path);

            if window.active {
                focus = Some(id);
            }
            out.push((window.descriptor, update));
            self.windows.push(WindowState {
                id,
                root: window.root,
                ids,
                objects,
            });
        }
        if focus.is_none() {
            focus = self.windows.first().map(|w| w.id);
        }
        Ok((out, focus))
    }

    /// Performs an action on its target object, then re-walks that window and
    /// returns the resulting full-tree update.
    async fn handle_action(&mut self, msg: PerformMsg) -> Option<SourceEvent> {
        let target = {
            let window = self.windows.iter().find(|w| w.id == msg.window)?;
            window.objects.get(&msg.node)?.clone()
        };
        mirror::perform(&self.conn, &target, msg.action).await.ok()?;
        self.rewalk(msg.window).await
    }

    /// Re-walks one window and rebuilds its tree, reusing its stable id map.
    async fn rewalk(&mut self, window: WindowId) -> Option<SourceEvent> {
        let index = self.windows.iter().position(|w| w.id == window)?;
        let root = self.windows[index].root.clone();
        let (nodes, objects_by_path) = mirror::walk_window(&self.conn, &root).await.ok()?;
        if nodes.is_empty() {
            return None;
        }
        let state = &mut self.windows[index];
        let update = build_window_update(&nodes, &mut state.ids);
        state.objects = index_objects(&nodes, &state.ids, &objects_by_path);
        Some(SourceEvent::TreeUpdate { window, update })
    }
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
