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

/// Caps per-character geometry reads, in Unicode scalar values per node.
/// Reading extents costs one bus round trip per character; nodes above this
/// cap carry no geometry at all.
pub const MAX_GEOMETRY_CHARS: usize = 512;

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
    /// Per-code-point window-relative extents, parallel to `text`'s code
    /// points; `None` when geometry was unavailable or not read.
    pub extents: Option<Vec<CharExtent>>,
}

/// One code point's window-relative extent as AT-SPI reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharExtent {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
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
    /// Whether the node's role has a caret, so a later refresh re-reads the
    /// caret and selection instead of leaving them cleared.
    pub caret_enabled: bool,
    /// The extents behind the runs' current geometry, reused by refreshes that
    /// read no geometry (caret/selection moves) while the text is unchanged.
    pub extents: Option<Vec<CharExtent>>,
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
        Role::Terminal => A::Terminal,
        Role::DocumentFrame
        | Role::DocumentText
        | Role::DocumentWeb
        | Role::DocumentEmail
        | Role::DocumentSpreadsheet
        | Role::DocumentPresentation => A::Document,
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

/// Whether a role is an editable text field.
fn is_editable_text_role(role: Role) -> bool {
    matches!(role, Role::Text | Role::Entry | Role::PasswordText)
}

/// Whether a role is static text: a label, terminal, document, or paragraph.
fn is_static_text_role(role: Role) -> bool {
    matches!(
        role,
        Role::Label
            | Role::Terminal
            | Role::Paragraph
            | Role::DocumentFrame
            | Role::DocumentText
            | Role::DocumentWeb
            | Role::DocumentEmail
            | Role::DocumentSpreadsheet
            | Role::DocumentPresentation
    )
}

/// Whether a node with this role and child structure has its AT-SPI `Text`
/// interface mirrored into synthesized [`Role::TextRun`] children. Editable
/// roles always qualify; static text roles qualify only when they have no
/// element children.
pub fn reads_text_runs(role: Role, has_element_children: bool) -> bool {
    is_editable_text_role(role) || (is_static_text_role(role) && !has_element_children)
}

/// Whether a text role has a user-movable caret whose position and selection are
/// mirrored. Editable fields and terminals do; static labels and documents do
/// not, so their runs carry no [`TextSelection`].
pub fn has_text_caret(role: Role) -> bool {
    is_editable_text_role(role) || role == Role::Terminal
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

/// Replaces unreported (all-zero) extents: a zero extent takes its
/// predecessor's right edge (zero width, the predecessor's y and height);
/// leading zeros take the first real extent's left edge. `None` when the node
/// has no real extent at all.
fn synthesize_extents(extents: &[CharExtent]) -> Option<Vec<CharExtent>> {
    fn is_unreported(extent: &CharExtent) -> bool {
        extent.x == 0 && extent.y == 0 && extent.width == 0 && extent.height == 0
    }
    let first_real = extents.iter().position(|e| !is_unreported(e))?;
    let mut out: Vec<CharExtent> = Vec::with_capacity(extents.len());
    for (index, extent) in extents.iter().enumerate() {
        if !is_unreported(extent) {
            out.push(*extent);
        } else if index < first_real {
            let donor = extents[first_real];
            out.push(CharExtent { x: donor.x, y: donor.y, width: 0, height: donor.height });
        } else {
            let prev = out[index - 1];
            out.push(CharExtent {
                x: prev.x + prev.width,
                y: prev.y,
                width: 0,
                height: prev.height,
            });
        }
    }
    Some(out)
}

/// A run's bounds plus run-relative per-char positions and advance widths,
/// from its slice of the node's synthesized extents. An empty run (the
/// trailing caret run) takes a zero-width rect at `prev_last`'s right edge.
fn run_geometry(
    chars: &[CharExtent],
    prev_last: Option<CharExtent>,
) -> Option<(accesskit::Rect, Vec<f32>, Vec<f32>)> {
    if chars.is_empty() {
        let prev = prev_last?;
        let edge = (prev.x + prev.width) as f64;
        let rect = accesskit::Rect {
            x0: edge,
            y0: prev.y as f64,
            x1: edge,
            y1: (prev.y + prev.height) as f64,
        };
        return Some((rect, Vec::new(), Vec::new()));
    }
    let mut x0 = i32::MAX;
    let mut y0 = i32::MAX;
    let mut x1 = i32::MIN;
    let mut y1 = i32::MIN;
    for extent in chars {
        x0 = x0.min(extent.x);
        y0 = y0.min(extent.y);
        x1 = x1.max(extent.x + extent.width);
        y1 = y1.max(extent.y + extent.height);
    }
    let positions = chars.iter().map(|e| (e.x - x0) as f32).collect();
    let widths = chars.iter().map(|e| e.width as f32).collect();
    let rect = accesskit::Rect {
        x0: x0 as f64,
        y0: y0 as f64,
        x1: x1 as f64,
        y1: y1 as f64,
    };
    Some((rect, positions, widths))
}

/// Builds the [`Role::TextRun`] child nodes for `text` and their layout. Each
/// run carries its value, per-code-point `character_lengths`, and word starts.
/// When `extents` is provided, each run additionally carries its bounds,
/// run-relative character positions and widths, and text direction, derived
/// from the node's synthesized extents.
pub fn build_text_runs(
    parent_path: &str,
    text: &str,
    extents: Option<&[CharExtent]>,
    ids: &mut NodeIdMap,
) -> (Vec<(NodeId, Node)>, Vec<TextRunLayout>) {
    let runs = split_runs(text);
    let mut nodes = Vec::with_capacity(runs.len());
    let mut layout = Vec::with_capacity(runs.len());
    let synthesized = extents
        .filter(|all| all.len() == text.chars().count())
        .and_then(synthesize_extents);
    let mut cursor = 0usize;
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
        if let Some(all) = synthesized.as_deref() {
            let prev_last = if cursor > 0 { all.get(cursor - 1).copied() } else { None };
            let slice = &all[cursor..cursor + chars];
            if let Some((rect, positions, widths)) = run_geometry(slice, prev_last) {
                node.set_bounds(rect);
                node.set_character_positions(positions);
                node.set_character_widths(widths);
                node.set_text_direction(accesskit::TextDirection::LeftToRight);
            }
        }
        cursor += chars;
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
    // Fresh extents win; a refresh that read none (caret/selection move)
    // reuses the cached ones as long as the text length still matches.
    let effective: Option<Vec<CharExtent>> = match &state.extents {
        Some(fresh) => Some(fresh.clone()),
        None => cache
            .extents
            .as_ref()
            .filter(|cached| cached.len() == state.text.chars().count())
            .cloned(),
    };
    let (runs, layout) = build_text_runs(parent_path, &state.text, effective.as_deref(), ids);
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
    cache.extents = effective;
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

/// The element children each walked node contributes to the emitted tree —
/// [`MirrorNode::children`] filtered to the walked set, the same filter
/// `build_node` applies. Keyed by AT-SPI path.
pub fn emitted_children(nodes: &[MirrorNode]) -> HashMap<String, Vec<String>> {
    let walked: HashSet<&str> = nodes.iter().map(|node| node.path.as_str()).collect();
    nodes
        .iter()
        .map(|node| {
            let children = node
                .children
                .iter()
                .filter(|child| walked.contains(child.as_str()))
                .cloned()
                .collect();
            (node.path.clone(), children)
        })
        .collect()
}

/// A spliced chain turned into a partial update plus the bookkeeping the
/// caller folds into its per-window children map.
pub struct SpliceResult {
    pub update: TreeUpdate,
    /// The element children each chain node was emitted with, by path.
    pub children: Vec<(String, Vec<String>)>,
}

/// Splices a freshly read ancestor chain into an existing window tree.
/// `chain[0]` is the already-known ancestor's fresh read, each following node
/// a child of its predecessor, the last the new focus target. The ancestor is
/// emitted with `ancestor_children` (the client tree's current children) plus
/// the chain child appended; interior nodes keep only children in `known` or
/// the chain, so a lazy grid's huge fresh child list can neither bloat nor
/// orphan the client tree. Returns a partial update (`tree: None`) whose
/// focus is the descendant, or `None` for a chain shorter than two nodes.
pub fn splice_chain_update(
    chain: &[MirrorNode],
    ancestor_children: &[String],
    known: &HashSet<String>,
    ids: &mut NodeIdMap,
    text_caches: &mut HashMap<String, TextNodeCache>,
) -> Option<SpliceResult> {
    if chain.len() < 2 {
        return None;
    }
    let chain_paths: HashSet<&str> = chain.iter().map(|node| node.path.as_str()).collect();
    let mut per_node_children: Vec<Vec<String>> = Vec::with_capacity(chain.len());
    for (index, node) in chain.iter().enumerate() {
        let mut children: Vec<String> = if index == 0 {
            ancestor_children.to_vec()
        } else {
            node.children
                .iter()
                .filter(|child| known.contains(*child) || chain_paths.contains(child.as_str()))
                .cloned()
                .collect()
        };
        if let Some(next) = chain.get(index + 1) {
            if !children.contains(&next.path) {
                children.push(next.path.clone());
            }
        }
        per_node_children.push(children);
    }
    let mut spliced: Vec<MirrorNode> = chain.to_vec();
    for (node, children) in spliced.iter_mut().zip(&per_node_children) {
        node.children = children.clone();
    }
    let mut walked: HashSet<&str> = known.iter().map(String::as_str).collect();
    walked.extend(spliced.iter().map(|node| node.path.as_str()));
    walked.extend(ancestor_children.iter().map(String::as_str));
    let mut nodes_out = Vec::new();
    let mut focus = None;
    for node in &spliced {
        let id = ids.id_for(&node.path);
        let built = build_node(node, id, ids, &walked);
        nodes_out.push((id, built.container));
        nodes_out.extend(built.runs);
        if let Some(cache) = built.cache {
            text_caches.insert(node.path.clone(), cache);
        }
        focus = Some(id);
    }
    let update = TreeUpdate {
        nodes: nodes_out,
        tree: None,
        tree_id: TreeId::ROOT,
        focus: focus?,
    };
    let children = spliced
        .iter()
        .map(|node| node.path.clone())
        .zip(per_node_children)
        .collect();
    Some(SpliceResult { update, children })
}

/// Merges a splice delta into a full-tree update: same-id nodes are replaced,
/// new ones appended, and the splice's focus wins. The full update's `tree`
/// is untouched.
pub fn merge_update(full: &mut TreeUpdate, splice: TreeUpdate) {
    for (id, node) in splice.nodes {
        match full.nodes.iter_mut().find(|(existing, _)| *existing == id) {
            Some(slot) => slot.1 = node,
            None => full.nodes.push((id, node)),
        }
    }
    full.focus = splice.focus;
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
            let (runs, layout) =
                build_text_runs(&node.path, &state.text, state.extents.as_deref(), ids);
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
                caret_enabled: has_text_caret(node.role),
                extents: state.extents.clone(),
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

    struct NoOpChanges;

    impl accesskit_consumer::TreeChangeHandler for NoOpChanges {
        fn node_added(&mut self, _: &accesskit_consumer::Node) {}
        fn node_updated(&mut self, _: &accesskit_consumer::Node, _: &accesskit_consumer::Node) {}
        fn focus_moved(
            &mut self,
            _: Option<&accesskit_consumer::Node>,
            _: Option<&accesskit_consumer::Node>,
        ) {}
        fn node_removed(&mut self, _: &accesskit_consumer::Node) {}
    }

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
    fn map_role_covers_terminal_and_document() {
        assert_eq!(map_role(Role::Terminal), accesskit::Role::Terminal);
        for role in [
            Role::DocumentFrame,
            Role::DocumentText,
            Role::DocumentWeb,
            Role::DocumentEmail,
            Role::DocumentSpreadsheet,
            Role::DocumentPresentation,
        ] {
            assert_eq!(map_role(role), accesskit::Role::Document, "{role:?}");
        }
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

    #[test]
    fn reads_text_runs_editable_always_static_only_as_leaf() {
        // Editable text roles get runs regardless of child structure.
        for role in [Role::Text, Role::Entry, Role::PasswordText] {
            assert!(reads_text_runs(role, false), "{role:?} leaf");
            assert!(reads_text_runs(role, true), "{role:?} with children");
        }
        // Static text roles get runs only as leaves; a structured document
        // keeps its element children instead of also emitting the whole text flat.
        for role in [
            Role::Label,
            Role::Terminal,
            Role::DocumentText,
            Role::DocumentWeb,
        ] {
            assert!(reads_text_runs(role, false), "{role:?} leaf");
            assert!(!reads_text_runs(role, true), "{role:?} with children");
        }
        // Non-text roles never get runs.
        for role in [Role::Button, Role::Panel, Role::Frame] {
            assert!(!reads_text_runs(role, false), "{role:?}");
        }
    }

    #[test]
    fn paragraph_reads_runs_as_leaf_and_stays_caret_less() {
        assert!(reads_text_runs(Role::Paragraph, false));
        assert!(!reads_text_runs(Role::Paragraph, true));
        assert!(!has_text_caret(Role::Paragraph));
    }

    #[test]
    fn consumer_reads_document_text_through_paragraph_runs() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/doc".into()];
        let mut doc = leaf("/doc", Role::DocumentText, "");
        doc.children = vec!["/doc/p1".into(), "/doc/p2".into()];
        let mut p1 = leaf("/doc/p1", Role::Paragraph, "");
        p1.text = Some(TextState {
            text: "One.".into(),
            caret: None,
            selection: None,
            extents: None,
        });
        let mut p2 = leaf("/doc/p2", Role::Paragraph, "");
        p2.text = Some(TextState {
            text: "Two.".into(),
            caret: None,
            selection: None,
            extents: None,
        });

        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root, doc, p1, p2], &mut ids, &mut HashMap::new());
        let doc_id = ids.get("/doc").unwrap();

        let tree = accesskit_consumer::Tree::new(update, false);
        let state = tree.state();
        let doc_node = state
            .node_by_tree_local_id(doc_id, accesskit::TreeId::ROOT)
            .expect("document present in consumer tree");
        assert!(doc_node.supports_text_ranges());
        assert_eq!(doc_node.document_range().text(), "One.Two.");
    }

    #[test]
    fn has_text_caret_only_for_editable_and_terminal() {
        for role in [Role::Text, Role::Entry, Role::PasswordText, Role::Terminal] {
            assert!(has_text_caret(role), "{role:?}");
        }
        for role in [Role::Label, Role::DocumentText, Role::DocumentWeb, Role::Button] {
            assert!(!has_text_caret(role), "{role:?}");
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
        let (runs, layout) = build_text_runs("/doc", "hi\nyo", None, &mut ids);
        assert_eq!(runs.len(), 2);
        let joined: String = runs.iter().map(|(_, n)| n.value().unwrap()).collect();
        assert_eq!(joined, "hi\nyo");
        for (_, node) in &runs {
            assert_eq!(node.role(), accesskit::Role::TextRun);
        }
        assert_eq!(char_lengths(&runs[0].1), vec![1, 1, 1]);
        assert_eq!(char_lengths(&runs[1].1), vec![1, 1]);
        assert_eq!(layout.iter().map(|r| r.chars).collect::<Vec<_>>(), vec![3, 2]);

        let (runs2, _) = build_text_runs("/doc", "hi\nyo", None, &mut ids);
        assert_eq!(
            runs.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            runs2.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "run ids are stable across rebuilds"
        );
    }

    #[test]
    fn empty_text_yields_one_empty_run() {
        let mut ids = NodeIdMap::new();
        let (runs, layout) = build_text_runs("/doc", "", None, &mut ids);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1.value(), Some(""));
        assert_eq!(layout[0].chars, 0);
    }

    #[test]
    fn text_position_maps_offsets_across_runs_and_clamps() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "ab\ncd", None, &mut ids);
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
        let (_, layout) = build_text_runs("/doc", "ab\ncd", None, &mut ids);
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
        let (_, layout) = build_text_runs("/doc", "é\nb", None, &mut ids);
        // Offset 1 is the '\n' after the 2-byte 'é', still index 1 (code points).
        assert_eq!(text_position(&layout, 1), TextPosition { node: layout[0].id, character_index: 1 });
        assert_eq!(text_position(&layout, 2), TextPosition { node: layout[1].id, character_index: 0 });
    }

    #[test]
    fn text_selection_direction_and_degenerate_cases() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "abcdef", None, &mut ids);
        let run = layout[0].id;
        let pos = |i| TextPosition { node: run, character_index: i };

        // Caret only: degenerate selection.
        let caret = TextState { text: "abcdef".into(), caret: Some(3), selection: None, extents: None };
        assert_eq!(text_selection(&caret, &layout), Some(TextSelection { anchor: pos(3), focus: pos(3) }));

        // Forward selection: caret at the end.
        let fwd = TextState { text: "abcdef".into(), caret: Some(4), selection: Some((1, 4)), extents: None };
        assert_eq!(text_selection(&fwd, &layout), Some(TextSelection { anchor: pos(1), focus: pos(4) }));

        // Backward selection: caret at the start.
        let back = TextState { text: "abcdef".into(), caret: Some(1), selection: Some((1, 4)), extents: None };
        assert_eq!(text_selection(&back, &layout), Some(TextSelection { anchor: pos(4), focus: pos(1) }));

        // Neither caret nor selection: nothing.
        let none = TextState { text: "abcdef".into(), caret: None, selection: None, extents: None };
        assert_eq!(text_selection(&none, &layout), None);
    }

    #[test]
    fn word_starts_mark_word_boundaries_and_cap_at_u8() {
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/doc", "foo bar\n", None, &mut ids);
        assert_eq!(runs[0].1.word_starts().to_vec(), vec![0u8, 4]);

        // A word start past 255 collapses word info to empty.
        let long = format!("{} b", "a".repeat(300));
        let (long_runs, _) = build_text_runs("/doc2", &long, None, &mut ids);
        assert!(long_runs[0].1.word_starts().is_empty());
    }

    #[test]
    fn build_window_update_appends_runs_and_populates_cache() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/doc".into()];
        let doc = text_node("/doc", TextState { text: "hi\n".into(), caret: Some(3), selection: None, extents: None });

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
    fn text_cache_records_caret_enabled_per_role() {
        let mut editable = leaf("/e", Role::Text, "");
        editable.text = Some(TextState { text: "x".into(), caret: Some(1), selection: None, extents: None });
        let mut label = leaf("/l", Role::Label, "hi");
        label.text = Some(TextState { text: "hi".into(), caret: None, selection: None, extents: None });

        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[editable, label], &mut ids, &mut caches);

        assert!(caches.get("/e").unwrap().caret_enabled, "editable text has a caret");
        assert!(!caches.get("/l").unwrap().caret_enabled, "static label has none");
    }

    #[test]
    fn static_label_with_text_keeps_value_and_gains_runs() {
        let mut label = leaf("/l", Role::Label, "Status: OK");
        label.text = Some(TextState { text: "Status: OK".into(), caret: None, selection: None, extents: None });

        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[label], &mut ids, &mut HashMap::new());
        let (_, node) = &update.nodes[0];

        assert_eq!(node.value(), Some("Status: OK"), "label keeps its name in value");
        assert_eq!(node.label(), None);
        assert_eq!(node.children().len(), 1, "one run for single-line text");
        let run = node.children()[0];
        let (_, run_node) = update.nodes.iter().find(|(id, _)| *id == run).unwrap();
        assert_eq!(run_node.role(), accesskit::Role::TextRun);
        assert_eq!(run_node.value(), Some("Status: OK"));
    }

    #[test]
    fn no_duplicate_child_ids_across_text_nodes() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into(), "/b".into()];
        let a = text_node("/a", TextState { text: "x\ny".into(), caret: None, selection: None, extents: None });
        let b = text_node("/b", TextState { text: "z".into(), caret: None, selection: None, extents: None });

        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root, a, b], &mut ids, &mut HashMap::new());
        let mut seen = HashSet::new();
        for (id, _) in &update.nodes {
            assert!(seen.insert(*id), "duplicate node id {id:?} in update");
        }
    }

    #[test]
    fn rebuild_text_node_emits_minimal_deltas() {
        let doc = text_node("/doc", TextState { text: "one\ntwo".into(), caret: Some(0), selection: None, extents: None });
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[doc], &mut ids, &mut caches);
        let cache = caches.get_mut("/doc").unwrap();

        // Caret-only move: exactly the container changes.
        let moved = TextState { text: "one\ntwo".into(), caret: Some(5), selection: None, extents: None };
        let delta = rebuild_text_node(cache, "/doc", &moved, &mut ids);
        assert_eq!(delta.len(), 1, "caret move is a single-node delta");
        assert_eq!(delta[0].0, cache.node_id);

        // Identical state: nothing changes.
        assert!(rebuild_text_node(cache, "/doc", &moved, &mut ids).is_empty());

        // Editing the second line: container (its selection tracks) + that run.
        let edited = TextState { text: "one\nTWO".into(), caret: Some(7), selection: None, extents: None };
        let ids_before = (cache.runs[0].0, cache.runs[1].0);
        let delta = rebuild_text_node(cache, "/doc", &edited, &mut ids);
        let changed_ids: HashSet<NodeId> = delta.iter().map(|(id, _)| *id).collect();
        assert!(changed_ids.contains(&cache.node_id));
        assert!(changed_ids.contains(&ids_before.1), "edited run 1 is included");
        assert!(!changed_ids.contains(&ids_before.0), "unchanged run 0 is not");
    }

    #[test]
    fn rebuild_text_node_shrinks_run_children() {
        let doc = text_node("/doc", TextState { text: "a\nb\nc".into(), caret: None, selection: None, extents: None });
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[doc], &mut ids, &mut caches);
        let cache = caches.get_mut("/doc").unwrap();
        let node_id = cache.node_id;

        let shorter = TextState { text: "a".into(), caret: None, selection: None, extents: None };
        let delta = rebuild_text_node(cache, "/doc", &shorter, &mut ids);
        let (_, parent) = delta.iter().find(|(id, _)| *id == node_id).unwrap();
        assert_eq!(parent.children().len(), 1, "container children shrink to one run");
    }

    #[test]
    fn caret_move_reuses_cached_extents_and_stays_a_minimal_delta() {
        let doc = text_node(
            "/doc",
            TextState {
                text: "hi".into(),
                caret: Some(0),
                selection: None,
                extents: Some(vec![ext(10, 0, 8, 16), ext(18, 0, 8, 16)]),
            },
        );
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[doc], &mut ids, &mut caches);
        let cache = caches.get_mut("/doc").unwrap();

        // A caret move re-reads no geometry; the cached extents keep the runs
        // identical, so only the container changes.
        let moved = TextState { text: "hi".into(), caret: Some(1), selection: None, extents: None };
        let delta = rebuild_text_node(cache, "/doc", &moved, &mut ids);
        assert_eq!(delta.len(), 1, "caret move stays a container-only delta");
        assert_eq!(delta[0].0, cache.node_id);
        assert!(
            cache.runs[0].1.bounds().is_some(),
            "cached run keeps its geometry across the caret move"
        );
    }

    #[test]
    fn text_change_with_fresh_extents_updates_run_geometry() {
        let doc = text_node(
            "/doc",
            TextState {
                text: "hi".into(),
                caret: Some(0),
                selection: None,
                extents: Some(vec![ext(10, 0, 8, 16), ext(18, 0, 8, 16)]),
            },
        );
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[doc], &mut ids, &mut caches);
        let cache = caches.get_mut("/doc").unwrap();

        let fresh = vec![ext(10, 0, 8, 16), ext(18, 0, 9, 16)];
        let edited = TextState {
            text: "hx".into(),
            caret: Some(2),
            selection: None,
            extents: Some(fresh.clone()),
        };
        let delta = rebuild_text_node(cache, "/doc", &edited, &mut ids);
        let run_id = cache.runs[0].0;
        let (_, run) = delta.iter().find(|(id, _)| *id == run_id).unwrap();
        assert_eq!(
            run.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 0.0, x1: 27.0, y1: 16.0 }),
            "edited run carries the fresh geometry"
        );
        assert_eq!(cache.extents.as_deref(), Some(&fresh[..]), "fresh extents cached");
    }

    #[test]
    fn text_change_without_extents_drops_stale_geometry() {
        let doc = text_node(
            "/doc",
            TextState {
                text: "hi".into(),
                caret: Some(0),
                selection: None,
                extents: Some(vec![ext(10, 0, 8, 16), ext(18, 0, 8, 16)]),
            },
        );
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        build_window_update(&[doc], &mut ids, &mut caches);
        let cache = caches.get_mut("/doc").unwrap();

        // Text length changed but no fresh extents arrived: stale geometry
        // must be dropped, not misapplied.
        let edited = TextState { text: "hey".into(), caret: Some(3), selection: None, extents: None };
        let delta = rebuild_text_node(cache, "/doc", &edited, &mut ids);
        let run_id = cache.runs[0].0;
        let (_, run) = delta.iter().find(|(id, _)| *id == run_id).unwrap();
        assert_eq!(run.bounds(), None, "rebuilt run carries no stale geometry");
        assert_eq!(cache.extents, None, "stale cached extents cleared");
    }

    #[test]
    fn clamp_text_truncates_at_code_point_boundary() {
        assert_eq!(clamp_text("short"), "short");
        let long = "a".repeat(MAX_TEXT_CHARS + 10);
        assert_eq!(clamp_text(&long).chars().count(), MAX_TEXT_CHARS);
    }

    // --- Text-run geometry ---

    fn ext(x: i32, y: i32, width: i32, height: i32) -> CharExtent {
        CharExtent { x, y, width, height }
    }

    #[test]
    fn runs_carry_geometry_when_extents_provided() {
        let extents = [
            ext(10, 0, 8, 16),
            ext(18, 0, 8, 16),
            ext(0, 0, 0, 0),
            ext(10, 16, 8, 16),
            ext(18, 16, 8, 16),
        ];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "ab\ncd", Some(&extents), &mut ids);

        assert_eq!(
            runs[0].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 0.0, x1: 26.0, y1: 16.0 })
        );
        assert_eq!(runs[0].1.character_positions(), Some(&[0.0, 8.0, 16.0][..]));
        assert_eq!(runs[0].1.character_widths(), Some(&[8.0, 8.0, 0.0][..]));
        assert_eq!(runs[0].1.text_direction(), Some(accesskit::TextDirection::LeftToRight));

        assert_eq!(
            runs[1].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 16.0, x1: 26.0, y1: 32.0 })
        );
        assert_eq!(runs[1].1.character_positions(), Some(&[0.0, 8.0][..]));
        assert_eq!(runs[1].1.character_widths(), Some(&[8.0, 8.0][..]));
    }

    #[test]
    fn zero_extent_newline_is_synthesized_from_predecessor() {
        let extents = [ext(10, 0, 8, 16), ext(0, 0, 0, 0)];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "a\n", Some(&extents), &mut ids);

        assert_eq!(runs[0].1.character_positions(), Some(&[0.0, 8.0][..]));
        assert_eq!(runs[0].1.character_widths(), Some(&[8.0, 0.0][..]));
        assert_eq!(
            runs[0].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 0.0, x1: 18.0, y1: 16.0 }),
            "the zero-extent newline must not pollute x0"
        );
    }

    #[test]
    fn trailing_empty_run_gets_a_zero_width_caret_rect() {
        let extents = [ext(10, 0, 8, 16), ext(0, 0, 0, 0)];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "a\n", Some(&extents), &mut ids);

        assert_eq!(
            runs[1].1.bounds(),
            Some(accesskit::Rect { x0: 18.0, y0: 0.0, x1: 18.0, y1: 16.0 })
        );
        assert_eq!(runs[1].1.character_positions(), Some(&[][..]));
        assert_eq!(runs[1].1.character_widths(), Some(&[][..]));
        assert_eq!(runs[1].1.text_direction(), Some(accesskit::TextDirection::LeftToRight));
    }

    #[test]
    fn extent_length_mismatch_drops_geometry_entirely() {
        let extents = [ext(10, 0, 8, 16)];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "ab", Some(&extents), &mut ids);

        assert_eq!(runs[0].1.bounds(), None);
        assert_eq!(runs[0].1.character_positions(), None);
        assert_eq!(runs[0].1.character_widths(), None);
        assert_eq!(runs[0].1.text_direction(), None);
    }

    #[test]
    fn no_extents_means_no_geometry_properties() {
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "ab", None, &mut ids);

        assert_eq!(runs[0].1.bounds(), None);
        assert_eq!(runs[0].1.character_positions(), None);
        assert_eq!(runs[0].1.character_widths(), None);
        assert_eq!(runs[0].1.text_direction(), None);
        assert_eq!(runs[0].1.value(), Some("ab"));
        assert_eq!(char_lengths(&runs[0].1), vec![1, 1]);
    }

    #[test]
    fn positions_are_run_relative_not_window_relative() {
        let extents = [ext(30, 0, 8, 16), ext(0, 0, 0, 0), ext(40, 16, 8, 16)];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "x\ny", Some(&extents), &mut ids);

        assert_eq!(runs[1].1.character_positions(), Some(&[0.0][..]), "run-relative, not 40.0");
        assert_eq!(
            runs[1].1.bounds(),
            Some(accesskit::Rect { x0: 40.0, y0: 16.0, x1: 48.0, y1: 32.0 })
        );
    }

    #[test]
    fn empty_text_has_no_geometry() {
        let extents: [CharExtent; 0] = [];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "", Some(&extents), &mut ids);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1.bounds(), None);
        assert_eq!(runs[0].1.character_positions(), None);
        assert_eq!(runs[0].1.character_widths(), None);
        assert_eq!(runs[0].1.text_direction(), None);
    }

    #[test]
    fn leading_zero_extent_backfills_from_first_real_char() {
        let extents = [ext(0, 0, 0, 0), ext(10, 16, 8, 16)];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "\na", Some(&extents), &mut ids);

        assert_eq!(
            runs[0].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 16.0, x1: 10.0, y1: 32.0 })
        );
        assert_eq!(runs[0].1.character_positions(), Some(&[0.0][..]));
        assert_eq!(runs[0].1.character_widths(), Some(&[0.0][..]));

        assert_eq!(
            runs[1].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 16.0, x1: 18.0, y1: 32.0 })
        );
        assert_eq!(runs[1].1.character_positions(), Some(&[0.0][..]));
        assert_eq!(runs[1].1.character_widths(), Some(&[8.0][..]));
    }

    #[test]
    fn consumer_computes_bounding_boxes_from_run_geometry() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/label".into()];
        let mut label = leaf("/label", Role::Label, "");
        label.text = Some(TextState {
            text: "ab".into(),
            caret: None,
            selection: None,
            extents: Some(vec![ext(10, 0, 8, 16), ext(18, 0, 8, 16)]),
        });

        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root, label], &mut ids, &mut HashMap::new());
        let label_id = ids.get("/label").unwrap();

        let tree = accesskit_consumer::Tree::new(update, false);
        let state = tree.state();
        let node = state
            .node_by_tree_local_id(label_id, accesskit::TreeId::ROOT)
            .expect("label node present in consumer tree");
        let range = node.document_range();
        let boxes = range.bounding_boxes();

        assert_eq!(boxes.len(), 1, "expected exactly one bounding box");
        let rect = boxes[0];
        let expected = accesskit::Rect { x0: 10.0, y0: 0.0, x1: 26.0, y1: 16.0 };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(
            close(rect.x0, expected.x0)
                && close(rect.y0, expected.y0)
                && close(rect.x1, expected.x1)
                && close(rect.y1, expected.y1),
            "expected {expected:?}, got {rect:?}"
        );
    }

    // --- Chain splicing ---

    #[test]
    fn emitted_children_filters_to_walked_set() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into(), "/lazy".into()];
        let a = leaf("/a", Role::Panel, "");
        let map = emitted_children(&[root, a]);
        assert_eq!(map["/win"], vec!["/a".to_owned()]);
        assert_eq!(map["/a"], Vec::<String>::new());
    }

    #[test]
    fn splice_appends_chain_under_known_ancestor() {
        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children = vec!["/table/cell".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/win".to_owned(), "/table".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let table_id = ids.id_for("/table");

        let result = splice_chain_update(
            &[fresh_table, cell],
            &[],
            &known,
            &mut ids,
            &mut HashMap::new(),
        )
        .expect("chain splices");

        let cell_id = ids.get("/table/cell").expect("cell id allocated");
        assert_eq!(result.update.focus, cell_id);
        assert!(result.update.tree.is_none());
        let (_, table_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == table_id)
            .expect("ancestor re-emitted");
        assert!(table_node.children().contains(&cell_id));
        assert!(result.update.nodes.iter().any(|(id, _)| *id == cell_id));
        assert_eq!(
            result.children,
            vec![
                ("/table".to_owned(), vec!["/table/cell".to_owned()]),
                ("/table/cell".to_owned(), Vec::new()),
            ]
        );
    }

    #[test]
    fn splice_preserves_ancestor_children_absent_from_fresh_read() {
        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children = vec!["/table/cell".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> =
            ["/table".to_owned(), "/table/a".to_owned(), "/table/b".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let a_id = ids.id_for("/table/a");
        let b_id = ids.id_for("/table/b");

        let result = splice_chain_update(
            &[fresh_table, cell],
            &["/table/a".to_owned(), "/table/b".to_owned()],
            &known,
            &mut ids,
            &mut HashMap::new(),
        )
        .expect("chain splices");

        let table_id = ids.get("/table").unwrap();
        let cell_id = ids.get("/table/cell").unwrap();
        let (_, table_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == table_id)
            .unwrap();
        assert_eq!(table_node.children(), &[a_id, b_id, cell_id]);
    }

    #[test]
    fn splice_ignores_unknown_fresh_children() {
        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children =
            vec!["/table/x1".into(), "/table/cell".into(), "/table/x2".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();

        let result =
            splice_chain_update(&[fresh_table, cell], &[], &known, &mut ids, &mut HashMap::new())
                .expect("chain splices");

        let table_id = ids.get("/table").unwrap();
        let cell_id = ids.get("/table/cell").unwrap();
        let (_, table_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == table_id)
            .unwrap();
        assert_eq!(table_node.children(), &[cell_id], "never-walked cells contribute nothing");
        assert!(ids.get("/table/x1").is_none());
    }

    #[test]
    fn splice_injects_missing_interior_link() {
        let table = leaf("/table", Role::Table, "grid");
        let row = leaf("/table/row", Role::Panel, "");
        let cell = leaf("/table/row/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();

        let result = splice_chain_update(
            &[table, row, cell],
            &[],
            &known,
            &mut ids,
            &mut HashMap::new(),
        )
        .expect("chain splices");

        let row_id = ids.get("/table/row").unwrap();
        let cell_id = ids.get("/table/row/cell").unwrap();
        let (_, row_node) = result
            .update
            .nodes
            .iter()
            .find(|(id, _)| *id == row_id)
            .expect("interior node emitted");
        assert_eq!(row_node.children(), &[cell_id]);
    }

    #[test]
    fn resplice_is_idempotent() {
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let build = |ids: &mut NodeIdMap| {
            let mut fresh_table = leaf("/table", Role::Table, "grid");
            fresh_table.children = vec!["/table/cell".into()];
            let cell = leaf("/table/cell", Role::TableCell, "A1");
            splice_chain_update(
                &[fresh_table, cell],
                &[],
                &known,
                ids,
                &mut HashMap::new(),
            )
            .expect("chain splices")
        };
        let first = build(&mut ids);
        let second = build(&mut ids);
        assert_eq!(first.update.focus, second.update.focus);
        assert_eq!(first.children, second.children);
        let ids_of = |r: &SpliceResult| {
            let mut v: Vec<_> = r.update.nodes.iter().map(|(id, _)| *id).collect();
            v.sort();
            v
        };
        assert_eq!(ids_of(&first), ids_of(&second));
    }

    #[test]
    fn splice_rejects_a_short_chain() {
        let table = leaf("/table", Role::Table, "grid");
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();
        assert!(splice_chain_update(&[table], &[], &known, &mut ids, &mut HashMap::new())
            .is_none());
        assert!(splice_chain_update(&[], &[], &known, &mut ids, &mut HashMap::new()).is_none());
    }

    #[test]
    fn spliced_text_node_builds_runs_and_cache() {
        let mut fresh_doc = leaf("/doc", Role::DocumentText, "");
        fresh_doc.children = vec!["/doc/p".into()];
        let mut p = leaf("/doc/p", Role::Paragraph, "");
        p.text = Some(TextState {
            text: "hi".into(),
            caret: None,
            selection: None,
            extents: None,
        });
        let known: HashSet<String> = ["/doc".to_owned()].into();
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();

        let result = splice_chain_update(&[fresh_doc, p], &[], &known, &mut ids, &mut caches)
            .expect("chain splices");

        let run_id = ids.get("/doc/p#run0").expect("run id allocated");
        assert!(result.update.nodes.iter().any(|(id, _)| *id == run_id));
        assert!(caches.contains_key("/doc/p"), "text cache recorded for later deltas");
    }

    #[test]
    fn merge_replaces_same_id_nodes_appends_new_and_adopts_focus() {
        let mut ids = NodeIdMap::new();
        let root_id = ids.id_for("/win");
        let extra_id = ids.id_for("/extra");
        let mut full = TreeUpdate {
            nodes: vec![(root_id, Node::new(accesskit::Role::Window))],
            tree: Some(Tree::new(root_id)),
            tree_id: TreeId::ROOT,
            focus: root_id,
        };
        let mut replacement = Node::new(accesskit::Role::Window);
        replacement.set_label("fresh");
        let splice = TreeUpdate {
            nodes: vec![
                (root_id, replacement),
                (extra_id, Node::new(accesskit::Role::Cell)),
            ],
            tree: None,
            tree_id: TreeId::ROOT,
            focus: extra_id,
        };

        merge_update(&mut full, splice);

        assert_eq!(full.nodes.len(), 2);
        assert_eq!(full.nodes[0].1.label(), Some("fresh".into()));
        assert_eq!(full.nodes[1].0, extra_id);
        assert_eq!(full.focus, extra_id);
        assert!(full.tree.is_some(), "merge never clears the full update's tree");
    }

    #[test]
    fn consumer_applies_spliced_cell_focus() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/table".into()];
        let table = leaf("/table", Role::Table, "grid");
        let mut ids = NodeIdMap::new();
        let mut caches = HashMap::new();
        let full = build_window_update(&[root, table], &mut ids, &mut caches);
        let mut tree = accesskit_consumer::Tree::new(full, false);

        let mut fresh_table = leaf("/table", Role::Table, "grid");
        fresh_table.children = vec!["/table/cell".into()];
        let cell = leaf("/table/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/win".to_owned(), "/table".to_owned()].into();
        let result = splice_chain_update(
            &[fresh_table, cell],
            &[],
            &known,
            &mut ids,
            &mut caches,
        )
        .expect("chain splices");
        let cell_id = ids.get("/table/cell").unwrap();

        tree.update_and_process_changes(result.update, &mut NoOpChanges);

        let state = tree.state();
        let cell_node = state
            .node_by_tree_local_id(cell_id, accesskit::TreeId::ROOT)
            .expect("spliced cell present in consumer tree");
        assert_eq!(state.focus_id_in_tree(), cell_node.id());
        assert_eq!(cell_node.role(), accesskit::Role::Cell);
    }
}
