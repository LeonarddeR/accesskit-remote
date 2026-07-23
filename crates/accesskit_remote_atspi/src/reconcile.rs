//! Pure, bus-free reconciliation of the tracked toplevel window set against a
//! fresh discovery snapshot. Operates on plain [`WindowKey`]s so it can be
//! unit tested without a live accessibility bus.

use std::collections::HashSet;

/// Stable identity of a toplevel window: its owning application's unique bus
/// name plus the frame's object path. AT-SPI object paths repeat across
/// applications, so both parts are needed to tell two apps' windows apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowKey {
    pub bus_name: String,
    pub path: String,
}

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
pub fn reconcile_windows(tracked: &[WindowKey], discovered: &[WindowKey]) -> WindowDiff {
    let tracked_set: HashSet<&WindowKey> = tracked.iter().collect();
    let discovered_set: HashSet<&WindowKey> = discovered.iter().collect();
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

    fn key(bus_name: &str, path: &str) -> WindowKey {
        WindowKey {
            bus_name: bus_name.to_owned(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn identical_sets_produce_no_changes() {
        let tracked = [key(":1.10", "/window/0"), key(":1.11", "/window/0")];
        let discovered = tracked.clone();
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, Vec::<usize>::new());
        assert_eq!(diff.removed, Vec::<usize>::new());
    }

    #[test]
    fn a_new_window_is_reported_as_added() {
        let tracked = [key(":1.10", "/window/0")];
        let discovered = [key(":1.10", "/window/0"), key(":1.10", "/window/1")];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, vec![1], "index into discovered of the new window");
        assert_eq!(diff.removed, Vec::<usize>::new());
    }

    #[test]
    fn a_vanished_window_is_reported_as_removed() {
        let tracked = [key(":1.10", "/window/0"), key(":1.10", "/window/1")];
        let discovered = [key(":1.10", "/window/0")];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, Vec::<usize>::new());
        assert_eq!(diff.removed, vec![1], "index into tracked of the gone window");
    }

    #[test]
    fn same_path_under_a_different_app_is_a_distinct_window() {
        let tracked = [key(":1.10", "/window/0")];
        let discovered = [key(":1.20", "/window/0")];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, vec![0], "the other app's window is new");
        assert_eq!(diff.removed, vec![0], "the original app's window is gone");
    }

    #[test]
    fn simultaneous_add_and_remove_are_both_reported() {
        let tracked = [key(":1.10", "/window/0"), key(":1.10", "/window/1")];
        let discovered = [key(":1.10", "/window/1"), key(":1.10", "/window/2")];
        let diff = reconcile_windows(&tracked, &discovered);
        assert_eq!(diff.added, vec![1], "/window/2 is new");
        assert_eq!(diff.removed, vec![0], "/window/0 is gone; /window/1 persists");
    }
}
