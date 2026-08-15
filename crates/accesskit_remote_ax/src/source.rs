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
use crate::{ax, opt_in, trust, walk};
use accesskit_remote::WindowId;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use accesskit_remote_source::coalesce::RewalkCoalescer;
use accesskit_remote_source::focus::FocusTracker;
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop};
use std::cell::RefCell;
use std::collections::HashSet;
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
        route_notifications(&queue, &windows, &mut pending, now);
        if !drain_due(&events, &names, &mut windows, &mut pending, now) {
            break;
        }
    }
    // Observers hold sources on this thread's run loop and must be dropped
    // here, before the thread and its loop go away.
    drop(observers);
}

/// Turns queued notifications into invalidations.
///
/// Every route currently converges on the debounced re-walk. That is the honest
/// first cut: a single-node refresh needs a per-node cache of what was last
/// emitted, without which "re-read one node" cannot answer "did anything
/// change?" and would emit on every keystroke. The routes are kept distinct
/// here so adding that cache changes this function and nothing above it.
fn route_notifications(
    queue: &Queue,
    windows: &[WindowState],
    pending: &mut RewalkCoalescer,
    now: Instant,
) {
    let drained: Vec<observe::Notification> = queue.borrow_mut().drain(..).collect();
    for notification in drained {
        let route = observe::route(&notification.notification);
        if route == Route::Ignore {
            continue;
        }
        // A notification names an element, not a window. Attributing it to
        // every window of the owning application is deliberately coarse: the
        // element may already be destroyed, so climbing to its window is not
        // always possible, and the debounce collapses the excess anyway.
        for window in windows.iter().filter(|w| w.pid == notification.pid) {
            pending.note(window.id, now);
        }
    }
}

/// Re-walks every window whose debounce has expired, emitting an update for
/// each. Returns false when the consumer has gone away.
fn drain_due(
    events: &mpsc::Sender<SourceEvent>,
    names: &Names,
    windows: &mut [WindowState],
    pending: &mut RewalkCoalescer,
    now: Instant,
) -> bool {
    for id in pending.take_due(now) {
        let Some(window) = windows.iter_mut().find(|w| w.id == id) else {
            continue;
        };
        let nodes = walk::walk_window(window.key.clone(), names);
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
