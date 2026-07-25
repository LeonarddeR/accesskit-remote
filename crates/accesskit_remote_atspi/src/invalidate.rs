//! Which live AT-SPI signals warrant re-reading one node, and how often. Pure
//! and clock-free like [`crate::coalesce`]: callers supply `now`, so the policy
//! is unit tested without tokio or a live bus.
//!
//! Structure and semantics are routed apart: `children-changed` debounces into
//! a subtree re-walk ([`crate::coalesce::RewalkCoalescer`]), while the state,
//! property, and selection signals here rate-limit a single-node refresh.

use accesskit_remote::WindowId;
use atspi::State;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Whether a state change alters what [`crate::mapping::node_states`] distills,
/// and so warrants re-reading the node. Everything else is discarded in O(1).
pub fn state_is_mirrored(state: State) -> bool {
    matches!(
        state,
        State::Focusable
            | State::Focused
            | State::Expandable
            | State::Expanded
            | State::Collapsed
            | State::Selectable
            | State::Selected
            | State::Checkable
            | State::Checked
            | State::Pressed
            | State::Indeterminate
            | State::HasPopup
            | State::Sensitive
            | State::Enabled
            | State::ReadOnly
            | State::Required
            | State::InvalidEntry
            | State::Modal
            | State::Multiselectable
            | State::Busy
            | State::Horizontal
            | State::Vertical
    )
}

/// Whether an `object:property-change` names a property the mapping mirrors.
/// Routed off the signal's property *string*, which is what AT-SPI puts on the
/// wire; `atspi`'s `Property` enum deserializes `accessible-value` (and
/// anything else it lacks a variant for) to `Property::Other`.
pub fn property_is_mirrored(property: &str) -> bool {
    matches!(
        property,
        "accessible-name" | "accessible-description" | "accessible-role" | "accessible-value"
    )
}

/// The nodes to re-read after a container's selection changed: the union of the
/// paths that were selected and those that are now, in `prev`-then-`now` order
/// with duplicates collapsed. Both ends need re-reading — one lost `selected`,
/// the other gained it.
pub fn selection_refresh_targets(prev: &[String], now: &[String]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::with_capacity(prev.len() + now.len());
    for path in prev.iter().chain(now) {
        if !targets.iter().any(|seen| seen == path) {
            targets.push(path.clone());
        }
    }
    targets
}

/// Rate-limits per-node refreshes, keyed by window and AT-SPI object path.
///
/// Leading edge plus trailing, unlike [`crate::coalesce::RewalkCoalescer`]'s
/// trailing-only debounce: the first change to a node emits at once, further
/// changes within `min_interval` are suppressed and collapse into one trailing
/// emit, deferred by each new change but never past `max_delay` after the first
/// suppressed one.
pub struct NodeRefreshLimiter {
    entries: HashMap<(WindowId, String), Entry>,
    min_interval: Duration,
    max_delay: Duration,
}

struct Entry {
    fired: Option<Instant>,
    deadline: Option<Instant>,
    suppressed_since: Option<Instant>,
}

impl NodeRefreshLimiter {
    pub fn new(min_interval: Duration, max_delay: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            min_interval,
            max_delay,
        }
    }

    /// Records a change to `path` in `window` observed at `now`, returning
    /// whether to refresh it immediately. A change that arrives while the node
    /// is rate-limited schedules a trailing refresh instead: `min_interval`
    /// after this change, but no later than `max_delay` after the change that
    /// began the burst.
    pub fn note(&mut self, window: WindowId, path: &str, now: Instant) -> bool {
        let min_interval = self.min_interval;
        let max_delay = self.max_delay;
        let entry = self.entries.entry((window, path.to_owned())).or_insert(Entry {
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
    pub fn take_due(&mut self, now: Instant) -> Vec<(WindowId, String)> {
        let mut due = Vec::new();
        let min_interval = self.min_interval;
        self.entries.retain(|(window, path), entry| {
            if entry.deadline.is_some_and(|deadline| deadline <= now) {
                due.push((*window, path.clone()));
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
    use crate::mapping::node_states;
    use atspi::StateSet;

    const MIN: Duration = Duration::from_millis(100);
    const CAP: Duration = Duration::from_millis(500);

    const W1: WindowId = WindowId(1);
    const W2: WindowId = WindowId(2);

    fn limiter() -> NodeRefreshLimiter {
        NodeRefreshLimiter::new(MIN, CAP)
    }

    /// Every [`State`] the bitflag representation defines, enumerated by bit.
    fn all_states() -> Vec<State> {
        (0..64)
            .filter_map(|bit| StateSet::from_bits(1u64 << bit).ok())
            .filter_map(|set| set.iter().next())
            .collect()
    }

    #[test]
    fn mirrored_states_are_exactly_the_forwarded_ones() {
        let baseline = node_states(StateSet::empty());
        let states = all_states();
        assert!(states.len() > 40, "the whole state surface is under test");
        for state in states {
            let distilled = node_states(StateSet::new(state)) != baseline;
            assert_eq!(
                state_is_mirrored(state),
                distilled,
                "{state:?}: mirrored={}, but node_states {} it",
                state_is_mirrored(state),
                if distilled { "distills" } else { "ignores" },
            );
        }
    }

    #[test]
    fn mirrored_properties_are_the_ones_the_mapping_reads() {
        for property in [
            "accessible-name",
            "accessible-description",
            "accessible-role",
            "accessible-value",
        ] {
            assert!(property_is_mirrored(property), "{property} reaches accesskit");
        }
        for property in ["accessible-parent", "accessible-table-caption", "", "name"] {
            assert!(!property_is_mirrored(property), "{property:?} is not mirrored");
        }
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
        assert!(selection_refresh_targets(&[], &[]).is_empty());
        assert_eq!(
            selection_refresh_targets(&["/a".to_owned(), "/a".to_owned()], &["/a".to_owned()]),
            vec!["/a".to_owned()],
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
