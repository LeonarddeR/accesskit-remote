//! AX attribute and action names.
//!
//! These are `CFSTR` macros in the system headers rather than exported
//! symbols, so no binding generator can surface them and they are spelled out
//! here — the strings themselves are the stable public contract.
//!
//! Held as a struct built once rather than as constants because every use site
//! needs a `CFString`, and minting one per attribute per node would allocate
//! millions of times across a desktop walk. One [`Names`] lives on the AX
//! thread for the process's lifetime.

use objc2_core_foundation::{CFRetained, CFString};

macro_rules! names {
    ($($field:ident => $value:literal),* $(,)?) => {
        /// The attribute and action names this crate reads, pre-converted.
        pub struct Names {
            $(pub $field: CFRetained<CFString>,)*
        }

        impl Names {
            pub fn new() -> Self {
                Self {
                    $($field: CFString::from_static_str($value),)*
                }
            }
        }

        #[cfg(test)]
        const ALL: &[(&str, &str)] = &[$((stringify!($field), $value),)*];
    };
}

names! {
    // Identity and structure.
    role => "AXRole",
    subrole => "AXSubrole",
    role_description => "AXRoleDescription",
    title => "AXTitle",
    description => "AXDescription",
    help => "AXHelp",
    identifier => "AXIdentifier",
    children => "AXChildren",
    parent => "AXParent",
    windows => "AXWindows",
    main_window => "AXMainWindow",
    focused_window => "AXFocusedWindow",
    focused_ui_element => "AXFocusedUIElement",
    menu_bar => "AXMenuBar",

    // Geometry. `AXFrame` carries origin and size together and is present on
    // every element surveyed, so it is the one read the walk makes.
    frame => "AXFrame",
    position => "AXPosition",
    size => "AXSize",

    // State.
    value => "AXValue",
    enabled => "AXEnabled",
    focused => "AXFocused",
    selected => "AXSelected",
    expanded => "AXExpanded",
    main => "AXMain",
    minimized => "AXMinimized",
    hidden => "AXHidden",
    placeholder => "AXPlaceholderValue",
    title_ui_element => "AXTitleUIElement",

    // The Chromium/Electron accessibility opt-in. Not a real attribute on any
    // native element; writing it is the request.
    manual_accessibility => "AXManualAccessibility",
}

impl Default for Names {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_an_ax_prefixed_string_that_survives_conversion() {
        let names = Names::new();
        // Guards against a typo'd literal, which would otherwise present as an
        // attribute that silently never reads on any application.
        for (field, value) in ALL {
            assert!(value.starts_with("AX"), "{field} = {value:?} is not an AX name");
            assert!(!value.contains(' '), "{field} = {value:?} has whitespace");
        }
        assert_eq!(names.role.to_string(), "AXRole");
        assert_eq!(names.manual_accessibility.to_string(), "AXManualAccessibility");
    }

    #[test]
    fn no_two_fields_share_a_name() {
        let mut seen: Vec<&str> = ALL.iter().map(|(_, value)| *value).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "a duplicated attribute name is a copy-paste slip");
    }
}
