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
- `TreeSource` seam wired: `DemoSource` implements `TreeSource` (buffers
  deltas in a pending vec, drained via `poll_events`; `initial_state`
  returns the focus tuple). `accesskit_remoted`'s `serve`/`pump`/`dispatch`
  run over `&mut dyn TreeSource`; `pump` drains `poll_events()` →
  `apply_source_event` after each chunk and tick, gated on an established
  session (events polled before that are dropped). Commit 58e78c2.
- `accesskit_remote_atspi` v0 — the AT-SPI mirror (Linux-only). Pure
  mapping core (`mapping.rs`): AT-SPI role → accesskit role with a
  `GenericContainer` fallback; stable path→NodeId allocator; Label text
  → `set_value` (UIA Name for labels comes from value); Click gated to
  clickable roles; Focus from `Focusable`. 6 unit tests. Async bus layer
  (`mirror.rs`): connect, discover visible toplevel frames across the
  desktop's apps, BFS-walk a frame. `AtspiSource` (`source.rs`) runs the
  bus on a dedicated tokio current-thread; `initial_state` blocks on the
  first enumeration snapshot; `perform` → bridge re-walks the window and
  emits a `TreeUpdate`; `poll_events` drains a std channel. **Verified
  live**: the `dump_tree` example enumerated gnome-text-editor into an
  83-node tree (root Window; correct Button / Label / MultilineTextInput
  / ProgressIndicator / ScrollView; Click only on the 7 real buttons).
  Deps `atspi 0.30` / `zbus 5.18` / `tokio`, gated to `cfg(linux)` so the
  crate is empty on Windows. Commit 9777500.

## In flight

1. `accesskit_remote_atspi` v0 is built and smoke-tested (above).
   Remaining for the mirror:
   - **Passive event reflection** (not wired yet). Today updates are
     action-driven only: `perform` → re-walk that window → emit
     `TreeUpdate`. Next: subscribe to atspi `event_stream()` and re-walk
     affected windows on focus / window-lifecycle / `children-changed`.
     Consistency discipline per Orca (events are hints; re-query before
     believing; re-walk on structural events; ~60s reconciliation) lands
     with this. See `P:\a11y\orca`.
   - **Window lifecycle**: emit `WindowAdded`/`WindowRemoved` as apps open
     and close toplevels (v0 enumerates once at connect).
   - App identity: `pid`/`toolkit` on `AppInfo` (v0 sets `name` only).
2. **Wire `AtspiSource` into `accesskit_remoted`** — the daemon still
   hardcodes `DemoSource`. Add a `cfg(linux)` dep on
   `accesskit_remote_atspi` and a flag (e.g. `--atspi`) that constructs
   `AtspiSource::new()?` instead of `DemoSource`. `pump`/`drain_source`
   already run over `&mut dyn TreeSource`, so this is construction + arg
   parsing only.
3. Test recipe for the milestone (GTK app tree visible from Windows;
   needs step 2 first):
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
- Rust **is** now installed in the WSL distro (rustup, cargo, rustc
  1.97.1); the spikes.md note is stale. Build/test the Linux-only crates
  from Windows via `wsl -e bash -lc '...'`. Use single-quoted PowerShell
  args so `$` reaches bash unexpanded. Keep `CARGO_TARGET_DIR=~/target-
  accesskit-remote` (native, drvfs is slow); build one crate
  (`-p accesskit_remote_atspi`), never `--workspace` on Linux (the
  Windows-only members won't build).
- Read vendored crate source from the WSL registry with the Read tool via
  the UNC path `\\wsl.localhost\Debian\home\leonard\.cargo\registry\src\
  index.crates.io-*\<crate>-<ver>\src\...` — invaluable for verifying an
  API instead of guessing.
- atspi smoke test (no Windows bridge needed): the a11y tree exists even
  without a mapped window, but GTK4 only publishes it when accessibility
  is enabled — `busctl --user set-property org.a11y.Bus /org/a11y/bus
  org.a11y.Status IsEnabled b true` before launching the app. Then
  `GSK_RENDERER=cairo LIBGL_ALWAYS_SOFTWARE=1 setsid gnome-text-editor &`,
  sleep ~7s, run `dump_tree`.
