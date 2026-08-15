//! Pure action planning: an AccessKit action plus everything known about its
//! target becomes an ordered list of AX calls, tried until one succeeds.
//!
//! The AT-SPI source needed this shape because "the interface is present but
//! the call returns `NotSupported`" was routine — GTK4 refused `GrabFocus` on
//! most roles, VCL toolbar toggles had empty action names, and a check button
//! could advertise the Action interface with nothing at index 0. Ordered
//! attempts were the only structural answer.
//!
//! macOS is in better shape, and the plan reflects that rather than copying
//! the Linux one. `AXUIElementCopyActionNames` says exactly which actions
//! exist, and `AXUIElementIsAttributeSettable` says exactly which attributes
//! can be written — so a call that cannot possibly work is never planned, and
//! ordering matters only where genuinely more than one route could apply.
//!
//! What arrives here is shaped by the consumer, and the AT-SPI work measured
//! it on real UIA: every pattern gesture is preceded by `Focus`; `Toggle`,
//! `Invoke` and `SelectionItem.Select` all arrive as `Click`; and
//! `RangeValue.SetValue` arrives as `SetValue` with numeric data.

use accesskit::{Action, ActionData, Role};

/// The AX action names this planner may emit.
pub mod actions {
    pub const PRESS: &str = "AXPress";
    pub const INCREMENT: &str = "AXIncrement";
    pub const DECREMENT: &str = "AXDecrement";
    pub const SHOW_MENU: &str = "AXShowMenu";
    pub const PICK: &str = "AXPick";
    pub const CONFIRM: &str = "AXConfirm";
    pub const CANCEL: &str = "AXCancel";
    pub const RAISE: &str = "AXRaise";
}

/// Attributes the planner may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Settable {
    Value,
    Focused,
    Selected,
    SelectedTextRange,
}

/// One concrete AX call the executor can attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum AxCall {
    /// `AXUIElementPerformAction` with this name.
    Perform(&'static str),
    /// `AXValue = <text>`.
    SetStringValue(String),
    /// `AXValue = <number>`.
    SetNumberValue(f64),
    /// `AXFocused = true`.
    Focus,
    /// `AXSelected = <flag>`.
    SetSelected(bool),
}

/// Everything the planner may consult about an action's target, read at
/// perform time.
pub struct ActionContext {
    pub role: Role,
    /// Action names the element reports, verbatim.
    pub actions: Vec<String>,
    /// Attributes the element reports as writable. This is the difference from
    /// AT-SPI: capability is known before a call is spent, not after.
    pub settable: Vec<Settable>,
    /// The element's current numeric value, for synthesising a step when the
    /// toolkit offers no increment action.
    pub value: Option<f64>,
}

impl ActionContext {
    fn has(&self, action: &str) -> bool {
        self.actions.iter().any(|name| name == action)
    }

    fn can_set(&self, what: Settable) -> bool {
        self.settable.contains(&what)
    }
}

/// The fraction of a slider's range one step moves when the element offers no
/// increment action of its own.
///
/// AX exposes no step size, so this is a choice rather than a reading. One
/// percent matches what the AT-SPI source synthesises when a toolkit reports
/// `MinimumIncrement = 0`, which GTK4 always did.
const SYNTHETIC_STEP_FRACTION: f64 = 0.01;

/// Plans the ordered AX calls that could carry out `action` against the element
/// described by `ctx`, most likely to succeed first.
///
/// An empty plan means the action cannot be expressed against this target at
/// all — which is a real answer, and better than spending a round trip to
/// discover it.
pub fn plan_action(ctx: &ActionContext, action: Action, data: Option<&ActionData>) -> Vec<AxCall> {
    match action {
        // Toggle, Invoke and SelectionItem.Select all arrive here.
        Action::Click => {
            let mut plan = Vec::new();
            if ctx.has(actions::PRESS) {
                plan.push(AxCall::Perform(actions::PRESS));
            }
            // Menu items and table rows are *picked* rather than pressed.
            if ctx.has(actions::PICK) {
                plan.push(AxCall::Perform(actions::PICK));
            }
            // Selecting an option is a legitimate way to "click" it, and some
            // list rows offer no action at all.
            if is_option_role(ctx.role) && ctx.can_set(Settable::Selected) {
                plan.push(AxCall::SetSelected(true));
            }
            if ctx.has(actions::CONFIRM) {
                plan.push(AxCall::Perform(actions::CONFIRM));
            }
            plan
        }

        Action::Focus => {
            let mut plan = Vec::new();
            if ctx.can_set(Settable::Focused) {
                plan.push(AxCall::Focus);
            }
            // A window cannot be focused by attribute; it is raised.
            if ctx.role == Role::Window && ctx.has(actions::RAISE) {
                plan.push(AxCall::Perform(actions::RAISE));
            }
            plan
        }

        Action::Expand => {
            let mut plan = Vec::new();
            if ctx.has(actions::SHOW_MENU) {
                plan.push(AxCall::Perform(actions::SHOW_MENU));
            }
            // A disclosure triangle expands by being pressed.
            if ctx.has(actions::PRESS) {
                plan.push(AxCall::Perform(actions::PRESS));
            }
            plan
        }

        Action::Collapse => {
            let mut plan = Vec::new();
            if ctx.has(actions::PRESS) {
                plan.push(AxCall::Perform(actions::PRESS));
            }
            // Dismissing an open menu is a cancel, not a second show.
            if ctx.has(actions::CANCEL) {
                plan.push(AxCall::Perform(actions::CANCEL));
            }
            plan
        }

        Action::Increment => step(ctx, 1.0),
        Action::Decrement => step(ctx, -1.0),

        Action::SetValue => match data {
            Some(ActionData::Value(text)) if ctx.can_set(Settable::Value) => {
                vec![AxCall::SetStringValue(text.to_string())]
            }
            Some(ActionData::NumericValue(value))
                if ctx.can_set(Settable::Value) && value.is_finite() =>
            {
                vec![AxCall::SetNumberValue(*value)]
            }
            _ => Vec::new(),
        },

        _ => Vec::new(),
    }
}

/// A one-step move, preferring the element's own increment action and falling
/// back to writing a synthesised value.
fn step(ctx: &ActionContext, direction: f64) -> Vec<AxCall> {
    let named = if direction > 0.0 {
        actions::INCREMENT
    } else {
        actions::DECREMENT
    };
    if ctx.has(named) {
        return vec![AxCall::Perform(named)];
    }
    // No increment action: move the value directly, if it can be written.
    match (ctx.value, ctx.can_set(Settable::Value)) {
        (Some(current), true) if current.is_finite() => {
            vec![AxCall::SetNumberValue(current + direction * SYNTHETIC_STEP_FRACTION)]
        }
        _ => Vec::new(),
    }
}

/// Roles whose members are selected rather than activated.
fn is_option_role(role: Role) -> bool {
    matches!(
        role,
        Role::ListBoxOption
            | Role::MenuItem
            | Role::MenuListOption
            | Role::Tab
            | Role::TreeItem
            | Role::Row
            | Role::Cell
            | Role::ListItem
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(role: Role, actions: &[&str], settable: &[Settable]) -> ActionContext {
        ActionContext {
            role,
            actions: actions.iter().map(|a| (*a).to_owned()).collect(),
            settable: settable.to_vec(),
            value: None,
        }
    }

    #[test]
    fn a_click_presses_when_the_element_offers_it() {
        let target = ctx(Role::Button, &[actions::PRESS], &[]);
        assert_eq!(
            plan_action(&target, Action::Click, None),
            vec![AxCall::Perform(actions::PRESS)]
        );
    }

    #[test]
    fn a_menu_item_is_picked_and_a_row_is_selected() {
        // Menu items answer AXPick rather than AXPress.
        let item = ctx(Role::MenuItem, &[actions::PICK], &[]);
        assert_eq!(
            plan_action(&item, Action::Click, None),
            vec![AxCall::Perform(actions::PICK)]
        );
        // A table row often offers no action at all; selecting it is the only
        // route, and it exists only because settability said so.
        let row = ctx(Role::Row, &[], &[Settable::Selected]);
        assert_eq!(
            plan_action(&row, Action::Click, None),
            vec![AxCall::SetSelected(true)]
        );
    }

    /// The macOS advantage over AT-SPI: a call that cannot work is never
    /// planned, rather than attempted and refused a round trip later.
    #[test]
    fn nothing_is_planned_that_the_element_cannot_do() {
        let inert = ctx(Role::Label, &[], &[]);
        assert!(plan_action(&inert, Action::Click, None).is_empty());
        assert!(plan_action(&inert, Action::Focus, None).is_empty());
        assert!(plan_action(&inert, Action::Increment, None).is_empty());
        assert!(plan_action(&inert, Action::Expand, None).is_empty());
        assert!(
            plan_action(&inert, Action::SetValue, Some(&ActionData::Value("x".into()))).is_empty(),
            "an unwritable value must not be attempted"
        );
    }

    #[test]
    fn focus_is_planned_only_where_it_can_be_written() {
        assert!(plan_action(&ctx(Role::Button, &[], &[]), Action::Focus, None).is_empty());
        assert_eq!(
            plan_action(&ctx(Role::Button, &[], &[Settable::Focused]), Action::Focus, None),
            vec![AxCall::Focus]
        );
        // A window is raised rather than focused by attribute.
        assert_eq!(
            plan_action(&ctx(Role::Window, &[actions::RAISE], &[]), Action::Focus, None),
            vec![AxCall::Perform(actions::RAISE)]
        );
    }

    #[test]
    fn a_step_prefers_the_elements_own_action() {
        let slider = ctx(Role::Slider, &[actions::INCREMENT, actions::DECREMENT], &[]);
        assert_eq!(
            plan_action(&slider, Action::Increment, None),
            vec![AxCall::Perform(actions::INCREMENT)]
        );
        assert_eq!(
            plan_action(&slider, Action::Decrement, None),
            vec![AxCall::Perform(actions::DECREMENT)]
        );
    }

    #[test]
    fn a_step_falls_back_to_writing_a_synthesised_value() {
        // AX exposes no step size, so a slider with no increment action can
        // still be moved — but only if its value is writable.
        let mut slider = ctx(Role::Slider, &[], &[Settable::Value]);
        slider.value = Some(0.5);
        assert_eq!(
            plan_action(&slider, Action::Increment, None),
            vec![AxCall::SetNumberValue(0.51)]
        );
        assert_eq!(
            plan_action(&slider, Action::Decrement, None),
            vec![AxCall::SetNumberValue(0.49)]
        );

        slider.settable.clear();
        assert!(
            plan_action(&slider, Action::Increment, None).is_empty(),
            "an unwritable slider with no increment action cannot be moved"
        );
    }

    #[test]
    fn set_value_distinguishes_text_from_numbers() {
        let field = ctx(Role::TextInput, &[], &[Settable::Value]);
        assert_eq!(
            plan_action(&field, Action::SetValue, Some(&ActionData::Value("hello".into()))),
            vec![AxCall::SetStringValue("hello".into())]
        );
        let slider = ctx(Role::Slider, &[], &[Settable::Value]);
        assert_eq!(
            plan_action(&slider, Action::SetValue, Some(&ActionData::NumericValue(0.25))),
            vec![AxCall::SetNumberValue(0.25)]
        );
    }

    #[test]
    fn a_non_finite_value_is_refused() {
        // NaN through the wire must not become a write.
        let slider = ctx(Role::Slider, &[], &[Settable::Value]);
        assert!(
            plan_action(&slider, Action::SetValue, Some(&ActionData::NumericValue(f64::NAN)))
                .is_empty()
        );
    }

    #[test]
    fn expand_shows_a_menu_and_collapse_cancels_it() {
        let menu_button = ctx(Role::Button, &[actions::SHOW_MENU, actions::PRESS], &[]);
        assert_eq!(
            plan_action(&menu_button, Action::Expand, None),
            vec![
                AxCall::Perform(actions::SHOW_MENU),
                AxCall::Perform(actions::PRESS)
            ],
            "the specific route is tried before the generic one"
        );
        let open_menu = ctx(Role::Menu, &[actions::CANCEL], &[]);
        assert_eq!(
            plan_action(&open_menu, Action::Collapse, None),
            vec![AxCall::Perform(actions::CANCEL)]
        );
    }
}
