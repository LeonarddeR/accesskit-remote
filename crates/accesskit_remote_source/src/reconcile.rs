//! Pure reconciliation of the tracked toplevel window set against a fresh
//! discovery snapshot.
//!
//! Generic over the key so each source supplies its own notion of window
//! identity — AT-SPI needs an application bus name plus an object path, while
//! a macOS source would key on a pid plus a `CGWindowID`. The diff itself only
//! ever compares keys for equality, so it is unit tested without any
//! accessibility API present.

use std::collections::HashSet;
use std::hash::Hash;

/// The result of diffing tracked windows against a fresh discovery: indices
/// into `discovered` that are new, and indices into `tracked` that are gone.
/// Windows present in both are absent from both lists.
#[derive(Debug, PartialEq, Eq)]
pub struct WindowDiff {
    pub added: Vec<usize>,
    pub removed: Vec<usize>,
}

/// Diffs the freshly `discovered` window keys against the currently `tracked`
/// ones by identity.
pub fn reconcile_windows<K: Eq + Hash>(tracked: &[K], discovered: &[K]) -> WindowDiff {
    let tracked_set: HashSet<&K> = tracked.iter().collect();
    let discovered_set: HashSet<&K> = discovered.iter().collect();
    let added = discovered
        .iter()
        .enumerate()
        .filter(|(_, key)| !tracked_set.contains(key))
        .map(|(index, _)| index)
        .collect();
    let removed = tracked
        .iter()
        .enumerate()
        .filter(|(_, key)| !discovered_set.contains(key))
        .map(|(index, _)| index)
        .collect();
    WindowDiff { added, removed }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-part key, standing in for the compound identities real sources
    /// use: the second component alone must not decide equality.
    fn key(app: u32, window: u32) -> (u32, u32) {
        (app, window)
    }

    #[test]
    fn identical_sets_produce_no_changes() {
        let tracked = [key(10, 0), key(11, 0)];
        let discovered = tracked;
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, Vec::<usize>::new());
        assert_eq!(diff.removed, Vec::<usize>::new());
    }

    #[test]
    fn a_new_window_is_reported_as_added() {
        let tracked = [key(10, 0)];
        let discovered = [key(10, 0), key(10, 1)];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, vec![1], "index into discovered of the new window");
        assert_eq!(diff.removed, Vec::<usize>::new());
    }

    #[test]
    fn a_vanished_window_is_reported_as_removed() {
        let tracked = [key(10, 0), key(10, 1)];
        let discovered = [key(10, 0)];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, Vec::<usize>::new());
        assert_eq!(diff.removed, vec![1], "index into tracked of the gone window");
    }

    #[test]
    fn the_whole_key_decides_identity() {
        let tracked = [key(10, 0)];
        let discovered = [key(20, 0)];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, vec![0], "the other app's window is new");
        assert_eq!(diff.removed, vec![0], "the original app's window is gone");
    }

    #[test]
    fn simultaneous_add_and_remove_are_both_reported() {
        let tracked = [key(10, 0), key(10, 1)];
        let discovered = [key(10, 1), key(10, 2)];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, vec![1], "window 2 is new");
        assert_eq!(diff.removed, vec![0], "window 0 is gone; window 1 persists");
    }

    #[test]
    fn empty_sets_are_handled_at_both_ends() {
        let none: [(u32, u32); 0] = [];
        let some = [key(10, 0), key(10, 1)];
        assert_eq!(reconcile_windows(&none, &some).added, vec![0, 1]);
        assert_eq!(reconcile_windows(&none, &some).removed, Vec::<usize>::new());
        assert_eq!(reconcile_windows(&some, &none).removed, vec![0, 1]);
        assert_eq!(reconcile_windows(&some, &none).added, Vec::<usize>::new());
        assert_eq!(reconcile_windows(&none, &none).added, Vec::<usize>::new());
    }
}
