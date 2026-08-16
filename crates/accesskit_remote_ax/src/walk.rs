//! Walking one window into an `accesskit::TreeUpdate`.
//!
//! Breadth-first from the window element, bounded in node count, tolerant of
//! elements that vanish mid-walk — about 4% of them do on a real desktop,
//! having been enqueued from a parent's child list and destroyed before they
//! could be read. That is ordinary, not an error, and must not produce a log
//! line per element.

use crate::element::{ElementKey, NodeIdMap};
use crate::names::Names;
use crate::node::{self, AxNode};
use std::collections::{HashMap, HashSet, VecDeque};

/// The most nodes one window contributes.
///
/// Matches the AT-SPI source's cap. It is a backstop against a pathological
/// application, not a budget anyone should reach: the largest window measured
/// here was 143 nodes.
pub const MAX_NODES_PER_WINDOW: usize = 5000;

/// The role of an application-level element, which must never appear inside a
/// window's subtree.
const APPLICATION_ROLE: &str = "AXApplication";

/// Walks a window breadth-first.
///
/// The returned vector is in discovery order, so the window root is first.
/// Elements that cannot be read are skipped silently; their children are not
/// reached, which is correct — an element that will not answer has no
/// retrievable subtree either.
pub fn walk_window(root: ElementKey, names: &Names) -> Vec<AxNode> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([root]);
    let mut seen: HashSet<ElementKey> = HashSet::new();
    while let Some(key) = queue.pop_front() {
        if out.len() >= MAX_NODES_PER_WINDOW {
            tracing::debug!(cap = MAX_NODES_PER_WINDOW, "window walk hit the node cap");
            break;
        }
        // Cycles are possible: an element may list a child that lists it back.
        if !seen.insert(key.clone()) {
            continue;
        }
        let Ok(node) = node::read(key, names) else {
            continue;
        };
        // A window's child list can name the *application* element (observed
        // on TextEdit and System Settings, 2026-08-15). That is a cycle back
        // to the root of everything, and `Role::Application` nested inside a
        // `Role::Window` is meaningless to a consumer. Drop it and do not
        // descend: whatever hangs off the application belongs to other windows.
        if !out.is_empty() && node.role == APPLICATION_ROLE {
            continue;
        }
        queue.extend(node.children.iter().cloned());
        out.push(node);
    }
    out
}

/// Assembles a walk into a full `TreeUpdate`.
///
/// Ids are allocated for every walked element *before* any node is built, so
/// that a node's child list can reference a sibling discovered later in the
/// same walk. Ids come from `ids`, which is append-only across walks, so an
/// element that survives a re-walk keeps its id and the consumer sees a delta
/// rather than a replacement.
pub fn build_window_update(nodes: &[AxNode], ids: &mut NodeIdMap) -> Option<accesskit::TreeUpdate> {
    let root = nodes.first()?;
    let root_id = ids.id_for(&root.key);
    for node in nodes {
        ids.id_for(&node.key);
    }

    // The window's own screen origin: every descendant's frame is expressed
    // relative to it, so bounds arrive in the space AccessKit expects.
    let origin = root.frame.map(|frame| (frame.origin.x, frame.origin.y));

    // Only elements the walk actually reached may appear as children. A child
    // list can name an element the walk skipped or never got to, and emitting
    // an id for a node that is not in the update would dangle on the consumer.
    let walked: HashSet<&ElementKey> = nodes.iter().map(|node| &node.key).collect();

    // AccessKit requires a strict tree: every node has at most one parent and
    // appears in exactly one child list, once. AX does not guarantee that.
    // macOS exposes a table cell under both `AXRows` and `AXColumns`, so a
    // walk that trusts `AXChildren` emits the same cell under several parents —
    // and `accesskit_consumer` does not tolerate it, it panics the consumer
    // outright with "TreeUpdate includes duplicate child". Measured on a
    // Wikipedia table: 12 cells with a Row parent and one or more
    // GenericContainer parents, one of which listed the same child twice.
    //
    // First parent in walk order keeps the child. That is the row, since rows
    // are reached before columns, which is also the hierarchy a reader wants.
    let mut claimed: HashSet<ElementKey> = HashSet::new();

    let mut focus = root_id;
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        let id = ids.id_for(&node.key);
        let mut built = node::build_container(node, origin);
        let mut children: Vec<accesskit::NodeId> = node
            .children
            .iter()
            .filter(|child| walked.contains(child))
            // `insert` returns false for a child another parent already took,
            // and also for one this same parent listed twice.
            .filter(|child| claimed.insert((*child).clone()))
            .filter_map(|child| ids.get(child))
            .collect();
        // A text element's content becomes TextRun children, which is the only
        // shape a consumer can resolve a range against. They are appended
        // rather than replacing the element children, since a text view can
        // legitimately contain both.
        if node::has_text_runs(node.accesskit_role()) {
            if let Some(value) = node.value.as_deref().and_then(crate::attr::as_string) {
                let (runs, layout) =
                    crate::text::build_runs(&value, |index| ids.run_id_for(id, index));
                if let Some((start, len)) = node.selected_range {
                    if let Some(selection) = crate::text::selection(&value, &layout, start, len) {
                        built.set_text_selection(selection);
                    }
                }
                children.extend(runs.iter().map(|(run_id, _)| *run_id));
                out.extend(runs);
            }
        }
        if !children.is_empty() {
            built.set_children(children);
        }
        if node.states.focused {
            focus = id;
        }
        out.push((id, built));
    }

    Some(accesskit::TreeUpdate {
        nodes: out,
        tree: Some(accesskit::Tree::new(root_id)),
        tree_id: accesskit::TreeId::ROOT,
        focus,
    })
}

/// The child element list each walked node was emitted with, keyed by element.
///
/// The refresh path consults this instead of a fresh read, so that changing a
/// node's semantics can never change the tree's structure.
pub fn emitted_children(nodes: &[AxNode]) -> HashMap<ElementKey, Vec<ElementKey>> {
    let walked: HashSet<&ElementKey> = nodes.iter().map(|node| &node.key).collect();
    // Must apply the same single-parent rule as `build_window_update`, in the
    // same order. If it did not, a refresh would hand a node back the children
    // the walk had deliberately taken from it, reintroducing the duplicate.
    let mut claimed: HashSet<ElementKey> = HashSet::new();
    nodes
        .iter()
        .map(|node| {
            let children: Vec<ElementKey> = node
                .children
                .iter()
                .filter(|child| walked.contains(child))
                .filter(|child| claimed.insert((*child).clone()))
                .cloned()
                .collect();
            (node.key.clone(), children)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeStates;
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    /// Distinct keys, using the pid as the discriminator so each is genuinely
    /// unequal without needing live elements.
    fn key(n: i32) -> ElementKey {
        // SAFETY: takes no arguments and always returns a valid element.
        ElementKey::new(n, unsafe { AXUIElement::new_system_wide() })
    }

    fn node(n: i32, role: &str, children: Vec<ElementKey>) -> AxNode {
        AxNode {
            key: key(n),
            role: role.to_owned(),
            subrole: None,
            title: String::new(),
            description: String::new(),
            value: None,
            frame: None,
            states: NodeStates::default(),
            children,
            selected_range: None,
            actions: Vec::new(),
        }
    }

    fn window(n: i32, children: Vec<ElementKey>) -> AxNode {
        let mut node = node(n, "AXWindow", children);
        node.frame = Some(CGRect::new(CGPoint::new(100.0, 200.0), CGSize::new(500.0, 400.0)));
        node
    }

    #[test]
    fn the_first_walked_node_is_the_root() {
        let nodes = [window(1, vec![key(2)]), node(2, "AXButton", vec![])];
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes, &mut ids).expect("a non-empty walk builds");
        let root = update.tree.as_ref().unwrap().root;
        assert_eq!(root, ids.get(&key(1)).unwrap());
        assert_eq!(update.nodes.len(), 2);
    }

    #[test]
    fn an_empty_walk_builds_nothing() {
        assert!(build_window_update(&[], &mut NodeIdMap::new()).is_none());
    }

    #[test]
    fn ids_survive_a_re_walk_so_the_consumer_sees_a_delta() {
        // The pay-off of stable element identity: the same element keeps its
        // node id, so a re-walk is an update rather than a replacement.
        let nodes = [window(1, vec![key(2)]), node(2, "AXButton", vec![])];
        let mut ids = NodeIdMap::new();
        let first = build_window_update(&nodes, &mut ids).unwrap();
        let second = build_window_update(&nodes, &mut ids).unwrap();
        let first_ids: Vec<_> = first.nodes.iter().map(|(id, _)| *id).collect();
        let second_ids: Vec<_> = second.nodes.iter().map(|(id, _)| *id).collect();
        assert_eq!(first_ids, second_ids);
        assert_eq!(ids.len(), 2, "a re-walk allocates no new ids");
    }

    #[test]
    fn a_child_the_walk_never_reached_is_not_emitted() {
        // A parent can name a child the walk skipped (unreadable, or past the
        // node cap). Emitting its id would dangle on the consumer.
        let nodes = [window(1, vec![key(2), key(99)]), node(2, "AXButton", vec![])];
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes, &mut ids).unwrap();
        let root_id = ids.get(&key(1)).unwrap();
        let (_, root) = update.nodes.iter().find(|(id, _)| *id == root_id).unwrap();
        assert_eq!(root.children().len(), 1, "only the reached child is emitted");
        assert!(ids.get(&key(99)).is_none(), "and the unreached one got no id");
    }

    #[test]
    fn focus_lands_on_the_focused_node_and_defaults_to_the_root() {
        let mut nodes = [window(1, vec![key(2)]), node(2, "AXButton", vec![])];
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes, &mut ids).unwrap();
        assert_eq!(update.focus, ids.get(&key(1)).unwrap(), "no focus falls back to the root");

        nodes[1].states.focused = true;
        let update = build_window_update(&nodes, &mut ids).unwrap();
        assert_eq!(update.focus, ids.get(&key(2)).unwrap());
    }

    #[test]
    fn descendant_bounds_are_relative_to_the_window() {
        let mut child = node(2, "AXButton", vec![]);
        child.frame = Some(CGRect::new(CGPoint::new(130.0, 240.0), CGSize::new(80.0, 22.0)));
        let nodes = [window(1, vec![key(2)]), child];
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes, &mut ids).unwrap();
        let child_id = ids.get(&key(2)).unwrap();
        let (_, built) = update.nodes.iter().find(|(id, _)| *id == child_id).unwrap();
        let bounds = built.bounds().expect("a sized child has bounds");
        // Window origin is (100, 200), so the child sits at (30, 40) within it.
        assert_eq!((bounds.x0, bounds.y0), (30.0, 40.0));
    }

    /// A window that lists the application element as a child must not emit it.
    /// It is a cycle back to the root of everything, and `Role::Application`
    /// inside a `Role::Window` means nothing to a consumer.
    #[test]
    fn the_application_element_is_never_part_of_a_window_subtree() {
        let nodes = [
            window(1, vec![key(2), key(3)]),
            node(2, "AXApplication", vec![]),
            node(3, "AXButton", vec![]),
        ];
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes[..1], &mut ids).unwrap();
        let root_id = ids.get(&key(1)).unwrap();
        let (_, root) = update.nodes.iter().find(|(id, _)| *id == root_id).unwrap();
        assert!(
            root.children().is_empty(),
            "an unwalked application child contributes no id"
        );
        // And the role map would place it wrongly if it ever did get through.
        assert_eq!(nodes[1].accesskit_role(), accesskit::Role::Application);
    }

    /// **Regression: this panicked the consumer.** macOS exposes a table cell
    /// under both `AXRows` and `AXColumns`, so two parents claim it. AccessKit
    /// requires a strict tree and aborts with "TreeUpdate includes duplicate
    /// child" — taking down every other mirrored window with it.
    #[test]
    fn a_child_claimed_by_two_parents_is_emitted_under_only_one() {
        let cell = key(9);
        let nodes = [
            window(1, vec![key(2), key(3)]),
            // A row and a column, both listing the same cell.
            node(2, "AXRow", vec![cell.clone()]),
            node(3, "AXColumn", vec![cell.clone()]),
            node(9, "AXCell", vec![]),
        ];
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes, &mut ids).unwrap();
        let cell_id = ids.get(&cell).unwrap();
        let claims = update
            .nodes
            .iter()
            .filter(|(_, node)| node.children().contains(&cell_id))
            .count();
        assert_eq!(claims, 1, "exactly one parent may list the cell");

        // The first parent in walk order keeps it — the row, which is also the
        // hierarchy a reader wants.
        let row_id = ids.get(&key(2)).unwrap();
        let (_, row) = update.nodes.iter().find(|(id, _)| *id == row_id).unwrap();
        assert_eq!(row.children(), &[cell_id]);
    }

    /// The same list naming a child twice is the other half of the live
    /// failure: one parent had `children=[1796, 1796]`.
    #[test]
    fn a_parent_listing_one_child_twice_emits_it_once() {
        let child = key(2);
        let nodes = [
            window(1, vec![child.clone(), child.clone()]),
            node(2, "AXButton", vec![]),
        ];
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&nodes, &mut ids).unwrap();
        let root_id = ids.get(&key(1)).unwrap();
        let (_, root) = update.nodes.iter().find(|(id, _)| *id == root_id).unwrap();
        assert_eq!(root.children().len(), 1);
    }

    /// The refresh path reads structure from `emitted_children`, so it must
    /// apply the same rule — otherwise a refresh hands a node back the children
    /// the walk had deliberately taken away, reintroducing the duplicate.
    #[test]
    fn emitted_children_applies_the_same_single_parent_rule() {
        let cell = key(9);
        let nodes = [
            window(1, vec![key(2), key(3)]),
            node(2, "AXRow", vec![cell.clone()]),
            node(3, "AXColumn", vec![cell.clone()]),
            node(9, "AXCell", vec![]),
        ];
        let children = emitted_children(&nodes);
        let claims = children.values().filter(|kids| kids.contains(&cell)).count();
        assert_eq!(claims, 1, "the refresh path must agree with the walk");
    }

    #[test]
    fn emitted_children_records_only_reached_elements() {
        let nodes = [window(1, vec![key(2), key(99)]), node(2, "AXButton", vec![])];
        let children = emitted_children(&nodes);
        assert_eq!(children.get(&key(1)).unwrap(), &vec![key(2)]);
        assert!(children.get(&key(2)).unwrap().is_empty());
    }
}
