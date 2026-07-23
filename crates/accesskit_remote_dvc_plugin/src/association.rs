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

/// Strip WSLg's RAIL decorations from a window title: the copy-mode prefix if
/// present, then a ` (<distro>)` suffix anchored to the known distro name.
pub fn normalize_rail_title<'a>(raw: &'a str, distro: &str) -> &'a str {
    let title = raw.strip_prefix(COPY_MODE_PREFIX).unwrap_or(raw);
    let suffix = format!(" ({distro})");
    title.strip_suffix(&suffix).unwrap_or(title)
}

/// Match a RAIL window against the remote window list: normalized-title
/// equality, disambiguated by app id when several windows share a title.
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
        [(id, _)] => Some(*id),
        several => {
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
        WindowInfo {
            title: title.to_owned(),
            app: AppInfo {
                name: "app".to_owned(),
                app_id: app_id.map(str::to_owned),
                pid: Some(1234),
                toolkit: Some("GTK".to_owned()),
                toolkit_version: None,
            },
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
        RailWindow {
            server_window_id: 0x1_0000_0005,
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
}
