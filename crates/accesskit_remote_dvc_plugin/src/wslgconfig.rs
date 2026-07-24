//! Editing the WSLg `[system-distro-env]` `WSLG_USE_WSLDVC_PRIVATE` flag in the
//! user's `.wslgconfig`, touching only that one line so surrounding content
//! (comments, blank lines, other sections/keys) survives unchanged.

use std::io;
use std::path::{Path, PathBuf};

const SECTION: &str = "[system-distro-env]";
const KEY: &str = "WSLG_USE_WSLDVC_PRIVATE";
const VALUE: &str = "true";

/// Ensure `[system-distro-env]` contains `WSLG_USE_WSLDVC_PRIVATE=true`.
pub fn set_flag(text: &str) -> String {
    edit(text, Op::Set)
}

/// Remove `WSLG_USE_WSLDVC_PRIVATE` from `[system-distro-env]`.
pub fn clear_flag(text: &str) -> String {
    edit(text, Op::Clear)
}

enum Op {
    Set,
    Clear,
}

fn is_section_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.ends_with(']')
}

fn is_target_section(line: &str) -> bool {
    line.trim().eq_ignore_ascii_case(SECTION)
}

fn is_target_key(line: &str) -> bool {
    let t = line.trim();
    if t.starts_with(';') || t.starts_with('#') {
        return false;
    }
    matches!(t.split_once('='), Some((name, _)) if name.trim().eq_ignore_ascii_case(KEY))
}

fn value_is(line: &str, want: &str) -> bool {
    matches!(line.trim().split_once('='), Some((_, v)) if v.trim() == want)
}

/// The `[start, end)` line range of the section body following the header at
/// `header`, ending at the next section header or end of file.
fn section_body(lines: &[String], header: usize) -> (usize, usize) {
    let start = header + 1;
    let end = lines[start..]
        .iter()
        .position(|l| is_section_header(l))
        .map(|p| start + p)
        .unwrap_or(lines.len());
    (start, end)
}

fn edit(text: &str, op: Op) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let had_trailing = text.is_empty() || text.ends_with('\n');
    // `str::lines` drops line terminators; re-join with the detected newline.
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let section = lines.iter().position(|l| is_target_section(l));

    match op {
        Op::Set => {
            let canonical = format!("{KEY}={VALUE}");
            match section {
                Some(header) => {
                    let (start, end) = section_body(&lines, header);
                    match lines[start..end].iter().position(|l| is_target_key(l)) {
                        Some(p) => {
                            let k = start + p;
                            if !value_is(&lines[k], VALUE) {
                                lines[k] = canonical;
                            }
                        }
                        None => lines.insert(start, canonical),
                    }
                }
                None => {
                    lines.push(SECTION.to_string());
                    lines.push(canonical);
                }
            }
        }
        Op::Clear => {
            if let Some(header) = section {
                let (start, end) = section_body(&lines, header);
                if let Some(p) = lines[start..end].iter().position(|l| is_target_key(l)) {
                    lines.remove(start + p);
                }
            }
        }
    }

    let mut out = lines.join(nl);
    if had_trailing && !out.is_empty() {
        out.push_str(nl);
    }
    out
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".wslgconfig"))
}

fn read_or_empty(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Write via a temp file in the same directory + rename, so a crash mid-write
/// never leaves a truncated `.wslgconfig`. On Windows `rename` replaces the
/// destination (same volume → atomic).
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".wslgconfig.tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

fn apply(path: &Path, transform: impl Fn(&str) -> String) -> io::Result<()> {
    let current = read_or_empty(path)?;
    let updated = transform(&current);
    if updated == current && path.exists() {
        return Ok(());
    }
    write_atomic(path, &updated)
}

/// Ensure the flag is set in `%USERPROFILE%\.wslgconfig`.
pub fn install() -> io::Result<()> {
    let path = config_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "USERPROFILE not set"))?;
    apply(&path, set_flag)
}

/// Remove the flag from `%USERPROFILE%\.wslgconfig` (no-op if the file is absent).
pub fn uninstall() -> io::Result<()> {
    let path = config_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "USERPROFILE not set"))?;
    if !path.exists() {
        return Ok(());
    }
    apply(&path, clear_flag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_on_empty_creates_section_and_key() {
        assert_eq!(set_flag(""), "[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=true\n");
    }

    #[test]
    fn set_appends_section_when_absent() {
        assert_eq!(
            set_flag("[other]\nfoo=bar\n"),
            "[other]\nfoo=bar\n[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=true\n"
        );
    }

    #[test]
    fn set_inserts_key_under_existing_section() {
        assert_eq!(
            set_flag("[system-distro-env]\n"),
            "[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=true\n"
        );
    }

    #[test]
    fn set_replaces_wrong_value() {
        assert_eq!(
            set_flag("[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=false\n"),
            "[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=true\n"
        );
    }

    #[test]
    fn set_is_noop_when_already_true() {
        let input = "[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=true\n";
        assert_eq!(set_flag(input), input);
    }

    #[test]
    fn set_preserves_comments_and_other_sections() {
        let input = "; top comment\n[other]\nx=1\n[system-distro-env]\n# inner\n";
        let out = set_flag(input);
        assert!(out.contains("; top comment"), "{out:?}");
        assert!(out.contains("[other]\nx=1"), "{out:?}");
        assert!(out.contains("# inner"), "{out:?}");
        assert!(out.contains("WSLG_USE_WSLDVC_PRIVATE=true"), "{out:?}");
    }

    #[test]
    fn set_idempotent() {
        for input in [
            "",
            "[system-distro-env]\n",
            "[other]\nx=1\n",
            "[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=false\n",
        ] {
            let once = set_flag(input);
            assert_eq!(set_flag(&once), once, "not idempotent for {input:?}");
        }
    }

    #[test]
    fn clear_removes_only_the_key() {
        assert_eq!(
            clear_flag("[system-distro-env]\nWSLG_USE_WSLDVC_PRIVATE=true\n"),
            "[system-distro-env]\n"
        );
    }

    #[test]
    fn clear_noop_when_absent() {
        let input = "[other]\nx=1\n";
        assert_eq!(clear_flag(input), input);
    }

    #[test]
    fn clear_preserves_other_keys_in_section() {
        assert_eq!(
            clear_flag("[system-distro-env]\nWSLG_USE_MSTSC=true\nWSLG_USE_WSLDVC_PRIVATE=true\n"),
            "[system-distro-env]\nWSLG_USE_MSTSC=true\n"
        );
    }

    #[test]
    fn preserves_crlf() {
        assert_eq!(
            set_flag("[system-distro-env]\r\n"),
            "[system-distro-env]\r\nWSLG_USE_WSLDVC_PRIVATE=true\r\n"
        );
    }

    #[test]
    fn io_roundtrip_creates_updates_and_clears() {
        let path =
            std::env::temp_dir().join(format!("wslgcfg-test-{}.ini", std::process::id()));
        let _ = std::fs::remove_file(&path);

        apply(&path, set_flag).unwrap();
        let after_set = std::fs::read_to_string(&path).unwrap();
        assert!(after_set.contains("[system-distro-env]"), "{after_set:?}");
        assert!(after_set.contains("WSLG_USE_WSLDVC_PRIVATE=true"), "{after_set:?}");

        apply(&path, set_flag).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            after_set,
            "second set not idempotent"
        );

        apply(&path, clear_flag).unwrap();
        let after_clear = std::fs::read_to_string(&path).unwrap();
        assert!(!after_clear.contains("WSLG_USE_WSLDVC_PRIVATE"), "{after_clear:?}");

        let _ = std::fs::remove_file(&path);
    }
}
