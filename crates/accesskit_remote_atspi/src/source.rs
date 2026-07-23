//! [`AtspiSource`]: a synchronous [`TreeSource`] backed by an async AT-SPI
//! bridge. A dedicated thread runs a tokio runtime that owns the connection
//! and the [`Mirror`] state; the sync side talks to it over channels —
//! actions in via a tokio channel, tree events out via a std channel.

use crate::mapping::{build_window_update, NodeIdMap};
use crate::mirror::{self, BridgeResult};
use accesskit_remote::WindowId;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use atspi::connection::AccessibilityConnection;
use atspi::events::object::ChildrenChangedEvent;
use atspi::events::{Event, EventProperties, ObjectEvents, WindowEvents};
use atspi::object_ref::ObjectRefOwned;
use std::collections::HashMap;
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
/// structural `children-changed` and window lifecycle/activation.
async fn register_events(conn: &AccessibilityConnection) -> BridgeResult<()> {
    conn.register_event::<ChildrenChangedEvent>().await?;
    conn.register_event::<WindowEvents>().await?;
    Ok(())
}

/// Authoritative mirror state: one [`WindowState`] per discovered toplevel
/// frame, keyed so re-walks reuse stable node ids. The connection is owned by
/// the bridge and passed in, so its event stream can borrow it alongside.
struct Mirror {
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
    fn new() -> Self {
        Self {
            windows: Vec::new(),
            next_id: 1,
        }
    }

    /// Discovers every visible frame, walks each into a tree, and returns the
    /// initial snapshot (windows with full trees, plus the focused window).
    async fn enumerate(&mut self, conn: &AccessibilityConnection) -> BridgeResult<Snapshot> {
        let discovered = mirror::discover_windows(conn).await?;
        let mut out = Vec::new();
        let mut focus = None;
        for mut window in discovered {
            let id = WindowId(self.next_id);
            self.next_id += 1;
            window.descriptor.id = id;

            let (nodes, objects_by_path) = mirror::walk_window(conn, &window.root).await?;
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

    /// Reflects an AT-SPI event: re-walks the affected window(s) so app-driven
    /// changes (and toolkit updates that lag an action's immediate re-walk)
    /// surface. `children-changed` fires on a deep node, so its window is
    /// matched by sender (app); window events fire on the frame, matched by
    /// its exact path.
    async fn handle_atspi_event(
        &mut self,
        conn: &AccessibilityConnection,
        event: Event,
    ) -> Vec<SourceEvent> {
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
