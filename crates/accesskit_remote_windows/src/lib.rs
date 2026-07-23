//! UIA host (Windows): exposes remote AccessKit trees to UI Automation.
//!
//! For the remote family what `accesskit_windows` is in-process: manages one
//! subclassing adapter per associated window (RAIL windows in the RDP client
//! for WSLg), catches window creation via in-context WinEvent hooks, and
//! implements the association between remote window keys and local HWNDs.
#![cfg(target_os = "windows")]
