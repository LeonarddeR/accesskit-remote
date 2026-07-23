//! Window-level focus tracking for the mirror.
//!
//! Pure, bus-free bookkeeping of which toplevel holds session focus. It
//! deduplicates and orders the window-level `FocusChanged` the mirror emits so
//! that redundant activations produce no wire traffic and a removed window
//! never produces a `FocusChanged(Some(removed))` — the daemon closes the
//! connection if asked to focus an unannounced window, and the client already
//! nulls its own focus when a focused window is removed.

use accesskit_remote::WindowId;

/// Tracks the currently focused toplevel and decides when a window-level
/// `FocusChanged` should be emitted.
#[derive(Debug, Default)]
pub struct FocusTracker {
    current: Option<WindowId>,
}

impl FocusTracker {
    /// Starts tracking with a known initial focus (as reported by the initial
    /// enumeration), so the first activation of the already-focused window is
    /// deduplicated.
    pub fn new(initial: Option<WindowId>) -> Self {
        Self { current: initial }
    }

    /// Records that `window` gained focus. Returns `Some(new_focus)` when the
    /// focus actually changed (the caller should emit it), or `None` when
    /// `window` already held focus.
    pub fn focus(&mut self, window: WindowId) -> Option<Option<WindowId>> {
        if self.current == Some(window) {
            return None;
        }
        self.current = Some(window);
        Some(self.current)
    }

    /// Records that `window` was deactivated. Clears focus and returns
    /// `Some(None)` only if `window` currently held it; a deactivate for any
    /// other window is ignored (tolerating an activate-before-deactivate
    /// ordering across a window switch).
    pub fn deactivate(&mut self, window: WindowId) -> Option<Option<WindowId>> {
        if self.current != Some(window) {
            return None;
        }
        self.current = None;
        Some(self.current)
    }

    /// Forgets `window` because it was removed. Never emits: clearing focus for
    /// a vanished window is the client's job, and emitting `Some(removed)` would
    /// reference an unannounced window.
    pub fn remove(&mut self, window: WindowId) {
        if self.current == Some(window) {
            self.current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W1: WindowId = WindowId(1);
    const W2: WindowId = WindowId(2);

    #[test]
    fn focus_emits_once_then_dedupes() {
        let mut tracker = FocusTracker::default();
        assert_eq!(tracker.focus(W1), Some(Some(W1)));
        assert_eq!(tracker.focus(W1), None, "re-focusing the same window is a no-op");
    }

    #[test]
    fn switching_windows_emits_the_new_focus() {
        let mut tracker = FocusTracker::new(Some(W1));
        assert_eq!(tracker.focus(W1), None, "initial focus is already tracked");
        assert_eq!(tracker.focus(W2), Some(Some(W2)));
    }

    #[test]
    fn deactivate_clears_only_the_current_window() {
        let mut tracker = FocusTracker::new(Some(W1));
        assert_eq!(tracker.deactivate(W2), None, "deactivating a non-current window is ignored");
        assert_eq!(tracker.deactivate(W1), Some(None));
        // Cleared: re-focusing W1 emits again.
        assert_eq!(tracker.focus(W1), Some(Some(W1)));
    }

    #[test]
    fn remove_of_current_clears_silently_and_next_focus_emits() {
        let mut tracker = FocusTracker::new(Some(W1));
        tracker.remove(W1);
        assert_eq!(tracker.focus(W2), Some(Some(W2)), "focus after a removal still emits");
    }

    #[test]
    fn remove_of_non_current_leaves_focus_intact() {
        let mut tracker = FocusTracker::new(Some(W1));
        tracker.remove(W2);
        // Intact: re-focusing the still-current W1 is a no-op.
        assert_eq!(tracker.focus(W1), None);
    }
}
