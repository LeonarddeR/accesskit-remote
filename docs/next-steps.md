# Continuation notes

State as of 2026-07-23. Everything below "works and is committed" was
verified live; see `docs/spikes.md` for environment findings and git log
for the build-up.

## What works end to end

- Protocol (`accesskit_remote`): framing, JSON codec, sans-I/O `Session`
  handshake. 24 tests, passing on Windows and Linux (WSL).
- Transport: TCP + vsock listener (Linux) + hvsocket connector (Windows);
  verified across the real WSL boundary. **Default dev port 4750** (52017
  is inside a Hyper-V excluded TCP range; check
  `netsh interface ipv4 show excludedportrange protocol=tcp`).
- Server/client cores; daemon (`accesskit_remoted --tcp|--vsock [PORT]`)
  with a demo source; `probe` example (client crate).
- UIA host (`accesskit_remote_windows::RemoteWindowBinding`) + `viewer`
  example: **full UIA round trip proven** — UIA client read the tree
  (FrameworkId 'AccessKit') and InvokePattern-clicked the demo button,
  label updated through daemon and back. UIA test script pattern: find
  window by name via System.Windows.Automation in Windows PowerShell 5.1.

## In flight (committed, not yet wired)

`accesskit_remote_server` now has `TreeSource` trait + `SourceEvent` +
`apply_source_event`. The daemon still calls `DemoSource` methods
directly. Next steps:

1. Make `DemoSource` implement `TreeSource` (buffer deltas in a pending
   vec, drain via `poll_events`; `initial_state` returns the tuple with
   focus). Generalize `serve`/`pump` in `accesskit_remoted/src/main.rs`
   over `&mut dyn TreeSource`: on tick and after each chunk, drain
   `poll_events()` → `apply_source_event` (only when established;
   drop events when not — initial sync covers state).
2. Build `accesskit_remote_atspi` v0 (the mirror). Design decisions made:
   - `atspi` crate (Odilia, ~0.29) + zbus, tokio current-thread runtime in
     a dedicated thread; bridge to the sync daemon with std mpsc (events
     out) + tokio mpsc (actions in). `AtspiSource` implements `TreeSource`
     over the channels.
   - v0 scope: enumerate desktop → applications → toplevel frames
     (visible+showing); walk subtrees; map AT-SPI role/name/states to
     accesskit nodes (Role::Label gets text via `set_value` — the UIA
     Name for labels comes from value, not label!). NodeIds: sequential
     u64 per D-Bus object path (keep a path→NodeId map per window).
   - Events v0: focus + window lifecycle + `children-changed` → full
     re-walk of that window, send whole tree as one update (overwrite
     semantics make this valid; client store prunes unreachable nodes).
     Fine-grained diffing and the text pattern come later.
   - Actions: Click → Action interface `do_action(0)`; Focus →
     `Component.GrabFocus`.
   - Consistency discipline per Orca analysis (see the "Mirror
     consistency strategy" section of the design plan and
     `P:\a11y\orca`): events are hints; re-query before believing;
     re-walk on structural events; ~60s reconciliation later.
3. Test recipe for the milestone (GTK app tree visible from Windows):
   - WSL: `CARGO_TARGET_DIR=~/target-accesskit-remote cargo build` (drvfs
     is slow; keep target dir native), run
     `accesskit_remoted --vsock 4750`, launch
     `GSK_RENDERER=cairo LIBGL_ALWAYS_SOFTWARE=1 gnome-text-editor`
     (without cairo renderer the GTK4 window may never map under WSLg).
   - Windows: `viewer --hvsocket <vm-id> 4750`; VM ID from msrdc cmdline
     `/v:` (only while a WSLg app runs) or `sudo hcsdiag list` row whose
     owner column is `WSL` (do NOT take the first GUID). Then UIA-inspect
     the viewer window.

## After that

- DVC plugin (`accesskit_remote_dvc_plugin`): port rd_pipe-rs COM
  scaffolding (user holds copyright, relicense MIT/Apache): `src/lib.rs`,
  `class_factory.rs`, `registry.rs`, plugin/listener halves of
  `rd_pipe_plugin.rs`, fake-DVC test harness from `tests/common/mod.rs`.
  New CLSID. Reuse `RemoteWindowBinding` on RAIL HWNDs; association via
  `WslgServerWindowId` HWND props + normalized titles (strip
  `[WARN:COPY MODE] ` prefix and ` (<distro>)` suffix). WSLg loading:
  `OptionalAddIns\WSLDVC_PRIVATE` + `.wslgconfig`
  `WSLG_USE_WSLDVC_PRIVATE=true`; our `VirtualChannelGetInstance` returns
  BOTH the stock WSLDVCPlugin instance (chain-load
  `C:\Program Files\WSL\WSLDVCPlugin.dll`) and ours. Regular `AddIns` is
  ignored by the /wslg msrdc instance (verified) but works for mstsc.
- In-context WinEvent hook (`EVENT_OBJECT_CREATE`/`SHOW`, own PID) to
  catch RAIL windows pre-visibility; late attach needs an upstream
  AccessKit patch (SubclassingAdapter panics on visible windows).

## Workflow notes

- Background processes started via Start-Process die with the sandbox
  job; use the harness `run_in_background` instead.
- Sandbox blocks HKLM writes; Windows `sudo` works when the user grants
  elevation.
- Commit each tested component without asking (user's standing
  instruction); stop only for real obstacles.
