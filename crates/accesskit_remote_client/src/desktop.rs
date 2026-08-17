//! Composing every remote window into one tree, for a client that has only one
//! window to hang it on.
//!
//! The RAIL arrangement gives each remote window its own local `RAIL_WINDOW`
//! HWND, so a binding per window is the natural shape and each tree stands
//! alone. A full-desktop RDP session has no such thing: there is one session
//! window showing a picture of the whole remote desktop, and a screen reader
//! attaching to it must be handed *everything* through that one host. macrdp is
//! full-desktop, so this is the shape it needs.
//!
//! **AccessKit already has the mechanism, and it is not id renumbering.** A node
//! carrying [`Node::set_tree_id`] is a *graft point* whose child is the root of
//! a separate subtree, and node ids are namespaced per subtree — the pair
//! (`NodeId`, `TreeId`) is what identifies a node. So each remote window's tree
//! crosses the wire and is grafted **verbatim**: the only thing this module
//! changes is the `tree_id` field on the update. Nothing renumbers, so nothing
//! can renumber wrongly, and an action coming back carries `target_tree`, which
//! names the window it belongs to.
//!
//! # The rules, all of which are panics
//!
//! `accesskit_consumer` enforces subtree bookkeeping with `panic!`, and it runs
//! inside the screen reader's process — a violation is a crashed screen reader,
//! not a glitch. The four that constrain this module, each pinned by a test:
//!
//! 1. **Graft before subtree.** Pushing a subtree whose graft node the root tree
//!    does not yet contain panics.
//! 2. **A subtree's first update must carry `tree` data.** So a window's first
//!    push is always a full snapshot, never a delta — even if a delta arrives
//!    first.
//! 3. **A graft node may not have children of its own.** Its only child is the
//!    subtree root.
//! 4. **Focus may not rest on a graft node whose subtree is absent.** This is
//!    the sharp one: the root tree is published the moment an adapter activates,
//!    before any subtree exists, so focus starts on the desktop root and moves
//!    to a window only once that window's subtree is live.

use crate::ClientConnection;
use accesskit::{Node, NodeId, Role, Tree, TreeId, TreeUpdate, Uuid};
use accesskit_remote::WindowId;
use std::collections::{HashMap, HashSet};

/// The desktop root: the node every remote window hangs under.
const ROOT: NodeId = NodeId(0);

/// One composed tree holding every remote window as a grafted subtree.
///
/// Feed it the client's state with [`sync`](Self::sync) and its live deltas
/// with [`delta`](Self::delta); apply what they return, in order, to the
/// platform adapter.
#[derive(Debug)]
pub struct DesktopTree {
    label: String,
    /// Grafted windows, in the order a reader will meet them. A `Vec` rather
    /// than a set because that order is user-visible and must not shuffle when
    /// an unrelated window opens.
    grafted: Vec<WindowId>,
    /// Windows whose subtree has actually been pushed. A graft is not a
    /// subtree, and focus may only land on one that is both.
    live: HashSet<WindowId>,
    focus: Option<WindowId>,
    /// Whether the root tree has been published at least once.
    started: bool,
    /// What each window's subtree actually holds, mirrored the way a consumer
    /// holds it: only what its root can reach.
    held: HashMap<WindowId, Held>,
}

/// One subtree as the consumer has it, so this side can tell what the consumer
/// still holds from what it was merely once sent.
///
/// **They are not the same set, and the difference aborts a screen reader.** A
/// consumer keeps only what the root reaches and discards the rest, so a node
/// orphaned by an earlier delta is gone from the consumer while the client
/// store — which never prunes — still has it. Naming such a node is a panic
/// inside the consumer, in a function that cannot unwind.
#[derive(Debug, Default)]
struct Held {
    children: HashMap<NodeId, Vec<NodeId>>,
    root: Option<NodeId>,
}

impl Held {
    /// Applies an update and prunes to what the root reaches, exactly as a
    /// consumer would.
    fn apply(&mut self, update: &TreeUpdate) {
        for (id, node) in &update.nodes {
            self.children.insert(*id, node.children().to_vec());
        }
        if let Some(tree) = update.tree.as_ref() {
            self.root = Some(tree.root);
        }
        let Some(root) = self.root else { return };
        let mut reachable = HashSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(children) = self.children.get(&id) {
                stack.extend(children.iter().copied());
            }
        }
        self.children.retain(|id, _| reachable.contains(id));
    }

    fn holds(&self, id: NodeId) -> bool {
        self.children.contains_key(&id)
    }
}

impl DesktopTree {
    /// `label` is what a reader announces for the desktop as a whole — the
    /// remote machine, not any one window.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            grafted: Vec::new(),
            live: HashSet::new(),
            focus: None,
            started: false,
            held: HashMap::new(),
        }
    }

    /// The subtree a window's nodes live in.
    ///
    /// Derived from the window id rather than random so that the same window is
    /// the same subtree across a re-sync, and so a test can name one.
    pub fn tree_id(window: WindowId) -> TreeId {
        // High bits mark the namespace, low bits carry the id: distinct from
        // the nil UUID that means ROOT, and never colliding with another
        // window.
        TreeId(Uuid::from_u128(
            (0xacce_5541_u128 << 96) | u128::from(window.0),
        ))
    }

    /// Which window an action belongs to, from the `target_tree` it carries.
    ///
    /// The whole of action routing in desktop mode: a UIA client acts on a node
    /// in some subtree, and the subtree names the window.
    pub fn window_for(&self, tree: TreeId) -> Option<WindowId> {
        self.grafted
            .iter()
            .copied()
            .find(|window| Self::tree_id(*window) == tree)
    }

    /// The graft node standing in for a window in the root tree.
    fn graft_id(window: WindowId) -> NodeId {
        // Offset past the desktop root. Root-tree ids are namespaced to the
        // root tree, so these cannot collide with any window's own node ids.
        NodeId(window.0.wrapping_add(1))
    }

    /// Whether anything has been published yet, i.e. whether the next root
    /// update is the tree's first and must carry `tree` data.
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Forgets what has been published, so the next [`sync`](Self::sync)
    /// rebuilds the whole desktop from scratch.
    ///
    /// Call this whenever the host adapter starts a fresh tree — a platform
    /// adapter activates on demand and discards anything pushed while no
    /// assistive technology was listening, so a composed tree that still
    /// believed those pushes had landed would graft onto nothing.
    pub fn reset(&mut self) {
        self.grafted.clear();
        self.live.clear();
        self.focus = None;
        self.started = false;
        // The adapter is starting an empty tree, so it holds nothing — and a
        // stale idea of what it holds is what lets a focus land on a node that
        // is no longer there.
        self.held.clear();
    }

    /// Brings the composed tree in line with the client's state.
    ///
    /// Returns the updates to apply **in order**: the root tree first if its
    /// window set changed, then a full snapshot per newly grafted window, then
    /// focus. Call it on adapter activation and whenever windows or focus
    /// change; it is idempotent, so calling it when nothing changed returns an
    /// empty vector.
    pub fn sync(&mut self, client: &mut ClientConnection) -> Vec<TreeUpdate> {
        let mut updates = Vec::new();

        // Keep the established order and append what is new, so an opening
        // window does not renumber a reader's mental map of the desktop.
        let current: HashSet<WindowId> = client.windows().collect();
        let mut order: Vec<WindowId> = self
            .grafted
            .iter()
            .copied()
            .filter(|window| current.contains(window))
            .collect();
        // Sorted, because `windows()` iterates a hash map: without this the
        // order a reader meets the desktop in is whatever the hasher decided
        // this run, and it changes on reconnect. Provider ids are handed out in
        // order of appearance, so sorting by id is also the order the windows
        // opened in.
        let mut fresh: Vec<WindowId> = client
            .windows()
            .filter(|window| !order.contains(window))
            .collect();
        fresh.sort_unstable_by_key(|window| window.0);
        order.extend(fresh);

        let structure_changed = order != self.grafted;
        if structure_changed || !self.started {
            self.live.retain(|window| current.contains(window));
            self.grafted = order;
            updates.push(self.root_update(None));
            self.started = true;
        }

        // Rule 1 and 2: the graft exists now, and a snapshot always carries
        // tree data, so this is the only shape a first push may take.
        for window in self.grafted.clone() {
            if self.live.contains(&window) {
                continue;
            }
            let Some(mut snapshot) = client.snapshot(window) else {
                // No tree yet — the provider announces a window before its
                // first update. It becomes live when the delta arrives.
                continue;
            };
            snapshot.tree_id = Self::tree_id(window);
            self.held.entry(window).or_default().apply(&snapshot);
            updates.push(snapshot);
            self.live.insert(window);
        }

        // Rule 4: only now, with the subtree pushed, may focus point at it.
        let wanted = client.focused_window().filter(|w| self.live.contains(w));
        if wanted != self.focus || structure_changed {
            self.focus = wanted;
            updates.push(self.root_update(wanted));
        }
        updates
    }

    /// Retags a live delta into its window's subtree.
    ///
    /// Returns `None` when that window has no subtree yet — pushing a delta
    /// then would panic (rule 2), and the snapshot [`sync`](Self::sync) takes
    /// afterwards carries the same content anyway.
    /// # Focus is checked against what the consumer holds, not what was sent
    ///
    /// A delta names a focused node, and the consumer validates it: if that
    /// node is neither in the delta nor still in its tree, it panics — inside a
    /// function that cannot unwind, so the screen reader's process aborts.
    /// Observed exactly that way, as `Focused ID #14 is not in the node list`,
    /// the moment a reader started delivering focus events.
    ///
    /// The client store cannot answer this, and 3c082f1's guard is not enough
    /// here: the store never prunes, so it happily reports a node the consumer
    /// threw away as unreachable. Only a mirror of the consumer's own pruning
    /// can tell, which is what [`Held`] is for. When the focus is not provably
    /// held, it falls back to the subtree root — a node that is held by
    /// definition, and a reader landing on the window itself rather than
    /// nothing.
    pub fn delta(&mut self, window: WindowId, mut update: TreeUpdate) -> Option<TreeUpdate> {
        if !self.live.contains(&window) {
            return None;
        }
        update.tree_id = Self::tree_id(window);
        let held = self.held.entry(window).or_default();
        let arriving = update.nodes.iter().any(|(id, _)| *id == update.focus);
        if !arriving && !held.holds(update.focus) {
            if let Some(root) = held.root {
                update.focus = root;
            }
        }
        held.apply(&update);
        Some(update)
    }

    /// The root tree: the desktop node and one graft per window.
    fn root_update(&self, focus: Option<WindowId>) -> TreeUpdate {
        let mut root = Node::new(Role::Window);
        root.set_label(self.label.clone());
        root.set_children(self.grafted.iter().copied().map(Self::graft_id).collect::<Vec<_>>());

        let mut nodes = vec![(ROOT, root)];
        for window in &self.grafted {
            // Rule 3: a graft node carries its tree id and nothing else. It is
            // a `GenericContainer` so `common_filter` drops it, leaving the
            // window's own root node as what a reader meets.
            let mut graft = Node::new(Role::GenericContainer);
            graft.set_tree_id(Self::tree_id(*window));
            nodes.push((Self::graft_id(*window), graft));
        }

        TreeUpdate {
            nodes,
            tree: Some(Tree::new(ROOT)),
            tree_id: TreeId::ROOT,
            focus: focus.map_or(ROOT, Self::graft_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientEvent;
    use accesskit_remote::{AppInfo, Message, PeerRole, Session, SessionConfig};

    /// A provider that says exactly what a test tells it to.
    fn provider() -> Session {
        Session::new(SessionConfig::new(PeerRole::Provider, "test-provider"))
    }

    fn app() -> AppInfo {
        AppInfo {
            name: "test".into(),
            app_id: None,
            pid: None,
            toolkit: None,
            toolkit_version: None,
        }
    }

    fn tree_of(label: &str) -> TreeUpdate {
        let root = NodeId(1);
        let mut node = Node::new(Role::Window);
        node.set_label(label);
        let mut button = Node::new(Role::Button);
        button.set_label("press me");
        node.set_children(vec![NodeId(2)]);
        TreeUpdate {
            nodes: vec![(root, node), (NodeId(2), button)],
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        }
    }

    /// An established client, plus a way to feed it provider messages.
    struct Harness {
        client: ClientConnection,
        provider: Session,
    }

    impl Harness {
        fn new() -> Self {
            let mut client = ClientConnection::new("test-client");
            let mut provider = provider();
            provider.handle_input(&client.take_output()).unwrap();
            client.handle_input(&provider.take_output()).unwrap();
            assert!(client.is_established());
            Self { client, provider }
        }

        fn send(&mut self, message: &Message) -> Vec<ClientEvent> {
            self.provider.send(message).unwrap();
            let out = self.provider.take_output();
            self.client.handle_input(&out).unwrap()
        }

        fn add_window(&mut self, id: u64, title: &str) {
            self.send(&Message::WindowAdded {
                window: WindowId(id),
                title: title.into(),
                app: app(),
                native_window_id: None,
            });
            self.send(&Message::TreeUpdate {
                window: WindowId(id),
                update: tree_of(title),
            });
        }
    }

    /// Applies a sequence to the real consumer, which panics on any of the four
    /// bookkeeping violations — so surviving this *is* the assertion.
    fn feed(updates: Vec<TreeUpdate>) -> accesskit_consumer::Tree {
        let mut updates = updates.into_iter();
        let first = updates.next().expect("at least the root tree");
        let mut tree = accesskit_consumer::Tree::new(first, true);
        for update in updates {
            tree.update_and_process_changes(update, &mut NoChanges);
        }
        tree
    }

    struct NoChanges;

    impl accesskit_consumer::TreeChangeHandler for NoChanges {
        fn node_added(&mut self, _: &accesskit_consumer::Node) {}
        fn node_updated(&mut self, _: &accesskit_consumer::Node, _: &accesskit_consumer::Node) {}
        fn focus_moved(
            &mut self,
            _: Option<&accesskit_consumer::Node>,
            _: Option<&accesskit_consumer::Node>,
        ) {
        }
        fn node_removed(&mut self, _: &accesskit_consumer::Node) {}
    }

    /// **The whole point.** Two windows from the provider become one tree a
    /// single host can serve, with both windows' contents reachable and their
    /// node ids untouched.
    #[test]
    fn every_window_becomes_a_subtree_of_one_desktop() {
        let mut harness = Harness::new();
        harness.add_window(1, "first");
        harness.add_window(2, "second");

        let mut desktop = DesktopTree::new("a mac");
        let tree = feed(desktop.sync(&mut harness.client));
        let state = tree.state();

        // Both windows' own root nodes are present, under their own subtrees,
        // still carrying NodeId(1) — no renumbering happened.
        for (window, label) in [(1u64, "first"), (2, "second")] {
            let node = state
                .node_by_tree_local_id(NodeId(1), DesktopTree::tree_id(WindowId(window)))
                .expect("the window's root is in its subtree");
            assert_eq!(node.label().as_deref(), Some(label));
        }
        assert_eq!(state.root().label().as_deref(), Some("a mac"), "and the desktop names itself");
    }

    /// **Rule 4, the sharp one.** An adapter activates and publishes the root
    /// tree before any subtree exists; focus resting on a graft node then is a
    /// panic inside the screen reader.
    #[test]
    fn focus_waits_for_the_subtree_it_points_at() {
        let mut harness = Harness::new();
        harness.send(&Message::WindowAdded {
            window: WindowId(1),
            title: "not yet drawn".into(),
            app: app(),
            native_window_id: None,
        });
        harness.send(&Message::FocusChanged { window: Some(WindowId(1)) });

        // The window is announced and focused, but has sent no tree, so there
        // is nothing to graft onto and focus must stay on the desktop.
        let mut desktop = DesktopTree::new("a mac");
        let updates = desktop.sync(&mut harness.client);
        assert!(
            updates.iter().all(|u| u.tree_id != TreeId::ROOT || u.focus == ROOT),
            "focus must not name a graft node whose subtree is absent",
        );
        feed(updates);

        // Once its tree arrives, focus may follow.
        harness.send(&Message::TreeUpdate {
            window: WindowId(1),
            update: tree_of("now drawn"),
        });
        let updates = desktop.sync(&mut harness.client);
        assert!(
            updates.last().unwrap().focus == DesktopTree::graft_id(WindowId(1)),
            "and now it does",
        );
    }

    /// **Rule 2.** A delta before the first snapshot would panic, so it is
    /// withheld — the snapshot that follows carries the same content.
    #[test]
    fn a_delta_for_an_ungrafted_window_is_withheld() {
        let mut desktop = DesktopTree::new("a mac");
        assert!(
            desktop.delta(WindowId(1), tree_of("early")).is_none(),
            "a subtree's first update must be a snapshot, not a delta",
        );
    }

    /// A delta after grafting is passed straight through, retagged and
    /// otherwise untouched — the property that makes this composition free.
    #[test]
    fn a_delta_after_grafting_is_only_retagged() {
        let mut harness = Harness::new();
        harness.add_window(1, "first");
        let mut desktop = DesktopTree::new("a mac");
        feed(desktop.sync(&mut harness.client));

        let original = tree_of("changed");
        let retagged = desktop.delta(WindowId(1), original.clone()).expect("grafted");
        assert_eq!(retagged.tree_id, DesktopTree::tree_id(WindowId(1)));
        assert_eq!(retagged.nodes, original.nodes, "nothing else is touched");
        assert_eq!(retagged.focus, original.focus);
    }

    /// **The abort.** A node the consumer has *pruned* is not the same as a
    /// node it never had, and the client store cannot tell them apart: it never
    /// prunes, so it reports an orphan as present and the focus guard on that
    /// path waves it through. The consumer then panics — in a function that
    /// cannot unwind, so the reader's process dies rather than misbehaves.
    ///
    /// Seen live as `Focused ID #14 is not in the node list`, the moment a
    /// reader began delivering focus events into a composed desktop.
    #[test]
    fn a_focus_on_a_node_the_consumer_has_pruned_is_corrected() {
        let mut harness = Harness::new();
        harness.add_window(1, "a window");
        let mut desktop = DesktopTree::new("a mac");
        // Collected rather than fed piecemeal: the consumer must see the whole
        // stream from its root tree onwards, as a real host applies it.
        let mut stream = desktop.sync(&mut harness.client);

        // The window drops its only child, so #2 is unreachable and every
        // consumer throws it away — while the client store still holds it.
        let mut orphaning = Node::new(Role::Window);
        orphaning.set_label("a window");
        orphaning.set_children(Vec::<NodeId>::new());
        let pruned = desktop
            .delta(
                WindowId(1),
                TreeUpdate {
                    nodes: vec![(NodeId(1), orphaning)],
                    tree: None,
                    tree_id: TreeId::ROOT,
                    focus: NodeId(1),
                },
            )
            .expect("grafted");

        // Now focus lands on the orphan.
        let corrected = desktop
            .delta(
                WindowId(1),
                TreeUpdate {
                    nodes: Vec::new(),
                    tree: None,
                    tree_id: TreeId::ROOT,
                    focus: NodeId(2),
                },
            )
            .expect("grafted");
        assert_eq!(
            corrected.focus,
            NodeId(1),
            "focus must fall back to the subtree root, which the consumer certainly holds",
        );

        // And the consumer agrees, which is the check that actually matters.
        stream.push(pruned);
        stream.push(corrected);
        feed(stream);
    }

    /// A window closing takes its subtree with it, and the survivors keep their
    /// place rather than shuffling under the reader.
    #[test]
    fn a_closing_window_leaves_the_others_where_they_were() {
        let mut harness = Harness::new();
        harness.add_window(1, "first");
        harness.add_window(2, "second");
        harness.add_window(3, "third");
        let mut desktop = DesktopTree::new("a mac");
        feed(desktop.sync(&mut harness.client));
        // Deterministic despite the client storing windows in a hash map — the
        // order a reader navigates in must not depend on the hasher, nor change
        // across a reconnect.
        assert_eq!(desktop.grafted, vec![WindowId(1), WindowId(2), WindowId(3)]);

        harness.send(&Message::WindowRemoved { window: WindowId(2) });
        let updates = desktop.sync(&mut harness.client);
        assert!(!updates.is_empty(), "the root tree changed");
        feed_more(updates);

        assert_eq!(
            desktop.grafted,
            vec![WindowId(1), WindowId(3)],
            "the survivors keep their places rather than shuffling",
        );
    }

    /// Feeding a sequence that does not start a tree: only checks it does not
    /// panic, which is what the consumer's rules are enforced with.
    fn feed_more(updates: Vec<TreeUpdate>) {
        let mut updates = updates.into_iter();
        let Some(first) = updates.next() else { return };
        let mut tree = accesskit_consumer::Tree::new(first, true);
        for update in updates {
            tree.update_and_process_changes(update, &mut NoChanges);
        }
    }

    /// **Regression: the abort that desktop mode hit and window mode did not.**
    ///
    /// A child-list change orphans the node that last held focus. The store
    /// prunes it on the next snapshot, and if the snapshot repeated the
    /// recorded focus, the subtree it was grafted into would have a focus that
    /// is not in it. `accesskit_consumer` rejects that from
    /// `update_host_focus_state`, i.e. from inside the platform adapter's
    /// window procedure — which is `extern "system"` and cannot unwind, so the
    /// process aborts rather than panicking catchably.
    ///
    /// Desktop mode hit it because it snapshots every window on every
    /// activation, where window mode snapshots one window once.
    #[test]
    fn a_focus_orphaned_by_a_child_list_change_never_reaches_the_host() {
        let mut harness = Harness::new();
        harness.add_window(1, "a window");

        // Focus the button, then re-parent it out of the tree — exactly what a
        // re-walk that drops a subtree does.
        let root = NodeId(1);
        let mut with_focus = tree_of("a window");
        with_focus.focus = NodeId(2);
        harness.send(&Message::TreeUpdate { window: WindowId(1), update: with_focus });

        let mut orphaning = Node::new(Role::Window);
        orphaning.set_label("a window");
        orphaning.set_children(Vec::<NodeId>::new());
        harness.send(&Message::TreeUpdate {
            window: WindowId(1),
            update: TreeUpdate {
                nodes: vec![(root, orphaning)],
                tree: None,
                tree_id: TreeId::ROOT,
                // The provider still believes the button has focus.
                focus: NodeId(2),
            },
        });

        let mut desktop = DesktopTree::new("a mac");
        let updates = desktop.sync(&mut harness.client);
        for update in &updates {
            if update.tree_id == TreeId::ROOT {
                continue;
            }
            let present: Vec<NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();
            assert!(
                present.contains(&update.focus),
                "a subtree whose focus is not in it aborts the host: focus {:?} in {present:?}",
                update.focus,
            );
        }
        // And the consumer agrees, which is the check that actually matters.
        feed(updates);
    }

    /// The same rule on the other path in: a live delta, not a snapshot.
    #[test]
    fn a_delta_naming_an_unknown_focus_is_corrected_before_it_is_handed_on() {
        let mut harness = Harness::new();
        harness.add_window(1, "a window");

        let events = harness.send(&Message::TreeUpdate {
            window: WindowId(1),
            update: TreeUpdate {
                nodes: Vec::new(),
                tree: None,
                tree_id: TreeId::ROOT,
                // A node this window has never had.
                focus: NodeId(9999),
            },
        });
        let focus = events
            .iter()
            .find_map(|event| match event {
                ClientEvent::TreeUpdated { update, .. } => Some(update.focus),
                _ => None,
            })
            .expect("the delta is surfaced");
        assert_eq!(focus, NodeId(1), "rewritten to the tree root, not passed on");
    }

    /// Action routing: a request names its subtree, and the subtree names the
    /// window it must be sent to.
    #[test]
    fn an_action_finds_its_window_through_the_subtree_it_names() {
        let mut harness = Harness::new();
        harness.add_window(7, "a window");
        let mut desktop = DesktopTree::new("a mac");
        desktop.sync(&mut harness.client);

        assert_eq!(
            desktop.window_for(DesktopTree::tree_id(WindowId(7))),
            Some(WindowId(7)),
        );
        assert_eq!(desktop.window_for(TreeId::ROOT), None, "the desktop itself is nobody's window");
    }

    /// Nothing changed, nothing to say — otherwise every poll would replace the
    /// whole desktop and the reader would be told so.
    #[test]
    fn a_sync_with_nothing_to_do_is_silent() {
        let mut harness = Harness::new();
        harness.add_window(1, "first");
        let mut desktop = DesktopTree::new("a mac");
        feed(desktop.sync(&mut harness.client));
        assert!(desktop.sync(&mut harness.client).is_empty());
    }
}
