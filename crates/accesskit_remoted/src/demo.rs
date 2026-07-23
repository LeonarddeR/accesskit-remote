//! A built-in demo tree source: one window with a label and a button whose
//! click counter updates the label. Lets consumers exercise the full pipe
//! (handshake, sync, live updates, actions) without an AT-SPI session.

use accesskit_remote::{AppInfo, WindowId};
use accesskit_remote_server::WindowDescriptor;

const WINDOW: WindowId = WindowId(1);
const ROOT: accesskit::NodeId = accesskit::NodeId(0);
const LABEL: accesskit::NodeId = accesskit::NodeId(1);
const BUTTON: accesskit::NodeId = accesskit::NodeId(2);

pub struct DemoSource {
    clicks: u32,
    focused: accesskit::NodeId,
}

impl DemoSource {
    pub fn new() -> Self {
        Self {
            clicks: 0,
            focused: BUTTON,
        }
    }

    pub fn initial_state(&self) -> Vec<(WindowDescriptor, accesskit::TreeUpdate)> {
        let descriptor = WindowDescriptor {
            id: WINDOW,
            title: "AccessKit Remote Demo".into(),
            app: AppInfo {
                name: "accesskit_remoted".into(),
                app_id: Some("dev.accesskit.RemoteDemo".into()),
                pid: Some(std::process::id()),
                toolkit: Some("accesskit_remoted demo".into()),
                toolkit_version: None,
            },
        };
        vec![(descriptor, self.full_tree())]
    }

    pub fn focus(&self) -> Option<WindowId> {
        Some(WINDOW)
    }

    /// Applies an action and returns the resulting tree delta, if any.
    pub fn perform(
        &mut self,
        window: WindowId,
        request: &accesskit::ActionRequest,
    ) -> Option<accesskit::TreeUpdate> {
        if window != WINDOW {
            return None;
        }
        match request.action {
            accesskit::Action::Click if request.target_node == BUTTON => {
                self.clicks += 1;
                Some(accesskit::TreeUpdate {
                    nodes: vec![(LABEL, self.label_node())],
                    tree: None,
                    tree_id: accesskit::TreeId::ROOT,
                    focus: self.focused,
                })
            }
            accesskit::Action::Focus
                if [ROOT, LABEL, BUTTON].contains(&request.target_node) =>
            {
                self.focused = request.target_node;
                Some(accesskit::TreeUpdate {
                    nodes: Vec::new(),
                    tree: None,
                    tree_id: accesskit::TreeId::ROOT,
                    focus: self.focused,
                })
            }
            _ => None,
        }
    }

    fn full_tree(&self) -> accesskit::TreeUpdate {
        let mut root = accesskit::Node::new(accesskit::Role::Window);
        root.set_label("AccessKit Remote Demo");
        root.set_children(vec![LABEL, BUTTON]);
        let mut button = accesskit::Node::new(accesskit::Role::Button);
        button.set_label("Click me");
        button.add_action(accesskit::Action::Click);
        button.add_action(accesskit::Action::Focus);
        accesskit::TreeUpdate {
            nodes: vec![(ROOT, root), (LABEL, self.label_node()), (BUTTON, button)],
            tree: Some(accesskit::Tree::new(ROOT)),
            tree_id: accesskit::TreeId::ROOT,
            focus: self.focused,
        }
    }

    fn label_node(&self) -> accesskit::Node {
        let mut label = accesskit::Node::new(accesskit::Role::Label);
        label.set_value(format!("Button clicked {} times", self.clicks));
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_updates_label() {
        let mut source = DemoSource::new();
        let update = source
            .perform(
                WINDOW,
                &accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: BUTTON,
                    data: None,
                },
            )
            .unwrap();
        assert_eq!(update.nodes.len(), 1);
        let (id, node) = &update.nodes[0];
        assert_eq!(*id, LABEL);
        assert_eq!(node.value().unwrap(), "Button clicked 1 times");
    }

    #[test]
    fn ignores_unknown_targets() {
        let mut source = DemoSource::new();
        assert!(source
            .perform(
                WindowId(99),
                &accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: BUTTON,
                    data: None,
                },
            )
            .is_none());
    }
}
