//! Workspace automation: build and package the distributable artifacts.
//!
//! Usage:
//!   cargo xtask build-windows [--target <triple>]...
//!   cargo xtask dist [--staging <dir>]

use std::path::{Path, PathBuf};
use std::process::{Command as Proc, ExitCode};

/// This workspace's version, stamped into the installer.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The DVC plugin crate whose DLL ships in the installer.
const DVC_PLUGIN: &str = "accesskit_remote_dvc_plugin";

/// Windows targets shipped by default (x64 + arm64).
const WINDOWS_TARGETS: [&str; 2] = ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"];

/// The parsed subcommand.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Build the DVC plugin DLL in release for each target.
    BuildWindows { targets: Vec<String> },
    /// Assemble the installer from a staging directory via makensis.
    Dist { staging: Option<PathBuf> },
}

fn usage() -> String {
    "usage: cargo xtask <build-windows [--target <triple>]... | dist [--staging <dir>]>".to_string()
}

fn parse(args: &[String]) -> Result<Command, String> {
    let (cmd, rest) = args.split_first().ok_or_else(usage)?;
    match cmd.as_str() {
        "build-windows" => parse_build_windows(rest),
        "dist" => parse_dist(rest),
        other => Err(format!("unknown command: {other}\n{}", usage())),
    }
}

fn parse_build_windows(rest: &[String]) -> Result<Command, String> {
    let mut targets = Vec::new();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--target" => {
                let t = it
                    .next()
                    .ok_or_else(|| "--target requires a value".to_string())?;
                targets.push(t.clone());
            }
            other => return Err(format!("unknown build-windows option: {other}")),
        }
    }
    if targets.is_empty() {
        targets = WINDOWS_TARGETS.iter().map(|s| s.to_string()).collect();
    }
    Ok(Command::BuildWindows { targets })
}

fn parse_dist(rest: &[String]) -> Result<Command, String> {
    let mut staging = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--staging" => {
                let p = it
                    .next()
                    .ok_or_else(|| "--staging requires a path".to_string())?;
                staging = Some(PathBuf::from(p));
            }
            other => return Err(format!("unknown dist option: {other}")),
        }
    }
    Ok(Command::Dist { staging })
}

/// The `-D` defines passed to makensis so the installer script can locate the
/// staged payload and stamp its version. NSIS accepts `-D` on every platform.
fn nsis_defines(version: &str, staging: &Path, outfile: &Path) -> Vec<String> {
    vec![
        format!("-DVERSION={version}"),
        format!("-DSTAGING={}", staging.display()),
        format!("-DOUTFILE={}", outfile.display()),
    ]
}

/// The repository root, one level above this crate's manifest.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest dir has a parent")
        .to_path_buf()
}

/// Locate the makensis executable: `$MAKENSIS`, then `PATH`, then the default
/// NSIS install directory on Windows.
fn find_makensis() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MAKENSIS") {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "makensis.exe" } else { "makensis" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if cfg!(windows) {
        for base in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(pf) = std::env::var_os(base) {
                let candidate = PathBuf::from(pf).join("NSIS").join("makensis.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn run_build_windows(targets: &[String]) -> Result<(), String> {
    for target in targets {
        eprintln!("xtask: building {DVC_PLUGIN} (release) for {target}");
        let status = Proc::new("cargo")
            .args(["build", "--release", "--target", target, "-p", DVC_PLUGIN])
            .status()
            .map_err(|e| format!("failed to spawn cargo: {e}"))?;
        if !status.success() {
            return Err(format!("cargo build failed for {target}"));
        }
    }
    Ok(())
}

fn run_dist(staging: Option<&Path>) -> Result<(), String> {
    let root = repo_root();
    let script = root.join("dist").join("windows").join("installer.nsi");
    if !script.exists() {
        return Err(format!(
            "installer script not found at {} (added in the installer phase)",
            script.display()
        ));
    }
    let staging = staging
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("target").join("dist"));
    let outfile = root
        .join("target")
        .join(format!("AccessKitRemote-Setup-{VERSION}.exe"));
    let makensis =
        find_makensis().ok_or_else(|| "makensis not found; install NSIS or set MAKENSIS".to_string())?;

    let defines = nsis_defines(VERSION, &staging, &outfile);
    eprintln!("xtask: {} {} {}", makensis.display(), defines.join(" "), script.display());
    let status = Proc::new(&makensis)
        .args(&defines)
        .arg(&script)
        .status()
        .map_err(|e| format!("failed to spawn makensis: {e}"))?;
    if !status.success() {
        return Err("makensis failed".to_string());
    }
    eprintln!("xtask: wrote {}", outfile.display());
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match cmd {
        Command::BuildWindows { targets } => run_build_windows(&targets),
        Command::Dist { staging } => run_dist(staging.as_deref()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn build_windows_defaults_to_both_arches() {
        assert_eq!(
            parse(&v(&["build-windows"])).unwrap(),
            Command::BuildWindows {
                targets: vec![
                    "x86_64-pc-windows-msvc".to_string(),
                    "aarch64-pc-windows-msvc".to_string(),
                ],
            },
        );
    }

    #[test]
    fn build_windows_takes_explicit_targets() {
        assert_eq!(
            parse(&v(&["build-windows", "--target", "aarch64-pc-windows-msvc"])).unwrap(),
            Command::BuildWindows {
                targets: vec!["aarch64-pc-windows-msvc".to_string()],
            },
        );
    }

    #[test]
    fn dist_staging_is_optional() {
        assert_eq!(parse(&v(&["dist"])).unwrap(), Command::Dist { staging: None });
        assert_eq!(
            parse(&v(&["dist", "--staging", "out"])).unwrap(),
            Command::Dist { staging: Some(PathBuf::from("out")) },
        );
    }

    #[test]
    fn unknown_or_missing_command_errors() {
        assert!(parse(&v(&["frobnicate"])).is_err());
        assert!(parse(&v(&[])).is_err());
    }

    #[test]
    fn nsis_defines_carry_version_and_paths() {
        assert_eq!(
            nsis_defines("1.2.3", Path::new("stage"), Path::new("out.exe")),
            vec![
                "-DVERSION=1.2.3".to_string(),
                "-DSTAGING=stage".to_string(),
                "-DOUTFILE=out.exe".to_string(),
            ],
        );
    }
}
