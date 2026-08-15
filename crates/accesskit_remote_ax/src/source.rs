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
//! The worker's loop already alternates between draining commands and running
//! the thread's run loop, even though nothing yet installs a run-loop source.
//! That is deliberate: it is the seam `AXObserver` plugs into, and building it
//! now means the observer phase adds sources rather than restructuring the
//! loop.

use crate::element::NodeIdMap;
use crate::names::Names;
use crate::{ax, opt_in, trust, walk};
use accesskit_remote::WindowId;
use accesskit_remote_server::{SourceEvent, TreeSource, WindowDescriptor};
use accesskit_remote_source::focus::FocusTracker;
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop};
use std::collections::HashSet;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// How long the worker gives the run loop before draining commands again.
///
/// Bounded rather than infinite so an action never waits on an accessibility
/// notification to wake the loop; the daemon polls at 50ms, so matching it
/// keeps the two in step.
const RUN_LOOP_SLICE: Duration = Duration::from_millis(50);

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
/// `key` and `ids` are unread until the observer phase adds re-walks, and are
/// kept now rather than added later because they must be captured *at* the
/// first walk to be correct: `ids` is append-only, and a map rebuilt on the
/// first re-walk would hand every element a new node id and turn that re-walk
/// into a full tree replacement — losing exactly the property the 100%
/// element-identity measurement was taken to establish.
#[allow(dead_code)]
struct WindowState {
    id: WindowId,
    key: crate::element::ElementKey,
    /// Append-only across re-walks, so an element that survives keeps its node
    /// id and the consumer receives a delta rather than a replacement.
    ids: NodeIdMap,
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

    let snapshot = enumerate(&names, &mut windows, &mut next_id, &mut focus);
    if init.send(snapshot).is_err() {
        return;
    }

    loop {
        match commands.try_recv() {
            Ok(Command::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return,
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
        let _ = &events;
        // Runs whatever run-loop sources exist — none yet, so this returns at
        // once and the slice is spent waiting. `AXObserver` will add its source
        // here without changing the loop.
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            RUN_LOOP_SLICE.as_secs_f64(),
            false,
        );
    }
}

/// Walks every window on the desktop into a full snapshot.
fn enumerate(
    names: &Names,
    windows: &mut Vec<WindowState>,
    next_id: &mut u64,
    focus: &mut FocusTracker,
) -> Snapshot {
    let mut out = Vec::new();
    let mut focused = None;
    let mut asked: HashSet<i32> = HashSet::new();

    for app in ax::running_apps() {
        // Chromium-based applications publish nothing until asked, and the ask
        // is per application, not per window.
        if asked.insert(app.pid) {
            opt_in::request(&app.element, names);
        }
        for window in ax::windows_of(&app, names).unwrap_or_default() {
            let nodes = walk::walk_window(window.key.clone(), names);
            let mut ids = NodeIdMap::new();
            let Some(update) = walk::build_window_update(&nodes, &mut ids) else {
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
                ids,
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
