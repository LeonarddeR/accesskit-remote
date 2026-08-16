//! [`AxSource`]: a synchronous [`TreeSource`] backed by a dedicated AX thread.
//!
//! Same shape as `AtspiSource`, for a different reason. There, a thread was
//! needed because the bus layer was async; here it is needed because
//! `AXUIElement` references are not `Send` and because `AXObserver` delivers
//! notifications only to a `CFRunLoop`. Either way the sync side talks to the
//! worker over channels carrying only owned, sendable data — `TreeUpdate`s out,
//! action requests in — and the constructor blocks until the first snapshot is
//! ready.
//!
//! The worker's loop alternates between draining commands, running the thread's
//! run loop — which is where observer callbacks fire — and draining whatever
//! those callbacks queued. Observers are registered per application on the same
//! thread, because a source added to any other run loop delivers nothing.

use crate::delta::EmittedTree;
use crate::element::{ElementKey, NodeIdMap};
use crate::names::Names;
use crate::observe::{self, AppObserver, Queue, Route};
use crate::{ax, node, opt_in, trust, walk};
use accesskit_remote::WindowId;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use accesskit_remote_source::coalesce::RewalkCoalescer;
use accesskit_remote_source::focus::FocusTracker;
use accesskit_remote_source::limiter::NodeRefreshLimiter;
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the worker gives the run loop before draining commands again.
///
/// Bounded rather than infinite so an action never waits on an accessibility
/// notification to wake the loop; the daemon polls at 50ms, so matching it
/// keeps the two in step.
const RUN_LOOP_SLICE: Duration = Duration::from_millis(50);

/// Quiet period after the last invalidation of a burst before the window is
/// re-walked, and the cap past which a window that keeps changing is walked
/// anyway. Inherited from the AT-SPI source, and the measured bursts justify
/// them: 33 `AXValueChanged` at a single timestamp, 49 `AXUIElementDestroyed`
/// from one app activation.
const REWALK_QUIET: Duration = Duration::from_millis(250);
const REWALK_MAX_DELAY: Duration = Duration::from_secs(2);

/// Minimum spacing between refreshes of the same node, and the cap past which
/// a node changing continuously is refreshed anyway. Leading edge plus
/// trailing, so the first change of a burst reaches the consumer at once —
/// which is what makes typing feel live — while the rest collapse into one.
const REFRESH_MIN_INTERVAL: Duration = Duration::from_millis(100);
const REFRESH_MAX_DELAY: Duration = Duration::from_millis(500);

/// How often the window set is re-discovered and the focused window re-walked.
///
/// Two jobs. It catches window lifecycle an application did not announce — the
/// safety net the AT-SPI source uses it for. And it is the only thing that
/// corrects geometry after a scroll: scrolling a WebKit page produces no
/// notification at all, so without this every element's bounds stay wrong from
/// the first scroll onwards.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);

/// How long a window is re-walked after an action is performed on it.
///
/// Covers the gap between an AX write being accepted and the application
/// actually updating, for controls that report nothing when driven.
///
/// Measured through a UIA client toggling an `NSSegmentedControl` segment:
/// with a 1500ms window, three of four activations reported their new state in
/// 0.9-1.7s and the fourth took 3.24s — it had missed the window and fallen
/// back to the reconcile tick. 3500ms covers that tail, which matters because
/// the failure is a screen reader announcing the *old* state after the user
/// pressed something.
///
/// The cost is bounded and only paid after an action: re-walks are still
/// debounced at 250ms, so this is a handful of extra walks of one window.
const SETTLE_AFTER_ACTION: Duration = Duration::from_millis(3500);

type Snapshot = (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>);

/// Work sent from the sync side to the AX thread.
enum Command {
    Perform {
        window: WindowId,
        request: accesskit::ActionRequest,
    },
    Shutdown,
}

/// A [`TreeSource`] that mirrors the live macOS accessibility tree.
pub struct AxSource {
    events: mpsc::Receiver<SourceEvent>,
    commands: mpsc::Sender<Command>,
    initial: Snapshot,
    _thread: JoinHandle<()>,
}

impl AxSource {
    /// Starts the AX thread and blocks until it has enumerated the desktop.
    ///
    /// Fails fast when the process lacks the Accessibility grant. Without it AX
    /// does not report an error — it reports an empty desktop — so a source
    /// that started anyway would look like a working mirror of a machine with
    /// no windows on it.
    pub fn new() -> Result<Self, String> {
        if !trust::is_trusted() {
            return Err(trust::untrusted_message());
        }
        let (events_tx, events_rx) = mpsc::channel();
        let (commands_tx, commands_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel();

        let thread = std::thread::Builder::new()
            .name("accesskit-ax".into())
            .spawn(move || worker(events_tx, commands_rx, init_tx))
            .map_err(|e| format!("spawning the AX thread: {e}"))?;

        let initial = init_rx
            .recv()
            .map_err(|_| "the AX thread exited before its first enumeration".to_owned())?;

        Ok(Self {
            events: events_rx,
            commands: commands_tx,
            initial,
            _thread: thread,
        })
    }
}

impl Drop for AxSource {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

impl TreeSource for AxSource {
    fn initial_state(&mut self) -> Snapshot {
        self.initial.clone()
    }

    fn perform(&mut self, window: WindowId, request: &accesskit::ActionRequest) {
        let _ = self.commands.send(Command::Perform {
            window,
            request: request.clone(),
        });
    }

    fn poll_events(&mut self) -> Vec<SourceEvent> {
        self.events.try_iter().collect()
    }
}

/// What the worker tracks per exported window.
///
/// `ids` is captured at the first walk and carried forward: it is append-only,
/// so an element that survives a re-walk keeps its node id and the consumer
/// receives a delta rather than a replacement. A map rebuilt per walk would
/// throw away exactly the property the 100% element-identity measurement was
/// taken to establish.
struct WindowState {
    id: WindowId,
    key: ElementKey,
    /// The owning application, so a notification can be traced to its window.
    pid: i32,
    /// Append-only across re-walks, so an element that survives keeps its node
    /// id and the consumer receives a delta rather than a replacement.
    ids: NodeIdMap,
    /// What the consumer already holds, so a re-walk is reduced to its
    /// difference instead of re-sending the whole window.
    emitted: EmittedTree,
    /// The window's screen origin at its last walk, so a refreshed node's
    /// bounds land in the same coordinate space as its siblings'.
    origin: Option<(f64, f64)>,
    /// The children each node was *emitted* with. A refresh reads them from
    /// here rather than from the element, because a refresh may change a
    /// node's semantics but never the tree's shape — a fresh child list from a
    /// lazily-populated table would otherwise splice thousands of unwalked
    /// nodes into the consumer's tree.
    children: HashMap<ElementKey, Vec<ElementKey>>,
    /// Keep re-walking this window until this moment, because an action was
    /// just performed on it.
    ///
    /// A single post-action walk is not enough. An AX write returns as soon as
    /// the application accepts it, not once the application has updated, so a
    /// walk 250ms later frequently reads the *old* state and then nothing
    /// prompts another look. Measured through UIA: a checkbox toggled by a
    /// consumer took 3.1-4.2s to report its new state, which is the reconcile
    /// tick — the post-action walk had already come and gone too early.
    settle_until: Option<Instant>,
}

/// The AX thread's body: enumerate once, hand the snapshot back, then serve.
fn worker(
    events: mpsc::Sender<SourceEvent>,
    commands: mpsc::Receiver<Command>,
    init: mpsc::Sender<Snapshot>,
) {
    let names = Names::new();
    let mut windows = Vec::new();
    let mut next_id = 1u64;
    let mut focus = FocusTracker::default();
    let queue: Queue = Rc::new(RefCell::new(Vec::new()));
    let mut observers: Vec<AppObserver> = Vec::new();
    let mut pending = RewalkCoalescer::new(REWALK_QUIET, REWALK_MAX_DELAY);
    let mut refresh: NodeRefreshLimiter<ElementKey> =
        NodeRefreshLimiter::new(REFRESH_MIN_INTERVAL, REFRESH_MAX_DELAY);
    let mut next_reconcile = Instant::now() + RECONCILE_INTERVAL;

    let snapshot =
        enumerate(&names, &mut windows, &mut next_id, &mut focus, &queue, &mut observers);
    if init.send(snapshot).is_err() {
        return;
    }

    loop {
        match commands.try_recv() {
            Ok(Command::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => break,
            Ok(Command::Perform { window, request }) => {
                perform(&names, &mut windows, &mut pending, window, &request);
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        // Services the observers' run-loop sources; their callbacks append to
        // `queue` from inside this call.
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            RUN_LOOP_SLICE.as_secs_f64(),
            false,
        );

        let now = Instant::now();
        if !route_notifications(
            &events, &queue, &names, &mut windows, &mut pending, &mut refresh, &mut focus, now,
        ) {
            break;
        }
        if !drain_due(&events, &names, &mut windows, &mut pending, &mut refresh, now) {
            break;
        }
        if now >= next_reconcile {
            next_reconcile = now + RECONCILE_INTERVAL;
            if !reconcile(
                &events, &names, &mut windows, &mut next_id, &mut focus, &queue, &mut observers,
                &mut pending, now,
            ) {
                break;
            }
        }
    }
    // Observers hold sources on this thread's run loop and must be dropped
    // here, before the thread and its loop go away.
    drop(observers);
}

/// Carries out one action request against the element its node id names.
///
/// Always marks the window for a re-walk afterwards, whether or not a route
/// succeeded. Some actions produce no notification at all — opening a menu is
/// the usual example — so the debounced walk is the only thing that surfaces
/// what the action did. The AT-SPI source does the same for the same reason.
fn perform(
    names: &Names,
    windows: &mut [WindowState],
    pending: &mut RewalkCoalescer,
    window: WindowId,
    request: &accesskit::ActionRequest,
) {
    let Some(state) = windows.iter().find(|state| state.id == window) else {
        tracing::debug!(window = window.0, "action for a window we no longer track");
        return;
    };
    let Some(key) = state.ids.key_for(request.target_node) else {
        tracing::debug!(
            window = window.0,
            node = request.target_node.0,
            "action for a node this window never emitted"
        );
        return;
    };
    // The role decides which routes are even plausible, and it must be the one
    // the consumer is looking at rather than a fresh guess.
    let role = node::read(key.clone(), names)
        .map(|node| node.accesskit_role())
        .unwrap_or(accesskit::Role::Unknown);
    ax::perform(key, request, role, names);
    // Watch the window for a short while rather than looking once. Some
    // controls emit no notification at all when driven — an `NSSegmentedControl`
    // segment is one — so this is the only thing that will notice the change.
    let now = Instant::now();
    pending.note(window, now);
    if let Some(state) = windows.iter_mut().find(|state| state.id == window) {
        state.settle_until = Some(now + SETTLE_AFTER_ACTION);
    }
}

/// Turns queued notifications into invalidations, and services the ones that
/// can be answered by re-reading a single node.
///
/// Returns false when the consumer has gone away.
#[allow(clippy::too_many_arguments)]
fn route_notifications(
    events: &mpsc::Sender<SourceEvent>,
    queue: &Queue,
    names: &Names,
    windows: &mut [WindowState],
    pending: &mut RewalkCoalescer,
    refresh: &mut NodeRefreshLimiter<ElementKey>,
    focus: &mut FocusTracker,
    now: Instant,
) -> bool {
    let drained: Vec<observe::Notification> = queue.borrow_mut().drain(..).collect();
    for notification in drained {
        let route = observe::route(&notification.notification);
        if route == Route::Ignore {
            continue;
        }
        let key = ElementKey::new(notification.pid, notification.element);

        match route {
            // A semantic or caret change touches one node. Re-reading that node
            // costs one round trip; re-walking its window costs the whole
            // window, which for a Catalyst application is ~7ms per node.
            Route::Refresh | Route::Text => {
                let Some(index) = window_holding(windows, &key) else {
                    // Not a node the consumer holds — it may be newly created,
                    // which is a structural change, so let the walk find it.
                    note_windows_of(windows, notification.pid, pending, now);
                    continue;
                };
                if !refresh.note(windows[index].id, &key, now) {
                    // Rate-limited: a trailing refresh is already scheduled.
                    continue;
                }
                if !emit_refresh(events, names, &mut windows[index], &key) {
                    return false;
                }
            }
            // Focus moved. Which *window* now holds it is what the wire
            // carries; focus within a window rides its next tree update. This
            // costs no reads: the focused element came with the notification.
            Route::Focus => {
                let window = window_holding(windows, &key)
                    .or_else(|| windows.iter().position(|w| w.pid == notification.pid))
                    .map(|index| windows[index].id);
                if let Some(id) = window {
                    // A window whose element moved focus is also the one whose
                    // geometry a reader is about to need, so refresh it.
                    pending.note(id, now);
                }
                if let Some(change) = window.and_then(|id| focus.focus(id)) {
                    if events.send(SourceEvent::FocusChanged(change)).is_err() {
                        return false;
                    }
                }
            }
            // Structure changed, or an element vanished: the shape of the
            // window is no longer known, so only a walk can re-establish it.
            _ => note_windows_of(windows, notification.pid, pending, now),
        }
    }
    true
}

/// The window whose tree currently contains `key`, if any.
fn window_holding(windows: &[WindowState], key: &ElementKey) -> Option<usize> {
    windows.iter().position(|window| window.ids.get(key).is_some())
}

/// Marks every window of an application for a debounced re-walk.
///
/// Deliberately coarse: a notification names an element, and by the time a
/// destruction arrives that element cannot be climbed to find its window. The
/// debounce collapses the excess.
fn note_windows_of(
    windows: &[WindowState],
    pid: i32,
    pending: &mut RewalkCoalescer,
    now: Instant,
) {
    for window in windows.iter().filter(|window| window.pid == pid) {
        pending.note(window.id, now);
    }
}

/// Re-reads one node and sends it, if anything about it actually changed.
///
/// **Semantics only, never structure.** The node's children come from what was
/// last emitted, not from a fresh read: a refresh must not be able to change
/// the tree's shape, or a lazily-populated table could splice thousands of
/// never-walked nodes into the consumer's tree by way of a value change.
fn emit_refresh(
    events: &mpsc::Sender<SourceEvent>,
    names: &Names,
    window: &mut WindowState,
    key: &ElementKey,
) -> bool {
    let Some(id) = window.ids.get(key) else {
        return true;
    };
    let Ok(read) = node::read(key.clone(), names) else {
        // The element died between the notification and the read. Its removal
        // is structural, and the walk will notice.
        return true;
    };
    let mut built = node::build_container(&read, window.origin);
    if let Some(children) = window.children.get(key) {
        let ids: Vec<accesskit::NodeId> = children
            .iter()
            .filter_map(|child| window.ids.get(child))
            // Only children the consumer actually holds. A node id can outlive
            // the node: `ids` is append-only by design, so an element removed
            // by a later walk still resolves here, and naming it in a delta
            // panics the consumer.
            .filter(|id| window.emitted.holds(*id))
            .collect();
        if !ids.is_empty() {
            built.set_children(ids);
        }
    }
    if !window.emitted.reduce_node(id, &built) {
        return true;
    }
    let update = accesskit::TreeUpdate {
        nodes: vec![(id, built)],
        tree: None,
        tree_id: accesskit::TreeId::ROOT,
        // Focus is unchanged by a semantic refresh, so it must be restated as
        // the consumer already believes it — a stale value here would move the
        // reader's cursor on every value change.
        focus: window.emitted.focus().unwrap_or(id),
    };
    events
        .send(SourceEvent::TreeUpdate {
            window: window.id,
            update,
        })
        .is_ok()
}

/// Re-walks every window whose debounce has expired, emitting an update for
/// each. Returns false when the consumer has gone away.
fn drain_due(
    events: &mpsc::Sender<SourceEvent>,
    names: &Names,
    windows: &mut [WindowState],
    pending: &mut RewalkCoalescer,
    refresh: &mut NodeRefreshLimiter<ElementKey>,
    now: Instant,
) -> bool {
    // Trailing single-node refreshes first: they are cheap, and doing them
    // before any re-walk means a value that settled during a burst reaches the
    // consumer without waiting on a whole window.
    for (window_id, key) in refresh.take_due(now) {
        let Some(index) = windows.iter().position(|window| window.id == window_id) else {
            continue;
        };
        if !emit_refresh(events, names, &mut windows[index], &key) {
            return false;
        }
    }
    for id in pending.take_due(now) {
        let Some(window) = windows.iter_mut().find(|w| w.id == id) else {
            continue;
        };
        let nodes = walk::walk_window(window.key.clone(), names);
        // Refresh reads structure from here, so it must track the walk.
        window.children = walk::emitted_children(&nodes);
        window.origin = nodes
            .first()
            .and_then(|root| root.frame)
            .map(|frame| (frame.origin.x, frame.origin.y));
        // An empty re-walk means the window is gone or unreadable. Removing it
        // is the reconcile path's job, which has the whole window set in view;
        // dropping the tree here would tell the consumer the window went blank.
        let Some(full) = walk::build_window_update(&nodes, &mut window.ids) else {
            continue;
        };
        // Most re-walks find nothing changed — a flickering node inside one
        // application was otherwise putting its whole tree on the wire every
        // second — so the walk is reduced to its difference, and a walk that
        // changed nothing sends nothing at all.
        // Still settling after an action: look again shortly, whether or not
        // anything changed this time.
        if window.settle_until.is_some_and(|until| now < until) {
            pending.note(id, now);
        } else {
            window.settle_until = None;
        }
        let Some(update) = window.emitted.reduce(full) else {
            continue;
        };
        if events
            .send(SourceEvent::TreeUpdate { window: id, update })
            .is_err()
        {
            return false;
        }
    }
    true
}

/// Re-discovers the window set, announcing what appeared and retiring what is
/// gone, and marks the focused window for a walk.
///
/// The safety net, and on macOS also a necessity rather than a backstop:
/// scrolling produces no notification, so this tick is the only thing that ever
/// corrects a scrolled window's geometry.
#[allow(clippy::too_many_arguments)]
fn reconcile(
    events: &mpsc::Sender<SourceEvent>,
    names: &Names,
    windows: &mut Vec<WindowState>,
    next_id: &mut u64,
    focus: &mut FocusTracker,
    queue: &Queue,
    observers: &mut Vec<AppObserver>,
    pending: &mut RewalkCoalescer,
    now: Instant,
) -> bool {
    let apps = ax::running_apps();
    let mut discovered: Vec<(ax::Window, i32)> = Vec::new();
    let mut live_pids: HashSet<i32> = HashSet::new();
    for app in &apps {
        live_pids.insert(app.pid);
        // An application seen for the first time needs its opt-in and its
        // observer; one already known must not be asked twice.
        if !observers.iter().any(|observer| observer.pid() == app.pid) {
            opt_in::request(&app.element, names);
            if let Ok((observer, _)) = AppObserver::new(app.pid, &app.element, queue) {
                observers.push(observer);
            }
        }
        for window in ax::windows_of(app, names).unwrap_or_default() {
            discovered.push((window, app.pid));
        }
    }
    // An application that quit takes its observer with it, so the run loop
    // stops carrying a source for a process that no longer exists.
    observers.retain(|observer| live_pids.contains(&observer.pid()));

    tracing::debug!(
        apps = apps.len(),
        discovered = discovered.len(),
        tracked = windows.len(),
        "reconcile"
    );
    let present: HashSet<&ElementKey> = discovered.iter().map(|(w, _)| &w.key).collect();
    let gone: Vec<WindowId> = windows
        .iter()
        .filter(|window| !present.contains(&window.key))
        .map(|window| window.id)
        .collect();
    for id in &gone {
        // Never emitted as a focus change: the client nulls its own focus when
        // a window is removed, and naming a retired window would reference one
        // the consumer has already been told to forget.
        focus.remove(*id);
        pending.discard(*id);
        if events.send(SourceEvent::WindowRemoved(*id)).is_err() {
            return false;
        }
    }
    windows.retain(|window| !gone.contains(&window.id));

    let known: HashSet<ElementKey> = windows.iter().map(|window| window.key.clone()).collect();
    for (window, pid) in discovered {
        if known.contains(&window.key) {
            // Already tracked. A window the user is looking at is the one whose
            // geometry matters, so keep it fresh against silent scrolling.
            if window.active {
                if let Some(state) = windows.iter().find(|state| state.key == window.key) {
                    pending.note(state.id, now);
                }
            }
            continue;
        }
        tracing::debug!(title = %window.title, pid, "reconcile found a new window");
        let nodes = walk::walk_window(window.key.clone(), names);
        let mut ids = NodeIdMap::new();
        let mut emitted = EmittedTree::new();
        let Some(update) = walk::build_window_update(&nodes, &mut ids)
            .and_then(|full| emitted.reduce(full))
        else {
            tracing::debug!(title = %window.title, "new window walked empty; retrying next tick");
            // Walks empty: almost always a window whose tree is not ready yet,
            // measured as a delay of a second or more after it appears. Leaving
            // it unannounced means the next tick picks it up, rather than the
            // consumer holding a permanently blank window.
            continue;
        };
        let id = WindowId(*next_id);
        *next_id += 1;
        let descriptor = WindowDescriptor {
            id,
            title: window.title,
            app: window.app,
            native_window_id: window.native_window_id,
        };
        windows.push(WindowState {
            id,
            key: window.key,
            pid,
            ids,
            emitted,
            origin: nodes
                .first()
                .and_then(|root| root.frame)
                .map(|frame| (frame.origin.x, frame.origin.y)),
            children: walk::emitted_children(&nodes),
            settle_until: None,
        });
        if events
            .send(SourceEvent::WindowAdded {
                descriptor,
                tree: update,
            })
            .is_err()
        {
            return false;
        }
        if window.active {
            if let Some(change) = focus.focus(id) {
                if events.send(SourceEvent::FocusChanged(change)).is_err() {
                    return false;
                }
            }
        }
    }
    true
}

/// Walks every window on the desktop into a full snapshot.
fn enumerate(
    names: &Names,
    windows: &mut Vec<WindowState>,
    next_id: &mut u64,
    focus: &mut FocusTracker,
    queue: &Queue,
    observers: &mut Vec<AppObserver>,
) -> Snapshot {
    let mut out = Vec::new();
    let mut focused = None;
    let mut asked: HashSet<i32> = HashSet::new();

    for app in ax::running_apps() {
        // Chromium-based applications publish nothing until asked, and the ask
        // is per application, not per window.
        if asked.insert(app.pid) {
            opt_in::request(&app.element, names);
            // One observer per application, registered on the application
            // element so its whole subtree is covered. Applications that
            // refuse simply produce no live updates; the periodic reconcile
            // is the backstop.
            match AppObserver::new(app.pid, &app.element, queue) {
                Ok((observer, declined)) => {
                    if !declined.is_empty() {
                        tracing::debug!(
                            app = %app.info.name,
                            declined = declined.len(),
                            "application refused some notifications"
                        );
                    }
                    observers.push(observer);
                }
                Err(error) => {
                    tracing::debug!(app = %app.info.name, %error, "no observer for application");
                }
            }
        }
        for window in ax::windows_of(&app, names).unwrap_or_default() {
            let nodes = walk::walk_window(window.key.clone(), names);
            let mut ids = NodeIdMap::new();
            let mut emitted = EmittedTree::new();
            let Some(update) = walk::build_window_update(&nodes, &mut ids)
                .and_then(|full| emitted.reduce(full))
            else {
                // A window that walks empty is not announced. It is usually a
                // freshly mapped one whose tree is not ready — measured as a
                // real delay of a second or more after launch — so leaving it
                // out means the next reconcile picks it up rather than the
                // consumer holding a broken window forever.
                continue;
            };
            let id = WindowId(*next_id);
            *next_id += 1;
            if window.active {
                focused = Some(id);
            }
            out.push((
                WindowDescriptor {
                    id,
                    title: window.title,
                    app: window.app,
                    native_window_id: window.native_window_id,
                },
                update,
            ));
            windows.push(WindowState {
                id,
                key: window.key,
                pid: app.pid,
                ids,
                emitted,
                origin: nodes
                    .first()
                    .and_then(|root| root.frame)
                    .map(|frame| (frame.origin.x, frame.origin.y)),
                children: walk::emitted_children(&nodes),
                settle_until: None,
            });
        }
    }

    // Something must hold focus or the consumer has nowhere to start.
    if focused.is_none() {
        focused = windows.first().map(|window| window.id);
    }
    *focus = FocusTracker::new(focused);
    (out, focused)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction must be total: on a machine with no grant it returns an
    /// actionable error rather than an empty mirror, and on a granted one it
    /// enumerates. CI has no grant, a developer machine does, and both are
    /// valid outcomes — what must never happen is a hang or a panic.
    #[test]
    fn construction_either_enumerates_or_explains_itself() {
        match AxSource::new() {
            Ok(mut source) => {
                let (windows, focus) = source.initial_state();
                if let Some(id) = focus {
                    assert!(
                        windows.iter().any(|(descriptor, _)| descriptor.id == id),
                        "the focused window must be one that was announced, or the daemon \
                         closes the connection"
                    );
                }
                for (descriptor, update) in &windows {
                    assert!(!update.nodes.is_empty(), "an announced window has a tree");
                    assert!(update.tree.is_some(), "and a root");
                    assert!(descriptor.id.0 > 0);
                }
                assert!(source.poll_events().is_empty(), "no events before observers exist");
            }
            Err(message) => {
                assert!(message.contains("Accessibility"), "{message}");
            }
        }
    }

    #[test]
    fn window_ids_are_unique_across_the_desktop() {
        let Ok(mut source) = AxSource::new() else {
            return; // No grant: covered by the test above.
        };
        let (windows, _) = source.initial_state();
        let mut ids: Vec<u64> = windows.iter().map(|(d, _)| d.id.0).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "a WindowId is never reused within a session");
    }
}
