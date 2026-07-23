//! Out-of-band hvsocket transport to the daemon in the WSL distro.
//!
//! The DVC channel stays vestigial; tree data flows over an hvsocket to
//! `accesskit_remoted --vsock`. The WSL VM id comes from the host RDP client's
//! own command line (`/v:<guid>`), which changes on every VM boot — parse it
//! fresh on each connect, never cache it.

use std::ffi::OsString;
use uuid::Uuid;

/// Extract the WSL VM id from the host process command line: the value of the
/// `/v:<guid>` argument.
pub fn parse_vm_id(args: impl Iterator<Item = OsString>) -> Option<Uuid> {
    args.filter_map(|a| a.into_string().ok())
        .find_map(|a| a.strip_prefix("/v:").and_then(|v| v.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter().map(OsString::from).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_msrdc_command_line() {
        let vm = parse_vm_id(args(&[
            "msrdc.exe",
            "/wslg",
            "/silent",
            "/v:FBDDE2F2-6CC4-4A2A-AC4D-CE69559CADC5",
            "/hvsocketserviceid:00000001-FACB-11E6-BD58-64006A7986D3",
            "/plugin:WSLDVC_PACKAGE",
            r"C:\Program Files\WSL\wslg.rdp",
        ]));
        assert_eq!(vm, Some("FBDDE2F2-6CC4-4A2A-AC4D-CE69559CADC5".parse().unwrap()));
    }

    #[test]
    fn missing_v_arg_is_none() {
        assert_eq!(parse_vm_id(args(&["mstsc.exe", "/wslg", "/silent"])), None);
    }

    #[test]
    fn malformed_guid_is_none() {
        assert_eq!(parse_vm_id(args(&["msrdc.exe", "/v:not-a-guid"])), None);
    }

    #[test]
    fn lowercase_guid_parses() {
        let vm = parse_vm_id(args(&["msrdc.exe", "/v:fbdde2f2-6cc4-4a2a-ac4d-ce69559cadc5"]));
        assert_eq!(vm, Some("fbdde2f2-6cc4-4a2a-ac4d-ce69559cadc5".parse().unwrap()));
    }
}
