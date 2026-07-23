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
- `accesskit_remoted --atspi` serves the live mirror: a Linux-only `--atspi`
  flag swaps `DemoSource` for `AtspiSource`; on non-Linux it errors and
  compiles out. Commit fdc6879.
- Passive event reflection: the bridge `select!`s over the action channel and
  a live AT-SPI event stream running on a *dedicated* connection (sharing one
  with method calls deadlocks — a full event broadcast stalls that
  connection's socket reader and starves in-flight replies). `children-changed`
  (matched to a window by app sender) and window activate/deactivate (matched
  by frame path) re-walk the affected window. Verified: clicking
  gnome-text-editor's New Tab first yields the stale 83-node re-walk, then a
  children-changed re-walk grows the tree to 107 (`passive_reflect` example).
  The milestone verified only the *static* initial tree over the wire, so a
  live GTK change reaching the Windows UIA tree end to end is composed from
  proven seams but not yet run as one. Commit 66e0a0e.
- `AppInfo` carries pid + toolkit: `discover_windows` reads toolkit
  name/version off the Application interface and pid via the a11y bus
  `GetConnectionUnixProcessID`. Verified live: gnome-text-editor → pid set,
  toolkit GTK 4.18.6. Commit cf0c413.
- **MILESTONE — a GTK app's tree is visible from Windows via UIA.** Full stack
  proven live: gnome-text-editor (GTK/AT-SPI, WSL) → `AtspiSource` →
  `accesskit_remoted --atspi --vsock 4750` → hvsocket → Windows `viewer` →
  `RemoteWindowBinding` UIA host. A Windows PowerShell 5.1
  System.Windows.Automation client read the viewer window: FrameworkId
  'AccessKit'; the 7 Buttons (Open, New Tab, Document Properties, Main Menu,
  Close) and the text Edit, matching the tree the TCP `probe` saw. Needed a
  viewer fix: on hvsocket a receive timeout surfaces as `ConnectionAborted`
  (not `WouldBlock`/`TimedOut`) while the connection stays usable, so retry it
  on hvsocket only — otherwise the first idle read drops the connection.
  Commit 18542fd.
- **Window lifecycle** (mirror): `AtspiSource` now emits `WindowAdded`/
  `WindowRemoved` as apps open and close toplevels. Pure `reconcile.rs`
  (`WindowKey` = unique bus name + object path; `reconcile_windows` diff, 5
  tests) compares the tracked set against a fresh `discover_windows` by
  `ObjectRef` identity; `Mirror::reconcile` drops vanished windows (announcing
  each) and walks new ones (announcing each). `add_discovered` is factored out
  of `enumerate` so both share the walk/build/track step (and no longer leak an
  id on an empty walk). **Trigger finding**: GTK4 does not emit
  `window:create`/`window:destroy` in this WSLg environment — a raw a11y-bus
  monitor (`busctl --address <a11y bus> monitor`) shows no `window:*` signals
  at all, even with our window match rule registered. Toplevel lifecycle
  instead arrives as `children-changed` on the AT-SPI root path
  (`/org/a11y/atspi/accessible/root`): an app's root gains a window child
  (add), or the registry root loses an app (remove), disambiguated by sender.
  So `is_window_lifecycle_event` reconciles on root-path `children-changed`
  (and on `window:create`/`destroy`, kept as a fallback for toolkits that emit
  them); deeper `children-changed` stays a same-window re-walk. Reconcile is
  idempotent (full-set diff), so the ~20 intra-window updates between opens
  cause no spurious announces. Added/removed windows emit no focus event; the
  client nulls its own focus when a focused window is removed (node-level focus
  deferred, #2). **Verified live** via the `window_lifecycle` example: across
  two clean-slate trials, 5/5 `--new-window` opens each produced exactly one
  `WindowAdded` (correct title/tree) and killing the apps produced
  `WindowRemoved` for every tracked window; the visibility race never bit.
  Commit 16b10e6.

## Remaining

1. **Periodic reconcile** (mirror): window add/remove now reconciles on
   root-path `children-changed` (done above), but a window that becomes
   Showing+Visible *without* re-signaling, or an app that dies without a root
   `remove`, is missed until the next root event. Not observed in testing (the
   add event fired after the window was visible in every trial), but it is the
   residual race Orca's ~60s reconciliation covers; a periodic reconcile is
   future work. See `P:\a11y\orca`.
2. **Node-level focus**: deferred from passive events to avoid a
   `state-changed` re-walk storm; today only window activate/deactivate
   re-walks the frame. The action's immediate re-walk still covers state-only
   results, so it stays even though passive re-walks now cover structure.
3. **Coalesce children-changed bursts**: New Tab fires ~28 full re-walks in 8s
   before settling. Full-tree re-walks are convergent, so this is a cost, not
   a correctness, issue; debounce is a future optimization.
4. **`probe` example** has the same latent hvsocket receive-timeout bug the
   `viewer` had (fixed in 18542fd); apply the same `ConnectionAborted`-retry
   if probe is ever driven over hvsocket.

## Notes / corrections

- The `Failed to create peer … Invalid address string` warning is NOT fixable
  from our Cargo.toml: `atspi`'s `connection` feature pulls `atspi-connection`
  with *its* default features (which include `p2p`) regardless of atspi's own
  `p2p` feature, and it has no `default-features = false`. The warning is a
  debug-only `eprintln` in `atspi-connection`, silently ignored in release
  builds. Left as is (the doc's "drop p2p" plan was moot).

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
- Never `pkill -f gnome-text-editor` (or `-f accesskit_remoted`) from a
  `bash -lc '<script>'` whose text contains that string: `-f` matches the
  script's own bash cmdline and SIGTERMs it (exit 15). Kill by pid, or guard
  with `me=$$; [ "$p" != "$me" ]`. `pgrep -a NAME` also fails for names >15
  chars — use `pgrep -af`.
- `wsl -e bash -lc '<script>'` exits 15 in this harness even on success; check
  the actual output, not the exit code.
- Cross-machine milestone recipe (proven): on WSL, enable a11y, then launch
  the app and daemon *detached* so they outlive the launching command —
  `... setsid gnome-text-editor >/tmp/gte.log 2>&1 </dev/null &`, sleep ~9s,
  `setsid accesskit_remoted --atspi --vsock 4750 >/tmp/daemon.log 2>&1
  </dev/null &`. On Windows: VM ID from `Get-CimInstance Win32_Process
  -Filter "Name='msrdc.exe'"` `/v:` (single msrdc while a WSLg app runs — no
  GUID ambiguity), run `viewer --hvsocket <vm-id> 4750` in the background,
  then UIA-inspect via **powershell.exe** (Windows PowerShell 5.1,
  `System.Windows.Automation`): find the RootElement child whose FrameworkId
  is 'AccessKit'. The AccessKit UIA provider collapses filtered/generic nodes,
  so the 83-node AT-SPI tree shows as ~13 UIA descendants (the real controls).
