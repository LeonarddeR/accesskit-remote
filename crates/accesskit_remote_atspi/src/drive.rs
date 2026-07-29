//! Pure action planning: given everything known about an action's target at
//! perform time ([`ActionContext`]), produces an ordered list of AT-SPI calls
//! ([`AtspiCall`]) for the executor to try in turn until one succeeds. This is
//! how "interface present but call returns NotSupported" is handled
//! structurally — GTK4 returns `NotSupported` for `GrabFocus` on some roles,
//! VCL toolbar toggles have empty action names, and a GTK4 check button can
//! have no action at index 0, so named-and-index fallbacks matter. Planning
//! holds no bus state and reads nothing itself; [`crate::mirror::perform`] is
//! the caller that gathers an `ActionContext` and drives the resulting plan.

use crate::mapping::ValueState;
use atspi::{Interface, InterfaceSet, Role};

/// Everything the planner may consult about the action's target, read at
/// perform time.
pub struct ActionContext {
    pub role: Role,
    pub interfaces: InterfaceSet,
    /// Action names from `Action.GetActions`, index-aligned.
    pub actions: Vec<String>,
    pub parent_interfaces: InterfaceSet,
    pub index_in_parent: Option<i32>,
    pub value: Option<ValueState>,
}

/// One concrete AT-SPI call the executor can attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum AtspiCall {
    DoAction(i32),
    GrabFocus,
    SelectChildOfParent(i32),
    SetCurrentValue(f64),
    SetTextContents(String),
}

/// Plans the ordered sequence of AT-SPI calls that could carry out `action`
/// against the node described by `ctx`, most likely to succeed first. The
/// executor tries each in turn until one succeeds; an empty plan means the
/// action cannot be expressed against this target at all.
pub fn plan_action(
    ctx: &ActionContext,
    action: accesskit::Action,
    data: Option<&accesskit::ActionData>,
) -> Vec<AtspiCall> {
    use accesskit::Action;
    match action {
        Action::Click => {
            let mut plan = Vec::new();
            plan.extend(select_child_step(ctx));
            plan.extend(do_action_steps(ctx, &["click", "press", "toggle", "activate"]));
            plan
        }
        Action::Focus => {
            let mut plan = vec![AtspiCall::GrabFocus];
            plan.extend(select_child_step(ctx));
            plan
        }
        Action::Expand => {
            do_action_steps(ctx, &["expand", "menu.popup", "popup", "show", "press"])
        }
        Action::Collapse => do_action_steps(
            ctx,
            &["collapse", "menu.popdown", "popdown", "close", "menu.popup", "press"],
        ),
        Action::Increment => value_step(ctx, 1.0),
        Action::Decrement => value_step(ctx, -1.0),
        Action::SetValue => match data {
            Some(accesskit::ActionData::Value(text))
                if ctx.interfaces.contains(Interface::EditableText) =>
            {
                vec![AtspiCall::SetTextContents(text.to_string())]
            }
            Some(accesskit::ActionData::NumericValue(value))
                if ctx.interfaces.contains(Interface::Value) && value.is_finite() =>
            {
                vec![AtspiCall::SetCurrentValue(*value)]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// A `Selection.SelectChild` on the parent, when the node is an option-like
/// child of a container that implements selection and its index is known.
fn select_child_step(ctx: &ActionContext) -> Option<AtspiCall> {
    if !ctx.parent_interfaces.contains(Interface::Selection) || !is_option_role(ctx.role) {
        return None;
    }
    ctx.index_in_parent.map(AtspiCall::SelectChildOfParent)
}

/// `DoAction` steps: every action whose name matches a candidate, in candidate
/// priority order, then the index-0 fallback. The fallback stays even when the
/// name list is empty (VCL reports `[""]`, GTK4 check buttons report nothing at
/// index 0 — a declined call costs one round trip and the executor moves on),
/// but requires the Action interface or a non-empty name list so a node with
/// no action surface at all plans nothing.
fn do_action_steps(ctx: &ActionContext, candidates: &[&str]) -> Vec<AtspiCall> {
    let mut indices: Vec<i32> = Vec::new();
    for candidate in candidates {
        for (index, name) in ctx.actions.iter().enumerate() {
            if name.to_lowercase() == *candidate && !indices.contains(&(index as i32)) {
                indices.push(index as i32);
            }
        }
    }
    if (ctx.interfaces.contains(Interface::Action) || !ctx.actions.is_empty())
        && !indices.contains(&0)
    {
        indices.push(0);
    }
    indices.into_iter().map(AtspiCall::DoAction).collect()
}

/// A clamped `Value.SetCurrentValue` one step away from the current value,
/// synthesising a step of a hundredth of the range when the toolkit reports
/// zero (GTK4 reports `MinimumIncrement = 0` on every value widget).
fn value_step(ctx: &ActionContext, direction: f64) -> Vec<AtspiCall> {
    let Some(value) = ctx.value else {
        return Vec::new();
    };
    let step = if value.step.is_finite() && value.step > 0.0 {
        value.step
    } else if value.minimum.is_finite() && value.maximum.is_finite() {
        (value.maximum - value.minimum) / 100.0
    } else {
        return Vec::new();
    };
    if !step.is_finite() || step <= 0.0 || !value.current.is_finite() {
        return Vec::new();
    }
    let mut target = value.current + direction * step;
    if value.minimum.is_finite() && value.maximum.is_finite() && value.minimum <= value.maximum {
        target = target.clamp(value.minimum, value.maximum);
    }
    vec![AtspiCall::SetCurrentValue(target)]
}

/// Roles that live as selectable options inside a `Selection` container.
pub(crate) fn is_option_role(role: Role) -> bool {
    matches!(
        role,
        Role::ListItem
            | Role::TreeItem
            | Role::PageTab
            | Role::MenuItem
            | Role::CheckMenuItem
            | Role::RadioMenuItem
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atspi::Interface;

    /// A context expressing nothing: no interfaces, no actions, no value, no
    /// parent selection. Individual tests override the fields their scenario
    /// needs via `..ctx()`.
    fn ctx() -> ActionContext {
        ActionContext {
            role: Role::Invalid,
            interfaces: InterfaceSet::empty(),
            actions: Vec::new(),
            parent_interfaces: InterfaceSet::empty(),
            index_in_parent: None,
            value: None,
        }
    }

    #[test]
    fn click_on_a_selectable_option_prefers_select_child_then_falls_back() {
        let mut parent_interfaces = InterfaceSet::empty();
        parent_interfaces.insert(Interface::Selection);
        let target = ActionContext {
            role: Role::ListItem,
            parent_interfaces,
            index_in_parent: Some(3),
            actions: vec!["Click".to_owned()],
            ..ctx()
        };

        let plan = plan_action(&target, accesskit::Action::Click, None);

        assert_eq!(
            plan,
            vec![AtspiCall::SelectChildOfParent(3), AtspiCall::DoAction(0)]
        );
    }

    #[test]
    fn focus_falls_back_from_grab_focus_to_select_child() {
        let mut parent_interfaces = InterfaceSet::empty();
        parent_interfaces.insert(Interface::Selection);
        let target = ActionContext {
            role: Role::ListItem,
            parent_interfaces,
            index_in_parent: Some(2),
            actions: Vec::new(),
            ..ctx()
        };

        let plan = plan_action(&target, accesskit::Action::Focus, None);

        assert_eq!(
            plan,
            vec![AtspiCall::GrabFocus, AtspiCall::SelectChildOfParent(2)]
        );
    }

    #[test]
    fn expand_prefers_a_named_action_and_falls_back_to_index_zero() {
        let named = ActionContext {
            actions: vec!["other".to_owned(), "menu.popup".to_owned()],
            ..ctx()
        };
        let expand_plan = plan_action(&named, accesskit::Action::Expand, None);
        assert_eq!(
            expand_plan,
            vec![AtspiCall::DoAction(1), AtspiCall::DoAction(0)],
            "a named action match plans that index, then falls back to index 0"
        );

        let unnamed = ActionContext {
            actions: vec![String::new()],
            ..ctx()
        };
        let expand_plan_unnamed = plan_action(&unnamed, accesskit::Action::Expand, None);
        assert_eq!(
            expand_plan_unnamed,
            vec![AtspiCall::DoAction(0)],
            "the VCL empty-name case still plans the index-0 fallback"
        );

        let collapse_plan = plan_action(&named, accesskit::Action::Collapse, None);
        assert_eq!(
            collapse_plan.first(),
            Some(&AtspiCall::DoAction(1)),
            "collapsing an open popup is driven by the same named action set as expand"
        );
        assert_eq!(collapse_plan.last(), Some(&AtspiCall::DoAction(0)));
    }

    #[test]
    fn increment_clamps_to_the_range_and_synthesises_a_step_when_the_toolkit_reports_zero() {
        let zero_step = ActionContext {
            value: Some(ValueState {
                current: 9.95,
                minimum: 0.0,
                maximum: 10.0,
                step: 0.0,
            }),
            ..ctx()
        };
        let increment_plan = plan_action(&zero_step, accesskit::Action::Increment, None);
        assert_eq!(
            increment_plan,
            vec![AtspiCall::SetCurrentValue(10.0)],
            "a synthesized step of (max - min) / 100, clamped to the range"
        );

        let real_step = ActionContext {
            value: Some(ValueState {
                current: 9.8,
                minimum: 0.0,
                maximum: 10.0,
                step: 0.5,
            }),
            ..ctx()
        };
        let increment_plan = plan_action(&real_step, accesskit::Action::Increment, None);
        assert_eq!(increment_plan, vec![AtspiCall::SetCurrentValue(10.0)]);
        let decrement_plan = plan_action(&real_step, accesskit::Action::Decrement, None);
        assert_eq!(decrement_plan, vec![AtspiCall::SetCurrentValue(9.3)]);
    }

    #[test]
    fn set_value_routes_to_editable_text_or_value_by_interface() {
        let mut editable_text = InterfaceSet::empty();
        editable_text.insert(Interface::EditableText);
        let text_target = ActionContext {
            interfaces: editable_text,
            ..ctx()
        };
        let string_data = accesskit::ActionData::Value("hello".into());
        let plan = plan_action(&text_target, accesskit::Action::SetValue, Some(&string_data));
        assert_eq!(plan, vec![AtspiCall::SetTextContents("hello".to_owned())]);

        let mut value_iface = InterfaceSet::empty();
        value_iface.insert(Interface::Value);
        let value_target = ActionContext {
            interfaces: value_iface,
            ..ctx()
        };
        let numeric_data = accesskit::ActionData::NumericValue(5.0);
        let plan = plan_action(&value_target, accesskit::Action::SetValue, Some(&numeric_data));
        assert_eq!(plan, vec![AtspiCall::SetCurrentValue(5.0)]);
    }

    #[test]
    fn unexpressible_actions_plan_no_steps() {
        assert_eq!(plan_action(&ctx(), accesskit::Action::Click, None), Vec::new());
        assert_eq!(plan_action(&ctx(), accesskit::Action::Expand, None), Vec::new());
        assert_eq!(plan_action(&ctx(), accesskit::Action::Increment, None), Vec::new());
    }
}
