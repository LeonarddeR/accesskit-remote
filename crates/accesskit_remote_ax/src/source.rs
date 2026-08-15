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

    let snapshot =
        enumerate(&names, &mut windows, &mut next_id, &mut focus, &queue, &mut observers);
    if init.send(snapshot).is_err() {
        return;
    }

    loop {
        match commands.try_recv() {
            Ok(Command::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => break,
            Ok(Command::Perform { window, request }) => {
                // Drive-back is a later phase; log at the seam so the pump's
                // own action log has a counterpart on this side.
                tracing::info!(
                    window = window.0,
                    action = ?request.action,
                    node = request.target_node.0,
                    "action requested (drive-back not implemented yet)"
                );
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
            &events, &queue, &names, &mut windows, &mut pending, &mut refresh, now,
        ) {
            break;
        }
        if !drain_due(&events, &names, &mut windows, &mut pending, &mut refresh, now) {
            break;
        }
    }
    // Observers hold sources on this thread's run loop and must be dropped
    // here, before the thread and its loop go away.
    drop(observers);
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
        let ids: Vec<accesskit::NodeId> =
            children.iter().filter_map(|child| window.ids.get(child)).collect();
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
