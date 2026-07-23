//! Pure, bus-free translation from a walked AT-SPI subtree into an AccessKit
//! [`TreeUpdate`]. Everything here operates on plain data so it can be unit
//! tested without a live accessibility bus.

use accesskit::{Node, NodeId, Tree, TreeId, TreeUpdate};
use atspi::{Role, State, StateSet};
use std::collections::HashMap;

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

/// Builds a full-tree [`TreeUpdate`] for one window. `nodes[0]` is the window
/// root; focus lands on the node marked focused, or the root if none is.
///
/// Panics if `nodes` is empty.
pub fn build_window_update(nodes: &[MirrorNode], ids: &mut NodeIdMap) -> TreeUpdate {
    let root_id = ids.id_for(&nodes[0].path);
    let mut out = Vec::with_capacity(nodes.len());
    let mut focus = root_id;
    for node in nodes {
        let id = ids.id_for(&node.path);
        if node.focused {
            focus = id;
        }
        out.push((id, build_node(node, ids)));
    }
    TreeUpdate {
        nodes: out,
        tree: Some(Tree::new(root_id)),
        tree_id: TreeId::ROOT,
        focus,
    }
}

fn build_node(node: &MirrorNode, ids: &mut NodeIdMap) -> Node {
    let role = map_role(node.role);
    let mut out = Node::new(role);
    if !node.name.is_empty() {
        if role == accesskit::Role::Label {
            out.set_value(node.name.clone());
        } else {
            out.set_label(node.name.clone());
        }
    }
    let children: Vec<NodeId> = node.children.iter().map(|p| ids.id_for(p)).collect();
    if !children.is_empty() {
        out.set_children(children);
    }
    if node.actionable && is_clickable_role(node.role) {
        out.add_action(accesskit::Action::Click);
    }
    if node.focusable {
        out.add_action(accesskit::Action::Focus);
    }
    out
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
        }
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
        let update = build_window_update(&[root, label, button], &mut ids);

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
        let update = build_window_update(&[panel, button], &mut ids);
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
        let update = build_window_update(&[label], &mut ids);
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
        let update = build_window_update(&[root, button], &mut ids);
        assert_eq!(update.focus, ids.get("/button").unwrap());
    }
}
