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

/// Whether this process was launched from an SSH session.
///
/// Worth detecting because it changes the remedy entirely: macOS attributes an
/// Accessibility grant to a *responsible* application, and a process tree
/// rooted in `sshd` has no GUI application to inherit one from. Granting the
/// binary itself does not help — the check still fails.
fn is_ssh_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

/// The error a source returns when the grant is missing, spelling out the fix
/// for the situation the caller is actually in.
pub fn untrusted_message() -> String {
    if is_ssh_session() {
        return "no Accessibility permission, and this is an SSH session.\n\
             macOS attributes the grant to a responsible GUI application, and a \
             process tree rooted in sshd has none — so granting this binary will \
             not help.\n\
             Either add /usr/libexec/sshd-keygen-wrapper under System Settings > \
             Privacy & Security > Accessibility and reconnect (this grants every \
             later SSH session the same access), or run from a terminal \
             application on the Mac itself with that terminal granted."
            .to_owned();
    }
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

    /// The remedy differs by situation, and pointing someone at the wrong one
    /// costs them a reboot's worth of confusion — so each branch is pinned.
    #[test]
    fn the_untrusted_message_matches_the_situation() {
        let message = untrusted_message();
        assert!(message.contains("Accessibility"), "{message}");
        if is_ssh_session() {
            assert!(
                message.contains("sshd"),
                "over SSH, granting the binary does nothing; the message must say so: {message}"
            );
        } else {
            assert!(
                message.contains("rebuild"),
                "a grant silently lost on rebuild is the papercut worth warning about: {message}"
            );
        }
    }
}
