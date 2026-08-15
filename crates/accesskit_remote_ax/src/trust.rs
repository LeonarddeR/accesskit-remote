//! The Accessibility (TCC) permission gate.
//!
//! Every AX read in this crate returns `kAXErrorAPIDisabled` — or, worse,
//! plausible-looking emptiness — until the host process holds the
//! Accessibility grant. That is a per-*binary* grant keyed on the code
//! signature, so it has no Linux counterpart: the AT-SPI source needed only a
//! bus to connect to.
//!
//! Checked once at construction so the failure is one clear message rather
//! than an empty desktop.

use objc2_application_services::AXIsProcessTrusted;

/// Whether this process holds the Accessibility grant.
///
/// Deliberately the promptless variant. The prompting form
/// (`AXIsProcessTrustedWithOptions` with `kAXTrustedCheckOptionPrompt`) opens a
/// system dialog, which is wrong for a daemon that may be running headless
/// under launchd with nobody there to answer it — and the prompt is shown at
/// most once per binary anyway, so it is not even reliable for a human.
pub fn is_trusted() -> bool {
    // SAFETY: no arguments, no pointers; reads a process-global TCC decision.
    unsafe { AXIsProcessTrusted() }
}

/// The error a source returns when the grant is missing, spelling out the fix
/// and the trap that follows it.
pub fn untrusted_message() -> String {
    let path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "this binary".to_owned());
    format!(
        "no Accessibility permission: add {path} under System Settings > Privacy \
         & Security > Accessibility.\n\
         The grant is keyed on the binary's code signature, so an unsigned or \
         ad-hoc-signed binary loses it on every rebuild. For development, grant \
         it to the terminal application and run under `cargo run` instead, which \
         inherits the grant."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check must be callable and total on any machine, granted or not —
    /// CI runners have no grant and must not hang or trap here.
    #[test]
    fn is_trusted_answers_without_prompting() {
        let _ = is_trusted();
    }

    #[test]
    fn the_untrusted_message_names_the_rebuild_trap() {
        let message = untrusted_message();
        assert!(message.contains("Accessibility"), "{message}");
        assert!(
            message.contains("rebuild"),
            "a grant silently lost on rebuild is the papercut worth warning about: {message}"
        );
    }
}
