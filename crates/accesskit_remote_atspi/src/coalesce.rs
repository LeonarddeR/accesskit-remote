//! Trailing per-window debounce for AT-SPI subtree re-walks. A burst of
//! invalidations for the same window collapses into one walk, fired `quiet`
//! after the last invalidation but never later than `max_delay` after the
//! first — so a window that keeps changing still gets walked eventually.
//! Pure and clock-free: callers supply `now`, so it is unit tested without
//! tokio or a live bus.

use accesskit_remote::WindowId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Coalesces repeated re-walk requests for the same window into a single
/// scheduled walk per window.
pub struct RewalkCoalescer {
    pending: HashMap<WindowId, Pending>,
    quiet: Duration,
    max_delay: Duration,
}

struct Pending {
    deadline: Instant,
    latest: Instant,
}

impl RewalkCoalescer {
    /// Creates a coalescer that fires `quiet` after the last note in a burst,
    /// capped at `max_delay` after the burst's first note.
    pub fn new(quiet: Duration, max_delay: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            quiet,
            max_delay,
        }
    }

    /// Records an invalidation for `window` observed at `now`. Starting a new
    /// burst schedules a walk at `now + quiet`, capped at `now + max_delay`;
    /// a note within an existing burst pushes the deadline out by `quiet`
    /// without moving past that burst's cap.
    pub fn note(&mut self, window: WindowId, now: Instant) {
        let entry = self.pending.entry(window).or_insert(Pending {
            deadline: now + self.quiet,
            latest: now + self.max_delay,
        });
        entry.deadline = (now + self.quiet).min(entry.latest);
    }

    /// The earliest deadline across all pending windows, or `None` if
    /// nothing is pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|pending| pending.deadline).min()
    }

    /// Removes and returns every window whose deadline is at or before
    /// `now`. Windows not yet due remain pending.
    pub fn take_due(&mut self, now: Instant) -> Vec<WindowId> {
        let mut due = Vec::new();
        self.pending.retain(|window, pending| {
            if pending.deadline <= now {
                due.push(*window);
                false
            } else {
                true
            }
        });
        due
    }

    /// Drops any pending re-walk for `window`. A no-op if none is pending.
    pub fn discard(&mut self, window: WindowId) {
        self.pending.remove(&window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_millis(250);
    const MAX: Duration = Duration::from_secs(2);

    const W1: WindowId = WindowId(1);
    const W2: WindowId = WindowId(2);

    fn coalescer() -> RewalkCoalescer {
        RewalkCoalescer::new(QUIET, MAX)
    }

    #[test]
    fn single_note_fires_after_quiet() {
        let base = Instant::now();
        let mut c = coalescer();
        c.note(W1, base);
        assert_eq!(c.next_deadline(), Some(base + QUIET));

        let not_yet = c.take_due(base + QUIET - Duration::from_millis(1));
        assert_eq!(not_yet, Vec::<WindowId>::new(), "not due yet");
        assert_eq!(c.next_deadline(), Some(base + QUIET), "still pending");

        let due = c.take_due(base + QUIET);
        assert_eq!(due, vec![W1]);
        assert_eq!(c.next_deadline(), None, "nothing left pending");
    }

    #[test]
    fn renote_within_quiet_extends_deadline() {
        let base = Instant::now();
        let mut c = coalescer();
        c.note(W1, base);
        c.note(W1, base + Duration::from_millis(100));

        assert_eq!(
            c.take_due(base + QUIET),
            Vec::<WindowId>::new(),
            "the renote pushed the deadline past the original quiet window"
        );
        assert_eq!(
            c.take_due(base + Duration::from_millis(100) + QUIET),
            vec![W1]
        );
    }

    #[test]
    fn steady_stream_capped_at_max_delay() {
        let base = Instant::now();
        let mut c = coalescer();

        let mut t = base;
        c.note(W1, t);
        assert!(c.next_deadline().unwrap() <= base + MAX);
        while t + Duration::from_millis(200) < base + MAX {
            t += Duration::from_millis(200);
            c.note(W1, t);
            assert!(
                c.next_deadline().unwrap() <= base + MAX,
                "a steady stream must never defer the deadline past the cap"
            );
        }

        // The last note above lands within `quiet` of the cap, so an
        // uncapped debounce would not be due yet; the cap makes it due.
        let due = c.take_due(base + MAX);
        assert_eq!(due, vec![W1]);
    }

    #[test]
    fn note_after_take_due_starts_a_fresh_burst() {
        let base = Instant::now();
        let mut c = coalescer();
        c.note(W1, base);
        assert_eq!(c.take_due(base + QUIET), vec![W1]);

        let t0 = base + Duration::from_secs(10);
        c.note(W1, t0);
        assert_eq!(
            c.next_deadline(),
            Some(t0 + QUIET),
            "a fresh burst debounces from its own first note"
        );

        // The cap resets with the burst too: a steady stream now caps at
        // t0 + max_delay, not the original base + max_delay.
        let mut t = t0;
        while t + Duration::from_millis(200) < t0 + MAX {
            t += Duration::from_millis(200);
            c.note(W1, t);
            assert!(c.next_deadline().unwrap() <= t0 + MAX);
        }
        assert_eq!(c.take_due(t0 + MAX), vec![W1]);
    }

    #[test]
    fn two_windows_independent_deadlines_and_min_next_deadline() {
        let base = Instant::now();
        let mut c = coalescer();
        c.note(W1, base);
        c.note(W2, base + Duration::from_millis(50));

        assert_eq!(c.next_deadline(), Some(base + QUIET), "w1's earlier deadline wins");

        let due = c.take_due(base + QUIET);
        assert_eq!(due, vec![W1], "only w1 is due");
        assert_eq!(
            c.next_deadline(),
            Some(base + Duration::from_millis(50) + QUIET),
            "w2 is still pending on its own deadline"
        );
    }

    #[test]
    fn take_due_leaves_undue_windows_pending() {
        let base = Instant::now();
        let mut c = coalescer();
        c.note(W1, base);
        c.note(W2, base + Duration::from_millis(50));

        assert_eq!(c.take_due(base + QUIET), vec![W1]);
        assert_eq!(
            c.next_deadline(),
            Some(base + Duration::from_millis(50) + QUIET),
            "w2 survives the drain that removed w1"
        );
    }

    #[test]
    fn discard_removes_a_window() {
        let base = Instant::now();
        let mut c = coalescer();
        c.note(W1, base);
        c.note(W2, base + Duration::from_millis(50));

        c.discard(W1);
        assert_eq!(
            c.next_deadline(),
            Some(base + Duration::from_millis(50) + QUIET),
            "only w2 remains pending"
        );
        assert_eq!(
            c.take_due(base + QUIET),
            Vec::<WindowId>::new(),
            "w1 was discarded before its old deadline"
        );

        c.discard(W1); // already gone: no-op
        c.discard(WindowId(999)); // never seen: no-op
    }
}
