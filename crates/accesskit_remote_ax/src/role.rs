//! `AXRole` + `AXSubrole` → [`accesskit::Role`].
//!
//! Two things govern this map.
//!
//! **Subrole first.** AX splits information the way AT-SPI does not: the role
//! says `AXTextField` and the *subrole* says whether it is a search field or a
//! password field; the role says `AXWindow` and the subrole says whether it is
//! a dialog. Mapping on the role alone would throw that away, so every lookup
//! consults the pair.
//!
//! **`GenericContainer` is load-bearing.** `accesskit_consumer::common_filter`
//! excludes exactly `GenericContainer` and `TextRun` from what a consumer
//! sees. Every structural container — `AXGroup`, `AXSplitGroup`, `AXScrollArea`,
//! `AXLayoutArea`, `AXUnknown` — therefore maps there and stays transparent.
//! Promoting any of them would surface every layout box in every application,
//! which is the mistake the AT-SPI mapping documents at length. The one
//! exception is a *named* group, which is a real ARIA-style grouping and is
//! promoted by [`refine`].

use accesskit::Role;

/// Maps an element's role and subrole to an AccessKit role.
///
/// Unrecognised roles fall to `GenericContainer` rather than `Unknown`:
/// `Unknown` survives the consumer's filter and would surface a mystery node,
/// while `GenericContainer` stays transparent and lets the element's children
/// speak for it. An unmapped role should be invisible, not confusing.
pub fn map(role: &str, subrole: Option<&str>) -> Role {
    if let Some(subrole) = subrole {
        if let Some(mapped) = from_subrole(subrole) {
            return mapped;
        }
    }
    from_role(role).unwrap_or(Role::GenericContainer)
}

/// Whether this role is one the map recognises, as opposed to one that reached
/// `GenericContainer` through the catch-all.
///
/// Both outcomes look identical in a tree, which is exactly why coverage needs
/// its own question: `ax_probe --roles` uses this to separate "deliberately
/// transparent" from "nobody has mapped this yet".
pub fn is_known(role: &str, subrole: Option<&str>) -> bool {
    subrole.and_then(from_subrole).is_some() || from_role(role).is_some()
}

/// Subroles that fully determine the role. A subrole this does not know falls
/// through to the plain role map.
fn from_subrole(subrole: &str) -> Option<Role> {
    Some(match subrole {
        // Window flavours. `AXDialog` is the distinction AT-SPI carries in the
        // role itself (`Role::Dialog`), so it has to be recovered here.
        "AXStandardWindow" => Role::Window,
        "AXDialog" | "AXSystemDialog" => Role::Dialog,
        "AXFloatingWindow" | "AXSystemFloatingWindow" => Role::Window,

        // Text-input flavours: the role for all three is `AXTextField`.
        "AXSearchField" => Role::SearchInput,
        "AXSecureTextField" => Role::PasswordInput,

        // A switch is a checkbox in role terms; only the subrole says it reads
        // as on/off rather than checked/unchecked.
        "AXToggle" | "AXSwitch" => Role::Switch,

        // Window-chrome and scrollbar-part buttons.
        "AXCloseButton" | "AXMinimizeButton" | "AXZoomButton" | "AXFullScreenButton"
        | "AXToolbarButton" | "AXSortButton" | "AXIncrementArrow" | "AXDecrementArrow"
        | "AXIncrementPage" | "AXDecrementPage" => Role::Button,

        "AXOutlineRow" => Role::TreeItem,
        "AXTableRow" => Role::Row,

        "AXContentList" => Role::List,
        "AXDefinitionList" | "AXDescriptionList" => Role::DescriptionList,

        "AXRatingIndicator" => Role::Slider,
        "AXTimeline" => Role::Group,

        // Explicitly decorative: the application is saying this carries no
        // meaning, so it should not reach the consumer at all.
        "AXDecorative" => Role::GenericContainer,

        _ => return None,
    })
}

fn from_role(role: &str) -> Option<Role> {
    Some(match role {
        "AXApplication" => Role::Application,
        // A bare `AXWindow` with no subrole; dialogs arrive via the subrole.
        "AXWindow" => Role::Window,
        "AXSheet" => Role::Dialog,
        "AXDrawer" | "AXPopover" => Role::Group,

        "AXButton" => Role::Button,
        // A pop-up button is macOS's dropdown: a closed list, not a text box.
        "AXPopUpButton" | "AXComboBox" => Role::ComboBox,
        // A menu button opens a menu rather than choosing a value; `has_popup`
        // is set separately from its state.
        "AXMenuButton" => Role::Button,
        "AXCheckBox" => Role::CheckBox,
        "AXRadioButton" => Role::RadioButton,
        "AXRadioGroup" => Role::RadioGroup,
        "AXDisclosureTriangle" => Role::DisclosureTriangle,
        "AXColorWell" => Role::ColorWell,

        "AXStaticText" => Role::Label,
        "AXTextField" => Role::TextInput,
        "AXTextArea" => Role::MultilineTextInput,
        "AXDateField" => Role::DateInput,
        "AXTimeField" => Role::TimeInput,
        "AXHeading" => Role::Heading,
        "AXLink" => Role::Link,
        "AXImage" => Role::Image,

        "AXMenuBar" => Role::MenuBar,
        "AXMenuBarItem" | "AXMenuItem" => Role::MenuItem,
        "AXMenu" => Role::Menu,

        "AXTable" => Role::Table,
        "AXGrid" => Role::Grid,
        "AXOutline" => Role::Tree,
        "AXList" => Role::List,
        "AXRow" => Role::Row,
        "AXCell" => Role::Cell,

        "AXSlider" => Role::Slider,
        "AXIncrementor" => Role::SpinButton,
        "AXProgressIndicator" | "AXBusyIndicator" => Role::ProgressIndicator,
        "AXLevelIndicator" | "AXRelevanceIndicator" => Role::Meter,
        "AXScrollBar" => Role::ScrollBar,
        "AXSplitter" => Role::Splitter,

        "AXToolbar" => Role::Toolbar,
        "AXTabGroup" => Role::TabList,
        "AXHelpTag" => Role::Tooltip,
        // WebKit's document root, and the reason Safari's page content appears
        // at all without any opt-in of its own.
        "AXWebArea" => Role::RootWebArea,

        // Structural containers: deliberately transparent. See the module docs
        // — `common_filter` drops exactly `GenericContainer` and `TextRun`.
        "AXGroup" | "AXSplitGroup" | "AXScrollArea" | "AXLayoutArea" | "AXLayoutItem"
        | "AXBrowser" | "AXMatte" | "AXRuler" | "AXRulerMarker" | "AXGrowArea" | "AXHandle"
        | "AXColumn" | "AXValueIndicator" | "AXUnknown" => Role::GenericContainer,

        _ => return None,
    })
}

/// Promotes a role once the element's own properties are known.
///
/// The single rule, mirroring the AT-SPI mapping's `refine_role`: a
/// `GenericContainer` that carries a name is a real grouping — a labelled
/// region a user navigates to — and becomes a `Group` so it survives the
/// consumer's filter. An unnamed one stays transparent.
pub fn refine(base: Role, has_name: bool) -> Role {
    if base == Role::GenericContainer && has_name {
        return Role::Group;
    }
    base
}

/// Whether this role is a toplevel a window-rooted walk starts from.
pub fn is_window(role: &str) -> bool {
    role == "AXWindow"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subrole_beats_role_where_it_carries_the_distinction() {
        // The whole reason the map takes a pair: these three share one role.
        assert_eq!(map("AXTextField", None), Role::TextInput);
        assert_eq!(map("AXTextField", Some("AXSearchField")), Role::SearchInput);
        assert_eq!(map("AXTextField", Some("AXSecureTextField")), Role::PasswordInput);

        // As do these two.
        assert_eq!(map("AXWindow", Some("AXStandardWindow")), Role::Window);
        assert_eq!(map("AXWindow", Some("AXDialog")), Role::Dialog);
        assert_eq!(map("AXWindow", Some("AXSystemDialog")), Role::Dialog);

        assert_eq!(map("AXCheckBox", None), Role::CheckBox);
        assert_eq!(map("AXCheckBox", Some("AXToggle")), Role::Switch);
    }

    #[test]
    fn an_unknown_subrole_falls_through_to_the_role() {
        // Applications invent subroles freely; an unrecognised one must not
        // discard what the role already told us.
        assert_eq!(map("AXButton", Some("AXSomethingNobodyHasSeen")), Role::Button);
        assert_eq!(map("AXSlider", Some("AXWhatever")), Role::Slider);
    }

    #[test]
    fn structural_containers_stay_transparent() {
        // `common_filter` drops exactly GenericContainer and TextRun, so this
        // set is what keeps layout boxes from flooding the consumer. Promoting
        // any of these is the documented mistake.
        for role in [
            "AXGroup",
            "AXSplitGroup",
            "AXScrollArea",
            "AXLayoutArea",
            "AXLayoutItem",
            "AXUnknown",
        ] {
            assert_eq!(map(role, None), Role::GenericContainer, "{role} must stay transparent");
        }
    }

    #[test]
    fn coverage_is_answerable_separately_from_the_mapping() {
        // A deliberately transparent role and an unmapped one both land on
        // GenericContainer, so only this can tell them apart.
        assert!(is_known("AXGroup", None), "AXGroup is transparent on purpose");
        assert!(!is_known("AXSomeFutureRole", None), "and this simply is not mapped");
        assert!(is_known("AXTextField", Some("AXSearchField")));
        assert!(is_known("AXAnything", Some("AXDialog")), "a known subrole is enough");
    }

    #[test]
    fn an_unmapped_role_hides_rather_than_puzzles() {
        // `Unknown` survives the consumer filter and would surface a mystery
        // node; `GenericContainer` lets the children speak instead.
        assert_eq!(map("AXSomeFutureRole", None), Role::GenericContainer);
        assert_ne!(map("AXSomeFutureRole", None), Role::Unknown);
    }

    #[test]
    fn only_a_named_generic_container_is_promoted() {
        assert_eq!(refine(Role::GenericContainer, false), Role::GenericContainer);
        assert_eq!(refine(Role::GenericContainer, true), Role::Group);
        // Refinement must never touch a role that was already meaningful.
        assert_eq!(refine(Role::Button, true), Role::Button);
        assert_eq!(refine(Role::Label, true), Role::Label);
    }

    /// The roles observed live on 2026-08-15 across Finder, TextEdit, Safari,
    /// System Settings and 1Password. Every one must map to something the
    /// consumer can use, or deliberately to `GenericContainer`.
    #[test]
    fn every_role_seen_on_a_real_desktop_is_accounted_for() {
        let observed = [
            ("AXWindow", Role::Window),
            ("AXButton", Role::Button),
            ("AXGroup", Role::GenericContainer),
            ("AXStaticText", Role::Label),
            ("AXHeading", Role::Heading),
            ("AXImage", Role::Image),
            ("AXLink", Role::Link),
            ("AXMenuButton", Role::Button),
            ("AXScrollArea", Role::GenericContainer),
            ("AXSplitGroup", Role::GenericContainer),
            ("AXTabGroup", Role::TabList),
            ("AXTextField", Role::TextInput),
            ("AXToolbar", Role::Toolbar),
            ("AXWebArea", Role::RootWebArea),
            ("AXTextArea", Role::MultilineTextInput),
        ];
        for (role, expected) in observed {
            assert_eq!(map(role, None), expected, "{role}");
        }
    }

    #[test]
    fn window_detection_matches_what_discovery_filters_on() {
        // Finder publishes its desktop inside `AXWindows` as an AXScrollArea;
        // discovery drops it on exactly this predicate.
        assert!(is_window("AXWindow"));
        assert!(!is_window("AXScrollArea"));
        assert!(!is_window("AXSheet"));
    }
}
