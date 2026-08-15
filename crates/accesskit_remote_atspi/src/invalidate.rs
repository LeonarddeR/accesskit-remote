//! Which live AT-SPI signals warrant re-reading one node.
//!
//! *How often* a node may then be re-read is source-agnostic and lives in
//! [`accesskit_remote_source::limiter`]; what is AT-SPI-specific is which
//! signals reach the mapping at all, which is what this module decides — in
//! O(1), before any bus call.

use atspi::State;

/// Whether a state change alters what [`crate::mapping::node_states`] distills,
/// and so warrants re-reading the node. Everything else is discarded in O(1).
pub fn state_is_mirrored(state: State) -> bool {
    matches!(
        state,
        State::Focusable
            | State::Focused
            | State::Expandable
            | State::Expanded
            | State::Collapsed
            | State::Selectable
            | State::Selected
            | State::Checkable
            | State::Checked
            | State::Pressed
            | State::Indeterminate
            | State::HasPopup
            | State::Sensitive
            | State::Enabled
            | State::ReadOnly
            | State::Required
            | State::InvalidEntry
            | State::Modal
            | State::Multiselectable
            | State::Busy
            | State::Horizontal
            | State::Vertical
    )
}

/// Whether an `object:property-change` names a property the mapping mirrors.
/// Routed off the signal's property *string*, which is what AT-SPI puts on the
/// wire; `atspi`'s `Property` enum deserializes `accessible-value` (and
/// anything else it lacks a variant for) to `Property::Other`.
pub fn property_is_mirrored(property: &str) -> bool {
    matches!(
        property,
        "accessible-name" | "accessible-description" | "accessible-role" | "accessible-value"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::node_states;
    use atspi::StateSet;

    /// Every [`State`] the bitflag representation defines, enumerated by bit.
    fn all_states() -> Vec<State> {
        (0..64)
            .filter_map(|bit| StateSet::from_bits(1u64 << bit).ok())
            .filter_map(|set| set.iter().next())
            .collect()
    }

    #[test]
    fn mirrored_states_are_exactly_the_forwarded_ones() {
        let baseline = node_states(StateSet::empty());
        let states = all_states();
        assert!(states.len() > 40, "the whole state surface is under test");
        for state in states {
            let distilled = node_states(StateSet::new(state)) != baseline;
            assert_eq!(
                state_is_mirrored(state),
                distilled,
                "{state:?}: mirrored={}, but node_states {} it",
                state_is_mirrored(state),
                if distilled { "distills" } else { "ignores" },
            );
        }
    }

    #[test]
    fn mirrored_properties_are_the_ones_the_mapping_reads() {
        for property in [
            "accessible-name",
            "accessible-description",
            "accessible-role",
            "accessible-value",
        ] {
            assert!(property_is_mirrored(property), "{property} reaches accesskit");
        }
        for property in ["accessible-parent", "accessible-table-caption", "", "name"] {
            assert!(!property_is_mirrored(property), "{property:?} is not mirrored");
        }
    }
}
