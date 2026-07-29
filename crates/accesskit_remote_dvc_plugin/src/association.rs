//! Matching remote windows to RAIL HWNDs.
//!
//! msrdc titles a RAIL window `[WARN:COPY MODE] <toplevel title> (<distro>)`
//! (the prefix only in RAIL copy mode, the suffix always); the undecorated
//! toplevel title equals the AT-SPI frame name for GTK apps, which is the
//! remote side's window title. The `WslgServerWindowId` HWND property is the
//! stable per-HWND key; the app id corroborates ambiguous title matches.

use accesskit_remote::WindowId;
use accesskit_remote_client::WindowInfo;

const COPY_MODE_PREFIX: &str = "[WARN:COPY MODE] ";

/// A discovered `RAIL_WINDOW` HWND's identifying data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailWindow {
    /// The `WslgServerWindowId` HWND property.
    pub server_window_id: u64,
    /// Raw window title, WSLg decorations included.
    pub title: String,
    /// The HWND's AppUserModelID (msrdc sets it to the Linux app id).
    pub app_user_model_id: Option<String>,
}

impl RailWindow {
    /// The Weston-side window id encoded in the low 32 bits of
    /// `server_window_id`; `None` when the property was absent (reads 0).
    pub fn weston_window_id(&self) -> Option<u64> {
        (self.server_window_id != 0).then(|| self.server_window_id & 0xFFFF_FFFF)
    }
}

/// Strip WSLg's RAIL decorations from a window title: the copy-mode prefix if
/// present, then a ` (<distro>)` suffix anchored to the known distro name.
pub fn normalize_rail_title<'a>(raw: &'a str, distro: &str) -> &'a str {
    let title = raw.strip_prefix(COPY_MODE_PREFIX).unwrap_or(raw);
    let suffix = format!(" ({distro})");
    title.strip_suffix(&suffix).unwrap_or(title)
}

/// Match a RAIL window against the remote window list: normalized-title
/// equality is the outer gate, then the Weston window id narrows (or vetoes)
/// a title match, then app id disambiguates whatever remains ambiguous.
/// Returns `None` when there is no match or the match stays ambiguous.
pub fn match_window(
    rail: &RailWindow,
    distro: &str,
    client_windows: &[(WindowId, WindowInfo)],
) -> Option<WindowId> {
    let title = normalize_rail_title(&rail.title, distro);
    let by_title: Vec<&(WindowId, WindowInfo)> =
        client_windows.iter().filter(|(_, info)| info.title == title).collect();
    match by_title.as_slice() {
        [] => None,
        [(id, info)] => match (rail.weston_window_id(), info.native_window_id) {
            (Some(rid), Some(nid)) if rid != nid => None,
            _ => Some(*id),
        },
        several => {
            if let Some(rid) = rail.weston_window_id() {
                let claiming: Vec<WindowId> = several
                    .iter()
                    .filter(|(_, info)| info.native_window_id == Some(rid))
                    .map(|(id, _)| *id)
                    .collect();
                if let [id] = claiming.as_slice() {
                    return Some(*id);
                }
                if claiming.is_empty() {
                    let unclaimed: Vec<WindowId> = several
                        .iter()
                        .filter(|(_, info)| info.native_window_id.is_none())
                        .map(|(id, _)| *id)
                        .collect();
                    if let [id] = unclaimed.as_slice() {
                        return Some(*id);
                    }
                }
            }
            let aumid = rail.app_user_model_id.as_deref()?;
            let by_app: Vec<WindowId> = several
                .iter()
                .filter(|(_, info)| info.app.app_id.as_deref() == Some(aumid))
                .map(|(id, _)| *id)
                .collect();
            match by_app.as_slice() {
                [id] => Some(*id),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit_remote::AppInfo;

    fn info(title: &str, app_id: Option<&str>) -> WindowInfo {
        info_native(title, app_id, None)
    }

    fn info_native(title: &str, app_id: Option<&str>, native_window_id: Option<u64>) -> WindowInfo {
        WindowInfo {
            title: title.to_owned(),
            app: AppInfo {
                name: "app".to_owned(),
                app_id: app_id.map(str::to_owned),
                pid: Some(1234),
                toolkit: Some("GTK".to_owned()),
                toolkit_version: None,
            },
            native_window_id,
        }
    }

    #[test]
    fn normalize_strips_prefix_and_distro_suffix() {
        assert_eq!(
            normalize_rail_title("[WARN:COPY MODE] New Document (Draft) - Text Editor (Debian)", "Debian"),
            "New Document (Draft) - Text Editor"
        );
    }

    #[test]
    fn normalize_without_prefix_vail_mode() {
        assert_eq!(normalize_rail_title("Text Editor (Debian)", "Debian"), "Text Editor");
    }

    #[test]
    fn normalize_keeps_unrelated_parens() {
        assert_eq!(normalize_rail_title("notes (draft) (Debian)", "Debian"), "notes (draft)");
        assert_eq!(normalize_rail_title("notes (draft)", "Debian"), "notes (draft)");
    }

    #[test]
    fn normalize_without_suffix() {
        assert_eq!(normalize_rail_title("Text Editor", "Debian"), "Text Editor");
    }

    #[test]
    fn normalize_strips_only_one_suffix() {
        assert_eq!(normalize_rail_title("App (Debian) (Debian)", "Debian"), "App (Debian)");
    }

    fn rail(title: &str, aumid: Option<&str>) -> RailWindow {
        rail_with_server_id(title, aumid, 0x1_0000_0005)
    }

    fn rail_with_server_id(title: &str, aumid: Option<&str>, server_window_id: u64) -> RailWindow {
        RailWindow {
            server_window_id,
            title: title.to_owned(),
            app_user_model_id: aumid.map(str::to_owned),
        }
    }

    #[test]
    fn match_unique_title() {
        let windows = vec![
            (WindowId(1), info("Text Editor", Some("org.gnome.TextEditor"))),
            (WindowId(2), info("Files", Some("org.gnome.Nautilus"))),
        ];
        let r = rail("[WARN:COPY MODE] Text Editor (Debian)", None);
        assert_eq!(match_window(&r, "Debian", &windows), Some(WindowId(1)));
    }

    #[test]
    fn match_ambiguous_resolved_by_app_id() {
        let windows = vec![
            (WindowId(1), info("Untitled", Some("org.gnome.TextEditor"))),
            (WindowId(2), info("Untitled", Some("org.gnome.Nautilus"))),
        ];
        let r = rail("Untitled (Debian)", Some("org.gnome.Nautilus"));
        assert_eq!(match_window(&r, "Debian", &windows), Some(WindowId(2)));
    }

    #[test]
    fn match_ambiguous_unresolved_is_none() {
        let windows = vec![
            (WindowId(1), info("Untitled", Some("org.gnome.TextEditor"))),
            (WindowId(2), info("Untitled", Some("org.gnome.TextEditor"))),
        ];
        let r = rail("Untitled (Debian)", Some("org.gnome.TextEditor"));
        assert_eq!(match_window(&r, "Debian", &windows), None);
    }

    #[test]
    fn match_no_title_hit_is_none() {
        let windows = vec![(WindowId(1), info("Files", Some("org.gnome.Nautilus")))];
        let r = rail("Text Editor (Debian)", None);
        assert_eq!(match_window(&r, "Debian", &windows), None);
    }

    #[test]
    fn weston_window_id_masks_high_bits() {
        let claimed = RailWindow {
            server_window_id: 0x1_0000_0005,
            title: String::new(),
            app_user_model_id: None,
        };
        assert_eq!(claimed.weston_window_id(), Some(5));

        let missing = RailWindow { server_window_id: 0, ..claimed };
        assert_eq!(missing.weston_window_id(), None);
    }

    #[test]
    fn match_ambiguous_resolved_by_native_window_id() {
        let windows = vec![
            (WindowId(1), info_native("Untitled", None, Some(5))),
            (WindowId(2), info_native("Untitled", None, Some(9))),
        ];
        let r = rail_with_server_id("Untitled (Debian)", None, 0x1_0000_0009);
        assert_eq!(match_window(&r, "Debian", &windows), Some(WindowId(2)));
    }

    #[test]
    fn ambiguous_with_unclaimed_id_binds_sole_unclaimed_candidate() {
        let windows = vec![
            (WindowId(1), info_native("Untitled", None, Some(5))),
            (WindowId(2), info_native("Untitled", None, None)),
        ];
        let r = rail_with_server_id("Untitled (Debian)", None, 0x1_0000_0009);
        assert_eq!(match_window(&r, "Debian", &windows), Some(WindowId(2)));
    }

    #[test]
    fn single_title_match_vetoed_by_conflicting_native_id() {
        let windows = vec![(WindowId(1), info_native("Text Editor", None, Some(5)))];
        let r = rail_with_server_id("Text Editor (Debian)", None, 0x1_0000_0009);
        assert_eq!(match_window(&r, "Debian", &windows), None);
    }

    #[test]
    fn single_title_match_with_matching_native_id_binds() {
        let windows = vec![(WindowId(1), info_native("Text Editor", None, Some(5)))];
        let r = rail_with_server_id("Text Editor (Debian)", None, 0x1_0000_0005);
        assert_eq!(match_window(&r, "Debian", &windows), Some(WindowId(1)));
    }

    #[test]
    fn missing_prop_degrades_to_title_matching() {
        let unique = vec![(WindowId(1), info_native("Text Editor", None, Some(5)))];
        let r = rail_with_server_id("Text Editor (Debian)", None, 0);
        assert_eq!(match_window(&r, "Debian", &unique), Some(WindowId(1)));

        let ambiguous = vec![
            (WindowId(1), info("Untitled", Some("org.gnome.TextEditor"))),
            (WindowId(2), info("Untitled", Some("org.gnome.TextEditor"))),
        ];
        let r = RailWindow {
            server_window_id: 0,
            title: "Untitled (Debian)".to_owned(),
            app_user_model_id: Some("org.gnome.TextEditor".to_owned()),
        };
        assert_eq!(match_window(&r, "Debian", &ambiguous), None);
    }
}
