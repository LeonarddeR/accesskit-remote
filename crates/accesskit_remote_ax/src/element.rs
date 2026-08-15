//! Element identity.
//!
//! The AT-SPI source identifies a node by its D-Bus object path — a string the
//! toolkit hands out and keeps stable, which is what lets `NodeIdMap` reuse
//! AccessKit node ids across a re-walk and what makes an unchanged re-read
//! emit nothing.
//!
//! AX has no such thing. There is no id attribute, no path, nothing printable
//! that survives a round trip. What it does have is `CFEqual`/`CFHash` on the
//! element reference itself: AppKit mints `AXUIElementRef`s that compare equal
//! when they denote the same underlying object, even across separate copies of
//! the reference.
//!
//! **How well that holds is the load-bearing unknown for this whole crate**, and
//! it is deliberately measurable: [`crate::probe`] walks a window twice and
//! reports what fraction of the second walk's elements compare equal to the
//! first's. A low ratio means every re-walk is a full tree replacement and the
//! delta architecture buys nothing — at which point the fix is a positional key
//! (the index chain from the window root) behind this same opaque type, which
//! is why [`ElementKey`] exposes no structure to its users.

use objc2_application_services::AXUIElement;
use objc2_core_foundation::{CFRetained, CFType};
use std::hash::{Hash, Hasher};

/// A node's identity, and the handle used to read it again.
///
/// Opaque on purpose: nothing outside this module may depend on identity being
/// the element reference, so the positional fallback stays a drop-in.
///
/// The owning pid is part of the identity. `CFHash` values are only unique
/// within a process, and this source tracks every application on the desktop
/// at once, so two unrelated elements in different apps could otherwise
/// collide in one map.
#[derive(Clone)]
pub struct ElementKey {
    pid: i32,
    element: CFRetained<AXUIElement>,
}

impl ElementKey {
    pub fn new(pid: i32, element: CFRetained<AXUIElement>) -> Self {
        Self { pid, element }
    }

    /// The element to read, for the I/O layer.
    pub fn element(&self) -> &AXUIElement {
        &self.element
    }

    /// The owning process.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    fn as_type(&self) -> &CFType {
        &self.element
    }
}

impl PartialEq for ElementKey {
    fn eq(&self, other: &Self) -> bool {
        // pid first: it is an integer compare, and it short-circuits the
        // CFEqual call for the common cross-application case.
        self.pid == other.pid && self.as_type() == other.as_type()
    }
}

impl Eq for ElementKey {}

impl Hash for ElementKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pid.hash(state);
        self.as_type().hash(state);
    }
}

impl core::fmt::Debug for ElementKey {
    /// Prints the pid and a short digest rather than `CFCopyDescription`.
    ///
    /// An `AXUIElement`'s description is itself an IPC call into the target
    /// application — formatting a few thousand of them while diagnosing a slow
    /// walk would be its own performance problem, and would deadlock if the app
    /// being described is the one that is hung.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        write!(f, "{}:{:08x}", self.pid, hasher.finish() as u32)
    }
}

/// Assigns AccessKit node ids to elements, stably across re-walks.
///
/// Append-only and per window, exactly as the AT-SPI `NodeIdMap` is: an id is
/// never reused, so a stale id on the consumer can never come to mean a
/// different node. [`get`](Self::get) is the non-allocating lookup used where
/// an *unseen* element must stay unseen.
#[derive(Default)]
pub struct NodeIdMap {
    map: std::collections::HashMap<ElementKey, accesskit::NodeId>,
    /// The reverse direction, for action requests: those name a node id and
    /// the executor needs the element it stands for.
    by_id: std::collections::HashMap<accesskit::NodeId, ElementKey>,
    /// Ids for the synthetic `TextRun` children of a text element, keyed by
    /// their parent and index. They are drawn from the same counter as element
    /// ids so the two can never collide, and they are just as append-only: a
    /// run whose id changed on every walk would make each keystroke replace the
    /// paragraph instead of updating it.
    runs: std::collections::HashMap<(accesskit::NodeId, usize), accesskit::NodeId>,
    next: u64,
}

impl NodeIdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the id for `key`, allocating a fresh one on first sight.
    pub fn id_for(&mut self, key: &ElementKey) -> accesskit::NodeId {
        if let Some(id) = self.map.get(key) {
            return *id;
        }
        let id = accesskit::NodeId(self.next);
        self.next += 1;
        self.map.insert(key.clone(), id);
        self.by_id.insert(id, key.clone());
        id
    }

    /// Returns the id previously assigned to `key`, if any. Never allocates
    /// one — a relation pointing at an unwalked element must not drag it into
    /// the tree.
    pub fn get(&self, key: &ElementKey) -> Option<accesskit::NodeId> {
        self.map.get(key).copied()
    }

    /// The id of a text element's `index`-th run, allocating on first sight.
    pub fn run_id_for(&mut self, parent: accesskit::NodeId, index: usize) -> accesskit::NodeId {
        if let Some(id) = self.runs.get(&(parent, index)) {
            return *id;
        }
        let id = accesskit::NodeId(self.next);
        self.next += 1;
        self.runs.insert((parent, index), id);
        id
    }

    /// The element a node id stands for, for carrying out an action against it.
    pub fn key_for(&self, id: accesskit::NodeId) -> Option<&ElementKey> {
        self.by_id.get(&id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_application_services::AXUIElement;

    /// The system-wide element is the one AXUIElement obtainable with no
    /// Accessibility grant, no session and no target application, so it is
    /// what makes these tests runnable in CI.
    fn system_wide() -> CFRetained<AXUIElement> {
        // SAFETY: takes no arguments and always returns a valid element.
        unsafe { AXUIElement::new_system_wide() }
    }

    #[test]
    fn two_references_to_one_element_are_one_key() {
        let a = ElementKey::new(0, system_wide());
        let b = ElementKey::new(0, system_wide());
        assert_eq!(a, b, "separately created references to the same element compare equal");

        let mut map = NodeIdMap::new();
        let first = map.id_for(&a);
        assert_eq!(map.id_for(&b), first, "so they get the same node id");
        assert_eq!(map.len(), 1, "and only one entry");
    }

    #[test]
    fn the_same_element_under_a_different_pid_is_a_different_key() {
        // Guards the reason pid is in the key at all: `CFHash` is only unique
        // within a process, and this source tracks every app at once.
        let a = ElementKey::new(101, system_wide());
        let b = ElementKey::new(202, system_wide());
        assert_ne!(a, b);

        let mut map = NodeIdMap::new();
        assert_ne!(map.id_for(&a), map.id_for(&b));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn ids_are_stable_and_never_reused() {
        let mut map = NodeIdMap::new();
        let keys: Vec<ElementKey> =
            (0..4).map(|pid| ElementKey::new(pid, system_wide())).collect();
        let first: Vec<accesskit::NodeId> = keys.iter().map(|k| map.id_for(k)).collect();
        let again: Vec<accesskit::NodeId> = keys.iter().map(|k| map.id_for(k)).collect();
        assert_eq!(first, again, "a re-walk reuses ids for surviving elements");

        // A newly seen element takes a fresh id, never one previously handed
        // out — a stale id on the consumer must not come to mean another node.
        let fresh = map.id_for(&ElementKey::new(99, system_wide()));
        assert!(!first.contains(&fresh));
    }

    #[test]
    fn an_id_resolves_back_to_its_element() {
        // An action names a node id; without this the executor has nothing to
        // perform against.
        let mut map = NodeIdMap::new();
        let key = ElementKey::new(7, system_wide());
        let id = map.id_for(&key);
        assert_eq!(map.key_for(id), Some(&key));
        assert_eq!(map.key_for(accesskit::NodeId(9999)), None);
    }

    #[test]
    fn run_ids_are_stable_and_never_collide_with_element_ids() {
        let mut map = NodeIdMap::new();
        let parent = map.id_for(&ElementKey::new(1, system_wide()));
        let first = map.run_id_for(parent, 0);
        let second = map.run_id_for(parent, 1);
        assert_ne!(first, second);
        assert_ne!(first, parent, "runs share the element counter, so cannot collide");
        // Stable across walks, or every keystroke would replace the paragraph.
        assert_eq!(map.run_id_for(parent, 0), first);
        assert_eq!(map.run_id_for(parent, 1), second);
        // A run of a different parent is a different run.
        let other = map.id_for(&ElementKey::new(2, system_wide()));
        assert_ne!(map.run_id_for(other, 0), first);
    }

    #[test]
    fn get_does_not_allocate_an_id() {
        let mut map = NodeIdMap::new();
        let key = ElementKey::new(1, system_wide());
        assert_eq!(map.get(&key), None);
        assert!(map.is_empty(), "a miss must not create an entry");
        let id = map.id_for(&key);
        assert_eq!(map.get(&key), Some(id));
    }

    #[test]
    fn debug_is_cheap_and_does_not_describe_the_element() {
        // `CFCopyDescription` on an AXUIElement is an IPC call into the target
        // app; it must not appear in a log line.
        let text = format!("{:?}", ElementKey::new(42, system_wide()));
        assert!(text.starts_with("42:"), "{text}");
        assert!(!text.contains("AXUIElement"), "{text}");
    }
}
