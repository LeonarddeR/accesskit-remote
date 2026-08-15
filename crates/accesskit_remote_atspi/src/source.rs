//! [`AtspiSource`]: a synchronous [`TreeSource`] backed by an async AT-SPI
//! bridge. A dedicated thread runs a tokio runtime that owns the connection
//! and the [`Mirror`] state; the sync side talks to it over channels —
//! actions in via a tokio channel, tree events out via a std channel.

use crate::app_id::AppIdResolver;
use crate::invalidate::{property_is_mirrored, state_is_mirrored};
use crate::mapping::{
    build_window_update, emitted_children, focus_update, merge_update, rebuild_text_node,
    refresh_node, splice_chain_update, text_offset, NodeIdMap, WindowCache,
};
use crate::mirror::{self, BridgeResult, SelectedChildren};
use crate::reconcile::WindowKey;
use accesskit_remote::WindowId;
use accesskit_remote_source::coalesce::RewalkCoalescer;
use accesskit_remote_source::focus::FocusTracker;
use accesskit_remote_source::limiter::{selection_refresh_targets, NodeRefreshLimiter};
use accesskit_remote_source::reconcile::reconcile_windows;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use atspi::connection::AccessibilityConnection;
use atspi::events::object::{
    ActiveDescendantChangedEvent, ChildrenChangedEvent, PropertyChangeEvent, SelectionChangedEvent,
    StateChangedEvent, TextCaretMovedEvent, TextChangedEvent, TextSelectionChangedEvent,
};
use atspi::events::{Event, EventProperties, ObjectEvents, WindowEvents};
use atspi::object_ref::{ObjectRef, ObjectRefOwned};
use atspi::zbus::names::UniqueName;
use atspi::zbus::zvariant::ObjectPath;
use atspi::State;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::StreamExt;

/// How often the mirror reconciles its tracked window set against a fresh
/// discovery, catching lifecycle signals the reactive path may have missed (a
/// window that becomes visible without re-signaling, or an app that dies without
/// a root removal).
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// Quiet period after the last deep `children-changed` of a burst before the
/// affected window is re-walked.
const REWALK_DEBOUNCE_QUIET: Duration = Duration::from_millis(250);

/// A window is re-walked at most this long after the first event of a burst,
/// even while events keep arriving.
const REWALK_DEBOUNCE_MAX_DELAY: Duration = Duration::from_secs(2);

/// Minimum spacing between semantic refreshes of the same node. The first
/// change emits immediately; changes inside this window collapse into one
/// trailing refresh.
const NODE_REFRESH_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// A node changing continuously (a progress bar) is refreshed at least this
/// often.
const NODE_REFRESH_MAX_DELAY: Duration = Duration::from_millis(500);

/// How many selected children a `selection-changed` will read individually
/// before falling back to a debounced re-walk of the whole window.
const MAX_SELECTED_CHILDREN: usize = 32;

type Snapshot = (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>);

/// The shared per-node refresh limiter, keyed by AT-SPI object path.
type NodeRefresh = NodeRefreshLimiter<String>;

/// An action to perform, routed from the sync side to the bridge thread with
/// its request payload intact.
struct PerformMsg {
    window: WindowId,
    action: accesskit::Action,
    node: accesskit::NodeId,
    data: Option<accesskit::ActionData>,
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
            data: request.data.clone(),
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
    let mut reconcile_timer = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile_timer.tick().await; // Drop the immediate first tick; the initial enumeration just ran.
    let mut pending = RewalkCoalescer::new(REWALK_DEBOUNCE_QUIET, REWALK_DEBOUNCE_MAX_DELAY);
    let mut refresh = NodeRefresh::new(NODE_REFRESH_MIN_INTERVAL, NODE_REFRESH_MAX_DELAY);
    'pump: loop {
        // One timer arm serves both schedules; each drains whatever is due.
        let next_timer = earliest(pending.next_deadline(), refresh.next_deadline());
        tokio::select! {
            action = actions_rx.recv() => match action {
                Some(msg) => mirror.handle_action(&conn, msg, &mut pending).await,
                None => break 'pump,
            },
            item = events.next() => match item {
                Some(Ok(event)) => {
                    let out = mirror
                        .handle_atspi_event(&conn, event, &mut pending, &mut refresh)
                        .await;
                    for source_event in out {
                        if events_tx.send(source_event).is_err() {
                            break 'pump;
                        }
                    }
                }
                Some(Err(_)) => {}
                None => break 'pump,
            },
            // The sleep expression is evaluated even when the guard disables
            // the arm, so the `None` case needs a valid (never-awaited) instant.
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                next_timer.unwrap_or_else(|| Instant::now() + RECONCILE_INTERVAL),
            )), if next_timer.is_some() => {
                let now = Instant::now();
                // Events arriving while a re-walk runs queue on the event
                // stream and re-note a fresh burst; nothing is lost.
                for window in pending.take_due(now) {
                    if let Some(source_event) = mirror.rewalk(&conn, window).await {
                        if events_tx.send(source_event).is_err() {
                            break 'pump;
                        }
                    }
                }
                let mut due: HashMap<WindowId, Vec<String>> = HashMap::new();
                for (window, path) in refresh.take_due(now) {
                    due.entry(window).or_default().push(path);
                }
                for (window, paths) in due {
                    for source_event in mirror.refresh_paths(&conn, window, &paths).await {
                        if events_tx.send(source_event).is_err() {
                            break 'pump;
                        }
                    }
                }
            },
            _ = reconcile_timer.tick() => {
                for source_event in mirror.reconcile(&conn).await {
                    if let SourceEvent::WindowRemoved(window) = &source_event {
                        pending.discard(*window);
                        refresh.discard(*window);
                    }
                    if events_tx.send(source_event).is_err() {
                        break 'pump;
                    }
                }
            }
        }
    }
}

/// Subscribes to the AT-SPI events that drive passive tree reflection:
/// structural `children-changed`, window lifecycle/activation, `state-changed`
/// (`:focused` moves focus, the mirrored states refresh the node),
/// `property-change` and `selection-changed` (also node refreshes),
/// `active-descendant-changed` (focus moving within a container), and the text
/// events that move the caret or change text/selection. The coarse
/// `state-changed` and `property-change` rules also deliver changes the mapping
/// does not forward; the handler discards those in O(1). Everything but the
/// structural and lifecycle events re-queries single nodes and never re-walks.
///
/// `bounds-changed` is deliberately not registered: it fires on every resize
/// and scroll, and bounds refresh with the re-walk.
async fn register_events(conn: &AccessibilityConnection) -> BridgeResult<()> {
    conn.register_event::<ChildrenChangedEvent>().await?;
    conn.register_event::<WindowEvents>().await?;
    conn.register_event::<StateChangedEvent>().await?;
    conn.register_event::<PropertyChangeEvent>().await?;
    conn.register_event::<SelectionChangedEvent>().await?;
    conn.register_event::<ActiveDescendantChangedEvent>().await?;
    conn.register_event::<TextCaretMovedEvent>().await?;
    conn.register_event::<TextChangedEvent>().await?;
    conn.register_event::<TextSelectionChangedEvent>().await?;
    Ok(())
}

/// The earlier of two optional deadlines.
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (deadline, None) | (None, deadline) => deadline,
    }
}

/// Authoritative mirror state: one [`WindowState`] per discovered toplevel
/// frame, keyed so re-walks reuse stable node ids. The connection is owned by
/// the bridge and passed in, so its event stream can borrow it alongside.
struct Mirror {
    windows: Vec<WindowState>,
    next_id: u64,
    focus: FocusTracker,
    app_ids: AppIdResolver,
}

struct WindowState {
    id: WindowId,
    root: ObjectRefOwned,
    ids: NodeIdMap,
    objects: HashMap<accesskit::NodeId, ObjectRefOwned>,
    /// The node this window last reported as focused, kept live so partial
    /// (focus-only, and caret) deltas can carry a non-stale `focus`.
    focus: accesskit::NodeId,
    /// What this window last emitted, keyed by AT-SPI object path: every
    /// container node the client holds, plus per-text-node bookkeeping for
    /// minimal caret and text-change deltas.
    cache: WindowCache,
    /// The element children each walked node was emitted with, by path — the
    /// client tree's current structure, consulted when splicing new nodes in.
    children: HashMap<String, Vec<String>>,
}

impl Mirror {
    fn new() -> Self {
        Self {
            windows: Vec::new(),
            next_id: 1,
            focus: FocusTracker::default(),
            app_ids: AppIdResolver::default(),
        }
    }

    /// Discovers every visible frame, walks each into a tree, and returns the
    /// initial snapshot (windows with full trees, plus the focused window).
    async fn enumerate(&mut self, conn: &AccessibilityConnection) -> BridgeResult<Snapshot> {
        let discovered = mirror::discover_windows(conn, &mut self.app_ids).await?;
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
        let mut cache = WindowCache::default();
        let update = build_window_update(&nodes, &mut ids, &mut cache);
        let objects = index_objects(&nodes, &ids, &objects_by_path);
        self.windows.push(WindowState {
            id,
            root: window.root,
            ids,
            objects,
            focus: update.focus,
            cache,
            children: emitted_children(&nodes),
        });
        Ok(Some((descriptor, update)))
    }

    /// Performs an action on its target object, then marks the window for a
    /// debounced re-walk. The feedback a consumer sees is the delta the
    /// toolkit's own state/text/children events produce; the walk is the safety
    /// net for an action a toolkit reports nothing for.
    /// `SetTextSelection` resolves its anchor/focus run positions against the
    /// target's text-node layout into global AT-SPI offsets (the run ids live in
    /// the layout, never in `objects`, so only the container node routes through
    /// `objects`).
    async fn handle_action(
        &mut self,
        conn: &AccessibilityConnection,
        msg: PerformMsg,
        pending: &mut RewalkCoalescer,
    ) {
        tracing::info!(
            action = ?msg.action,
            node = msg.node.0,
            window = msg.window.0,
            "action request",
        );
        let (target, selection) = {
            let Some(window) = self.windows.iter().find(|w| w.id == msg.window) else {
                return;
            };
            let Some(target) = window.objects.get(&msg.node).cloned() else {
                return;
            };
            let selection = if msg.action == accesskit::Action::SetTextSelection {
                let resolved = msg.data.as_ref().and_then(|data| {
                    let accesskit::ActionData::SetTextSelection(sel) = data else {
                        return None;
                    };
                    let cache = window.cache.text.get(target.path_as_str())?;
                    let anchor = text_offset(&cache.layout, &sel.anchor)?;
                    let focus = text_offset(&cache.layout, &sel.focus)?;
                    Some((anchor, focus))
                });
                match resolved {
                    Some(offsets) => Some(offsets),
                    None => return,
                }
            } else {
                None
            };
            (target, selection)
        };
        match selection {
            Some((anchor, focus)) => {
                if let Err(e) = mirror::set_text_selection(conn, &target, anchor, focus).await {
                    tracing::warn!(path = target.path_as_str(), "set_text_selection failed: {e}");
                    return;
                }
            }
            None => {
                if let Err(e) =
                    mirror::perform(conn, &target, msg.action, msg.data.as_ref()).await
                {
                    tracing::warn!(
                        action = ?msg.action,
                        path = target.path_as_str(),
                        "perform action failed: {e}",
                    );
                    return;
                }
            }
        }
        pending.note(msg.window, Instant::now());
    }

    /// Reflects an AT-SPI event. A toplevel add/remove (see
    /// [`is_window_lifecycle_event`]) triggers a full [`reconcile`](Self::reconcile).
    /// A `state-changed:focused` gain, or an `active-descendant-changed` moving
    /// focus within a container, emits a node-level focus delta without a re-walk
    /// (see [`handle_focus_change`](Self::handle_focus_change) and
    /// [`handle_active_descendant`](Self::handle_active_descendant)).
    /// A deep `children-changed` is matched to its windows by sender (app) and
    /// marked in `pending`; the debounce arm in [`bridge_main`] re-walks each
    /// window once its burst settles. An activate/deactivate is matched to the
    /// frame by sender and path, re-walks it immediately, and advances
    /// window-level focus.
    ///
    /// A mirrored state change, a mirrored property change, or a selection
    /// change refreshes the affected node's semantics (see
    /// [`refresh_semantics`](Self::refresh_semantics) and
    /// [`refresh_selection`](Self::refresh_selection)), rate-limited per node by
    /// `refresh` — structure is left to the debounced re-walk, so these paths
    /// never mark `pending`.
    async fn handle_atspi_event(
        &mut self,
        conn: &AccessibilityConnection,
        event: Event,
        pending: &mut RewalkCoalescer,
        refresh: &mut NodeRefresh,
    ) -> Vec<SourceEvent> {
        if is_window_lifecycle_event(&event) {
            let out = self.reconcile(conn).await;
            for source_event in &out {
                if let SourceEvent::WindowRemoved(window) = source_event {
                    pending.discard(*window);
                    refresh.discard(*window);
                }
            }
            return out;
        }
        if let Event::Object(ObjectEvents::StateChanged(ev)) = &event {
            if ev.state == State::Focused {
                let enabled = ev.enabled;
                return self.handle_focus_change(conn, &event, enabled).await;
            }
            if !state_is_mirrored(ev.state) {
                return Vec::new();
            }
            let (sender, path) = (event.sender(), event.path());
            return self
                .refresh_semantics(conn, sender.as_str(), path.as_str(), refresh)
                .await;
        }
        if let Event::Object(ObjectEvents::PropertyChange(ev)) = &event {
            if !property_is_mirrored(&ev.property) {
                return Vec::new();
            }
            let (sender, path) = (event.sender(), event.path());
            return self
                .refresh_semantics(conn, sender.as_str(), path.as_str(), refresh)
                .await;
        }
        if let Event::Object(ObjectEvents::SelectionChanged(_)) = &event {
            let (sender, path) = (event.sender(), event.path());
            return self
                .refresh_selection(conn, sender.as_str(), path.as_str(), refresh, pending)
                .await;
        }
        if let Event::Object(ObjectEvents::ActiveDescendantChanged(ev)) = &event {
            let sender = event.sender();
            let path = ev.descendant.path_as_str();
            return match self.handle_active_descendant(sender.as_str(), path) {
                Some(out) => out,
                None => self.splice_active_descendant(conn, sender.as_str(), path).await,
            };
        }
        match &event {
            // Caret and selection moves change no text, so the cached run
            // geometry still applies and none is re-read. (Scrolling can
            // stale it, but AT-SPI emits no event for that either way.)
            Event::Object(ObjectEvents::TextCaretMoved(ev)) => {
                return self.refresh_text(conn, &ev.item, false).await
            }
            Event::Object(ObjectEvents::TextChanged(ev)) => {
                return self.refresh_text(conn, &ev.item, true).await
            }
            Event::Object(ObjectEvents::TextSelectionChanged(ev)) => {
                return self.refresh_text(conn, &ev.item, false).await
            }
            _ => {}
        }
        if let Event::Object(ObjectEvents::ChildrenChanged(_)) = &event {
            let sender = event.sender();
            let now = Instant::now();
            for window in self.windows_of_sender(sender.as_str()) {
                pending.note(window, now);
            }
            return Vec::new();
        }
        let activation = matches!(&event, Event::Window(WindowEvents::Activate(_)));
        let deactivation = matches!(&event, Event::Window(WindowEvents::Deactivate(_)));
        let targets: Vec<WindowId> = match &event {
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
            return self.emit_node_focus(window, node);
        }
        let targets = self.windows_of_sender(sender.as_str());
        let mut out = Vec::new();
        for window in targets {
            if let Some(source_event) = self.rewalk(conn, window).await {
                out.push(source_event);
            }
        }
        out
    }

    /// The ids of every tracked window owned by the app at `sender`.
    fn windows_of_sender(&self, sender: &str) -> Vec<WindowId> {
        self.windows
            .iter()
            .filter(|w| w.root.name().is_some_and(|n| n.as_str() == sender))
            .map(|w| w.id)
            .collect()
    }

    /// Reflects an `active-descendant-changed`: focus moving among a container's
    /// descendants (lists, trees, combo boxes). Resolves the new descendant to
    /// its window and node by the emitting app's bus name and the descendant's
    /// object path, and emits a node-level focus delta. A descendant absent from
    /// the current tree returns `None`, signaling the caller to read and splice
    /// it in on demand. Item selection *state* is not forwarded here; that stays
    /// governed by re-walks.
    fn handle_active_descendant(
        &mut self,
        sender: &str,
        descendant_path: &str,
    ) -> Option<Vec<SourceEvent>> {
        resolve_focus_target(&self.windows, sender, descendant_path)
            .map(|(window, node)| self.emit_node_focus(window, node))
    }

    /// Emits a node-level focus move: keeps the window's live `focus` in step,
    /// emits a focus-only delta (no re-walk), and a window-level `FocusChanged`
    /// when the focused window changed.
    fn emit_node_focus(&mut self, window: WindowId, node: accesskit::NodeId) -> Vec<SourceEvent> {
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
        out
    }

    /// Resolves an active descendant missing from the walked tree by reading
    /// it and its ancestors up to a known node directly off the bus, splicing
    /// the chain into the owning window, and focusing it. Emits nothing when
    /// the chain cannot be read or no tracked window anchors it. The event
    /// sender (not the event body's embedded name) addresses the objects,
    /// matching `resolve_focus_target`'s sender pinning.
    async fn splice_active_descendant(
        &mut self,
        conn: &AccessibilityConnection,
        sender: &str,
        descendant_path: &str,
    ) -> Vec<SourceEvent> {
        let Ok(name) = UniqueName::try_from(sender.to_owned()) else {
            return Vec::new();
        };
        let Ok(path) = ObjectPath::try_from(descendant_path.to_owned()) else {
            return Vec::new();
        };
        let descendant = ObjectRef::new_owned(name, path);
        let candidates: Vec<usize> = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.root.name().is_some_and(|n| n.as_str() == sender))
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let known: HashSet<String> = candidates
            .iter()
            .flat_map(|&index| self.windows[index].children.keys().cloned())
            .collect();
        let Some(chain) =
            mirror::read_chain_to_known(conn, &descendant, &known, mirror::MAX_SPLICE_HOPS).await
        else {
            return Vec::new();
        };
        let anchor = chain[0].0.path.clone();
        let Some(index) = candidates
            .into_iter()
            .find(|&index| self.windows[index].children.contains_key(&anchor))
        else {
            return Vec::new();
        };
        let Some(update) = self.apply_spliced_chain(index, &chain) else {
            return Vec::new();
        };
        let window = self.windows[index].id;
        let mut out = vec![SourceEvent::TreeUpdate { window, update }];
        if let Some(change) = self.focus.focus(window) {
            out.push(SourceEvent::FocusChanged(change));
        }
        out
    }

    /// Applies a freshly read chain to the window at `index`: allocates ids,
    /// builds the splice update, and folds the chain into `objects`,
    /// `children`, and `focus`. `None` when the chain's first node is not a
    /// known ancestor of this window.
    fn apply_spliced_chain(
        &mut self,
        index: usize,
        chain: &[(crate::mapping::MirrorNode, ObjectRefOwned)],
    ) -> Option<accesskit::TreeUpdate> {
        let nodes: Vec<crate::mapping::MirrorNode> =
            chain.iter().map(|(node, _)| node.clone()).collect();
        let state = &mut self.windows[index];
        let ancestor_children = state.children.get(nodes.first()?.path.as_str())?.clone();
        let known: HashSet<String> = state.children.keys().cloned().collect();
        let result = splice_chain_update(
            &nodes,
            &ancestor_children,
            &known,
            &mut state.ids,
            &mut state.cache,
        )?;
        for (node, object) in chain {
            if let Some(id) = state.ids.get(&node.path) {
                state.objects.insert(id, object.clone());
            }
        }
        for (path, children) in result.children {
            state.children.insert(path, children);
        }
        state.focus = result.update.focus;
        Some(result.update)
    }

    /// Re-reads one node's semantics after a state or property change and emits
    /// a minimal delta. The node must be one the emitting app's window tree
    /// currently holds, so an untracked object costs no bus call; the per-node
    /// limiter decides whether to read now or schedule a trailing refresh.
    async fn refresh_semantics(
        &mut self,
        conn: &AccessibilityConnection,
        sender: &str,
        path: &str,
        refresh: &mut NodeRefresh,
    ) -> Vec<SourceEvent> {
        let Some(index) = resolve_tracked_node(&self.windows, sender, path) else {
            return Vec::new();
        };
        let window = self.windows[index].id;
        if !refresh.note(window, path, Instant::now()) {
            return Vec::new();
        }
        self.refresh_paths(conn, window, &[path.to_owned()]).await
    }

    /// Reflects a container's `selection-changed` by re-reading the children
    /// whose `selected` state can have moved: those the client currently shows
    /// as selected, plus those the container now reports. A container with more
    /// than [`MAX_SELECTED_CHILDREN`] selected falls back to the debounced
    /// re-walk.
    async fn refresh_selection(
        &mut self,
        conn: &AccessibilityConnection,
        sender: &str,
        path: &str,
        refresh: &mut NodeRefresh,
        pending: &mut RewalkCoalescer,
    ) -> Vec<SourceEvent> {
        let Some(index) = resolve_tracked_node(&self.windows, sender, path) else {
            return Vec::new();
        };
        let state = &self.windows[index];
        let window = state.id;
        let Some(container) = state.ids.get(path).and_then(|id| state.objects.get(&id)).cloned()
        else {
            return Vec::new();
        };
        let emitted = state.children.get(path).cloned().unwrap_or_default();
        let previously: Vec<String> = emitted
            .iter()
            .filter(|child| {
                state.cache.nodes.get(*child).is_some_and(|node| node.is_selected() == Some(true))
            })
            .cloned()
            .collect();

        let selected = match mirror::read_selected_children(conn, &container, MAX_SELECTED_CHILDREN)
            .await
        {
            SelectedChildren::Paths(paths) => paths,
            SelectedChildren::TooMany => {
                pending.note(window, Instant::now());
                return Vec::new();
            }
            SelectedChildren::Unavailable => return Vec::new(),
        };

        let now = Instant::now();
        let targets: Vec<String> = selection_refresh_targets(&previously, &selected)
            .into_iter()
            .filter(|target| emitted.iter().any(|child| child == target))
            .filter(|target| refresh.note(window, target, now))
            .collect();
        self.refresh_paths(conn, window, &targets).await
    }

    /// Re-reads each path's own AT-SPI state (no text, no children) and emits
    /// one delta of the nodes whose semantics changed. The window and its cache
    /// are resolved *after* the reads: a re-walk between them can prune a path,
    /// and [`refresh_node`] then declines it.
    ///
    /// This relies on `bridge_main` running one `select!` branch to completion —
    /// a future `tokio::spawn` of this work would need its own guard against a
    /// concurrent re-walk. [`refresh_text`](Self::refresh_text) has the same
    /// shape.
    async fn refresh_paths(
        &mut self,
        conn: &AccessibilityConnection,
        window: WindowId,
        paths: &[String],
    ) -> Vec<SourceEvent> {
        if paths.is_empty() {
            return Vec::new();
        }
        let mut fresh = Vec::with_capacity(paths.len());
        for path in paths {
            let Some(object) = self.object_at(window, path) else {
                continue;
            };
            if let Some((node, _)) = mirror::read_node(conn.connection(), &object, false).await {
                fresh.push(node);
            }
        }
        let Some(index) = self.windows.iter().position(|w| w.id == window) else {
            return Vec::new();
        };
        let state = &mut self.windows[index];
        let mut nodes = Vec::new();
        for node in &fresh {
            let Some(id) = state.ids.get(&node.path) else {
                continue;
            };
            let Some(children) = state.children.get(&node.path).cloned() else {
                continue;
            };
            if let Some(refreshed) =
                refresh_node(node, id, &children, &mut state.ids, &mut state.cache)
            {
                nodes.push(refreshed);
            }
        }
        if nodes.is_empty() {
            return Vec::new();
        }
        vec![SourceEvent::TreeUpdate {
            window,
            update: accesskit::TreeUpdate {
                nodes,
                tree: None,
                tree_id: accesskit::TreeId::ROOT,
                focus: state.focus,
            },
        }]
    }

    /// The AT-SPI object behind one path in one window, for a targeted re-read.
    fn object_at(&self, window: WindowId, path: &str) -> Option<ObjectRefOwned> {
        let state = self.windows.iter().find(|w| w.id == window)?;
        let id = state.ids.get(path)?;
        state.objects.get(&id).cloned()
    }

    /// Reflects an AT-SPI text event (caret move, text change, selection change)
    /// by re-querying just the emitting node's `Text` interface and emitting a
    /// minimal delta of the changed nodes — never a re-walk. Gated on the object
    /// being a tracked text node of the sending app, so unrelated objects and
    /// untracked apps cost no bus call.
    async fn refresh_text(
        &mut self,
        conn: &AccessibilityConnection,
        item: &ObjectRefOwned,
        with_geometry: bool,
    ) -> Vec<SourceEvent> {
        let Some(sender) = item.name() else {
            return Vec::new();
        };
        let sender = sender.as_str();
        let path = item.path_as_str();
        let Some(index) = self.windows.iter().position(|w| {
            w.root.name().is_some_and(|n| n.as_str() == sender) && w.cache.text.contains_key(path)
        }) else {
            return Vec::new();
        };
        let with_caret = self.windows[index].cache.text[path].caret_enabled;
        let Some(state) =
            mirror::read_text_state(conn.connection(), item, with_caret, with_geometry).await
        else {
            return Vec::new();
        };
        let window_state = &mut self.windows[index];
        let changed = rebuild_text_node(
            &mut window_state.cache,
            path,
            &state,
            &mut window_state.ids,
        );
        if changed.is_empty() {
            return Vec::new();
        }
        vec![SourceEvent::TreeUpdate {
            window: window_state.id,
            update: accesskit::TreeUpdate {
                nodes: changed,
                tree: None,
                tree_id: accesskit::TreeId::ROOT,
                focus: window_state.focus,
            },
        }]
    }

    /// Re-walks one window and rebuilds its tree, reusing its stable id map.
    /// A focused node the fresh walk cannot see (a spliced lazy cell) is
    /// re-spliced into the update; when that fails, the walk's own focus
    /// stands.
    async fn rewalk(
        &mut self,
        conn: &AccessibilityConnection,
        window: WindowId,
    ) -> Option<SourceEvent> {
        let index = self.windows.iter().position(|w| w.id == window)?;
        let root = self.windows[index].root.clone();
        let prev_focus_obj = {
            let state = &self.windows[index];
            state.objects.get(&state.focus).cloned()
        };
        let (nodes, objects_by_path) = mirror::walk_window(conn, &root).await.ok()?;
        if nodes.is_empty() {
            return None;
        }
        let state = &mut self.windows[index];
        let mut update = build_window_update(&nodes, &mut state.ids, &mut state.cache);
        state.objects = index_objects(&nodes, &state.ids, &objects_by_path);
        state.children = emitted_children(&nodes);
        state.focus = update.focus;
        if let Some(prev) = prev_focus_obj {
            if !objects_by_path.contains_key(prev.path_as_str()) {
                let known: HashSet<String> =
                    self.windows[index].children.keys().cloned().collect();
                if let Some(chain) =
                    mirror::read_chain_to_known(conn, &prev, &known, mirror::MAX_SPLICE_HOPS).await
                {
                    if let Some(splice) = self.apply_spliced_chain(index, &chain) {
                        merge_update(&mut update, splice);
                    }
                }
            }
        }
        Some(SourceEvent::TreeUpdate { window, update })
    }

    /// Reconciles the tracked window set against a fresh discovery: drops and
    /// announces vanished toplevels, walks and announces newly visible ones.
    /// A removed window is dropped from the focus tracker; the client nulls its
    /// own focus reference on `WindowRemoved`, so no `FocusChanged` is emitted.
    async fn reconcile(&mut self, conn: &AccessibilityConnection) -> Vec<SourceEvent> {
        let discovered = match mirror::discover_windows(conn, &mut self.app_ids).await {
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

/// The index of the tracked window that currently holds `path` in the client's
/// tree, for the app at `sender`. Resolution goes through `cache.nodes` — the
/// set of nodes the client actually has — so a path the walk never emitted, or
/// one a re-walk pruned, resolves to nothing and costs no bus read.
fn resolve_tracked_node(windows: &[WindowState], sender: &str, path: &str) -> Option<usize> {
    windows.iter().position(|w| {
        w.root.name().is_some_and(|n| n.as_str() == sender) && w.cache.nodes.contains_key(path)
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
        let mut children = HashMap::new();
        let mut cache = WindowCache::default();
        if walked {
            children.insert(root_path.to_owned(), vec![node_path.to_owned()]);
            children.insert(node_path.to_owned(), Vec::new());
            cache
                .nodes
                .insert(node_path.to_owned(), accesskit::Node::new(accesskit::Role::Button));
        }
        WindowState {
            id: WindowId(id),
            root,
            ids,
            objects,
            focus: node_id,
            cache,
            children,
        }
    }

    fn obj(sender: &'static str, path: &'static str) -> ObjectRefOwned {
        ObjectRef::new_owned(
            UniqueName::from_static_str_unchecked(sender),
            ObjectPath::from_static_str_unchecked(path),
        )
    }

    fn mirror_node(path: &str, role: atspi::Role, name: &str) -> crate::mapping::MirrorNode {
        crate::mapping::MirrorNode {
            path: path.to_owned(),
            role,
            name: name.to_owned(),
            ..Default::default()
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

    fn mirror_with(windows: Vec<WindowState>) -> Mirror {
        Mirror {
            windows,
            next_id: 100,
            focus: FocusTracker::new(None),
            app_ids: AppIdResolver::default(),
        }
    }

    #[test]
    fn active_descendant_emits_a_focus_only_delta_and_window_focus() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/item", true);
        let node = win.ids.get("/win/1/item").unwrap();
        let mut mirror = mirror_with(vec![win]);

        let out = mirror
            .handle_active_descendant(":1.1", "/win/1/item")
            .expect("descendant resolves");

        assert_eq!(out.len(), 2, "a focus-only delta plus a window focus change");
        match &out[0] {
            SourceEvent::TreeUpdate { window, update } => {
                assert_eq!(*window, WindowId(1));
                assert!(update.nodes.is_empty(), "focus-only delta touches no nodes");
                assert_eq!(update.focus, node);
            }
            _ => panic!("expected a TreeUpdate"),
        }
        assert!(matches!(out[1], SourceEvent::FocusChanged(Some(WindowId(1)))));
        assert_eq!(mirror.windows[0].focus, node, "window focus advanced to the descendant");
    }

    #[test]
    fn active_descendant_absent_from_the_tree_escalates() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/item", true);
        let mut mirror = mirror_with(vec![win]);
        assert!(mirror.handle_active_descendant(":1.1", "/win/1/gone").is_none());
    }

    #[test]
    fn apply_spliced_chain_updates_objects_children_and_focus() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/table", true);
        let mut mirror = mirror_with(vec![win]);
        let mut fresh_table = mirror_node("/win/1/table", atspi::Role::Table, "grid");
        fresh_table.children = vec!["/win/1/table/cell".to_owned()];
        let chain = vec![
            (fresh_table, obj(":1.1", "/win/1/table")),
            (
                mirror_node("/win/1/table/cell", atspi::Role::TableCell, "A1"),
                obj(":1.1", "/win/1/table/cell"),
            ),
        ];

        let update = mirror.apply_spliced_chain(0, &chain).expect("splice applies");

        let state = &mirror.windows[0];
        let cell = state.ids.get("/win/1/table/cell").expect("cell id allocated");
        assert_eq!(update.focus, cell);
        assert!(update.tree.is_none());
        assert!(state.objects.contains_key(&cell), "action routing reaches the cell");
        assert_eq!(state.children["/win/1/table"], vec!["/win/1/table/cell".to_owned()]);
        assert_eq!(state.focus, cell);
    }

    #[test]
    fn apply_spliced_chain_twice_is_idempotent() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/table", true);
        let mut mirror = mirror_with(vec![win]);
        let chain = || {
            let mut fresh_table = mirror_node("/win/1/table", atspi::Role::Table, "grid");
            fresh_table.children = vec!["/win/1/table/cell".to_owned()];
            vec![
                (fresh_table, obj(":1.1", "/win/1/table")),
                (
                    mirror_node("/win/1/table/cell", atspi::Role::TableCell, "A1"),
                    obj(":1.1", "/win/1/table/cell"),
                ),
            ]
        };
        let first = mirror.apply_spliced_chain(0, &chain()).expect("splice applies");
        let second = mirror.apply_spliced_chain(0, &chain()).expect("re-splice applies");
        assert_eq!(first.focus, second.focus);
        assert_eq!(
            mirror.windows[0].children["/win/1/table"],
            vec!["/win/1/table/cell".to_owned()],
            "no duplicate child entries"
        );
    }

    #[test]
    fn apply_spliced_chain_without_an_anchored_ancestor_applies_nothing() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/table", true);
        let mut mirror = mirror_with(vec![win]);
        let chain = vec![
            (mirror_node("/elsewhere", atspi::Role::Table, ""), obj(":1.1", "/elsewhere")),
            (
                mirror_node("/elsewhere/cell", atspi::Role::TableCell, ""),
                obj(":1.1", "/elsewhere/cell"),
            ),
        ];
        assert!(mirror.apply_spliced_chain(0, &chain).is_none());
    }

    #[test]
    fn windows_of_sender_returns_all_and_only_that_senders_windows() {
        let mirror = mirror_with(vec![
            window_state(1, ":1.1", "/win/1", "/win/1/node", true),
            window_state(2, ":1.1", "/win/2", "/win/2/node", true),
            window_state(3, ":1.2", "/win/3", "/win/3/node", true),
        ]);
        assert_eq!(
            mirror.windows_of_sender(":1.1"),
            vec![WindowId(1), WindowId(2)]
        );
        assert_eq!(mirror.windows_of_sender(":1.9"), Vec::<WindowId>::new());
    }

    #[test]
    fn state_change_for_an_untracked_path_emits_nothing() {
        // Resolution failing is what makes the handler emit nothing without
        // costing a bus read.
        let windows = vec![
            window_state(1, ":1.1", "/win/1", "/win/1/check", true),
            window_state(2, ":1.2", "/win/2", "/win/2/check", true),
        ];
        assert_eq!(resolve_tracked_node(&windows, ":1.1", "/win/1/check"), Some(0));
        assert_eq!(resolve_tracked_node(&windows, ":1.2", "/win/2/check"), Some(1));
        assert_eq!(
            resolve_tracked_node(&windows, ":1.1", "/win/1/other"),
            None,
            "a path the walk never emitted"
        );
        assert_eq!(
            resolve_tracked_node(&windows, ":1.9", "/win/1/check"),
            None,
            "an untracked app's sender"
        );
        assert_eq!(
            resolve_tracked_node(&windows, ":1.1", "/win/2/check"),
            None,
            "another window's node under this sender"
        );

        // A node pruned from the client's tree by a re-walk resolves to nothing.
        let pruned = vec![window_state(3, ":1.3", "/win/3", "/win/3/gone", false)];
        assert_eq!(resolve_tracked_node(&pruned, ":1.3", "/win/3/gone"), None);
    }

    #[test]
    fn active_descendant_in_the_focused_window_dedups_the_window_focus() {
        let win = window_state(1, ":1.1", "/win/1", "/win/1/item", true);
        let node = win.ids.get("/win/1/item").unwrap();
        let mut mirror = Mirror {
            windows: vec![win],
            next_id: 100,
            focus: FocusTracker::new(Some(WindowId(1))),
            app_ids: AppIdResolver::default(),
        };
        let out = mirror
            .handle_active_descendant(":1.1", "/win/1/item")
            .expect("descendant resolves");
        assert_eq!(out.len(), 1, "an already-focused window emits only the focus delta");
        assert!(matches!(&out[0], SourceEvent::TreeUpdate { .. }));
        assert_eq!(mirror.windows[0].focus, node);
    }
}
