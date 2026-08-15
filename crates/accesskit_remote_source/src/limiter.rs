//! How often one node may be re-read after a semantic change. Pure and
//! clock-free like [`crate::coalesce`]: callers supply `now`, so the policy is
//! unit tested without a live accessibility API.
//!
//! Structure and semantics are routed apart: a structural change debounces into
//! a subtree re-walk ([`crate::coalesce::RewalkCoalescer`]), while state,
//! property and selection changes rate-limit a single-node refresh here.

use accesskit_remote::WindowId;
use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// The nodes to re-read after a container's selection changed: the union of the
/// keys that were selected and those that are now, in `prev`-then-`now` order
/// with duplicates collapsed. Both ends need re-reading — one lost `selected`,
/// the other gained it.
pub fn selection_refresh_targets<K: Clone + PartialEq>(prev: &[K], now: &[K]) -> Vec<K> {
    let mut targets: Vec<K> = Vec::with_capacity(prev.len() + now.len());
    for key in prev.iter().chain(now) {
        if !targets.iter().any(|seen| seen == key) {
            targets.push(key.clone());
        }
    }
    targets
}

/// Rate-limits per-node refreshes, keyed by window and by whatever identifies a
/// node in the source's own accessibility API (an AT-SPI object path, a macOS
/// element key).
///
/// Leading edge plus trailing, unlike [`crate::coalesce::RewalkCoalescer`]'s
/// trailing-only debounce: the first change to a node emits at once, further
/// changes within `min_interval` are suppressed and collapse into one trailing
/// emit, deferred by each new change but never past `max_delay` after the first
/// suppressed one.
pub struct NodeRefreshLimiter<K> {
    entries: HashMap<(WindowId, K), Entry>,
    min_interval: Duration,
    max_delay: Duration,
}

struct Entry {
    fired: Option<Instant>,
    deadline: Option<Instant>,
    suppressed_since: Option<Instant>,
}

impl<K: Eq + Hash + Clone> NodeRefreshLimiter<K> {
    pub fn new(min_interval: Duration, max_delay: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            min_interval,
            max_delay,
        }
    }

    /// Records a change to `key` in `window` observed at `now`, returning
    /// whether to refresh it immediately. A change that arrives while the node
    /// is rate-limited schedules a trailing refresh instead: `min_interval`
    /// after this change, but no later than `max_delay` after the change that
    /// began the burst.
    ///
    /// Takes the key by borrow so a source keyed on `String` can pass a `&str`
    /// without allocating at the call site; the entry itself is owned either
    /// way.
    pub fn note<Q>(&mut self, window: WindowId, key: &Q, now: Instant) -> bool
    where
        Q: ?Sized + ToOwned<Owned = K>,
    {
        let min_interval = self.min_interval;
        let max_delay = self.max_delay;
        let entry = self.entries.entry((window, key.to_owned())).or_insert(Entry {
            fired: None,
            deadline: None,
            suppressed_since: None,
        });
        let quiet = entry.fired.is_none_or(|fired| now >= fired + min_interval);
        if entry.deadline.is_none() && quiet {
            entry.fired = Some(now);
            entry.suppressed_since = None;
            return true;
        }
        let since = *entry.suppressed_since.get_or_insert(now);
        entry.deadline = Some((now + min_interval).min(since + max_delay));
        false
    }

    /// The earliest trailing deadline across all rate-limited nodes, or `None`
    /// when none is scheduled.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.values().filter_map(|entry| entry.deadline).min()
    }

    /// Returns every node whose trailing deadline is at or before `now`,
    /// clearing that deadline and restarting the node's interval. Nodes idle
    /// longer than `min_interval` are forgotten, which a later change re-notes
    /// as a fresh leading edge.
    pub fn take_due(&mut self, now: Instant) -> Vec<(WindowId, K)> {
        let mut due = Vec::new();
        let min_interval = self.min_interval;
        self.entries.retain(|(window, key), entry| {
            if entry.deadline.is_some_and(|deadline| deadline <= now) {
                due.push((*window, key.clone()));
                entry.deadline = None;
                entry.suppressed_since = None;
                entry.fired = Some(now);
            }
            entry.deadline.is_some() || entry.fired.is_some_and(|fired| fired + min_interval > now)
        });
        due
    }

    /// Drops every entry belonging to `window`.
    pub fn discard(&mut self, window: WindowId) {
        self.entries.retain(|(id, _), _| *id != window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_millis(100);
    const CAP: Duration = Duration::from_millis(500);

    const W1: WindowId = WindowId(1);
    const W2: WindowId = WindowId(2);

    fn limiter() -> NodeRefreshLimiter<String> {
        NodeRefreshLimiter::new(MIN, CAP)
    }

    #[test]
    fn selection_targets_union_old_and_new_selection() {
        let prev = vec!["/a".to_owned(), "/b".to_owned()];
        let now = vec!["/b".to_owned(), "/c".to_owned()];
        assert_eq!(
            selection_refresh_targets(&prev, &now),
            vec!["/a".to_owned(), "/b".to_owned(), "/c".to_owned()],
            "both ends of the move are refreshed, each once",
        );
        assert_eq!(selection_refresh_targets(&[], &now), now);
        assert_eq!(selection_refresh_targets(&prev, &[]), prev);
        assert!(selection_refresh_targets::<String>(&[], &[]).is_empty());
        assert_eq!(
            selection_refresh_targets(&["/a".to_owned(), "/a".to_owned()], &["/a".to_owned()]),
            vec!["/a".to_owned()],
        );
    }

    /// The limiter is keyed on whatever the source uses for node identity, not
    /// on `String` — a source keying on an opaque element handle gets the same
    /// policy.
    #[test]
    fn any_hashable_key_works() {
        let base = Instant::now();
        let mut limiter: NodeRefreshLimiter<(u32, u64)> = NodeRefreshLimiter::new(MIN, CAP);
        assert!(limiter.note(W1, &(7, 42), base));
        assert!(!limiter.note(W1, &(7, 42), base + Duration::from_millis(5)));
        assert!(limiter.note(W1, &(7, 43), base), "a different key is independent");
        let deadline = limiter.next_deadline().expect("the suppressed change is scheduled");
        assert_eq!(
            limiter.take_due(deadline),
            vec![(W1, (7, 42))],
            "the suppressed change emits trailing, under its own key"
        );
    }

    #[test]
    fn limiter_fires_the_first_note_immediately() {
        let base = Instant::now();
        let mut limiter = limiter();

        assert!(limiter.note(W1, "/a", base), "a node's first change emits at once");
        assert_eq!(limiter.next_deadline(), None, "nothing trailing after a leading emit");
        assert!(limiter.note(W1, "/b", base), "a sibling is rate-limited independently");
        assert!(limiter.note(W2, "/a", base), "the same path in another window is its own key");
        assert!(
            limiter.note(W1, "/a", base + MIN),
            "a change after the interval elapsed emits at once again"
        );
    }

    /// A `String`-keyed source reaches `note` both ways: with a borrowed `&str`
    /// straight off an event, and with a `&String` out of a collection it is
    /// filtering. Both must resolve to the same entry without a turbofish.
    #[test]
    fn a_str_and_a_string_reference_name_the_same_entry() {
        let base = Instant::now();
        let mut limiter = limiter();

        assert!(limiter.note(W1, "/a", base), "&str: leading edge");

        let owned = vec!["/a".to_owned()];
        let suppressed: Vec<&String> = owned
            .iter()
            .filter(|key| !limiter.note(W1, *key, base + Duration::from_millis(5)))
            .collect();
        assert_eq!(
            suppressed.len(),
            1,
            "&String hit the entry the &str created, so it was rate-limited"
        );
    }

    #[test]
    fn limiter_rate_limits_a_storm_to_one_trailing_emit() {
        let base = Instant::now();
        let mut limiter = limiter();
        assert!(limiter.note(W1, "/a", base));

        let mut now = base;
        for _ in 0..10 {
            now += Duration::from_millis(5);
            assert!(!limiter.note(W1, "/a", now), "a change inside the interval is suppressed");
        }
        let deadline = limiter.next_deadline().expect("a trailing refresh is scheduled");
        assert!(deadline >= base + MIN, "the trailing emit respects the minimum interval");
        assert!(deadline <= base + Duration::from_millis(5) + CAP, "and the cap");

        assert_eq!(
            limiter.take_due(deadline),
            vec![(W1, "/a".to_owned())],
            "ten suppressed changes collapse into one emit"
        );
        assert_eq!(limiter.next_deadline(), None, "and only one");
    }

    #[test]
    fn limiter_never_defers_a_steady_stream_past_the_cap() {
        let base = Instant::now();
        let mut limiter = limiter();
        assert!(limiter.note(W1, "/a", base));

        let mut now = base;
        let first_suppressed = base + Duration::from_millis(20);
        while now < base + Duration::from_secs(2) {
            now += Duration::from_millis(20);
            limiter.note(W1, "/a", now);
            assert!(
                limiter.next_deadline().unwrap() <= first_suppressed + CAP,
                "a progress bar changing forever still emits every cap"
            );
        }
        assert_eq!(limiter.take_due(first_suppressed + CAP), vec![(W1, "/a".to_owned())]);
    }

    #[test]
    fn take_due_leaves_undue_nodes_and_discard_drops_a_window() {
        let base = Instant::now();
        let mut limiter = limiter();
        assert!(limiter.note(W1, "/a", base));
        assert!(limiter.note(W2, "/b", base + Duration::from_millis(50)));
        assert!(!limiter.note(W1, "/a", base + Duration::from_millis(10)));
        assert!(!limiter.note(W2, "/b", base + Duration::from_millis(60)));

        let first = limiter.next_deadline().expect("w1 is due first");
        assert_eq!(limiter.take_due(first), vec![(W1, "/a".to_owned())]);
        assert!(limiter.next_deadline().is_some(), "w2 stays scheduled");

        limiter.discard(W2);
        assert_eq!(limiter.next_deadline(), None, "discarding a window drops its nodes");
        assert!(limiter.take_due(base + Duration::from_secs(1)).is_empty());
    }
}
