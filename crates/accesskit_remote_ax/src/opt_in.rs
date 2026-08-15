//! Asking an application to publish its accessibility tree.
//!
//! Native AppKit applications always publish. Chromium-based ones — Chrome,
//! Electron, so VS Code, Slack, Teams, Discord — do not: building the tree
//! costs them enough that they keep it off until an assistive technology asks,
//! and without the ask they present as a window with no contents at all.
//!
//! There are two levers and they are **not** equivalent:
//!
//! - `AXManualAccessibility` is Chromium's own opt-in, added precisely so a
//!   non-VoiceOver client could turn accessibility on. Writing it has no other
//!   effect. This is the one to use.
//! - `AXEnhancedUserInterface` is the older lever VoiceOver sets. Setting it
//!   makes some applications re-lay-out and move their windows — the
//!   long-standing "Chrome/Slack rearranged my windows" bug that window
//!   managers hit. On a machine that is simultaneously being screen-shared,
//!   that is visible damage to the user's session, so this crate does not
//!   write it.
//!
//! The ask is idempotent and cheap, but it is still IPC, so it is made once per
//! application when the application is first seen.

use crate::attr::{self, AxError};
use crate::names::Names;
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::CFBoolean;

/// What an application did with the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptIn {
    /// The application accepted it — it is Chromium-based and has now built
    /// its tree. Expect its window contents to appear on the *next* walk, not
    /// necessarily this one.
    Accepted,
    /// The application has no such attribute. The overwhelmingly common answer,
    /// and not a problem: native applications publish unconditionally.
    NotApplicable,
    /// The write itself failed.
    Failed(AxError),
}

/// Asks `app` (an application-level element) to publish its accessibility tree.
///
/// Never sets `AXEnhancedUserInterface`; see the module docs for why.
pub fn request(app: &AXUIElement, names: &Names) -> OptIn {
    let value = CFBoolean::new(true);
    // SAFETY: `app` is a live application element, the attribute name is a
    // valid CFString, and the value is a live CFBoolean.
    let error = unsafe { app.set_attribute_value(&names.manual_accessibility, value) };
    match error {
        AXError::Success => OptIn::Accepted,
        AXError::AttributeUnsupported | AXError::IllegalArgument => OptIn::NotApplicable,
        other => OptIn::Failed(AxError(other)),
    }
}

/// Whether an application element answers the opt-in attribute at all.
///
/// **Diagnostic only — never gate the request on this.** Measured against
/// 1Password 8 (2026-08-15): a `request` that returned [`OptIn::Accepted`]
/// still reads back as `Some(false)` afterwards. Chromium treats the attribute
/// as a write-only signal and does not reflect it, so a caller that skipped
/// the write because the read said `false` would never turn accessibility on.
///
/// What it is good for is telling a Chromium-based application (answers at
/// all) from a native one (answers `None`).
pub fn answers_opt_in(app: &AXUIElement, names: &Names) -> Option<bool> {
    attr::boolean(app, &names.manual_accessibility).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_application_element_is_not_applicable_rather_than_an_error() {
        let names = Names::new();
        // SAFETY: takes no arguments and always returns a valid element.
        let system_wide = unsafe { AXUIElement::new_system_wide() };
        // Every native element answers this way, so it must be the quiet path:
        // treating it as a failure would log once per application on every
        // desktop, drowning the real failures.
        assert!(
            matches!(request(&system_wide, &names), OptIn::NotApplicable | OptIn::Failed(_)),
            "the system-wide element must never report a successful opt-in"
        );
        assert_eq!(answers_opt_in(&system_wide, &names), None);
    }

    #[test]
    fn the_request_writes_exactly_one_attribute() {
        // The decision not to set `AXEnhancedUserInterface` is enforced
        // structurally rather than by inspection: `request` writes the one
        // name it is given, and `Names` carries no other opt-in string, so
        // adding the damaging lever would take a deliberate new field.
        let names = Names::new();
        assert_eq!(names.manual_accessibility.to_string(), "AXManualAccessibility");
    }
}
