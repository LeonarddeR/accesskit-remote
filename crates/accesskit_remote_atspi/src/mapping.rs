//! Pure, bus-free translation from a walked AT-SPI subtree into an AccessKit
//! [`TreeUpdate`]. Everything here operates on plain data so it can be unit
//! tested without a live accessibility bus.

use accesskit::{Node, NodeId, TextPosition, TextSelection, Tree, TreeId, TreeUpdate};
use atspi::{Interface, InterfaceSet, Role, State, StateSet};
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
///
/// Each `Option` field corresponds to an AT-SPI interface the object
/// advertises, so `interfaces` doubles as the gate for which extra reads the
/// walk performed: `text` ⇔ `Text`, `bounds` ⇔ `Component`, `value` ⇔
/// `Value`, `table` ⇔ `Table`, `cell` ⇔ `TableCell`.
///
/// `attributes` and `relations` are different: both come from the
/// `Accessible` interface every object already advertises, so there is no
/// interface to gate them on. Instead they are read only for roles that can
/// carry them ([`role_reads_attributes`], [`role_reads_relations`]).
#[derive(Debug, Clone)]
pub struct MirrorNode {
    pub path: String,
    pub role: Role,
    pub name: String,
    /// The object's longer description; empty when absent. Read in the same
    /// round trip as the name.
    pub description: String,
    /// The interfaces the object advertises, gating the walk's optional reads
    /// and the actions that can be planned against it.
    pub interfaces: InterfaceSet,
    pub states: NodeStates,
    pub children: Vec<String>,
    /// Text-interface state for text-input roles; `None` for non-text nodes.
    pub text: Option<TextState>,
    /// The object's own window-relative extents from `Component.GetExtents`;
    /// `None` when the interface is absent or the read failed.
    pub bounds: Option<CharExtent>,
    /// Value-interface numeric range and current value; `None` when the
    /// interface is absent or the role is not one that carries a value
    /// ([`role_reads_value`]).
    pub value: Option<ValueState>,
    /// Table-interface row and column counts; `None` when the interface is
    /// absent.
    pub table: Option<TableState>,
    /// TableCell-interface position and span; `None` when the interface is
    /// absent.
    pub cell: Option<CellState>,
    /// Presentation attributes (`placeholder-text`, `level`, `posinset`,
    /// `setsize`); read only for roles that can carry them
    /// ([`role_reads_attributes`]), not gated by any interface.
    pub attributes: NodeAttributes,
    /// AT-SPI relations targeting other objects by path, forward directions
    /// only; read only for roles that can carry them
    /// ([`role_reads_relations`]), not gated by any interface.
    pub relations: Vec<Relation>,
}

impl Default for MirrorNode {
    fn default() -> Self {
        Self {
            path: String::new(),
            role: Role::Invalid,
            name: String::new(),
            description: String::new(),
            interfaces: InterfaceSet::empty(),
            states: NodeStates::default(),
            children: Vec::new(),
            text: None,
            bounds: None,
            value: None,
            table: None,
            cell: None,
            attributes: NodeAttributes::default(),
            relations: Vec::new(),
        }
    }
}

impl MirrorNode {
    /// Whether the object advertises the AT-SPI `Action` interface.
    pub fn is_actionable(&self) -> bool {
        self.interfaces.contains(Interface::Action)
    }
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

/// One window-relative extent as AT-SPI reports it: a code point's, or a
/// whole object's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharExtent {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The AT-SPI `Value` interface state of one node: the current numeric value,
/// the minimum and maximum of its range, and the increment one step changes
/// it by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ValueState {
    pub current: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub step: f64,
}

/// The AT-SPI `Table` interface's row and column counts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableState {
    pub rows: i32,
    pub columns: i32,
}

/// The AT-SPI `TableCell` interface's position within its table: zero-based
/// row and column, and the number of rows and columns it spans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellState {
    pub row: i32,
    pub column: i32,
    pub row_span: i32,
    pub column_span: i32,
}

/// Presentation attributes read from AT-SPI's untyped object `Attributes`
/// map, gated to the roles that can carry them ([`role_reads_attributes`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeAttributes {
    pub placeholder: Option<String>,
    pub level: Option<usize>,
    pub position_in_set: Option<usize>,
    pub size_of_set: Option<usize>,
}

/// One AccessKit relation this mapping consumes. AT-SPI reports relations in
/// both directions; only the forward direction of each pair is kept
/// ([`map_relation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    LabelledBy,
    Controls,
    DescribedBy,
    Details,
    ErrorMessage,
    PopupFor,
}

/// One AT-SPI relation of `kind`, targeting the object paths in `targets`.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub kind: RelationKind,
    pub targets: Vec<String>,
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
    pub element_children: Vec<NodeId>,
    pub runs: Vec<(NodeId, Node)>,
    pub layout: Vec<TextRunLayout>,
    /// Whether the node's role has a caret, so a later refresh re-reads the
    /// caret and selection instead of leaving them cleared.
    pub caret_enabled: bool,
    /// The extents behind the runs' current geometry, reused by refreshes that
    /// read no geometry (caret/selection moves) while the text is unchanged.
    pub extents: Option<Vec<CharExtent>>,
    /// The container's own bounds, anchoring an empty field's caret run.
    pub container_bounds: Option<CharExtent>,
}

/// What one window last emitted, keyed by AT-SPI object path: `nodes` is
/// exactly the set of container nodes the client's tree currently holds, and
/// `text` the extra bookkeeping a text delta diffs against. Both are written by
/// [`build_window_update`] (which also prunes vanished paths) and
/// [`splice_chain_update`]; `nodes` is additionally written by a single-node
/// refresh, making it the one authority for a node's last-emitted form.
#[derive(Debug, Default)]
pub struct WindowCache {
    pub nodes: HashMap<String, Node>,
    pub text: HashMap<String, TextNodeCache>,
}

/// Translates an AT-SPI [`Role`] into the nearest AccessKit role.
pub fn map_role(role: Role) -> accesskit::Role {
    use accesskit::Role as A;
    match role {
        Role::Frame | Role::Window => A::Window,
        Role::Dialog => A::Dialog,
        Role::Alert => A::Alert,
        Role::Label => A::Label,
        Role::Button | Role::ToggleButton | Role::PushButtonMenu => A::Button,
        Role::CheckBox => A::CheckBox,
        Role::CheckMenuItem => A::MenuItemCheckBox,
        Role::RadioButton => A::RadioButton,
        Role::RadioMenuItem => A::MenuItemRadio,
        Role::Menu | Role::PopupMenu => A::Menu,
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

        Role::Separator => A::Splitter,
        Role::LevelBar => A::Meter,
        Role::Rating => A::Meter,
        Role::Dial => A::Slider,
        Role::TableRow => A::Row,
        Role::RowHeader | Role::TableRowHeader => A::RowHeader,
        Role::ColumnHeader | Role::TableColumnHeader => A::ColumnHeader,
        Role::TreeTable => A::TreeGrid,
        Role::ToolTip => A::Tooltip,
        Role::Static => A::Label,
        Role::Notification | Role::InfoBar => A::Alert,
        Role::TitleBar => A::TitleBar,
        Role::Header => A::Header,
        Role::Footer => A::Footer,
        Role::Canvas | Role::DrawingArea => A::Canvas,
        Role::Audio => A::Audio,
        Role::Video => A::Video,
        Role::Editbar => A::TextInput,
        Role::Embedded => A::EmbeddedObject,
        Role::ImageMap => A::Image,
        Role::CHART => A::Figure,
        Role::Autocomplete => A::ListBox,
        Role::TearoffMenuItem => A::MenuItem,
        // AT-SPI defines these three as specialized dialogs, not as the
        // controls that open them.
        Role::FileChooser | Role::FontChooser | Role::ColorChooser => A::Dialog,
        Role::DateEditor => A::DateInput,
        Role::Article => A::Article,
        Role::BlockQuote => A::Blockquote,
        Role::Caption => A::Caption,
        Role::Comment => A::Comment,
        Role::Form => A::Form,
        Role::Landmark => A::Region,
        Role::Log => A::Log,
        Role::Marquee => A::Marquee,
        Role::Math | Role::MathFraction | Role::MathRoot => A::Math,
        Role::Footnote => A::DocFootnote,
        Role::Timer => A::Timer,
        Role::Definition | Role::DescriptionValue => A::Definition,
        Role::DescriptionList => A::DescriptionList,
        Role::DescriptionTerm => A::Term,
        Role::Mark => A::Mark,
        Role::Suggestion => A::Suggestion,
        Role::ContentDeletion => A::ContentDeletion,
        Role::ContentInsertion => A::ContentInsertion,

        _ => A::GenericContainer,
    }
}

/// Whether the node is a control the user can operate, as opposed to a layout
/// container or a piece of static content.
fn is_control(node: &MirrorNode) -> bool {
    node.states.focusable || is_clickable_role(node.role) || node.is_actionable()
}

/// Sharpens a mapped role using the rest of the node, for the cases AT-SPI's
/// role alone cannot distinguish.
///
/// A named `Grouping` is ARIA's `group` — a deliberate semantic grouping — so
/// it is promoted out of the transparent-container fallback. `Panel` (what GTK
/// emits for a plain layout box) is left alone whether named or not, because
/// promoting it would surface every box in the client tree.
pub fn refine_role(base: accesskit::Role, node: &MirrorNode) -> accesskit::Role {
    if node.role == Role::Grouping && !node.name.is_empty() {
        return accesskit::Role::Group;
    }
    base
}

/// The subset of AT-SPI state the mapping forwards to AccessKit, distilled from
/// a [`StateSet`]. An `Option` field is `None` when the concept does not apply
/// to the node, distinguishing that from an explicit `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NodeStates {
    pub focusable: bool,
    pub focused: bool,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    pub toggled: Option<accesskit::Toggled>,
    pub has_popup: bool,
    /// Whether the object reports itself operable. GTK4 sets only `Sensitive`
    /// (and omits it when the widget is explicitly disabled); at-spi2-atk sets
    /// `Sensitive` and `Enabled` together. Its *absence* is what marks a
    /// control disabled, so this is stored positively and gated on the node
    /// being a control at all — see [`build_node`].
    pub sensitive: bool,
    pub read_only: bool,
    pub required: bool,
    pub invalid: bool,
    pub modal: bool,
    pub multiselectable: bool,
    pub busy: bool,
    /// `None` when the object reports neither or both orientations.
    pub orientation: Option<accesskit::Orientation>,
}

/// Distills the subset of AT-SPI state the mapping forwards from a [`StateSet`].
pub fn node_states(states: StateSet) -> NodeStates {
    use accesskit::Toggled;
    let expanded = (states.contains(State::Expandable)
        || states.contains(State::Expanded)
        || states.contains(State::Collapsed))
    .then(|| states.contains(State::Expanded));
    let selected = (states.contains(State::Selectable) || states.contains(State::Selected))
        .then(|| states.contains(State::Selected));
    let toggled = if states.contains(State::Indeterminate) {
        Some(Toggled::Mixed)
    } else if states.contains(State::Checked) || states.contains(State::Pressed) {
        Some(Toggled::True)
    } else if states.contains(State::Checkable) {
        Some(Toggled::False)
    } else {
        None
    };
    let orientation = match (
        states.contains(State::Horizontal),
        states.contains(State::Vertical),
    ) {
        (true, false) => Some(accesskit::Orientation::Horizontal),
        (false, true) => Some(accesskit::Orientation::Vertical),
        _ => None,
    };
    NodeStates {
        focusable: states.contains(State::Focusable),
        focused: states.contains(State::Focused),
        expanded,
        selected,
        toggled,
        has_popup: states.contains(State::HasPopup),
        sensitive: states.contains(State::Sensitive) || states.contains(State::Enabled),
        read_only: states.contains(State::ReadOnly),
        required: states.contains(State::Required),
        invalid: states.contains(State::InvalidEntry),
        modal: states.contains(State::Modal),
        multiselectable: states.contains(State::Multiselectable),
        busy: states.contains(State::Busy),
        orientation,
    }
}

/// Parses the AT-SPI object attributes the mapping forwards out of the
/// interface's untyped string map: `placeholder-text`, `level`, `posinset`,
/// and `setsize`. A value that fails to parse (or, for `placeholder-text`, is
/// empty) is dropped to `None` rather than surfaced malformed; keys the
/// mapping does not recognize (`toolkit`, `keyshortcuts`, `xml-roles`, ...)
/// are ignored.
pub fn parse_attributes(map: &HashMap<String, String>) -> NodeAttributes {
    let count = |key: &str| map.get(key).and_then(|value| value.parse::<usize>().ok());
    NodeAttributes {
        placeholder: map
            .get("placeholder-text")
            .filter(|value| !value.is_empty())
            .cloned(),
        level: count("level"),
        position_in_set: count("posinset"),
        size_of_set: count("setsize"),
    }
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

/// Whether a role conventionally carries a numeric value. LibreOffice
/// advertises the Value interface on menu items and table cells too, where a
/// numeric range is meaningless to a consumer, so the interface alone is too
/// broad.
pub(crate) fn role_reads_value(role: Role) -> bool {
    matches!(
        role,
        Role::Slider
            | Role::SpinButton
            | Role::ProgressBar
            | Role::LevelBar
            | Role::ScrollBar
            | Role::Dial
    )
}

/// Whether a role is one whose object attributes are worth reading: text
/// inputs and other roles that can carry a placeholder, a heading or item
/// level, or a position within a set.
pub(crate) fn role_reads_attributes(role: Role) -> bool {
    matches!(
        role,
        Role::Text
            | Role::PasswordText
            | Role::Entry
            | Role::Editbar
            | Role::Terminal
            | Role::Heading
            | Role::ListItem
            | Role::TreeItem
            | Role::RadioButton
            | Role::PageTab
            | Role::TableCell
    )
}

/// Whether a role is one whose relations are worth reading: controls and the
/// containers/labels/targets they conventionally point at or are pointed at
/// from.
pub(crate) fn role_reads_relations(role: Role) -> bool {
    matches!(
        role,
        Role::Panel
            | Role::Grouping
            | Role::ScrollPane
            | Role::PageTab
            | Role::Button
            | Role::ToggleButton
            | Role::CheckBox
            | Role::RadioButton
            | Role::ComboBox
            | Role::SpinButton
            | Role::Slider
            | Role::ScrollBar
            | Role::Text
            | Role::Entry
            | Role::PasswordText
            | Role::Editbar
    )
}

/// Maps an AT-SPI [`atspi::RelationType`] to the [`RelationKind`] this mapping
/// consumes. AT-SPI relations come in reciprocal pairs; only the forward
/// direction of each pair the mapping cares about is kept — the reverse
/// (`LabelFor`, `ControlledBy`, `DescriptionFor`, ...) maps to `None`, as does
/// every relation type the mapping does not consume at all.
pub(crate) fn map_relation(kind: atspi::RelationType) -> Option<RelationKind> {
    use atspi::RelationType;
    match kind {
        RelationType::LabelledBy => Some(RelationKind::LabelledBy),
        RelationType::ControllerFor => Some(RelationKind::Controls),
        RelationType::DescribedBy => Some(RelationKind::DescribedBy),
        RelationType::Details => Some(RelationKind::Details),
        RelationType::ErrorMessage => Some(RelationKind::ErrorMessage),
        RelationType::PopupFor => Some(RelationKind::PopupFor),
        _ => None,
    }
}

/// Whether a role is one whose whole purpose is to carry a checked or pressed
/// state, so a consumer should always find one on it.
fn is_toggleable_role(role: Role) -> bool {
    matches!(
        role,
        Role::CheckBox
            | Role::CheckMenuItem
            | Role::RadioButton
            | Role::RadioMenuItem
            | Role::ToggleButton
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
/// from the node's synthesized extents. An empty text's single run takes a
/// zero-width rect at `container_bounds`' left edge when no extent can anchor
/// it.
pub fn build_text_runs(
    parent_path: &str,
    text: &str,
    extents: Option<&[CharExtent]>,
    container_bounds: Option<CharExtent>,
    ids: &mut NodeIdMap,
) -> (Vec<(NodeId, Node)>, Vec<TextRunLayout>) {
    let runs = split_runs(text);
    let mut nodes = Vec::with_capacity(runs.len());
    let mut layout = Vec::with_capacity(runs.len());
    let synthesized = extents
        .filter(|all| all.len() == text.chars().count())
        .and_then(synthesize_extents);
    let caret_anchor = if synthesized.is_none() && text.is_empty() {
        container_bounds
    } else {
        None
    };
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
        } else if let Some(anchor) = caret_anchor {
            let edge = anchor.x as f64;
            node.set_bounds(accesskit::Rect {
                x0: edge,
                y0: anchor.y as f64,
                x1: edge,
                y1: (anchor.y + anchor.height) as f64,
            });
            node.set_character_positions(Vec::new());
            node.set_character_widths(Vec::new());
            node.set_text_direction(accesskit::TextDirection::LeftToRight);
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
    cache: &mut WindowCache,
    parent_path: &str,
    state: &TextState,
    ids: &mut NodeIdMap,
) -> Vec<(NodeId, Node)> {
    let Some(text) = cache.text.get(parent_path) else {
        return Vec::new();
    };
    let Some(old_parent) = cache.nodes.get(parent_path).cloned() else {
        return Vec::new();
    };
    // Fresh extents win; a refresh that read none (caret/selection move)
    // reuses the cached ones as long as the text length still matches.
    let effective: Option<Vec<CharExtent>> = match &state.extents {
        Some(fresh) => Some(fresh.clone()),
        None => text
            .extents
            .as_ref()
            .filter(|cached| cached.len() == state.text.chars().count())
            .cloned(),
    };
    let (runs, layout) = build_text_runs(
        parent_path,
        &state.text,
        effective.as_deref(),
        text.container_bounds,
        ids,
    );
    let mut parent = old_parent.clone();
    let mut children = text.element_children.clone();
    children.extend(runs.iter().map(|(id, _)| *id));
    parent.set_children(children);
    match text_selection(state, &layout) {
        Some(selection) => parent.set_text_selection(selection),
        None => parent.clear_text_selection(),
    }

    let mut changed = Vec::new();
    if parent != old_parent {
        changed.push((text.node_id, parent.clone()));
    }
    for (index, (id, node)) in runs.iter().enumerate() {
        let differs = match text.runs.get(index) {
            Some((old_id, old_node)) => old_id != id || old_node != node,
            None => true,
        };
        if differs {
            changed.push((*id, node.clone()));
        }
    }

    cache.nodes.insert(parent_path.to_owned(), parent);
    let text = cache
        .text
        .get_mut(parent_path)
        .expect("text cache present (checked above)");
    text.runs = runs;
    text.layout = layout;
    text.extents = effective;
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
    cache: &mut WindowCache,
) -> TreeUpdate {
    let walked: HashSet<&str> = nodes.iter().map(|node| node.path.as_str()).collect();
    // Every walked path holds its id before the first node is built, so a
    // relation target resolves whether or not the walk has reached it yet.
    for node in nodes {
        ids.id_for(&node.path);
    }
    let root_id = ids.id_for(&nodes[0].path);
    let mut out = Vec::with_capacity(nodes.len());
    let mut focus = root_id;
    let mut live: HashSet<String> = HashSet::new();
    for node in nodes {
        let id = ids.id_for(&node.path);
        if node.states.focused {
            focus = id;
        }
        let built = build_node(node, id, ids, &walked);
        cache.nodes.insert(node.path.clone(), built.container.clone());
        out.push((id, built.container));
        out.extend(built.runs);
        if let Some(text) = built.cache {
            live.insert(node.path.clone());
            cache.text.insert(node.path.clone(), text);
        }
    }
    cache.nodes.retain(|path, _| walked.contains(path.as_str()));
    cache.text.retain(|path, _| live.contains(path));
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
    cache: &mut WindowCache,
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
        ids.id_for(&node.path);
    }
    for node in &spliced {
        let id = ids.id_for(&node.path);
        let built = build_node(node, id, ids, &walked);
        cache.nodes.insert(node.path.clone(), built.container.clone());
        nodes_out.push((id, built.container));
        nodes_out.extend(built.runs);
        if let Some(text) = built.cache {
            cache.text.insert(node.path.clone(), text);
        } else {
            cache.text.remove(&node.path);
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

/// Rebuilds one node's semantics from a fresh read and diffs it against the
/// node's last-emitted form, returning the node to emit — or `None` when
/// nothing changed or the path is no longer in the client's tree.
///
/// A refresh changes semantics only, never structure: the children come from
/// `emitted_children` (the paths the client currently holds) plus, for a text
/// node, its cached run ids, so the fresh read's own child list is ignored and
/// allocates no ids. The last-emitted text selection is carried over verbatim,
/// since a refresh reads no text.
pub fn refresh_node(
    fresh: &MirrorNode,
    id: NodeId,
    emitted_children: &[String],
    ids: &mut NodeIdMap,
    cache: &mut WindowCache,
) -> Option<(NodeId, Node)> {
    let old = cache.nodes.get(&fresh.path)?.clone();
    let runs: Vec<NodeId> = cache
        .text
        .get(&fresh.path)
        .map(|text| text.runs.iter().map(|(run_id, _)| *run_id).collect())
        .unwrap_or_default();

    let mut container = build_container(fresh);
    apply_relations(&mut container, &fresh.relations, ids);
    let mut children: Vec<NodeId> = emitted_children.iter().map(|path| ids.id_for(path)).collect();
    children.extend(runs);
    if !children.is_empty() {
        container.set_children(children);
    }
    match old.text_selection() {
        Some(selection) => container.set_text_selection(*selection),
        None => container.clear_text_selection(),
    }

    if container == old {
        return None;
    }
    cache.nodes.insert(fresh.path.clone(), container.clone());
    Some((id, container))
}

/// A built node: the container plus any synthesized text-run children and, for
/// text nodes, the cache used to diff later text deltas.
struct BuiltNode {
    container: Node,
    runs: Vec<(NodeId, Node)>,
    cache: Option<TextNodeCache>,
}

/// Builds everything about a node that comes from its own AT-SPI read: role,
/// label, description, states, actions, and bounds. Children and text selection
/// are added by the caller, which knows the tree's structure.
fn build_container(node: &MirrorNode) -> Node {
    let role = refine_role(map_role(node.role), node);
    let mut container = Node::new(role);
    if !node.name.is_empty() {
        if role == accesskit::Role::Label {
            container.set_value(node.name.clone());
        } else {
            container.set_label(node.name.clone());
        }
    }
    if !node.description.is_empty() && node.description != node.name {
        container.set_description(node.description.clone());
    }
    match node.states.toggled {
        Some(toggled) => container.set_toggled(toggled),
        // A role that is inherently a toggle keeps reporting one even when the
        // toolkit's state set carries neither Checked nor Checkable.
        None if is_toggleable_role(node.role) => container.set_toggled(accesskit::Toggled::False),
        None => {}
    }
    if let Some(expanded) = node.states.expanded {
        container.set_expanded(expanded);
    }
    if let Some(selected) = node.states.selected {
        container.set_selected(selected);
    }
    if node.states.has_popup {
        container.set_has_popup(accesskit::HasPopup::Menu);
    }
    // Only a control can be disabled. Layout boxes report no Sensitive either,
    // and announcing every one of them as disabled would be worse than silence.
    if !node.states.sensitive && is_control(node) {
        container.set_disabled();
    }
    if node.states.read_only {
        container.set_read_only();
    }
    if node.states.required {
        container.set_required();
    }
    if node.states.invalid {
        container.set_invalid(accesskit::Invalid::True);
    }
    if node.states.modal {
        container.set_modal();
    }
    if node.states.multiselectable {
        container.set_multiselectable();
    }
    if node.states.busy {
        container.set_busy();
    }
    if let Some(orientation) = node.states.orientation {
        container.set_orientation(orientation);
    }
    if node.is_actionable() && is_clickable_role(node.role) {
        container.add_action(accesskit::Action::Click);
    }
    if node.states.focusable {
        container.add_action(accesskit::Action::Focus);
    }
    if let Some(bounds) = node.bounds {
        if bounds.width > 0 && bounds.height > 0 {
            container.set_bounds(accesskit::Rect {
                x0: bounds.x as f64,
                y0: bounds.y as f64,
                x1: (bounds.x + bounds.width) as f64,
                y1: (bounds.y + bounds.height) as f64,
            });
        }
    }
    if let Some(value) = node.value {
        if value.current.is_finite() {
            container.set_numeric_value(value.current);
            if value.minimum.is_finite()
                && value.maximum.is_finite()
                && value.step.is_finite()
                && value.minimum < value.maximum
            {
                container.set_min_numeric_value(value.minimum);
                container.set_max_numeric_value(value.maximum);
                container.set_numeric_value_step(value.step);
            }
        }
    }
    if let Some(table) = node.table {
        if table.rows >= 0 {
            container.set_row_count(table.rows as usize);
        }
        if table.columns >= 0 {
            container.set_column_count(table.columns as usize);
        }
    }
    if let Some(cell) = node.cell {
        if cell.row >= 0 {
            container.set_row_index(cell.row as usize);
        }
        if cell.column >= 0 {
            container.set_column_index(cell.column as usize);
        }
        if cell.row_span > 0 {
            container.set_row_span(cell.row_span as usize);
        }
        if cell.column_span > 0 {
            container.set_column_span(cell.column_span as usize);
        }
    }
    if let Some(placeholder) = &node.attributes.placeholder {
        container.set_placeholder(placeholder.clone());
    }
    if let Some(level) = node.attributes.level {
        container.set_level(level);
    }
    if let Some(position) = node.attributes.position_in_set {
        container.set_position_in_set(position);
    }
    if let Some(size) = node.attributes.size_of_set {
        container.set_size_of_set(size);
    }
    container
}

/// Adds a node's relations to its container, resolving each target path to the
/// id it was walked under and dropping the targets that have none — an
/// unwalked path stays unwalked rather than gaining an id here. A relation
/// left with no resolved target sets no property at all.
///
/// Runs immediately after [`build_container`] wherever a container is built,
/// so a walk and a refresh of the same node write the same properties in the
/// same order.
fn apply_relations(container: &mut Node, relations: &[Relation], ids: &NodeIdMap) {
    for relation in relations {
        let targets: Vec<NodeId> = relation
            .targets
            .iter()
            .filter_map(|path| ids.get(path))
            .collect();
        let Some(&first) = targets.first() else {
            continue;
        };
        match relation.kind {
            RelationKind::LabelledBy => container.set_labelled_by(targets),
            RelationKind::Controls => container.set_controls(targets),
            RelationKind::DescribedBy => container.set_described_by(targets),
            RelationKind::Details => container.set_details(targets),
            RelationKind::ErrorMessage => container.set_error_message(first),
            RelationKind::PopupFor => {}
        }
    }
}

fn build_node(
    node: &MirrorNode,
    id: NodeId,
    ids: &mut NodeIdMap,
    walked: &HashSet<&str>,
) -> BuiltNode {
    let mut container = build_container(node);
    apply_relations(&mut container, &node.relations, ids);
    let element_children: Vec<NodeId> = node
        .children
        .iter()
        .filter(|path| walked.contains(path.as_str()))
        .map(|path| ids.id_for(path))
        .collect();
    match &node.text {
        Some(state) => {
            let (runs, layout) = build_text_runs(
                &node.path,
                &state.text,
                state.extents.as_deref(),
                node.bounds,
                ids,
            );
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
                element_children,
                runs: runs.clone(),
                layout,
                caret_enabled: has_text_caret(node.role),
                extents: state.extents.clone(),
                container_bounds: node.bounds,
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
            ..Default::default()
        }
    }

    fn build(nodes: &[MirrorNode], ids: &mut NodeIdMap) -> TreeUpdate {
        build_window_update(nodes, ids, &mut WindowCache::default())
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
    fn window_cache_holds_exactly_the_emitted_containers() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into(), "/b".into()];
        let a = leaf("/a", Role::Button, "A");
        let b = leaf("/b", Role::Label, "B");

        let mut ids = NodeIdMap::new();
        let mut cache = WindowCache::default();
        let update = build_window_update(&[root, a.clone(), b], &mut ids, &mut cache);

        let emitted: HashMap<NodeId, Node> = update.nodes.iter().cloned().collect();
        for path in ["/win", "/a", "/b"] {
            let id = ids.get(path).expect("id allocated");
            assert_eq!(
                cache.nodes.get(path),
                emitted.get(&id),
                "cache holds the node emitted for {path}",
            );
        }
        assert_eq!(cache.nodes.len(), 3);

        let mut shrunk = leaf("/win", Role::Frame, "w");
        shrunk.children = vec!["/a".into()];
        build_window_update(&[shrunk, a], &mut ids, &mut cache);
        assert_eq!(cache.nodes.len(), 2);
        assert!(cache.nodes.contains_key("/a"));
        assert!(!cache.nodes.contains_key("/b"), "vanished path is pruned");
    }

    #[test]
    fn map_role_covers_common_roles_with_container_fallback() {
        assert_eq!(map_role(Role::Frame), accesskit::Role::Window);
        assert_eq!(map_role(Role::Label), accesskit::Role::Label);
        assert_eq!(map_role(Role::Button), accesskit::Role::Button);
        assert_eq!(map_role(Role::PageTab), accesskit::Role::Tab);
        assert_eq!(map_role(Role::Viewport), accesskit::Role::GenericContainer);
    }

    #[test]
    fn map_role_covers_menu_roles() {
        assert_eq!(map_role(Role::MenuBar), accesskit::Role::MenuBar);
        assert_eq!(map_role(Role::Menu), accesskit::Role::Menu);
        assert_eq!(map_role(Role::MenuItem), accesskit::Role::MenuItem);
        assert_eq!(map_role(Role::CheckMenuItem), accesskit::Role::MenuItemCheckBox);
        assert_eq!(map_role(Role::RadioMenuItem), accesskit::Role::MenuItemRadio);
        // Previously fell through to GenericContainer, losing menu semantics.
        assert_eq!(map_role(Role::PopupMenu), accesskit::Role::Menu);
        assert_eq!(map_role(Role::PushButtonMenu), accesskit::Role::Button);
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
    fn map_role_covers_the_gtk4_and_libreoffice_role_surface() {
        use accesskit::Role as A;

        for (atspi, expected) in [
            (Role::Separator, A::Splitter),
            (Role::LevelBar, A::Meter),
            (Role::TableRow, A::Row),
            (Role::RowHeader, A::RowHeader),
            (Role::TableRowHeader, A::RowHeader),
            (Role::ColumnHeader, A::ColumnHeader),
            (Role::TableColumnHeader, A::ColumnHeader),
            (Role::TreeTable, A::TreeGrid),
            (Role::ToolTip, A::Tooltip),
            (Role::Static, A::Label),
            (Role::Notification, A::Alert),
            (Role::InfoBar, A::Alert),
            (Role::TitleBar, A::TitleBar),
            (Role::Header, A::Header),
            (Role::Footer, A::Footer),
            (Role::Canvas, A::Canvas),
            (Role::DrawingArea, A::Canvas),
            (Role::Audio, A::Audio),
            (Role::Video, A::Video),
            (Role::Editbar, A::TextInput),
            (Role::Article, A::Article),
            (Role::BlockQuote, A::Blockquote),
            (Role::Caption, A::Caption),
            (Role::Comment, A::Comment),
            (Role::Form, A::Form),
            (Role::Landmark, A::Region),
            (Role::Log, A::Log),
            (Role::Marquee, A::Marquee),
            (Role::Math, A::Math),
            (Role::MathFraction, A::Math),
            (Role::MathRoot, A::Math),
            (Role::Footnote, A::DocFootnote),
            (Role::Timer, A::Timer),
            (Role::Definition, A::Definition),
            (Role::DescriptionList, A::DescriptionList),
            (Role::DescriptionTerm, A::Term),
            (Role::DescriptionValue, A::Definition),
            (Role::Mark, A::Mark),
            (Role::Suggestion, A::Suggestion),
            (Role::ContentDeletion, A::ContentDeletion),
            (Role::ContentInsertion, A::ContentInsertion),
            (Role::Embedded, A::EmbeddedObject),
            (Role::ImageMap, A::Image),
            (Role::Dial, A::Slider),
            (Role::Rating, A::Meter),
            (Role::CHART, A::Figure),
            (Role::Autocomplete, A::ListBox),
            (Role::TearoffMenuItem, A::MenuItem),
            (Role::FileChooser, A::Dialog),
            (Role::FontChooser, A::Dialog),
            (Role::ColorChooser, A::Dialog),
            (Role::DateEditor, A::DateInput),
        ] {
            assert_eq!(map_role(atspi), expected, "{atspi:?}");
        }
    }

    #[test]
    fn structural_roles_stay_filter_transparent() {
        // These carry no semantics a screen reader can use; mapping them to real
        // roles would surface every layout box in the client tree.
        for role in [
            Role::Panel,
            Role::Filler,
            Role::Grouping,
            Role::OptionPane,
            Role::RootPane,
            Role::LayeredPane,
            Role::GlassPane,
            Role::Viewport,
            Role::SplitPane,
            Role::Page,
            Role::Ruler,
            Role::RedundantObject,
            Role::Extended,
            Role::Unknown,
            Role::Invalid,
        ] {
            assert_eq!(map_role(role), accesskit::Role::GenericContainer, "{role:?}");
        }
    }

    #[test]
    fn consumer_keeps_unnamed_containers_out_of_the_tree() {
        use accesskit_consumer::FilterResult;

        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/plain".into(), "/named".into()];
        let mut plain = leaf("/plain", Role::Grouping, "");
        plain.children = vec!["/plain/b".into()];
        let mut named = leaf("/named", Role::Grouping, "Formatting");
        named.children = vec!["/named/b".into()];
        let one = leaf("/plain/b", Role::Button, "One");
        let two = leaf("/named/b", Role::Button, "Two");

        let mut ids = NodeIdMap::new();
        let update = build(&[root, plain, named, one, two], &mut ids);
        let tree = accesskit_consumer::Tree::new(update, false);
        let state = tree.state();
        let node = |path: &str| {
            state
                .node_by_tree_local_id(ids.get(path).unwrap(), TreeId::ROOT)
                .expect("node present in consumer tree")
        };

        assert_eq!(
            accesskit_consumer::common_filter(&node("/plain")),
            FilterResult::ExcludeNode,
            "an unnamed layout container stays transparent",
        );
        assert_eq!(
            accesskit_consumer::common_filter(&node("/named")),
            FilterResult::Include,
            "a named group carries semantics worth surfacing",
        );
    }

    fn states(flags: &[State]) -> StateSet {
        flags.iter().collect()
    }

    #[test]
    fn node_states_distills_toggle_expand_select_popup() {
        use accesskit::Toggled;

        // Checkable/checked/indeterminate → Toggled.
        assert_eq!(
            node_states(states(&[State::Focusable, State::Checkable, State::Checked])).toggled,
            Some(Toggled::True),
        );
        assert_eq!(
            node_states(states(&[State::Checkable, State::Indeterminate])).toggled,
            Some(Toggled::Mixed),
        );
        assert_eq!(
            node_states(states(&[State::Checkable])).toggled,
            Some(Toggled::False),
        );
        // A pressed toggle button reports Pressed, not Checkable.
        assert_eq!(node_states(states(&[State::Pressed])).toggled, Some(Toggled::True));

        // Expandable / Collapsed / Expanded → expanded flag.
        assert_eq!(node_states(states(&[State::Expandable])).expanded, Some(false));
        assert_eq!(
            node_states(states(&[State::Expandable, State::Expanded])).expanded,
            Some(true),
        );

        // Selectable / Selected → selected flag.
        assert_eq!(node_states(states(&[State::Selectable])).selected, Some(false));
        assert_eq!(
            node_states(states(&[State::Selectable, State::Selected])).selected,
            Some(true),
        );

        // Plain nodes leave every applicable-state as absent.
        let plain = node_states(states(&[State::Showing]));
        assert_eq!(plain.toggled, None);
        assert_eq!(plain.expanded, None);
        assert_eq!(plain.selected, None);
        assert!(!plain.has_popup);

        assert!(node_states(states(&[State::HasPopup])).has_popup);
        assert!(node_states(states(&[State::Focusable])).focusable);
        assert!(node_states(states(&[State::Focused])).focused);
    }

    #[test]
    fn node_states_distills_the_full_forwarded_state_set() {
        use accesskit::Orientation;

        // GTK4 never emits State::Enabled (it sets only Sensitive, and omits
        // that when the widget is explicitly disabled); at-spi2-atk emits both.
        // Either one therefore means "not disabled".
        assert!(node_states(states(&[State::Sensitive])).sensitive);
        assert!(node_states(states(&[State::Enabled])).sensitive);
        assert!(!node_states(states(&[State::Focusable])).sensitive);

        assert!(node_states(states(&[State::ReadOnly])).read_only);
        assert!(node_states(states(&[State::Required])).required);
        assert!(node_states(states(&[State::InvalidEntry])).invalid);
        assert!(node_states(states(&[State::Modal])).modal);
        assert!(node_states(states(&[State::Multiselectable])).multiselectable);
        assert!(node_states(states(&[State::Busy])).busy);

        assert_eq!(
            node_states(states(&[State::Horizontal])).orientation,
            Some(Orientation::Horizontal),
        );
        assert_eq!(
            node_states(states(&[State::Vertical])).orientation,
            Some(Orientation::Vertical),
        );
        assert_eq!(
            node_states(states(&[State::Horizontal, State::Vertical])).orientation,
            None,
            "a contradictory orientation is dropped rather than guessed",
        );
        assert_eq!(node_states(states(&[])).orientation, None);
    }

    #[test]
    fn build_node_forwards_the_new_states() {
        use accesskit::Orientation;

        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/off".into(), "/entry".into(), "/bar".into()];

        // Focusable but not sensitive: a genuinely disabled control.
        let mut off = leaf("/off", Role::Button, "Off");
        off.states.focusable = true;

        let mut entry = leaf("/entry", Role::Entry, "E");
        entry.states.sensitive = true;
        entry.states.read_only = true;
        entry.states.required = true;
        entry.states.invalid = true;

        let mut bar = leaf("/bar", Role::ScrollBar, "B");
        bar.states.sensitive = true;
        bar.states.orientation = Some(Orientation::Horizontal);
        bar.states.busy = true;

        let mut ids = NodeIdMap::new();
        let update = build(&[root, off, entry, bar], &mut ids);
        let by_id: HashMap<NodeId, Node> = update.nodes.iter().cloned().collect();
        let node = |path: &str| by_id.get(&ids.get(path).unwrap()).unwrap().clone();

        assert!(node("/off").is_disabled(), "insensitive control is disabled");
        assert!(!node("/entry").is_disabled());
        assert!(node("/entry").is_read_only());
        assert!(node("/entry").is_required());
        assert_eq!(node("/entry").invalid(), Some(accesskit::Invalid::True));
        assert_eq!(node("/bar").orientation(), Some(Orientation::Horizontal));
        assert!(node("/bar").is_busy());
    }

    #[test]
    fn description_reaches_accesskit_and_never_duplicates_the_label() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into(), "/b".into()];
        let mut a = leaf("/a", Role::Button, "Save");
        a.description = "Save the current document".into();
        // GTK duplicates the name into the description on several widgets --
        // forwarding that makes a screen reader say the label twice.
        let mut b = leaf("/b", Role::Button, "Open");
        b.description = "Open".into();

        let mut ids = NodeIdMap::new();
        let update = build(&[root, a, b], &mut ids);
        let by_id: HashMap<NodeId, Node> = update.nodes.iter().cloned().collect();

        assert_eq!(
            by_id[&ids.get("/a").unwrap()].description(),
            Some("Save the current document"),
        );
        assert_eq!(
            by_id[&ids.get("/b").unwrap()].description(),
            None,
            "a description echoing the label is dropped",
        );
    }

    #[test]
    fn toggleable_roles_always_report_a_toggle_state() {
        // LibreOffice/gtk3 omits State::Checkable on an unchecked check button,
        // so its `toggled` would collapse to None and the node would lose its
        // UIA Toggle pattern exactly while unchecked.
        for role in [
            Role::CheckBox,
            Role::CheckMenuItem,
            Role::RadioButton,
            Role::RadioMenuItem,
            Role::ToggleButton,
        ] {
            let node = leaf("/n", role, "x");
            assert_eq!(node.states.toggled, None, "{role:?} reads no toggle state");
            let update = build(std::slice::from_ref(&node), &mut NodeIdMap::new());
            assert_eq!(
                update.nodes[0].1.toggled(),
                Some(accesskit::Toggled::False),
                "{role:?} still reports a toggle",
            );
        }

        let button = leaf("/b", Role::Button, "Go");
        let update = build(std::slice::from_ref(&button), &mut NodeIdMap::new());
        assert_eq!(update.nodes[0].1.toggled(), None, "a plain button is not a toggle");

        let mut mixed = leaf("/c", Role::CheckBox, "x");
        mixed.states.toggled = Some(accesskit::Toggled::Mixed);
        let update = build(std::slice::from_ref(&mixed), &mut NodeIdMap::new());
        assert_eq!(
            update.nodes[0].1.toggled(),
            Some(accesskit::Toggled::Mixed),
            "a read state wins over the floor",
        );
    }

    #[test]
    fn disabled_is_not_stamped_on_inert_containers() {
        // A layout box reports no Sensitive either, but it is not a control and
        // must not reach the client announced as disabled.
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/panel".into()];
        let panel = leaf("/panel", Role::Panel, "");

        let mut ids = NodeIdMap::new();
        let update = build(&[root, panel], &mut ids);
        let by_id: HashMap<NodeId, Node> = update.nodes.iter().cloned().collect();
        assert!(!by_id[&ids.get("/panel").unwrap()].is_disabled());
        assert!(!by_id[&ids.get("/win").unwrap()].is_disabled());
    }

    #[test]
    fn no_node_is_ever_hidden() {
        // is_hidden() makes the consumer drop the whole subtree, and GTK reports
        // Showing=false for scrolled-out rows a screen reader still needs. The
        // walk filters visibility at the window level instead.
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into()];
        let a = leaf("/a", Role::Button, "A");

        let mut ids = NodeIdMap::new();
        let update = build(&[root, a], &mut ids);
        assert!(
            update.nodes.iter().all(|(_, node)| !node.is_hidden()),
            "no mirrored node is ever marked hidden",
        );
    }

    #[test]
    fn build_node_forwards_states_to_accesskit() {
        use accesskit::{HasPopup, Toggled};

        let mut root = leaf("/win", Role::Frame, "App");
        root.children = vec!["/check".into(), "/combo".into(), "/opt".into()];
        let mut check = leaf("/check", Role::CheckMenuItem, "Bold");
        check.states.toggled = Some(Toggled::True);
        let mut combo = leaf("/combo", Role::ComboBox, "Font");
        combo.states.has_popup = true;
        combo.states.expanded = Some(false);
        let mut opt = leaf("/opt", Role::ListItem, "Item");
        opt.states.selected = Some(true);

        let mut ids = NodeIdMap::new();
        let update = build(&[root, check, combo, opt], &mut ids);

        let node = |path: &str| {
            let id = ids.get(path).unwrap();
            update.nodes.iter().find(|(i, _)| *i == id).unwrap().1.clone()
        };
        assert_eq!(node("/check").toggled(), Some(Toggled::True));
        assert_eq!(node("/combo").has_popup(), Some(HasPopup::Menu));
        assert_eq!(node("/combo").is_expanded(), Some(false));
        assert_eq!(node("/opt").is_selected(), Some(true));
    }

    #[test]
    fn builds_window_tree_with_children_and_actions() {
        let mut root = leaf("/win", Role::Frame, "Editor");
        root.children = vec!["/label".into(), "/button".into()];
        let label = leaf("/label", Role::Label, "hello");
        let mut button = leaf("/button", Role::Button, "Click me");
        button.interfaces.insert(Interface::Action);
        button.states.focusable = true;

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
        panel.interfaces.insert(Interface::Action);
        let mut button = leaf("/button", Role::Button, "Go");
        button.interfaces.insert(Interface::Action);

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
        button.states.focused = true;
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
        let update = build_window_update(&[root, doc, p1, p2], &mut ids, &mut WindowCache::default());
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
        let (runs, layout) = build_text_runs("/doc", "hi\nyo", None, None, &mut ids);
        assert_eq!(runs.len(), 2);
        let joined: String = runs.iter().map(|(_, n)| n.value().unwrap()).collect();
        assert_eq!(joined, "hi\nyo");
        for (_, node) in &runs {
            assert_eq!(node.role(), accesskit::Role::TextRun);
        }
        assert_eq!(char_lengths(&runs[0].1), vec![1, 1, 1]);
        assert_eq!(char_lengths(&runs[1].1), vec![1, 1]);
        assert_eq!(layout.iter().map(|r| r.chars).collect::<Vec<_>>(), vec![3, 2]);

        let (runs2, _) = build_text_runs("/doc", "hi\nyo", None, None, &mut ids);
        assert_eq!(
            runs.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            runs2.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "run ids are stable across rebuilds"
        );
    }

    #[test]
    fn empty_text_yields_one_empty_run() {
        let mut ids = NodeIdMap::new();
        let (runs, layout) = build_text_runs("/doc", "", None, None, &mut ids);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1.value(), Some(""));
        assert_eq!(layout[0].chars, 0);
    }

    #[test]
    fn text_position_maps_offsets_across_runs_and_clamps() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "ab\ncd", None, None, &mut ids);
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
        let (_, layout) = build_text_runs("/doc", "ab\ncd", None, None, &mut ids);
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
        let (_, layout) = build_text_runs("/doc", "é\nb", None, None, &mut ids);
        // Offset 1 is the '\n' after the 2-byte 'é', still index 1 (code points).
        assert_eq!(text_position(&layout, 1), TextPosition { node: layout[0].id, character_index: 1 });
        assert_eq!(text_position(&layout, 2), TextPosition { node: layout[1].id, character_index: 0 });
    }

    #[test]
    fn text_selection_direction_and_degenerate_cases() {
        let mut ids = NodeIdMap::new();
        let (_, layout) = build_text_runs("/doc", "abcdef", None, None, &mut ids);
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
        let (runs, _) = build_text_runs("/doc", "foo bar\n", None, None, &mut ids);
        assert_eq!(runs[0].1.word_starts().to_vec(), vec![0u8, 4]);

        // A word start past 255 collapses word info to empty.
        let long = format!("{} b", "a".repeat(300));
        let (long_runs, _) = build_text_runs("/doc2", &long, None, None, &mut ids);
        assert!(long_runs[0].1.word_starts().is_empty());
    }

    #[test]
    fn build_window_update_appends_runs_and_populates_cache() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/doc".into()];
        let doc = text_node("/doc", TextState { text: "hi\n".into(), caret: Some(3), selection: None, extents: None });

        let mut ids = NodeIdMap::new();
        let mut caches = WindowCache::default();
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
        assert!(caches.text.contains_key("/doc"), "text cache populated");

        // Vanished text paths are pruned from the cache on the next build.
        let root2 = leaf("/win", Role::Frame, "w");
        build_window_update(&[root2], &mut ids, &mut caches);
        assert!(!caches.text.contains_key("/doc"), "cache pruned when node absent");
    }

    #[test]
    fn text_cache_records_caret_enabled_per_role() {
        let mut editable = leaf("/e", Role::Text, "");
        editable.text = Some(TextState { text: "x".into(), caret: Some(1), selection: None, extents: None });
        let mut label = leaf("/l", Role::Label, "hi");
        label.text = Some(TextState { text: "hi".into(), caret: None, selection: None, extents: None });

        let mut ids = NodeIdMap::new();
        let mut caches = WindowCache::default();
        build_window_update(&[editable, label], &mut ids, &mut caches);

        assert!(caches.text.get("/e").unwrap().caret_enabled, "editable text has a caret");
        assert!(!caches.text.get("/l").unwrap().caret_enabled, "static label has none");
    }

    #[test]
    fn static_label_with_text_keeps_value_and_gains_runs() {
        let mut label = leaf("/l", Role::Label, "Status: OK");
        label.text = Some(TextState { text: "Status: OK".into(), caret: None, selection: None, extents: None });

        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[label], &mut ids, &mut WindowCache::default());
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
        let update = build_window_update(&[root, a, b], &mut ids, &mut WindowCache::default());
        let mut seen = HashSet::new();
        for (id, _) in &update.nodes {
            assert!(seen.insert(*id), "duplicate node id {id:?} in update");
        }
    }

    #[test]
    fn rebuild_text_node_emits_minimal_deltas() {
        let doc = text_node("/doc", TextState { text: "one\ntwo".into(), caret: Some(0), selection: None, extents: None });
        let mut ids = NodeIdMap::new();
        let mut caches = WindowCache::default();
        build_window_update(&[doc], &mut ids, &mut caches);

        // Caret-only move: exactly the container changes.
        let moved = TextState { text: "one\ntwo".into(), caret: Some(5), selection: None, extents: None };
        let delta = rebuild_text_node(&mut caches, "/doc", &moved, &mut ids);
        assert_eq!(delta.len(), 1, "caret move is a single-node delta");
        assert_eq!(delta[0].0, caches.text["/doc"].node_id);

        // Identical state: nothing changes.
        assert!(rebuild_text_node(&mut caches, "/doc", &moved, &mut ids).is_empty());

        // Editing the second line: container (its selection tracks) + that run.
        let edited = TextState { text: "one\nTWO".into(), caret: Some(7), selection: None, extents: None };
        let ids_before = (caches.text["/doc"].runs[0].0, caches.text["/doc"].runs[1].0);
        let delta = rebuild_text_node(&mut caches, "/doc", &edited, &mut ids);
        let changed_ids: HashSet<NodeId> = delta.iter().map(|(id, _)| *id).collect();
        assert!(changed_ids.contains(&caches.text["/doc"].node_id));
        assert!(changed_ids.contains(&ids_before.1), "edited run 1 is included");
        assert!(!changed_ids.contains(&ids_before.0), "unchanged run 0 is not");
    }

    #[test]
    fn rebuild_text_node_shrinks_run_children() {
        let doc = text_node("/doc", TextState { text: "a\nb\nc".into(), caret: None, selection: None, extents: None });
        let mut ids = NodeIdMap::new();
        let mut caches = WindowCache::default();
        build_window_update(&[doc], &mut ids, &mut caches);
        let node_id = caches.text["/doc"].node_id;

        let shorter = TextState { text: "a".into(), caret: None, selection: None, extents: None };
        let delta = rebuild_text_node(&mut caches, "/doc", &shorter, &mut ids);
        let (_, parent) = delta.iter().find(|(id, _)| *id == node_id).unwrap();
        assert_eq!(parent.children().len(), 1, "container children shrink to one run");
    }

    // --- Per-node refresh ---

    #[test]
    fn refresh_node_returns_nothing_when_the_read_is_unchanged() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into()];
        let mut check = leaf("/a", Role::CheckBox, "Check");
        check.states.toggled = Some(accesskit::Toggled::False);
        check.states.focusable = true;
        check.states.sensitive = true;

        let mut ids = NodeIdMap::new();
        let mut cache = WindowCache::default();
        build_window_update(&[root.clone(), check.clone()], &mut ids, &mut cache);

        let check_id = ids.get("/a").unwrap();
        assert!(
            refresh_node(&check, check_id, &[], &mut ids, &mut cache).is_none(),
            "an identical read emits nothing"
        );
        let root_id = ids.get("/win").unwrap();
        assert!(
            refresh_node(&root, root_id, &["/a".to_owned()], &mut ids, &mut cache).is_none(),
            "a container rebuilt with its emitted children emits nothing"
        );
    }

    #[test]
    fn refresh_node_emits_one_node_for_a_toggle_change() {
        let mut check = leaf("/a", Role::CheckBox, "Check");
        check.states.toggled = Some(accesskit::Toggled::False);
        let mut ids = NodeIdMap::new();
        let mut cache = WindowCache::default();
        build_window_update(&[check.clone()], &mut ids, &mut cache);
        let id = ids.get("/a").unwrap();

        let mut fresh = check.clone();
        fresh.states.toggled = Some(accesskit::Toggled::True);
        let (emitted_id, node) =
            refresh_node(&fresh, id, &[], &mut ids, &mut cache).expect("the toggle changed");

        assert_eq!(emitted_id, id);
        assert_eq!(node.toggled(), Some(accesskit::Toggled::True));
        assert_eq!(
            cache.nodes["/a"].toggled(),
            Some(accesskit::Toggled::True),
            "the cache advances to the emitted node"
        );
        assert!(
            refresh_node(&fresh, id, &[], &mut ids, &mut cache).is_none(),
            "re-reading the same state is a no-op"
        );
    }

    #[test]
    fn refresh_node_ignores_the_fresh_reads_children() {
        let mut root = leaf("/grid", Role::Table, "Sheet");
        root.children = vec!["/a".into(), "/b".into()];
        let mut ids = NodeIdMap::new();
        let mut cache = WindowCache::default();
        build_window_update(
            &[root.clone(), leaf("/a", Role::TableCell, "A"), leaf("/b", Role::TableCell, "B")],
            &mut ids,
            &mut cache,
        );
        let id = ids.get("/grid").unwrap();
        let emitted = vec!["/a".to_owned(), "/b".to_owned()];

        // A lazy grid's fresh read sees thousands of cells the client never got.
        let mut fresh = root.clone();
        fresh.children = (0..5000).map(|index| format!("/cell{index}")).collect();
        fresh.states.busy = true;
        let (_, node) = refresh_node(&fresh, id, &emitted, &mut ids, &mut cache).expect("busy changed");

        assert_eq!(
            node.children(),
            &[ids.get("/a").unwrap(), ids.get("/b").unwrap()],
            "structure comes from what the client holds"
        );
        assert_eq!(ids.get("/cell0"), None, "unemitted children allocate no ids");
    }

    #[test]
    fn refresh_node_preserves_a_text_nodes_runs_and_selection() {
        let doc = text_node(
            "/doc",
            TextState { text: "one\ntwo".into(), caret: Some(2), selection: None, extents: None },
        );
        let mut ids = NodeIdMap::new();
        let mut cache = WindowCache::default();
        build_window_update(std::slice::from_ref(&doc), &mut ids, &mut cache);
        let id = ids.get("/doc").unwrap();
        let runs_before = cache.nodes["/doc"].children().to_vec();
        let selection_before = cache.nodes["/doc"].text_selection().cloned();
        assert_eq!(runs_before.len(), 2);
        assert!(selection_before.is_some());

        // A refresh reads no text, so `text` is None on the fresh node.
        let mut fresh = doc.clone();
        fresh.text = None;
        fresh.states.read_only = true;
        let (_, node) = refresh_node(&fresh, id, &[], &mut ids, &mut cache).expect("read_only changed");

        assert!(node.is_read_only());
        assert_eq!(node.children(), runs_before.as_slice(), "run children survive");
        assert_eq!(node.text_selection(), selection_before.as_ref(), "selection survives");
    }

    #[test]
    fn refresh_node_and_rebuild_text_node_share_one_cache_slot() {
        let doc = text_node(
            "/doc",
            TextState { text: "a".into(), caret: Some(0), selection: None, extents: None },
        );
        let mut ids = NodeIdMap::new();
        let mut cache = WindowCache::default();
        build_window_update(std::slice::from_ref(&doc), &mut ids, &mut cache);
        let id = ids.get("/doc").unwrap();

        let mut fresh = doc.clone();
        fresh.text = None;
        fresh.description = "Document body".into();
        refresh_node(&fresh, id, &[], &mut ids, &mut cache).expect("the description changed");

        let edited = TextState { text: "ab".into(), caret: Some(2), selection: None, extents: None };
        let delta = rebuild_text_node(&mut cache, "/doc", &edited, &mut ids);
        let (_, parent) = delta.iter().find(|(emitted, _)| *emitted == id).expect("container changed");
        assert_eq!(
            parent.description(),
            Some("Document body"),
            "the text rebuild diffs against the refreshed node"
        );
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
        let mut caches = WindowCache::default();
        build_window_update(&[doc], &mut ids, &mut caches);

        // A caret move re-reads no geometry; the cached extents keep the runs
        // identical, so only the container changes.
        let moved = TextState { text: "hi".into(), caret: Some(1), selection: None, extents: None };
        let delta = rebuild_text_node(&mut caches, "/doc", &moved, &mut ids);
        assert_eq!(delta.len(), 1, "caret move stays a container-only delta");
        assert_eq!(delta[0].0, caches.text["/doc"].node_id);
        assert!(
            caches.text["/doc"].runs[0].1.bounds().is_some(),
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
        let mut caches = WindowCache::default();
        build_window_update(&[doc], &mut ids, &mut caches);

        let fresh = vec![ext(10, 0, 8, 16), ext(18, 0, 9, 16)];
        let edited = TextState {
            text: "hx".into(),
            caret: Some(2),
            selection: None,
            extents: Some(fresh.clone()),
        };
        let delta = rebuild_text_node(&mut caches, "/doc", &edited, &mut ids);
        let run_id = caches.text["/doc"].runs[0].0;
        let (_, run) = delta.iter().find(|(id, _)| *id == run_id).unwrap();
        assert_eq!(
            run.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 0.0, x1: 27.0, y1: 16.0 }),
            "edited run carries the fresh geometry"
        );
        assert_eq!(caches.text["/doc"].extents.as_deref(), Some(&fresh[..]), "fresh extents cached");
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
        let mut caches = WindowCache::default();
        build_window_update(&[doc], &mut ids, &mut caches);

        // Text length changed but no fresh extents arrived: stale geometry
        // must be dropped, not misapplied.
        let edited = TextState { text: "hey".into(), caret: Some(3), selection: None, extents: None };
        let delta = rebuild_text_node(&mut caches, "/doc", &edited, &mut ids);
        let run_id = caches.text["/doc"].runs[0].0;
        let (_, run) = delta.iter().find(|(id, _)| *id == run_id).unwrap();
        assert_eq!(run.bounds(), None, "rebuilt run carries no stale geometry");
        assert_eq!(caches.text["/doc"].extents, None, "stale cached extents cleared");
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
        let (runs, _) = build_text_runs("/n", "ab\ncd", Some(&extents), None, &mut ids);

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
        let (runs, _) = build_text_runs("/n", "a\n", Some(&extents), None, &mut ids);

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
        let (runs, _) = build_text_runs("/n", "a\n", Some(&extents), None, &mut ids);

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
        let (runs, _) = build_text_runs("/n", "ab", Some(&extents), None, &mut ids);

        assert_eq!(runs[0].1.bounds(), None);
        assert_eq!(runs[0].1.character_positions(), None);
        assert_eq!(runs[0].1.character_widths(), None);
        assert_eq!(runs[0].1.text_direction(), None);
    }

    #[test]
    fn no_extents_means_no_geometry_properties() {
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "ab", None, None, &mut ids);

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
        let (runs, _) = build_text_runs("/n", "x\ny", Some(&extents), None, &mut ids);

        assert_eq!(runs[1].1.character_positions(), Some(&[0.0][..]), "run-relative, not 40.0");
        assert_eq!(
            runs[1].1.bounds(),
            Some(accesskit::Rect { x0: 40.0, y0: 16.0, x1: 48.0, y1: 32.0 })
        );
    }

    #[test]
    fn empty_text_without_container_bounds_has_no_geometry() {
        let extents: [CharExtent; 0] = [];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "", Some(&extents), None, &mut ids);

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1.bounds(), None);
        assert_eq!(runs[0].1.character_positions(), None);
        assert_eq!(runs[0].1.character_widths(), None);
        assert_eq!(runs[0].1.text_direction(), None);
    }

    #[test]
    fn empty_text_anchors_caret_to_container() {
        let extents: [CharExtent; 0] = [];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs(
            "/n",
            "",
            Some(&extents),
            Some(ext(10, 20, 200, 30)),
            &mut ids,
        );

        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 10.0, y1: 50.0 })
        );
        assert_eq!(runs[0].1.character_positions(), Some(&[][..]));
        assert_eq!(runs[0].1.character_widths(), Some(&[][..]));
        assert_eq!(runs[0].1.text_direction(), Some(accesskit::TextDirection::LeftToRight));
    }

    #[test]
    fn nonempty_text_without_extents_ignores_container() {
        let mut ids = NodeIdMap::new();
        let (runs, _) =
            build_text_runs("/n", "ab", None, Some(ext(10, 20, 200, 30)), &mut ids);
        assert_eq!(runs[0].1.bounds(), None, "a synthetic anchor never mixes with real text");
    }

    #[test]
    fn clearing_text_keeps_the_container_caret_anchor() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/entry".into()];
        let mut entry = leaf("/entry", Role::Entry, "");
        entry.bounds = Some(ext(10, 20, 200, 30));
        entry.text = Some(TextState {
            text: "a".into(),
            caret: Some(1),
            selection: None,
            extents: Some(vec![ext(10, 20, 8, 16)]),
        });
        let mut ids = NodeIdMap::new();
        let mut caches = WindowCache::default();
        build_window_update(&[root, entry], &mut ids, &mut caches);

        let cleared = TextState {
            text: String::new(),
            caret: Some(0),
            selection: None,
            extents: Some(Vec::new()),
        };
        let changed = rebuild_text_node(&mut caches, "/entry", &cleared, &mut ids);
        let run_id = ids.get("/entry#run0").unwrap();
        let (_, run) = changed
            .iter()
            .find(|(id, _)| *id == run_id)
            .expect("emptied run re-emitted");
        assert_eq!(
            run.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 10.0, y1: 50.0 })
        );
    }

    #[test]
    fn consumer_exposes_caret_anchor_on_empty_field() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/entry".into()];
        let mut entry = leaf("/entry", Role::Entry, "");
        entry.bounds = Some(ext(10, 20, 200, 30));
        entry.text = Some(TextState {
            text: String::new(),
            caret: Some(0),
            selection: None,
            extents: Some(Vec::new()),
        });
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root, entry], &mut ids, &mut WindowCache::default());
        let run_id = ids.get("/entry#run0").unwrap();

        let tree = accesskit_consumer::Tree::new(update, false);
        let state = tree.state();
        let run = state
            .node_by_tree_local_id(run_id, accesskit::TreeId::ROOT)
            .expect("empty run present");
        assert_eq!(
            run.bounding_box(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 10.0, y1: 50.0 })
        );
    }

    #[test]
    fn leading_zero_extent_backfills_from_first_real_char() {
        let extents = [ext(0, 0, 0, 0), ext(10, 16, 8, 16)];
        let mut ids = NodeIdMap::new();
        let (runs, _) = build_text_runs("/n", "\na", Some(&extents), None, &mut ids);

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
        let update = build_window_update(&[root, label], &mut ids, &mut WindowCache::default());
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

    #[test]
    fn container_bounds_become_node_bounds() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.bounds = Some(ext(10, 20, 200, 30));
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root], &mut ids, &mut WindowCache::default());
        assert_eq!(
            update.nodes[0].1.bounds(),
            Some(accesskit::Rect { x0: 10.0, y0: 20.0, x1: 210.0, y1: 50.0 })
        );
    }

    #[test]
    fn zero_area_container_bounds_are_dropped() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.bounds = Some(ext(10, 20, 0, 30));
        let mut ids = NodeIdMap::new();
        let update = build_window_update(&[root], &mut ids, &mut WindowCache::default());
        assert_eq!(update.nodes[0].1.bounds(), None);
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
            &mut WindowCache::default(),
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
            &mut WindowCache::default(),
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
            splice_chain_update(&[fresh_table, cell], &[], &known, &mut ids, &mut WindowCache::default())
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
            &mut WindowCache::default(),
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
                &mut WindowCache::default(),
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
        assert!(splice_chain_update(&[table], &[], &known, &mut ids, &mut WindowCache::default())
            .is_none());
        assert!(splice_chain_update(&[], &[], &known, &mut ids, &mut WindowCache::default()).is_none());
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
        let mut caches = WindowCache::default();

        let result = splice_chain_update(&[fresh_doc, p], &[], &known, &mut ids, &mut caches)
            .expect("chain splices");

        let run_id = ids.get("/doc/p#run0").expect("run id allocated");
        assert!(result.update.nodes.iter().any(|(id, _)| *id == run_id));
        assert!(caches.text.contains_key("/doc/p"), "text cache recorded for later deltas");
    }

    #[test]
    fn splice_evicts_stale_text_cache_for_run_less_chain_nodes() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/doc".into()];
        let mut doc = leaf("/doc", Role::DocumentText, "");
        doc.text = Some(TextState {
            text: "old".into(),
            caret: None,
            selection: None,
            extents: None,
        });
        let mut ids = NodeIdMap::new();
        let mut caches = WindowCache::default();
        build_window_update(&[root, doc], &mut ids, &mut caches);
        assert!(caches.text.contains_key("/doc"), "walked text leaf seeds a cache");

        let mut fresh_doc = leaf("/doc", Role::DocumentText, "");
        fresh_doc.children = vec!["/doc/p".into()];
        let p = leaf("/doc/p", Role::Paragraph, "");
        let known: HashSet<String> = ["/win".to_owned(), "/doc".to_owned()].into();
        splice_chain_update(&[fresh_doc, p], &[], &known, &mut ids, &mut caches)
            .expect("chain splices");

        assert!(
            !caches.text.contains_key("/doc"),
            "a chain node emitted without runs sheds its stale cache"
        );
    }

    #[test]
    fn splice_filters_unknown_fresh_children_of_interior_nodes() {
        let table = leaf("/table", Role::Table, "grid");
        let mut row = leaf("/table/row", Role::Panel, "");
        row.children = vec!["/table/row/x1".into(), "/table/row/cell".into()];
        let cell = leaf("/table/row/cell", Role::TableCell, "A1");
        let known: HashSet<String> = ["/table".to_owned()].into();
        let mut ids = NodeIdMap::new();

        let result = splice_chain_update(
            &[table, row, cell],
            &[],
            &known,
            &mut ids,
            &mut WindowCache::default(),
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
        assert_eq!(row_node.children(), &[cell_id], "unknown fresh child filtered out");
        assert_eq!(
            result.children.iter().find(|(p, _)| p == "/table/row").unwrap().1,
            vec!["/table/row/cell".to_owned()],
            "bookkeeping records only the filtered children"
        );
        assert!(ids.get("/table/row/x1").is_none(), "no id allocated for the unknown child");
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
        let mut caches = WindowCache::default();
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

    #[test]
    fn consumer_survives_rewalk_that_drops_the_spliced_focus() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/table".into()];
        let table = leaf("/table", Role::Table, "grid");
        let walk_nodes = vec![root, table];
        let known: HashSet<String> = ["/win".to_owned(), "/table".to_owned()].into();
        let chain = || {
            let mut fresh_table = leaf("/table", Role::Table, "grid");
            fresh_table.children = vec!["/table/cell".into()];
            vec![fresh_table, leaf("/table/cell", Role::TableCell, "A1")]
        };

        let mut ids = NodeIdMap::new();
        let mut caches = WindowCache::default();
        let full_a = build_window_update(&walk_nodes, &mut ids, &mut caches);
        let mut tree = accesskit_consumer::Tree::new(full_a, false);

        let splice = splice_chain_update(&chain(), &[], &known, &mut ids, &mut caches)
            .expect("chain splices");
        let cell_id = ids.get("/table/cell").unwrap();
        assert_eq!(splice.update.focus, cell_id);
        tree.update_and_process_changes(splice.update, &mut NoOpChanges);

        let mut full_b = build_window_update(&walk_nodes, &mut ids, &mut caches);
        assert_ne!(full_b.focus, cell_id, "the walk alone reverts focus");
        let resplice = splice_chain_update(&chain(), &[], &known, &mut ids, &mut caches)
            .expect("re-splice applies");
        merge_update(&mut full_b, resplice.update);
        tree.update_and_process_changes(full_b, &mut NoOpChanges);

        let state = tree.state();
        let cell_node = state
            .node_by_tree_local_id(cell_id, accesskit::TreeId::ROOT)
            .expect("cell survives the merged rewalk");
        assert_eq!(state.focus_id_in_tree(), cell_node.id());
    }

    #[test]
    fn value_interface_state_reaches_accesskit_range_properties() {
        let mut node = leaf("/slider", Role::Slider, "Volume");
        node.value = Some(ValueState { current: 5.0, minimum: 0.0, maximum: 10.0, step: 1.0 });

        let update = build(std::slice::from_ref(&node), &mut NodeIdMap::new());
        let built = &update.nodes[0].1;

        assert_eq!(built.numeric_value(), Some(5.0));
        assert_eq!(built.min_numeric_value(), Some(0.0));
        assert_eq!(built.max_numeric_value(), Some(10.0));
        assert_eq!(built.numeric_value_step(), Some(1.0));
    }

    #[test]
    fn degenerate_value_range_emits_only_the_current_value() {
        let mut flat_range = leaf("/flat", Role::Slider, "Flat");
        flat_range.value = Some(ValueState { current: 3.0, minimum: 4.0, maximum: 4.0, step: 1.0 });

        let mut nan_step = leaf("/nan-step", Role::Slider, "Jumpy");
        nan_step.value =
            Some(ValueState { current: 3.0, minimum: 0.0, maximum: 10.0, step: f64::NAN });

        for node in [flat_range, nan_step] {
            let update = build(std::slice::from_ref(&node), &mut NodeIdMap::new());
            let built = &update.nodes[0].1;
            assert_eq!(built.numeric_value(), Some(3.0), "{}", node.path);
            assert_eq!(built.min_numeric_value(), None, "{}", node.path);
            assert_eq!(built.max_numeric_value(), None, "{}", node.path);
            assert_eq!(built.numeric_value_step(), None, "{}", node.path);
        }
    }

    #[test]
    fn table_counts_reach_accesskit() {
        let mut grid = leaf("/grid", Role::Table, "Grid");
        grid.table = Some(TableState { rows: 5, columns: 3 });
        let update = build(std::slice::from_ref(&grid), &mut NodeIdMap::new());
        let built = &update.nodes[0].1;
        assert_eq!(built.row_count(), Some(5));
        assert_eq!(built.column_count(), Some(3));

        let mut negative_counts = leaf("/broken-grid", Role::Table, "Broken");
        negative_counts.table = Some(TableState { rows: -1, columns: -1 });
        let update = build(std::slice::from_ref(&negative_counts), &mut NodeIdMap::new());
        let built = &update.nodes[0].1;
        assert_eq!(built.row_count(), None);
        assert_eq!(built.column_count(), None);
    }

    #[test]
    fn cell_coordinates_reach_accesskit() {
        let mut cell = leaf("/grid/cell", Role::TableCell, "A1");
        cell.cell = Some(CellState { row: 2, column: 3, row_span: 1, column_span: 2 });
        let update = build(std::slice::from_ref(&cell), &mut NodeIdMap::new());
        let built = &update.nodes[0].1;
        assert_eq!(built.row_index(), Some(2));
        assert_eq!(built.column_index(), Some(3));
        assert_eq!(built.row_span(), Some(1));
        assert_eq!(built.column_span(), Some(2));

        let mut spanless = leaf("/grid/spanless", Role::TableCell, "B1");
        spanless.cell = Some(CellState { row: 4, column: 0, row_span: 0, column_span: -1 });
        let update = build(std::slice::from_ref(&spanless), &mut NodeIdMap::new());
        let built = &update.nodes[0].1;
        assert_eq!(built.row_index(), Some(4));
        assert_eq!(built.column_index(), Some(0));
        assert_eq!(built.row_span(), None);
        assert_eq!(built.column_span(), None);
    }

    #[test]
    fn value_reads_are_gated_to_value_bearing_roles() {
        for role in [
            Role::Slider,
            Role::SpinButton,
            Role::ProgressBar,
            Role::LevelBar,
            Role::ScrollBar,
            Role::Dial,
        ] {
            assert!(role_reads_value(role), "{role:?} carries a value");
        }
        for role in [Role::MenuItem, Role::TableCell, Role::Label, Role::Paragraph] {
            assert!(!role_reads_value(role), "{role:?} must not read Value");
        }
    }

    // --- Object attributes and relations (Phase 5) ---

    #[test]
    fn parse_attributes_reads_placeholder_level_posinset_and_setsize() {
        let mut map = HashMap::new();
        map.insert("placeholder-text".to_owned(), "Search…".to_owned());
        map.insert("level".to_owned(), "2".to_owned());
        map.insert("posinset".to_owned(), "3".to_owned());
        map.insert("setsize".to_owned(), "9".to_owned());
        let attrs = parse_attributes(&map);
        assert_eq!(attrs.placeholder, Some("Search…".to_owned()));
        assert_eq!(attrs.level, Some(2));
        assert_eq!(attrs.position_in_set, Some(3));
        assert_eq!(attrs.size_of_set, Some(9));

        let mut bad = HashMap::new();
        bad.insert("level".to_owned(), "x".to_owned());
        bad.insert("placeholder-text".to_owned(), String::new());
        bad.insert("posinset".to_owned(), "-1".to_owned());
        let bad_attrs = parse_attributes(&bad);
        assert_eq!(bad_attrs.level, None, "non-numeric level is dropped");
        assert_eq!(bad_attrs.placeholder, None, "empty placeholder-text is dropped");
        assert_eq!(bad_attrs.position_in_set, None, "negative posinset is dropped");

        let mut unknown = HashMap::new();
        unknown.insert("toolkit".to_owned(), "gtk".to_owned());
        unknown.insert("keyshortcuts".to_owned(), "Ctrl+F".to_owned());
        unknown.insert("xml-roles".to_owned(), "search".to_owned());
        assert_eq!(
            parse_attributes(&unknown),
            NodeAttributes::default(),
            "unrecognized keys are ignored"
        );
    }

    #[test]
    fn attribute_reads_are_gated_to_roles_that_can_carry_them() {
        for role in [
            Role::Text,
            Role::PasswordText,
            Role::Entry,
            Role::Editbar,
            Role::Terminal,
            Role::Heading,
            Role::ListItem,
            Role::TreeItem,
            Role::RadioButton,
            Role::PageTab,
            Role::TableCell,
        ] {
            assert!(role_reads_attributes(role), "{role:?} should read attributes");
        }
        for role in [
            Role::MenuItem,
            Role::CheckMenuItem,
            Role::RadioMenuItem,
            Role::Panel,
            Role::Filler,
            Role::Label,
            Role::Paragraph,
        ] {
            assert!(!role_reads_attributes(role), "{role:?} should not read attributes");
        }

        for role in [
            Role::Panel,
            Role::Grouping,
            Role::PageTab,
            Role::ToggleButton,
            Role::Button,
            Role::CheckBox,
            Role::RadioButton,
            Role::Text,
            Role::ScrollBar,
            Role::ScrollPane,
            Role::ComboBox,
            Role::SpinButton,
            Role::Slider,
        ] {
            assert!(role_reads_relations(role), "{role:?} should read relations");
        }
        for role in [
            Role::MenuItem,
            Role::CheckMenuItem,
            Role::Filler,
            Role::Label,
            Role::Paragraph,
            Role::ListItem,
            Role::TableCell,
            Role::Section,
        ] {
            assert!(!role_reads_relations(role), "{role:?} should not read relations");
        }
    }

    #[test]
    fn relations_map_to_accesskit_and_drop_unwalked_targets() {
        let mut a = leaf("/a", Role::Panel, "A");
        let b = leaf("/b", Role::Panel, "B");
        a.relations = vec![
            Relation {
                kind: RelationKind::LabelledBy,
                targets: vec![b.path.clone(), "/unwalked/path".to_owned()],
            },
            Relation { kind: RelationKind::Controls, targets: vec![b.path.clone()] },
        ];

        let mut ids = NodeIdMap::new();
        let update = build(&[a, b], &mut ids);
        let b_id = ids.get("/b").expect("B was walked");
        let a_id = ids.get("/a").unwrap();
        let a_node = &update.nodes.iter().find(|(id, _)| *id == a_id).unwrap().1;

        assert_eq!(a_node.labelled_by(), &[b_id], "labelled_by resolves the walked target");
        assert_eq!(a_node.controls(), &[b_id], "controls resolves the walked target");
        assert_eq!(
            ids.get("/unwalked/path"),
            None,
            "a relation target that was never walked allocates no id"
        );
    }

    #[test]
    fn forward_relations_reach_accesskit_by_kind() {
        let mut a = leaf("/a", Role::Text, "A");
        let target = leaf("/target", Role::Panel, "T");
        a.relations = vec![
            Relation { kind: RelationKind::DescribedBy, targets: vec![target.path.clone()] },
            Relation { kind: RelationKind::Details, targets: vec![target.path.clone()] },
            Relation { kind: RelationKind::ErrorMessage, targets: vec![target.path.clone()] },
        ];

        let mut ids = NodeIdMap::new();
        let update = build(&[a, target], &mut ids);
        let target_id = ids.get("/target").unwrap();
        let a_id = ids.get("/a").unwrap();
        let a_node = &update.nodes.iter().find(|(id, _)| *id == a_id).unwrap().1;

        assert_eq!(a_node.described_by(), &[target_id]);
        assert_eq!(a_node.details(), &[target_id]);
        assert_eq!(a_node.error_message(), Some(target_id));
    }

    #[test]
    fn popup_for_is_parsed_but_never_emitted() {
        assert_eq!(map_relation(atspi::RelationType::PopupFor), Some(RelationKind::PopupFor));

        let mut modal = leaf("/popup", Role::Panel, "Popup");
        let owner = leaf("/owner", Role::Panel, "Owner");
        modal.relations =
            vec![Relation { kind: RelationKind::PopupFor, targets: vec![owner.path.clone()] }];

        let mut ids = NodeIdMap::new();
        let update = build(&[modal, owner], &mut ids);
        let modal_id = ids.get("/popup").unwrap();
        let modal_node = &update.nodes.iter().find(|(id, _)| *id == modal_id).unwrap().1;

        assert!(modal_node.labelled_by().is_empty());
        assert!(modal_node.controls().is_empty());
        assert!(modal_node.described_by().is_empty());
        assert!(modal_node.details().is_empty());
        assert!(modal_node.error_message().is_none());
    }

    #[test]
    fn map_relation_consumes_forward_directions_only() {
        use atspi::RelationType;

        for reverse in [RelationType::LabelFor, RelationType::ControlledBy, RelationType::DescriptionFor]
        {
            assert_eq!(map_relation(reverse), None, "{reverse:?} is the reverse direction");
        }
        assert_eq!(map_relation(RelationType::LabelledBy), Some(RelationKind::LabelledBy));
        assert_eq!(map_relation(RelationType::ControllerFor), Some(RelationKind::Controls));
        assert_eq!(map_relation(RelationType::DescribedBy), Some(RelationKind::DescribedBy));
        assert_eq!(map_relation(RelationType::Details), Some(RelationKind::Details));
        assert_eq!(map_relation(RelationType::ErrorMessage), Some(RelationKind::ErrorMessage));
    }

    #[test]
    fn attributes_reach_accesskit_node_properties() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/search".into(), "/heading".into()];
        let mut search = leaf("/search", Role::Text, "");
        search.attributes.placeholder = Some("Search…".to_owned());
        let mut heading = leaf("/heading", Role::Heading, "Title");
        heading.attributes.level = Some(2);
        heading.attributes.position_in_set = Some(3);
        heading.attributes.size_of_set = Some(9);

        let mut ids = NodeIdMap::new();
        let update = build(&[root, search, heading], &mut ids);
        let node = |path: &str| {
            let id = ids.get(path).unwrap();
            update.nodes.iter().find(|(i, _)| *i == id).unwrap().1.clone()
        };

        assert_eq!(node("/search").placeholder(), Some("Search…"));
        assert_eq!(node("/heading").level(), Some(2));
        assert_eq!(node("/heading").position_in_set(), Some(3));
        assert_eq!(node("/heading").size_of_set(), Some(9));
    }

    #[test]
    fn refresh_node_preserves_relations_on_unchanged_reads() {
        let mut root = leaf("/win", Role::Frame, "w");
        root.children = vec!["/a".into(), "/b".into()];
        let mut a = leaf("/a", Role::Panel, "A");
        let b = leaf("/b", Role::Panel, "B");
        a.relations = vec![Relation { kind: RelationKind::LabelledBy, targets: vec![b.path.clone()] }];

        let mut ids = NodeIdMap::new();
        let mut cache = WindowCache::default();
        build_window_update(&[root.clone(), a.clone(), b.clone()], &mut ids, &mut cache);
        let a_id = ids.get("/a").unwrap();

        // Refreshing with the identical relation set emits nothing.
        let unchanged =
            refresh_node(&a, a_id, &[], &mut ids, &mut cache);
        assert!(unchanged.is_none(), "identical relation set yields no delta");

        // Retargeting the relation must be picked up as a change.
        let mut retargeted = a.clone();
        retargeted.relations =
            vec![Relation { kind: RelationKind::LabelledBy, targets: vec!["/other".to_owned()] }];
        let changed = refresh_node(&retargeted, a_id, &[], &mut ids, &mut cache);
        assert!(changed.is_some(), "a retargeted relation must be re-emitted");
    }
}
