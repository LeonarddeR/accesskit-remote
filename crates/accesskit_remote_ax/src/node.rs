//! One element flattened into the fields the mapping needs, and the single
//! function that turns it into an `accesskit::Node`.
//!
//! Mirrors the AT-SPI source's `MirrorNode` + `build_container` split, and for
//! the same load-bearing reason: **`accesskit::Node` compares property values
//! in insertion order**. A node built by the walk and the same node rebuilt by
//! a later refresh must go through one function, in one order, or an unchanged
//! read emits a spurious delta. Sharing [`build_container`] is what makes
//! "unchanged ⇒ no wire traffic" hold, and it is why the refresh path may never
//! grow its own property-setting code.
//!
//! Reading is batched. AX has no interface set to gate reads on, so unlike
//! AT-SPI there is nothing to consult before asking — every element is asked
//! for the same attribute set, and
//! [`AXUIElementCopyMultipleAttributeValues`][crate::attr::multiple] makes that
//! one IPC crossing rather than eleven.

use crate::attr::{self, AxError};
use crate::element::ElementKey;
use crate::names::Names;
use crate::role;
use accesskit::{Node, Role};
use objc2_core_foundation::{CFRetained, CFString, CFType, CGRect};

/// The attributes every element is asked for, in one batch.
///
/// Order is the contract between [`read`] and the indices below; they are read
/// together so the walk costs one round trip per element.
fn batch(names: &Names) -> [CFRetained<CFString>; 9] {
    [
        names.role.clone(),
        names.subrole.clone(),
        names.title.clone(),
        names.description.clone(),
        names.value.clone(),
        names.frame.clone(),
        names.enabled.clone(),
        names.focused.clone(),
        names.selected.clone(),
    ]
}

const I_ROLE: usize = 0;
const I_SUBROLE: usize = 1;
const I_TITLE: usize = 2;
const I_DESCRIPTION: usize = 3;
const I_VALUE: usize = 4;
const I_FRAME: usize = 5;
const I_ENABLED: usize = 6;
const I_FOCUSED: usize = 7;
const I_SELECTED: usize = 8;

/// The states the mapping distils from an element's attributes.
///
/// Tri-state where absence is meaningful: `Some(false)` means the element has
/// the concept and it is off, `None` means it has no such concept at all. A
/// checkbox that is unchecked and a label that cannot be checked must not look
/// alike — the first gets a Toggle pattern, the second must not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeStates {
    /// `None` when the element does not report enablement. Stored positively:
    /// only an explicit `false` disables.
    pub enabled: Option<bool>,
    /// Whether focus can be *placed* here, taken from `AXFocused` being
    /// settable rather than from its value. AX has no separate "focusable",
    /// and settability is the honest answer — it is also what the drive-back
    /// phase will consult before planning a focus write.
    pub focusable: bool,
    pub focused: bool,
    pub selected: Option<bool>,
    /// Whether `AXValue` can be written, which is what makes `SetValue`
    /// worth advertising to a consumer.
    pub value_settable: bool,
}

/// One element, flattened.
pub struct AxNode {
    pub key: ElementKey,
    pub role: String,
    pub subrole: Option<String>,
    /// The element's name. AX splits this across `AXTitle` and `AXDescription`
    /// and different toolkits favour different ones, so both are kept and the
    /// mapping decides.
    pub title: String,
    pub description: String,
    /// `AXValue`, uninterpreted: it is a string on a text field, a number on a
    /// slider and a boolean on a checkbox, so only the role can read it.
    pub value: Option<CFRetained<CFType>>,
    /// Screen-space frame. Converted to window-relative by the caller, which
    /// is the only place that knows the window origin.
    pub frame: Option<CGRect>,
    pub states: NodeStates,
    /// The AX action names the element reports. Declaring an action a consumer
    /// can request is separate from being able to carry it out, and both are
    /// needed: AccessKit's Windows adapter gates `InvokePattern` on the node
    /// declaring `Action::Click`.
    pub actions: Vec<String>,
    pub children: Vec<ElementKey>,
    /// The UTF-16 selection range, read only for elements that carry text.
    pub selected_range: Option<(usize, usize)>,
}

impl AxNode {
    /// The AccessKit role this element maps to, subrole included.
    pub fn accesskit_role(&self) -> Role {
        role::refine(
            role::map(&self.role, self.subrole.as_deref()),
            !self.name().is_empty(),
        )
    }

    /// The element's accessible name: `AXTitle` when it has one, else
    /// `AXDescription`. Toolkits disagree about which to populate — of the
    /// elements surveyed on a real desktop, 29% carried a title and 54% a
    /// description — so neither alone is sufficient.
    pub fn name(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else {
            &self.description
        }
    }
}

/// Whether this role's text should be exposed as `TextRun` children.
///
/// The UIA Text pattern needs a role in `supports_text_ranges` with at least
/// one `TextRun` child, so this set decides where text is readable at all.
/// Deliberately narrow: giving every label a caret is what made GTK report a
/// degenerate caret-at-zero on every static string.
pub fn has_text_runs(role: Role) -> bool {
    matches!(
        role,
        Role::TextInput
            | Role::MultilineTextInput
            | Role::SearchInput
            | Role::PasswordInput
            | Role::Document
            | Role::Terminal
    )
}

/// Reads one element's whole attribute set in a single round trip.
///
/// Children are read separately: they are an element array rather than a
/// scalar, and asking for them in the same batch would mean retaining every
/// child of every node even where the walk is about to stop.
pub fn read(key: ElementKey, names: &Names) -> Result<AxNode, AxError> {
    let values = attr::multiple(key.element(), &batch(names))?;
    let text = |index: usize| -> String {
        values
            .get(index)
            .and_then(|v| v.as_deref())
            .and_then(attr::as_string)
            .unwrap_or_default()
    };
    let flag = |index: usize| -> Option<bool> {
        values.get(index).and_then(|v| v.as_deref()).and_then(attr::as_bool)
    };

    let role = text(I_ROLE);
    let subrole = values
        .get(I_SUBROLE)
        .and_then(|v| v.as_deref())
        .and_then(attr::as_string)
        .filter(|s| !s.is_empty());

    let mut node = AxNode {
        role,
        subrole,
        title: text(I_TITLE),
        description: text(I_DESCRIPTION),
        value: values.get(I_VALUE).and_then(|v| v.clone()),
        frame: values.get(I_FRAME).and_then(|v| v.as_deref()).and_then(attr::as_rect),
        states: NodeStates {
            enabled: flag(I_ENABLED),
            // Settability, not value: see `NodeStates::focusable`. This is a
            // second round trip, and the only one the batch cannot absorb.
            focusable: attr::is_settable(key.element(), &names.focused).unwrap_or(false),
            focused: flag(I_FOCUSED).unwrap_or(false),
            selected: flag(I_SELECTED),
            value_settable: attr::is_settable(key.element(), &names.value).unwrap_or(false),
        },
        selected_range: None,
        actions: attr::action_names(key.element()).unwrap_or_default(),
        children: attr::elements(key.element(), &names.children)
            .unwrap_or_default()
            .into_iter()
            .map(|child| ElementKey::new(key.pid(), child))
            .collect(),
        key,
    };
    // Only text elements pay for the selection read.
    if has_text_runs(node.accesskit_role()) {
        node.selected_range = attr::value(node.key.element(), &names.selected_text_range)
            .ok()
            .flatten()
            .as_deref()
            .and_then(attr::as_range);
    }
    Ok(node)
}

/// Builds everything about a node that comes from its own read: role, name,
/// description, state and bounds.
///
/// Children and text are added by the caller, which knows the tree's structure.
/// **Every property write for a node goes through here**, in this order — see
/// the module docs.
pub fn build_container(node: &AxNode, window_origin: Option<(f64, f64)>) -> Node {
    let role = node.accesskit_role();
    let mut container = Node::new(role);

    let name = node.name();
    if !name.is_empty() {
        // A label's text *is* its value, not a name pointing at other content.
        if role == Role::Label {
            container.set_value(name);
        } else {
            container.set_label(name);
        }
    } else if role == Role::Label {
        // `AXStaticText` carries its text in `AXValue`, not `AXTitle` — which
        // is why Calculator's display and most of System Settings' sidebar
        // arrived empty, and a large part of why 65% of elements had no name.
        if let Some(text) = node.value.as_deref().and_then(attr::as_string) {
            if !text.is_empty() {
                container.set_value(text);
            }
        }
    }
    // Only worth carrying when it says something the name did not.
    if !node.description.is_empty() && node.description != name {
        container.set_description(node.description.clone());
    }

    // `AXValue` is whatever the role says it is: the text of a field, the
    // on/off of a checkbox, the position of a slider. Reading it without
    // interpreting it by role would put a string where a consumer expects a
    // number — and leaving it out entirely, as an earlier version did, makes
    // typing invisible: the node is byte-identical before and after, so the
    // delta reducer correctly concludes nothing happened.
    if let Some(value) = node.value.as_deref() {
        match role {
            Role::TextInput
            | Role::MultilineTextInput
            | Role::SearchInput
            | Role::PasswordInput
            | Role::DateInput
            | Role::TimeInput => {
                if let Some(text) = attr::as_string(value) {
                    container.set_value(text);
                }
            }
            Role::CheckBox | Role::RadioButton | Role::Switch => {
                // AppKit reports these as 0/1/2, where 2 is the mixed state a
                // tri-state checkbox shows for a partial selection.
                if let Some(number) = attr::as_f64(value) {
                    container.set_toggled(match number as i64 {
                        0 => accesskit::Toggled::False,
                        2 => accesskit::Toggled::Mixed,
                        _ => accesskit::Toggled::True,
                    });
                }
            }
            Role::Slider | Role::SpinButton | Role::ProgressIndicator | Role::Meter
            | Role::ScrollBar => {
                if let Some(number) = attr::as_f64(value) {
                    container.set_numeric_value(number);
                }
            }
            _ => {}
        }
    }

    if let Some(selected) = node.states.selected {
        container.set_selected(selected);
    }
    // Absence is not disablement: 39% of elements do not report `AXEnabled` at
    // all, and announcing every one of them as disabled would be far worse
    // than saying nothing.
    if node.states.enabled == Some(false) {
        container.set_disabled();
    }

    if node.states.focusable {
        container.add_action(accesskit::Action::Focus);
    }
    // Declaring is separate from executing, and the executor having landed
    // first is why no button in the mirror was pressable: `drive.rs` could
    // carry out seven actions while `Action::Focus` was the only one any node
    // advertised, so AccessKit's Windows adapter offered `InvokePattern`
    // nowhere. Declare exactly what this element can actually do.
    for action in &node.actions {
        match action.as_str() {
            "AXPress" | "AXPick" | "AXConfirm" => container.add_action(accesskit::Action::Click),
            "AXShowMenu" => {
                container.add_action(accesskit::Action::Expand);
                container.add_action(accesskit::Action::Collapse);
            }
            "AXIncrement" => container.add_action(accesskit::Action::Increment),
            "AXDecrement" => container.add_action(accesskit::Action::Decrement),
            _ => {}
        }
    }
    // A writable value is a settable one, whatever route reaches it.
    if node.states.value_settable {
        container.add_action(accesskit::Action::SetValue);
    }

    if let Some(frame) = node.frame {
        // AX reports screen coordinates with a top-left origin; AccessKit wants
        // them relative to the window. Only the caller knows the origin, so a
        // node read outside a window context simply carries no bounds rather
        // than bounds in the wrong space.
        if let Some((origin_x, origin_y)) = window_origin {
            if frame.size.width > 0.0 && frame.size.height > 0.0 {
                let x = frame.origin.x - origin_x;
                let y = frame.origin.y - origin_y;
                container.set_bounds(accesskit::Rect {
                    x0: x,
                    y0: y,
                    x1: x + frame.size.width,
                    y1: y + frame.size.height,
                });
            }
        }
    }

    container
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::{CFNumber, CGPoint, CGSize};

    fn key() -> ElementKey {
        // SAFETY: takes no arguments and always returns a valid element.
        ElementKey::new(0, unsafe { AXUIElement::new_system_wide() })
    }

    fn node(role: &str) -> AxNode {
        AxNode {
            key: key(),
            role: role.to_owned(),
            subrole: None,
            title: String::new(),
            description: String::new(),
            value: None,
            frame: None,
            states: NodeStates::default(),
            children: Vec::new(),
            selected_range: None,
            actions: Vec::new(),
        }
    }

    /// Comparing two `Node`s by their serialized form, because `accesskit::Node`
    /// is not `PartialEq` — the same trick the AT-SPI end-to-end test uses.
    fn same(a: &Node, b: &Node) -> bool {
        serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
    }

    /// **The invariant the whole delta path rests on.** Two builds of an
    /// unchanged node must be byte-identical, or every refresh emits a
    /// spurious update. `accesskit::Node` compares properties in insertion
    /// order, so this holds only while one function sets them.
    #[test]
    fn rebuilding_an_unchanged_node_produces_an_identical_node() {
        let mut n = node("AXButton");
        n.title = "Save".into();
        n.states.enabled = Some(true);
        n.states.focusable = true;
        n.frame = Some(CGRect::new(CGPoint::new(30.0, 40.0), CGSize::new(80.0, 22.0)));

        let origin = Some((10.0, 20.0));
        assert!(same(&build_container(&n, origin), &build_container(&n, origin)));
    }

    /// Typing must change the node, or the delta reducer correctly concludes
    /// nothing happened and the change never reaches the consumer. This is the
    /// test for the gap that made text editing invisible.
    #[test]
    fn a_text_field_carries_its_contents() {
        let mut n = node("AXTextArea");
        assert_eq!(n.accesskit_role(), Role::MultilineTextInput);
        assert_eq!(build_container(&n, None).value(), None);

        n.value = Some(CFString::from_static_str("hello").into());
        assert_eq!(build_container(&n, None).value(), Some("hello"));

        n.value = Some(CFString::from_static_str("hello world").into());
        assert_eq!(build_container(&n, None).value(), Some("hello world"));
    }

    #[test]
    fn a_checkbox_reads_its_value_as_a_toggle_including_mixed() {
        let mut n = node("AXCheckBox");
        n.value = Some(CFNumber::new_i32(0).into());
        assert_eq!(build_container(&n, None).toggled(), Some(accesskit::Toggled::False));
        n.value = Some(CFNumber::new_i32(1).into());
        assert_eq!(build_container(&n, None).toggled(), Some(accesskit::Toggled::True));
        // AppKit uses 2 for the partial state a tri-state checkbox shows.
        n.value = Some(CFNumber::new_i32(2).into());
        assert_eq!(build_container(&n, None).toggled(), Some(accesskit::Toggled::Mixed));
    }

    #[test]
    fn a_slider_reads_its_value_as_a_number_not_a_string() {
        let mut n = node("AXSlider");
        n.value = Some(CFNumber::new_f64(0.42).into());
        let built = build_container(&n, None);
        assert_eq!(built.numeric_value(), Some(0.42));
        assert_eq!(built.value(), None, "a slider's value is not text");
    }

    #[test]
    fn a_value_on_a_role_that_has_no_use_for_one_is_ignored() {
        // Plenty of elements carry AXValue with something meaningless in it;
        // putting a string on a Button would be worse than dropping it.
        let mut n = node("AXButton");
        n.value = Some(CFString::from_static_str("nonsense").into());
        let built = build_container(&n, None);
        assert_eq!(built.value(), None);
        assert_eq!(built.numeric_value(), None);
    }

    #[test]
    fn a_label_carries_its_text_as_a_value() {
        // A label's text is its content, not a name pointing elsewhere; the
        // AT-SPI mapping makes the same distinction.
        let mut n = node("AXStaticText");
        n.title = "Total".into();
        let built = build_container(&n, None);
        assert_eq!(built.role(), Role::Label);
        assert_eq!(built.value(), Some("Total"));
        assert_eq!(built.label(), None);
    }

    /// **Regression.** `AXStaticText` keeps its text in `AXValue`, not
    /// `AXTitle`. Reading only the title left Calculator's display and most of
    /// System Settings' sidebar empty, and was a large part of why 65% of
    /// mirrored elements had no name at all.
    #[test]
    fn a_static_text_takes_its_content_from_the_value() {
        let mut n = node("AXStaticText");
        assert_eq!(build_container(&n, None).value(), None);
        n.value = Some(CFString::from_static_str("1,234").into());
        assert_eq!(build_container(&n, None).value(), Some("1,234"));

        // A title still wins where one exists.
        n.title = "Total".into();
        assert_eq!(build_container(&n, None).value(), Some("Total"));
    }

    /// **Regression.** Executing an action and *declaring* it are separate, and
    /// only the executor had landed: every node advertised `Focus` and nothing
    /// else, so AccessKit's Windows adapter offered `InvokePattern` on none of
    /// the 89 buttons in a live mirror.
    #[test]
    fn an_element_declares_the_actions_it_can_actually_perform() {
        let mut n = node("AXButton");
        assert!(!build_container(&n, None).supports_action(accesskit::Action::Click));

        n.actions = vec!["AXPress".into()];
        assert!(build_container(&n, None).supports_action(accesskit::Action::Click));

        // A menu button opens rather than activates.
        n.actions = vec!["AXShowMenu".into()];
        let built = build_container(&n, None);
        assert!(built.supports_action(accesskit::Action::Expand));
        assert!(built.supports_action(accesskit::Action::Collapse));

        n.actions = vec!["AXIncrement".into(), "AXDecrement".into()];
        let built = build_container(&n, None);
        assert!(built.supports_action(accesskit::Action::Increment));
        assert!(built.supports_action(accesskit::Action::Decrement));

        // Nothing is claimed that the element did not report.
        n.actions = vec!["AXSomethingElse".into()];
        assert!(!build_container(&n, None).supports_action(accesskit::Action::Click));
    }

    #[test]
    fn a_writable_value_advertises_set_value() {
        let mut n = node("AXTextArea");
        assert!(!build_container(&n, None).supports_action(accesskit::Action::SetValue));
        n.states.value_settable = true;
        assert!(build_container(&n, None).supports_action(accesskit::Action::SetValue));
    }

    #[test]
    fn a_name_falls_back_from_title_to_description() {
        // 29% of elements carry a title and 54% a description, so neither
        // alone names the tree.
        let mut n = node("AXButton");
        n.description = "Close".into();
        assert_eq!(n.name(), "Close");
        assert_eq!(build_container(&n, None).label(), Some("Close"));

        n.title = "Sluiten".into();
        assert_eq!(n.name(), "Sluiten", "a title wins when present");
    }

    #[test]
    fn a_description_equal_to_the_name_is_not_repeated() {
        let mut n = node("AXButton");
        n.title = "Close".into();
        n.description = "Close".into();
        assert_eq!(build_container(&n, None).description(), None);
    }

    #[test]
    fn absence_of_enabled_is_not_disablement() {
        // 39% of real elements never report AXEnabled. Announcing them all as
        // disabled would be much worse than saying nothing.
        let mut n = node("AXButton");
        assert_eq!(n.states.enabled, None);
        assert!(!build_container(&n, None).is_disabled());

        n.states.enabled = Some(true);
        assert!(!build_container(&n, None).is_disabled());
        n.states.enabled = Some(false);
        assert!(build_container(&n, None).is_disabled());
    }

    #[test]
    fn bounds_are_window_relative_and_zero_area_is_dropped() {
        let mut n = node("AXButton");
        n.frame = Some(CGRect::new(CGPoint::new(30.0, 40.0), CGSize::new(80.0, 22.0)));

        // Screen coordinates minus the window origin.
        let built = build_container(&n, Some((10.0, 20.0)));
        let bounds = built.bounds().expect("a sized element has bounds");
        assert_eq!((bounds.x0, bounds.y0, bounds.x1, bounds.y1), (20.0, 20.0, 100.0, 42.0));

        // Without a window origin the coordinate space is unknown, and wrong
        // bounds are worse than none.
        assert!(build_container(&n, None).bounds().is_none());

        n.frame = Some(CGRect::new(CGPoint::new(30.0, 40.0), CGSize::new(0.0, 0.0)));
        assert!(build_container(&n, Some((10.0, 20.0))).bounds().is_none());
    }

    #[test]
    fn focusable_comes_from_settability_and_offers_the_action() {
        let mut n = node("AXButton");
        assert!(!build_container(&n, None).supports_action(accesskit::Action::Focus));
        n.states.focusable = true;
        assert!(build_container(&n, None).supports_action(accesskit::Action::Focus));
    }

    #[test]
    fn a_named_group_is_promoted_but_an_unnamed_one_stays_transparent() {
        let mut n = node("AXGroup");
        assert_eq!(n.accesskit_role(), Role::GenericContainer);
        n.title = "Formatting".into();
        assert_eq!(n.accesskit_role(), Role::Group);
    }
}
