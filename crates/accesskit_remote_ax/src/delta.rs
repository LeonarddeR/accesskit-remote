//! Reducing a re-walk to what actually changed.
//!
//! A re-walk produces the whole window every time. Sending that whole window
//! every time is what the AT-SPI source avoids with its per-node cache, and
//! measurement here shows why it matters: an **idle** desktop was emitting six
//! updates and 800 nodes in eight seconds, because one node inside System
//! Settings flickers and the entire 133-node tree rode along with it.
//!
//! This is the tree-level form of the same guarantee — *unchanged ⇒ no wire
//! traffic*. It exploits the property the identity measurement established: an
//! element that survives a re-walk keeps its node id, so the same id in two
//! walks denotes the same node and the two can be compared at all.
//!
//! Comparison is by serialized form because `accesskit::Node` implements no
//! equality. That is also exactly the comparison the wire performs, so two
//! nodes compare equal here precisely when sending the second would have been
//! a no-op for the consumer.

use accesskit::{NodeId, TreeUpdate};
use std::collections::HashMap;

/// What a window last put on the wire, so the next walk can be reduced to its
/// difference.
#[derive(Default)]
pub struct EmittedTree {
    nodes: HashMap<NodeId, Vec<u8>>,
    focus: Option<NodeId>,
    /// Whether the consumer has received this window's root and tree yet.
    announced: bool,
}

impl EmittedTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many nodes the consumer is currently holding.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The focus the consumer currently believes in, for building a delta that
    /// must not disturb it.
    pub fn focus(&self) -> Option<NodeId> {
        self.focus
    }

    /// Whether this window has been announced yet, i.e. whether a delta is
    /// meaningful at all.
    pub fn is_announced(&self) -> bool {
        self.announced
    }

    /// Whether the consumer currently holds this node.
    ///
    /// A delta may only name children the consumer already has, or that the
    /// same delta carries. Referencing anything else panics
    /// `accesskit_consumer` with "children ids which are neither in the current
    /// tree nor the ID of another node from the update" — observed live from
    /// the refresh path, which was rebuilding a child list from element keys
    /// recorded at the last walk without checking they had survived it.
    pub fn holds(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
    }

    /// Compares one re-read node against what the consumer holds.
    ///
    /// Unlike [`reduce`](Self::reduce) this must **not** prune the cache: a
    /// single-node refresh says nothing about the other nodes, and treating
    /// their absence as removal would make the next re-walk resend the entire
    /// window — the exact traffic this module exists to avoid.
    pub fn reduce_node(&mut self, id: NodeId, node: &accesskit::Node) -> bool {
        let Ok(encoded) = serde_json::to_vec(node) else {
            return true;
        };
        if self.nodes.get(&id) == Some(&encoded) {
            return false;
        }
        self.nodes.insert(id, encoded);
        true
    }

    /// Reduces a full walk to the update that should actually be sent, or
    /// `None` when nothing changed.
    ///
    /// The first call passes everything through and carries the `tree`, since
    /// the consumer has no prior state to diff against. Later calls send only
    /// changed nodes and omit `tree`, which the client treats as a delta.
    ///
    /// A node that vanished needs no mention: it leaves its parent's child
    /// list, and the client prunes anything no longer reachable from the root
    /// when it next takes a snapshot.
    pub fn reduce(&mut self, full: TreeUpdate) -> Option<TreeUpdate> {
        let mut changed = Vec::new();
        let mut seen = HashMap::with_capacity(full.nodes.len());

        for (id, node) in &full.nodes {
            let encoded = match serde_json::to_vec(node) {
                Ok(encoded) => encoded,
                // A node that will not serialize cannot be diffed, so send it
                // and let the wire's own encoder report the problem.
                Err(_) => {
                    changed.push((*id, node.clone()));
                    continue;
                }
            };
            if self.nodes.get(id) != Some(&encoded) {
                changed.push((*id, node.clone()));
            }
            seen.insert(*id, encoded);
        }

        // Nodes the walk no longer reached are dropped from the cache, so that
        // an element returning later is sent again rather than suppressed
        // against a stale entry.
        self.nodes = seen;

        let focus_moved = self.focus != Some(full.focus);
        if !self.announced {
            self.announced = true;
            self.focus = Some(full.focus);
            return Some(full);
        }
        if changed.is_empty() && !focus_moved {
            return None;
        }
        self.focus = Some(full.focus);
        Some(TreeUpdate {
            nodes: changed,
            // A delta carries no tree: the root and its id are unchanged, and
            // resending them would make the client re-seat the whole window.
            tree: None,
            tree_id: full.tree_id,
            focus: full.focus,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit::{Node, Role, Tree, TreeId};

    fn update(nodes: Vec<(NodeId, Node)>, focus: NodeId) -> TreeUpdate {
        TreeUpdate {
            nodes,
            tree: Some(Tree::new(NodeId(0))),
            tree_id: TreeId::ROOT,
            focus,
        }
    }

    fn labelled(role: Role, label: &str) -> Node {
        let mut node = Node::new(role);
        node.set_label(label);
        node
    }

    fn tree(label: &str) -> TreeUpdate {
        update(
            vec![
                (NodeId(0), {
                    let mut root = Node::new(Role::Window);
                    root.set_children(vec![NodeId(1)]);
                    root
                }),
                (NodeId(1), labelled(Role::Button, label)),
            ],
            NodeId(0),
        )
    }

    #[test]
    fn the_first_walk_is_sent_whole_with_its_tree() {
        let mut emitted = EmittedTree::new();
        let first = emitted.reduce(tree("Save")).expect("the first walk is always sent");
        assert_eq!(first.nodes.len(), 2);
        assert!(first.tree.is_some(), "the consumer needs the root to seat the window");
        assert_eq!(emitted.len(), 2);
    }

    /// **The invariant this module exists for.** An idle desktop was emitting
    /// 800 nodes in 8 seconds before this: one flickering node dragged its
    /// whole 133-node tree onto the wire every time.
    #[test]
    fn an_unchanged_re_walk_sends_nothing() {
        let mut emitted = EmittedTree::new();
        emitted.reduce(tree("Save")).unwrap();
        assert!(emitted.reduce(tree("Save")).is_none());
        assert!(emitted.reduce(tree("Save")).is_none(), "and stays quiet");
    }

    #[test]
    fn only_the_changed_node_is_sent() {
        let mut emitted = EmittedTree::new();
        emitted.reduce(tree("Save")).unwrap();
        let delta = emitted.reduce(tree("Saved")).expect("a changed label is a change");
        assert_eq!(delta.nodes.len(), 1, "the untouched root must not ride along");
        assert_eq!(delta.nodes[0].0, NodeId(1));
        assert!(delta.tree.is_none(), "a delta must not re-seat the window");
    }

    #[test]
    fn a_focus_move_alone_is_worth_sending() {
        // Focus lives on the update rather than on any node, so it would
        // otherwise be lost whenever no node changed.
        let mut emitted = EmittedTree::new();
        emitted.reduce(tree("Save")).unwrap();
        let delta = emitted
            .reduce(update(tree("Save").nodes, NodeId(1)))
            .expect("focus moved");
        assert!(delta.nodes.is_empty(), "no node changed, so none is sent");
        assert_eq!(delta.focus, NodeId(1));
        // And it settles again.
        assert!(emitted.reduce(update(tree("Save").nodes, NodeId(1))).is_none());
    }

    #[test]
    fn a_new_node_is_sent_and_a_vanished_one_is_forgotten() {
        let mut emitted = EmittedTree::new();
        emitted.reduce(tree("Save")).unwrap();

        let mut grown = tree("Save");
        grown.nodes.push((NodeId(2), labelled(Role::Button, "Cancel")));
        let delta = emitted.reduce(grown).expect("a new node is a change");
        assert_eq!(delta.nodes.len(), 1);
        assert_eq!(delta.nodes[0].0, NodeId(2));
        assert_eq!(emitted.len(), 3);

        // The node goes away: nothing is sent for it — it leaves its parent's
        // child list and the client prunes it — but the cache must forget it,
        // or its return would be suppressed against a stale entry.
        assert!(emitted.reduce(tree("Save")).is_none());
        assert_eq!(emitted.len(), 2);

        let mut again = tree("Save");
        again.nodes.push((NodeId(2), labelled(Role::Button, "Cancel")));
        let delta = emitted.reduce(again).expect("the returning node is sent again");
        assert_eq!(delta.nodes[0].0, NodeId(2));
    }

    #[test]
    fn a_single_node_refresh_reports_only_real_changes() {
        let mut emitted = EmittedTree::new();
        emitted.reduce(tree("Save")).unwrap();
        assert!(
            !emitted.reduce_node(NodeId(1), &labelled(Role::Button, "Save")),
            "an unchanged re-read is not worth sending"
        );
        assert!(emitted.reduce_node(NodeId(1), &labelled(Role::Button, "Saved")));
        assert!(
            !emitted.reduce_node(NodeId(1), &labelled(Role::Button, "Saved")),
            "and settles once sent"
        );
    }

    /// A single-node refresh must not disturb the cache's view of the rest of
    /// the window. If it pruned, the next re-walk would find nothing cached
    /// and resend every node — precisely the traffic this module removes.
    #[test]
    fn a_single_node_refresh_leaves_the_rest_of_the_cache_intact() {
        let mut emitted = EmittedTree::new();
        emitted.reduce(tree("Save")).unwrap();
        assert_eq!(emitted.len(), 2);
        emitted.reduce_node(NodeId(1), &labelled(Role::Button, "Saved"));
        assert_eq!(emitted.len(), 2, "the root is still known");
        // The following re-walk sees only the one genuine difference.
        let delta = emitted.reduce(tree("Save")).expect("the label reverted");
        assert_eq!(delta.nodes.len(), 1);
    }

    #[test]
    fn a_node_reverting_to_its_previous_value_is_sent() {
        // 133 <-> 134 oscillation observed live: the tree must not get stuck
        // believing a node still holds a value it has since changed away from
        // and back to.
        let mut emitted = EmittedTree::new();
        emitted.reduce(tree("Save")).unwrap();
        assert!(emitted.reduce(tree("Saved")).is_some());
        assert!(emitted.reduce(tree("Save")).is_some(), "reverting is also a change");
        assert!(emitted.reduce(tree("Save")).is_none());
    }
}
