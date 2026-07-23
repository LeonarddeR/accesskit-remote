//! Pure, bus-free translation from a walked AT-SPI subtree into an AccessKit
//! [`TreeUpdate`]. Everything here operates on plain data so it can be unit
//! tested without a live accessibility bus.

use accesskit::{Node, NodeId, TextPosition, TextSelection, Tree, TreeId, TreeUpdate};
use atspi::{Role, State, StateSet};
use std::collections::{HashMap, HashSet};

/// Caps the text mirrored from one node, in Unicode scalar values. Longer text
/// is truncated (with the caret/selection clamped into range) so a huge
/// document cannot bloat a single tree update.
pub const MAX_TEXT_CHARS: usize = 65_536;

/// Assigns stable, sequential AccessKit [`NodeId`]s to AT-SPI object paths
/// within one window. The same path always maps to the same id for the life
/// of the map.
#[derive(Debug, Default)]
pub struct NodeIdMap {
    map: HashMap<String, NodeId>,
    next: u64,
}

impl NodeIdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the id for `path`, allocating a fresh one on first sight.
    pub fn id_for(&mut self, path: &str) -> NodeId {
        if let Some(id) = self.map.get(path) {
            return *id;
        }
        let id = NodeId(self.next);
        self.next += 1;
        self.map.insert(path.to_owned(), id);
        id
    }

    /// Returns the id previously assigned to `path`, if any.
    pub fn get(&self, path: &str) -> Option<NodeId> {
        self.map.get(path).copied()
    }
}

/// One AT-SPI object flattened into the fields the mapping needs. Produced by
/// the async walk; the object path (not a proxy) is the identity.
#[derive(Debug, Clone)]
pub struct MirrorNode {
    pub path: String,
    pub role: Role,
    pub name: String,
    pub focusable: bool,
    pub focused: bool,
    pub actionable: bool,
    pub children: Vec<String>,
    /// Text-interface state for text-input roles; `None` for non-text nodes.
    pub text: Option<TextState>,
}

/// The AT-SPI `Text` interface state of one node. Offsets are Unicode scalar
/// value (code point) indices, as AT-SPI defines them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextState {
    pub text: String,
    pub caret: Option<usize>,
    pub selection: Option<(usize, usize)>,
}

/// One synthesized [`Role::TextRun`] child: its node id and code-point count,
/// used to map a global offset to a [`TextPosition`].
#[derive(Debug, Clone, PartialEq)]
pub struct TextRunLayout {
    pub id: NodeId,
    pub chars: usize,
}

/// The last-emitted artifacts for one text node, kept so a text event can emit
/// a minimal delta rather than a full rebuild.
#[derive(Debug, Clone)]
pub struct TextNodeCache {
    pub node_id: NodeId,
    pub parent: Node,
    pub element_children: Vec<NodeId>,
    pub runs: Vec<(NodeId, Node)>,
    pub layout: Vec<TextRunLayout>,
}

/// Translates an AT-SPI [`Role`] into the nearest AccessKit role.
pub fn map_role(role: Role) -> accesskit::Role {
    use accesskit::Role as A;
    match role {
        Role::Frame | Role::Window => A::Window,
        Role::Dialog => A::Dialog,
        Role::Alert => A::Alert,
        Role::Label => A::Label,
        Role::Button | Role::ToggleButton => A::Button,
        Role::CheckBox => A::CheckBox,
        Role::CheckMenuItem => A::MenuItemCheckBox,
        Role::RadioButton => A::RadioButton,
        Role::RadioMenuItem => A::MenuItemRadio,
        Role::Menu => A::Menu,
        Role::MenuBar => A::MenuBar,
        Role::MenuItem => A::MenuItem,
        Role::Panel | Role::Filler => A::GenericContainer,
        Role::Text => A::MultilineTextInput,
        Role::Entry => A::TextInput,
        Role::PasswordText => A::PasswordInput,
        Role::List => A::List,
        Role::ListItem => A::ListItem,
        Role::ListBox => A::ListBox,
        Role::ComboBox => A::ComboBox,
        Role::ScrollBar => A::ScrollBar,
        Role::ScrollPane => A::ScrollView,
        Role::Slider => A::Slider,
        Role::SpinButton => A::SpinButton,
        Role::ProgressBar => A::ProgressIndicator,
        Role::ToolBar => A::Toolbar,
        Role::PageTab => A::Tab,
        Role::PageTabList => A::TabList,
        Role::Table => A::Table,
        Role::TableCell => A::Cell,
        Role::Tree => A::Tree,
        Role::TreeItem => A::TreeItem,
        Role::Heading => A::Heading,
        Role::Link => A::Link,
        Role::Image | Role::Icon => A::Image,
        Role::StatusBar => A::Status,
        Role::Application => A::Application,
        Role::Section => A::Section,
        Role::Paragraph => A::Paragraph,
        _ => A::GenericContainer,
    }
}

/// Distills the subset of AT-SPI state the mapping cares about from a
/// [`StateSet`].
pub fn node_flags(states: StateSet) -> (bool, bool) {
    (states.contains(State::Focusable), states.contains(State::Focused))
}

/// Whether a role is one whose default action a consumer should expose as
/// Click. GTK exposes the AT-SPI Action interface on many containers, so the
/// interface alone is too broad; this narrows it to conventionally clickable
/// roles.
fn is_clickable_role(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::ToggleButton
            | Role::CheckBox
            | Role::CheckMenuItem
            | Role::RadioButton
            | Role::RadioMenuItem
            | Role::MenuItem
            | Role::Menu
            | Role::PageTab
            | Role::Link
            | Role::ListItem
            | Role::TreeItem
    )
}

/// Whether a role's AT-SPI `Text` interface should be mirrored into synthesized
/// text runs. Narrowed to editable text roles; static text (labels, documents)
/// keeps its name→value mapping for now.
pub fn is_text_input_role(role: Role) -> bool {
    matches!(role, Role::Text | Role::Entry | Role::PasswordText)
}

/// Truncates `text` to at most [`MAX_TEXT_CHARS`] code points, returning a
/// prefix that ends on a code-point boundary.
pub fn clamp_text(text: &str) -> &str {
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((byte_index, _)) => &text[..byte_index],
        None => text,
    }
}

/// The synthetic AT-SPI-style path of run `index` under `parent_path`. `#` is
/// illegal in a D-Bus object path, so these never collide with real paths.
fn run_path(parent_path: &str, index: usize) -> String {
    format!("{parent_path}#run{index}")
}

/// Splits text into one run per hard line, keeping the trailing `\n` in each
/// run. Empty text, or text ending in `\n`, yields a final empty run so the
/// end-of-document caret has a run to land on.
fn split_runs(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut runs: Vec<&str> = text.split_inclusive('\n').collect();
    if text.ends_with('\n') {
        runs.push("");
    }
    runs
}

/// The UTF-8 byte length of each code point in `run`. The sum equals the run's
/// byte length, as `character_lengths` requires.
fn character_lengths(run: &str) -> Vec<u8> {
    run.chars().map(|c| c.len_utf8() as u8).collect()
}

/// Word-start code-point indices within `run`: index 0 plus each
/// whitespace→non-whitespace transition. Returns empty when any index exceeds
/// the `u8` slice ceiling, degrading word navigation to line granularity.
fn word_starts(run: &str) -> Vec<u8> {
    let mut starts: Vec<usize> = Vec::new();
    let mut prev_ws = false;
    for (i, c) in run.chars().enumerate() {
        if i == 0 || (prev_ws && !c.is_whitespace()) {
            starts.push(i);
        }
        prev_ws = c.is_whitespace();
    }
    if starts.iter().any(|&s| s > u8::MAX as usize) {
        return Vec::new();
    }
    starts.into_iter().map(|s| s as u8).collect()
}

/// Builds the [`Role::TextRun`] child nodes for `text` and their layout. Each
/// run carries its value, per-code-point `character_lengths`, and word starts.
pub fn build_text_runs(
    parent_path: &str,
    text: &str,
    ids: &mut NodeIdMap,
) -> (Vec<(NodeId, Node)>, Vec<TextRunLayout>) {
    let runs = split_runs(text);
    let mut nodes = Vec::with_capacity(runs.len());
    let mut layout = Vec::with_capacity(runs.len());
    for (index, run) in runs.iter().enumerate() {
        let id = ids.id_for(&run_path(parent_path, index));
        let lengths = character_lengths(run);
        let chars = lengths.len();
        let mut node = Node::new(accesskit::Role::TextRun);
        node.set_value((*run).to_owned());
        node.set_character_lengths(lengths);
        let words = word_starts(run);
        if !words.is_empty() {
            node.set_word_starts(words);
        }
        nodes.push((id, node));
        layout.push(TextRunLayout { id, chars });
    }
    (nodes, layout)
}

/// Maps a global code-point `offset` to a [`TextPosition`] within `layout`. An
/// offset at a run boundary lands on the next run's start (index 0), so an
/// end-of-line caret sits on the line break; an offset past the end clamps to
/// the last run's end.
pub fn text_position(layout: &[TextRunLayout], offset: usize) -> TextPosition {
    let mut remaining = offset;
    for run in layout {
        if remaining < run.chars {
            return TextPosition { node: run.id, character_index: remaining };
        }
        remaining -= run.chars;
    }
    match layout.last() {
        Some(run) => TextPosition { node: run.id, character_index: run.chars },
        None => TextPosition { node: NodeId(0), character_index: 0 },
    }
}

/// Maps a [`TextPosition`] (a run's [`NodeId`] plus a code-point index within it)
/// back to a global code-point offset within `layout` — the inverse of
/// [`text_position`]. Returns `None` when the run id is not one of `layout`'s.
pub fn text_offset(layout: &[TextRunLayout], position: &TextPosition) -> Option<usize> {
    let mut offset = 0;
    for run in layout {
        if run.id == position.node {
            return Some(offset + position.character_index);
        }
        offset += run.chars;
    }
    None
}

/// Builds the [`TextSelection`] for a text node from its caret and selection.
/// A caret alone is a degenerate selection (anchor == focus); with a real
/// selection the caret marks the focus end, so its direction is recovered.
/// Returns `None` when there is neither a caret nor a selection.
pub fn text_selection(state: &TextState, layout: &[TextRunLayout]) -> Option<TextSelection> {
    let (anchor, focus) = match state.selection {
        Some((start, end)) if start != end => match state.caret {
            Some(caret) if caret == start => (end, start),
            _ => (start, end),
        },
        _ => {
            let caret = state.caret?;
            (caret, caret)
        }
    };
    Some(TextSelection {
        anchor: text_position(layout, anchor),
        focus: text_position(layout, focus),
    })
}

/// Rebuilds one text node from fresh [`TextState`] against its cache, returning
/// only the nodes that changed (the container when its selection or run set
/// changed, plus any run whose content differs), and updating the cache. An
/// empty result means nothing changed.
pub fn rebuild_text_node(
    cache: &mut TextNodeCache,
    parent_path: &str,
    state: &TextState,
    ids: &mut NodeIdMap,
) -> Vec<(NodeId, Node)> {
    let (runs, layout) = build_text_runs(parent_path, &state.text, ids);
    let mut parent = cache.parent.clone();
    let mut children = cache.element_children.clone();
    children.extend(runs.iter().map(|(id, _)| *id));
    parent.set_children(children);
    match text_selection(state, &layout) {
        Some(selection) => parent.set_text_selection(selection),
        None => parent.clear_text_selection(),
    }

    let mut changed = Vec::new();
    if parent != cache.parent {
        changed.push((cache.node_id, parent.clone()));
    }
    for (index, (id, node)) in runs.iter().enumerate() {
        let differs = match cache.runs.get(index) {
            Some((old_id, old_node)) => old_id != id || old_node != node,
            None => true,
        };
        if differs {
            changed.push((*id, node.clone()));
        }
    }

    cache.parent = parent;
    cache.runs = runs;
    cache.layout = layout;
    changed
}

/// Builds a full-tree [`TreeUpdate`] for one window. `nodes[0]` is the window
/// root; focus lands on the node marked focused, or the root if none is. Text
/// nodes gain synthesized [`Role::TextRun`] children, and `text_caches` is
/// refreshed to match (entries for vanished paths are dropped).
///
/// Panics if `nodes` is empty.
pub fn build_window_update(
    nodes: &[MirrorNode],
    ids: &mut NodeIdMap,
    text_caches: &mut HashMap<String, TextNodeCache>,
) -> TreeUpdate {
    let walked: HashSet<&str> = nodes.iter().map(|node| node.path.as_str()).collect();
    let root_id = ids.id_for(&nodes[0].path);
    let mut out = Vec::with_capacity(nodes.len());
    let mut focus = root_id;
    let mut live: HashSet<String> = HashSet::new();
    for node in nodes {
        let id = ids.id_for(&node.path);
        if node.focused {
            focus = id;
        }
        let built = build_node(node, id, ids, &walked);
        out.push((id, built.container));
        out.extend(built.runs);
        if let Some(cache) = built.cache {
            live.insert(node.path.clone());
            text_caches.insert(node.path.clone(), cache);
        }
    }
    text_caches.retain(|path, _| live.contains(path));
    TreeUpdate {
        nodes: out,
        tree: Some(Tree::new(root_id)),
        tree_id: TreeId::ROOT,
        focus,
    }
}

/// A focus-only delta: moves focus to `focus` without touching any node. The
/// target must already exist in the window's tree; `tree` is `None` because no
/// structural change accompanies it.
pub fn focus_update(focus: NodeId) -> TreeUpdate {
    TreeUpdate {
        nodes: Vec::new(),
        tree: None,
        tree_id: TreeId::ROOT,
        focus,
    }
}

/// A built node: the container plus any synthesized text-run children and, for
/// text nodes, the cache used to diff later text deltas.
struct BuiltNode {
    container: Node,
    runs: Vec<(NodeId, Node)>,
    cache: Option<TextNodeCache>,
}

fn build_node(
    node: &MirrorNode,
    id: NodeId,
    ids: &mut NodeIdMap,
    walked: &HashSet<&str>,
) -> BuiltNode {
    let role = map_role(node.role);
    let mut container = Node::new(role);
    if !node.name.is_empty() {
        if role == accesskit::Role::Label {
            container.set_value(node.name.clone());
        } else {
            container.set_label(node.name.clone());
        }
    }
    let element_children: Vec<NodeId> = node
        .children
        .iter()
        .filter(|path| walked.contains(path.as_str()))
        .map(|path| ids.id_for(path))
        .collect();
    if node.actionable && is_clickable_role(node.role) {
        container.add_action(accesskit::Action::Click);
    }
    if node.focusable {
        container.add_action(accesskit::Action::Focus);
    }
    match &node.text {
        Some(state) => {
            let (runs, layout) = build_text_runs(&node.path, &state.text, ids);
            let mut children = element_children.clone();
            children.extend(runs.iter().map(|(rid, _)| *rid));
            if !children.is_empty() {
                container.set_children(children);
            }
            if let Some(selection) = text_selection(state, &layout) {
                container.set_text_selection(selection);
            }
            let cache = TextNodeCache {
                node_id: id,
                parent: container.clone(),
                element_children,
                runs: runs.clone(),
                layout,
            };
            BuiltNode { container, runs, cache: Some(cache) }
        }
        None => {
            if !element_children.is_empty() {
                container.set_children(element_children);
            }
            BuiltNode { container, runs: Vec::new(), cache: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(path: &str, role: Role, name: &str) -> MirrorNode {
        MirrorNode {
            path: path.to_owned(),
            role,
            name: name.to_owned(),
            focusable: false,
            focused: false,
            actionable: false,
            children: Vec::new(),
            text: None,
        }
    }

    fn build(nodes: &[MirrorNode], ids: &mut NodeIdMap) -> TreeUpdate {
        build_window_update(nodes, ids, &mut HashMap::new())
    }

    #[test]
    fn node_id_map_is_stable_and_sequential() {
        let mut ids = NodeIdMap::new();
        let a = ids.id_for("/a");
        let b = ids.id_for("/b");
        assert_eq!(a, NodeId(0));
        assert_eq!(b, NodeId(1));
        assert_eq!(ids.id_for("/a"), a, "same path reuses its id");
        assert_eq!(ids.get("/b"), Some(b));
        assert_eq!(ids.get("/missing"), None);
    }

    #[test]
    fn map_role_covers_common_roles_with_container_fallback() {
        assert_eq!(map_role(Role::Frame), accesskit::Role::Window);
        assert_eq!(map_role(Role::Label), accesskit::Role::Label);
        assert_eq!(map_role(Role::Button), accesskit::Role::Button);
        assert_eq!(map_role(Role::PageTab), accesskit::Role::Tab);
        assert_eq!(map_role(Role::Separator), accesskit::Role::GenericContainer);
    }

    #[test]
    fn builds_window_tree_with_children_and_actions() {
        let mut root = leaf("/win", Role::Frame, "Editor");
        root.children = vec!["/label".into(), "/button".into()];
        let label = leaf("/label", Role::Label, "hello");
        let mut button = leaf("/button", Role::Button, "Click me");
        button.actionable = true;
        button.focusable = true;

        let mut ids = NodeIdMap::new();
        let update = build(&[root, label, button], &mut ids);

        assert_eq!(update.nodes.len(), 3);
        let root_id = ids.get("/win").unwrap();
        assert_eq!(update.tree.as_ref().unwrap().root, root_id);
        assert_eq!(update.focus, root_id, "no focused node falls back to root");

        let button_id = ids.get("/button").unwrap();
        let (_, button_node) = update
            .nodes
            .iter()
            .find(|(id, _)| *id == button_id)
            .unwrap();
        assert!(button_node.supports_action(accesskit::Action::Click));
        assert!(button_node.supports_action(accesskit::Action::Focus));
    }

    #[test]
    fn click_is_gated_to_clickable_roles() {
        let mut panel = leaf("/panel", Role::Panel, "");
        panel.actionable = true;
        let mut button = leaf("/button", Role::Button, "Go");
        button.actionable = true;

        let mut ids = NodeIdMap::new();
        let update = build(&[panel, button], &mut ids);
        let panel_node = &update.nodes[0].1;
        let button_node = &update.nodes[1].1;
        assert!(
            !panel_node.supports_action(accesskit::Action::Click),
            "a container that merely implements the Action interface is not clickable"
        );
        assert!(button_node.supports_action(accesskit::Action::Click));
    }

    #[test]
    fn label_text_lands_in_value_not_label() {
        let label = leaf("/l", Role::Label, "status text");
        let mut ids = NodeIdMap::new();
        let update = build(&[label], &mut ids);
        let (_, node) = &update.nodes[0];
        assert_eq!(node.value(), Some("status text"));
        assert_eq!(node.label(), None);
    }

    #[test]
    fn focus_points_at_the_focused_node() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/button".into()];
        let mut button = leaf("/button", Role::Button, "b");
        button.focused = true;
        let mut ids = NodeIdMap::new();
        let update = build(&[root, button], &mut ids);
        assert_eq!(update.focus, ids.get("/button").unwrap());
    }

    #[test]
    fn child_refs_to_unwalked_paths_are_dropped() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/present".into(), "/cutoff".into()];
        let present = leaf("/present", Role::Button, "here");

        let mut ids = NodeIdMap::new();
        let update = build(&[root, present], &mut ids);

        assert_eq!(ids.get("/cutoff"), None, "no id allocated for an unwalked path");
        let root_id = ids.get("/win").unwrap();
        let present_id = ids.get("/present").unwrap();
        let (_, root_node) = update.nodes.iter().find(|(id, _)| *id == root_id).unwrap();
        assert_eq!(root_node.children().to_vec(), vec![present_id]);

        let present_ids: HashSet<NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();
        for (_, node) in &update.nodes {
            for child in node.children() {
                assert!(present_ids.contains(child), "child {child:?} has no node");
            }
        }
    }

    // --- Text / caret synthesis ---

    fn text_node(path: &str, state: TextState) -> MirrorNode {
        let mut node = leaf(path, Role::Text, "");
        node.text = Some(state);
        node
    }

    fn char_lengths(run: &Node) -> Vec<u8> {
        run.character_lengths().to_vec()
    }

    #[test]
    fn split_runs_keeps_line_breaks_and_trails_empty() {
        assert_eq!(split_runs(""), vec![""]);
        assert_eq!(split_runs("a"), vec!["a"]);
        assert_eq!(split_runs("a\n"), vec!["a\n", ""]);
        assert_eq!(split_runs("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_runs("a\n\nb"), vec!["a\n", "\n", "b"]);
    }

    #[test]
    fn character_lengths_are_per_code_point_utf8_bytes() {
        assert_eq!(character_lengths("ab\n"), vec![1, 1, 1]);
        assert_eq!(character_lengths("é"), vec![2]);
        assert_eq!(character_lengths("\u{a0}"), vec![2]);
        assert_eq!(character_lengths("\u{1f44d}"), vec![4]);
        for s in ["", "ab", "héllo\n", "\u{1f44d}x"] {
            let sum: usize = character_lengths(s).iter().map(|&n| n as usize).sum();
            assert_eq!(sum, s.len(), "lengths of {s:?} must sum to byte length");
        }
    }

    #[test]
    fn build_text_runs_concatenate_to_input_with_stable_ids() {
        let mut ids = NodeIdMap::new();
        let (runs, layout) = build_text_runs("/doc", "hi\nyo", &mut ids);
        assert_eq!(runs.len(), 2);
        let joined: String = runs.iter().map(|(_, n)| n.value().unwrap()).collect();
        assert_eq!(joined, "hi\nyo");
        for (_, node) in &runs {
            assert_eq!(node.role(), accesskit::Role::TextRun);
        }
        assert_eq!(char_lengths(&runs[0].1), vec![1, 1, 1]);
        assert_eq!(char_lengths(&runs[1].1), vec![1, 1]);
        assert_eq!(layout.iter().map(|r| r.chars).collect::<Vec<_>>(), vec![3, 2]);

        let (runs2, _) = build_text_runs("/doc", "hi\nyo", &mut ids);
        assert_eq!(
            runs.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            runs2.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "run ids are stable across rebuilds"
        );
    }

    #[test]
    fn empty_text_yields_one_empty_run() {
        let mut ids = NodeIdMap::new();
        let (runs, layout) = build_text_runs("/doc", "", &mut ids);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1.value(), Some(""));
        assert_eq!(layout[0].chars, 0);
    }

    #[test]
    fn text_position_maps_offsets_across_runs_and_clamps() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "ab\ncd", &mut ids);
        let (r0, r1) = (layout[0].id, layout[1].id);
        assert_eq!(text_position(&layout, 0), TextPosition { node: r0, character_index: 0 });
        // Offset 2 lands ON the line break inside run 0.
        assert_eq!(text_position(&layout, 2), TextPosition { node: r0, character_index: 2 });
        // Offset 3 is the start of the next line.
        assert_eq!(text_position(&layout, 3), TextPosition { node: r1, character_index: 0 });
        // End of document clamps to the last run's end.
        assert_eq!(text_position(&layout, 5), TextPosition { node: r1, character_index: 2 });
        assert_eq!(text_position(&layout, 99), TextPosition { node: r1, character_index: 2 });
    }

    #[test]
    fn text_offset_is_the_inverse_of_text_position() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "ab\ncd", &mut ids);
        let total: usize = layout.iter().map(|r| r.chars).sum();
        // Round-trips at the start, an interior offset, a run boundary, and the
        // end-of-text boundary (where off-by-one bugs live).
        for k in [0usize, 1, 3, total] {
            assert_eq!(
                text_offset(&layout, &text_position(&layout, k)),
                Some(k),
                "round-trip at offset {k}"
            );
        }
        // A run id absent from the layout resolves to nothing.
        let bogus = TextPosition { node: NodeId(9999), character_index: 0 };
        assert_eq!(text_offset(&layout, &bogus), None);
    }

    #[test]
    fn text_position_counts_code_points_not_bytes() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "é\nb", &mut ids);
        // Offset 1 is the '\n' after the 2-byte 'é', still index 1 (code points).
        assert_eq!(text_position(&layout, 1), TextPosition { node: layout[0].id, character_index: 1 });
        assert_eq!(text_position(&layout, 2), TextPosition { node: layout[1].id, character_index: 0 });
    }

    #[test]
    fn text_selection_direction_and_degenerate_cases() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "abcdef", &mut ids);
        let run = layout[0].id;
        let pos = |i| TextPosition { node: run, character_index: i };

        // Caret only: degenerate selection.
        let caret = TextState { text: "abcdef".into(), caret: Some(3), selection: None };
        assert_eq!(text_selection(&caret, &layout), Some(TextSelection { anchor: pos(3), focus: pos(3) }));

        // Forward selection: caret at the end.
        let fwd = TextState { text: "abcdef".into(), caret: Some(4), selection: Some((1, 4)) };
        assert_eq!(text_selection(&fwd, &layout), Some(TextSelection { anchor: pos(1), focus: pos(4) }));

        // Backward selection: caret at the start.
        let back = TextState { text: "abcdef".into(), caret: Some(1), selection: Some((1, 4)) };
        assert_eq!(text_selection(&back, &layout), Some(TextSelection { anchor: pos(4), focus: pos(1) }));

        // Neither caret nor selection: nothing.
        let none = TextState { text: "abcdef".into(), caret: None, selection: None };
        assert_eq!(text_selection(&none, &layout), None);
    }

    #[test]
    fn word_starts_mark_word_boundaries_and_cap_at_u8() {
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/doc", "foo bar\n", &mut ids);
        assert_eq!(runs[0].1.word_starts().to_vec(), vec![0u8, 4]);

        // A word start past 255 collapses word info to empty.
        let long = format!("{} b", "a".repeat(300));
        let (long_runs, _) = build_text_runs("/doc2", &long, &mut ids);
        assert!(long_runs[0].1.word_starts().is_empty());
    }

    #[test]
    fn build_window_update_appends_runs_and_populates_cache() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/doc".into()];
        let doc = text_node("/doc", TextState { text: "hi\n".into(), caret: Some(3), selection: None });

        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        let update = build_window_update(&[root, doc], &mut ids, &mut caches);

        let doc_id = ids.get("/doc").unwrap();
        let (_, doc_node) = update.nodes.iter().find(|(id, _)| *id == doc_id).unwrap();
        // Two runs for "hi\n": ["hi\n", ""].
        assert_eq!(doc_node.children().len(), 2);
        assert!(doc_node.text_selection().is_some());
        for child in doc_node.children() {
            let (_, n) = update.nodes.iter().find(|(id, _)| id == child).unwrap();
            assert_eq!(n.role(), accesskit::Role::TextRun);
        }
        assert!(caches.contains_key("/doc"), "text cache populated");

        // Vanished text paths are pruned from the cache on the next build.
        let root2 = leaf("/win", Role::Frame, "w");
        build_window_update(&[root2], &mut ids, &mut caches);
        assert!(!caches.contains_key("/doc"), "cache pruned when node absent");
    }

    #[test]
    fn no_duplicate_child_ids_across_text_nodes() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into(), "/b".into()];
        let a = text_node("/a", TextState { text: "x\ny".into(), caret: None, selection: None });
        let b = text_node("/b", TextState { text: "z".into(), caret: None, selection: None });

        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root, a, b], &mut ids, &mut HashMap::new());
        let mut seen = HashSet::new();
        for (id, _) in &update.nodes {
            assert!(seen.insert(*id), "duplicate node id {id:?} in update");
        }
    }

    #[test]
    fn rebuild_text_node_emits_minimal_deltas() {
        let doc = text_node("/doc", TextState { text: "one\ntwo".into(), caret: Some(0), selection: None });
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[doc], &mut ids, &mut caches);
        let cache = caches.get_mut("/doc").unwrap();

        // Caret-only move: exactly the container changes.
        let moved = TextState { text: "one\ntwo".into(), caret: Some(5), selection: None };
        let delta = rebuild_text_node(cache, "/doc", &moved, &mut ids);
        assert_eq!(delta.len(), 1, "caret move is a single-node delta");
        assert_eq!(delta[0].0, cache.node_id);

        // Identical state: nothing changes.
        assert!(rebuild_text_node(cache, "/doc", &moved, &mut ids).is_empty());

        // Editing the second line: container (its selection tracks) + that run.
        let edited = TextState { text: "one\nTWO".into(), caret: Some(7), selection: None };
        let ids_before = (cache.runs[0].0, cache.runs[1].0);
        let delta = rebuild_text_node(cache, "/doc", &edited, &mut ids);
        let changed_ids: HashSet<NodeId> = delta.iter().map(|(id, _)| *id).collect();
        assert!(changed_ids.contains(&cache.node_id));
        assert!(changed_ids.contains(&ids_before.1), "edited run 1 is included");
        assert!(!changed_ids.contains(&ids_before.0), "unchanged run 0 is not");
    }

    #[test]
    fn rebuild_text_node_shrinks_run_children() {
        let doc = text_node("/doc", TextState { text: "a\nb\nc".into(), caret: None, selection: None });
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[doc], &mut ids, &mut caches);
        let cache = caches.get_mut("/doc").unwrap();
        let node_id = cache.node_id;

        let shorter = TextState { text: "a".into(), caret: None, selection: None };
        let delta = rebuild_text_node(cache, "/doc", &shorter, &mut ids);
        let (_, parent) = delta.iter().find(|(id, _)| *id == node_id).unwrap();
        assert_eq!(parent.children().len(), 1, "container children shrink to one run");
    }

    #[test]
    fn clamp_text_truncates_at_code_point_boundary() {
        assert_eq!(clamp_text("short"), "short");
        let long = "a".repeat(MAX_TEXT_CHARS + 10);
        assert_eq!(clamp_text(&long).chars().count(), MAX_TEXT_CHARS);
    }
}
