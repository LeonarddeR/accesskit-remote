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
- **DVC plugin scaffolding + live load** (`accesskit_remote_dvc_plugin`): COM
  scaffolding ported from rd_pipe-rs (relicensed MIT/Apache), exposed via the
  **instance method** — the DLL exports `VirtualChannelGetInstance`; no class
  factory, no CLSID, no `DllInstall` (manual registration). `IWTSPlugin` →
  `IWTSListenerCallback` → `IWTSVirtualChannelCallback` (the channel callback is
  a logging **stub**; the real transport is hvsocket, still to come). Hardcoded
  `AccessKit` channel name; `DllMain` logs to `%TEMP%\AccessKitDvc.log` (level
  via `ACCESSKIT_DVC_LOG`). The export chain-loads the stock `WSLDVCPlugin.dll`
  and returns both plug-ins **only** when it occupies the `WSLDVC_PRIVATE` slot
  (gated on `/plugin:WSLDVC_PRIVATE` in the host command line); the merge never
  writes past the caller's array and reports the actual count written, our
  plug-in written last. 12 tests (5 unit incl. a live probe of the real stock
  DLL; 7 integration over a fake DVC framework). Build/test:
  `cargo build|test -p accesskit_remote_dvc_plugin --target x86_64-pc-windows-msvc`
  (the cdylib is loaded at runtime, so `build` must precede `test`; never
  `--workspace`). **Verified live on both routes:**
  - *mstsc dev path* (HKCU `AddIns\<name>` `Name` = DLL path, `.wslgconfig`
    `WSLG_USE_MSTSC=true`): loaded into the connected mstsc; log shows probe
    `reporting 1 (chain=false)`, fetch `cap=1 wrote 1`, `Initialize` → `Creating
    listener … AccessKit` → `Client connected`. (mstsc shows a per-launch
    RemoteApp trust dialog that needs a real interactive Connect click; synthetic
    clicks don't complete it — Spike 2d.)
  - *msrdc production path* (HKLM `OptionalAddIns\WSLDVC_PRIVATE` `Name` = DLL
    path, `WSLG_USE_WSLDVC_PRIVATE=true`): both our DLL **and** the stock
    `WSLDVCPlugin.dll` loaded into the connected msrdc; log shows `Chain-loaded
    stock plug-in …`, probe `reporting 2 (ours=1, stock=1, chain=true)`, fetch
    `cap=2 wrote 2`. **msrdc allocated a 2-element array, so the count>1 return
    is confirmed safe — the buffer-overflow linchpin is resolved, not just
    mitigated.** weston's `rdp_rail_notify_app_list()` fired, i.e. the
    chain-loaded stock plug-in accepted its channel → WSLg integration intact.
    **Caveat**: no `RAIL_WINDOW` toplevel mapped this session; a control run of
    *default* WSLg (no plugin) also mapped none (empty GTK app logs both times),
    so this is a boot/GTK-surface condition, **not** a plugin regression.
    (WSLDVCPlugin governs the Start-Menu app-list only; RAIL windowing is msrdc +
    weston rdprail-shell, independent of it.) Commits ba4c457 (Spike 2d),
    0963a4b (scaffolding), 6c7ab5c (chain-load).

- **MILESTONE — full RAIL E2E: a GTK app's live tree on its real RAIL window,
  read and driven via UIA, all from inside msrdc.** The DVC plugin now runs the
  whole consumer stack: `Connected` starts a per-RDP-connection `Session` —
  hvsocket pump (VM id parsed fresh from `/v:`, port 4750 or
  `ACCESSKIT_DVC_PORT`, viewer-style loop with `ConnectionAborted` retry and
  connect-retry until the daemon is up) plus a RAIL hook thread. The hook is a
  process-wide **in-context** `SetWinEventHook` (CREATE..NAMECHANGE, own pid);
  its proc runs synchronously on the emitting window's owning thread —
  **empirically confirmed** (log: owning thread == current thread) — where it
  matches unattached `RAIL_WINDOW`s against the remote window registry
  (normalized title; distro from Lxss registry for the anchored suffix strip;
  `WslgServerWindowId` logged) and installs the new **visible-window adapter**
  right there. That adapter (`accesskit_remote_windows::install_visible_adapter`)
  hosts the lower-level `accesskit_windows::Adapter` under a manual subclass —
  no visibility precondition, so attaching to the already-visible RAIL HWND
  works (the `SubclassingAdapter` panic is bypassed, not patched). Tree deltas
  cross threads via a registered window message (`post_delta` boxes the update;
  the subclass wndproc applies + raises on the owning thread); UIA actions flow
  back through the mpsc channel → pump → `request_action`. **Verified live**:
  UIA client on the real `RAIL_WINDOW` (FrameworkId 'AccessKit') read
  gnome-text-editor's tree (13 UIA descendants of the 83-node AT-SPI tree, all
  7 buttons + Edit), InvokePattern'd **New Tab** → GTK opened a tab → tree grew
  to 106 nodes → deltas posted back → UIA re-read 22 descendants. Also verified
  env-independent: the `visible_demo` example attaches AFTER showing a plain
  window, UIA reads it, clicks round-trip (label 0→1→3). Unit tests cover title
  normalization, association matching, and `/v:` parsing (25 tests total in the
  plugin + windows crates). Commits 0bce2b3 (pump), step-3/4 commits
  (visible adapter, rail hook), plus docs.

- **Focus & caret/text-selection forwarding.** Node focus and caret now flow
  end to end as ordinary tree updates (no protocol change).
  - *Focus* (mirror): the event connection subscribes to `object:state-changed`;
    a `:focused` gain resolves the emitting object to its window+node
    (`resolve_focus_target`, gated on the node still being in the current
    `objects` map) and emits a focus-only `TreeUpdate` (`{nodes:[], focus}`) with
    no re-walk, plus a deduped window-level `FocusChanged` via a pure
    `FocusTracker`. Window activate/deactivate also advance window focus.
    `WindowState.focus` is kept live from every build so partial (focus/caret)
    deltas never revert focus. Registration matches Orca
    (`P:\A11y\orca` event_manager.py: `state-changed:focused` with `detail1` ==
    `enabled`; Orca doesn't use legacy `focus:`).
  - *Focus → UIA* (Windows): `post_focus(hwnd, is_focused)` mirrors
    `post_delta`, driving the visible adapter's `update_window_focus_state` on
    the window's own thread — required because the consumer only raises UIA
    focus events while it believes the window is host-focused, and a RAIL window
    never gets its own `WM_SETFOCUS`. The plugin's `Registry` tracks the focused
    remote window and posts unfocus/focus on `FocusChanged`; `try_attach` seeds
    `is_focused` from a remote focus that arrived before the window attached.
  - *Caret/text* (mirror): the walk reads the AT-SPI `Text` interface for
    text-input roles into synthesized `Role::TextRun` children (one per hard
    line; `character_lengths` = per-code-point UTF-8 byte lengths; ids
    `"path#runN"`) plus a `TextSelection` on the container (caret = the
    selection focus end; code-point offsets). `text-caret-moved`/`text-changed`/
    `text-selection-changed` re-query just that node and emit a minimal delta
    (`rebuild_text_node`) — never a re-walk. `accesskit_windows` exposes the UIA
    Text pattern automatically, so no Windows-side caret code.
  - **Verification.** Pure logic is unit-tested (focus + 13 text tests in the
    mirror; 5 Registry focus tests in the plugin). Live-verified where the
    environment allows: `visible_demo` — a UIA `SetFocus` round-tripped a
    focus-only delta and UIA `FocusedElement` tracked it (the provider raises
    `UIA_AutomationFocusChangedEventId` on that path, confirmed in
    `accesskit_windows` `focus_moved`); `dump_tree` — gnome-text-editor's
    `MultilineTextInput` gained TextRun children + a caret `TextSelection` read
    from the Text interface; `caret_reflect` — editing the document emitted
    `text-changed` → a 3-node delta (container + two runs) carrying the new text
    with no re-walk. **Environment caveat:** headless WSL has no window manager,
    so GTK4 emits **no** `state-changed:focused` or `window:activate` (only
    `children-changed` and object state changes like Pressed/Indeterminate) —
    the same class of gap as `window:create/destroy`. Focus events are expected
    on the msrdc/RAIL path (which *does* deliver `window:activate`) and remain
    to be exercised interactively there. GTK's `SetCaretOffset` is `NotSupported`
    over AT-SPI, so caret motion can't be driven headlessly; `text-changed`
    (which *does* fire) exercises the identical `refresh_text` handler.
    Commits 150f4cb (focus emit), 6becbb4 (post_focus), c9c8ee8 (plugin route),
    4d5933a (TextRun synth), cd5c039 (Text reads), 43a2923 (text events).

- **Active-descendant focus, caret drive & periodic reconcile.**
  - *Active-descendant* (5a): the event connection subscribes to
    `object:active-descendant-changed`; a move resolves the new descendant to its
    window+node (`handle_active_descendant` → shared `emit_node_focus`, reusing
    `resolve_focus_target` by sender + descendant path) and emits a focus-only
    delta with no re-walk. Forwards the focus *pointer* only; item selection state
    stays governed by re-walks.
  - *Caret drive* (5b): UIA `Action::SetTextSelection` now maps to AT-SPI
    `set_caret_offset`/`set_selection`. `AtspiSource::perform` carries the payload
    (previously dropped) through `PerformMsg`; `handle_action` resolves the
    anchor/focus TextRun positions to global code-point offsets via the container's
    text-node layout (new pure `mapping::text_offset`, the inverse of
    `text_position` — TextRun ids are synthesized, so only the container routes
    through `objects`); `mirror::set_text_selection` writes the Text interface.
  - *Periodic reconcile* (remaining 1): a 60s `tokio::time::interval` arm in
    `bridge_main` drives the idempotent `Mirror::reconcile` as a safety net (tokio
    `time` feature enabled for the crate).
  - **Verification.** `text_offset` round-trips against `text_position` at the
    start / interior / end-of-text boundary; active-descendant resolution+emit is
    unit-tested; 38 unit tests pass. Periodic reconcile ran past the 60s tick
    against gnome-text-editor with no panic and no spurious window churn.
    **Environment caveat:** active-descendant and caret drive are *not*
    live-verifiable headlessly (GTK4 emits no focus events headlessly; GTK's
    `set_caret_offset` is `NotSupported` over AT-SPI) — wired + unit-tested, live
    verification deferred to the interactive RAIL path. Also fixed: the `probe`
    example now retries the hvsocket `ConnectionAborted` receive-timeout like the
    `viewer` (remaining 4). Commits ad24120 (probe), a1e8b46 (atspi additions).

- **Static text runs (Label/Terminal/Document).** The walk now mirrors the AT-SPI
  `Text` interface into `Role::TextRun` children for the static text roles, not
  just editable inputs. Pure `reads_text_runs(role, has_children)` gives
  Label/Terminal/Document* runs, gated to *leaves* — a structured document (one
  with element children) keeps its child structure instead of also emitting the
  whole text flat; editable roles are exempt from the leaf gate (always get runs).
  `map_role` now maps `Terminal → Terminal` and the six `Document*` roles →
  `Document`, matching accesskit's `supports_text_ranges` role set exactly
  (`{text inputs} ∪ {Label, Document, Terminal}`, each needing ≥1 `TextRun` child —
  confirmed by reading `accesskit_consumer` `text.rs`), so the UIA Text pattern
  surfaces on them. `has_text_caret(role)` keeps caret/selection only for editable
  fields and terminals; static labels/documents expose readable runs with
  `SupportedTextSelection = None` (`has_text_selection()` false → `_None` in the
  consumer). This matters because GTK reports `caret_offset = 0` (not `-1`) on
  non-selectable labels, which would otherwise stamp a degenerate caret-at-0 on
  every one — including on live `text-changed` re-reads, since a status label is a
  tracked text node now. `read_text_state` gained a `with_caret` arg (also skipping
  two bus calls per static node) and `TextNodeCache.caret_enabled` carries the bit
  so `refresh_text` honors it without needing the role. **Verified live**
  (`dump_tree` vs gnome-text-editor): all 9 `Label` nodes gained exactly one
  `TextRun` with `sel=None`, while the editable document (`MultilineTextInput`)
  kept its real caret (`sel=Some`). **Decisions/caveats:** Terminal keeps its caret
  (a terminal's caret-at-0 is the real home cursor, not an AtkText artifact) —
  correct default but not headlessly verifiable, like the caret-drive note; a
  *selectable* Document's caret is **deferred** (Document* is treated caret-less
  like Label, right for read-only views). Terminal/Document runs are wired +
  unit-tested but gnome-text-editor has no such node to exercise them live. 44 unit
  tests. Commit 438a852.

## Remaining

1. **Periodic reconcile** (mirror): ~~future work~~ **done** — a 60s
   `tokio::time::interval` arm in `bridge_main` now drives the idempotent
   `Mirror::reconcile` (see the milestone above), the safety net Orca's ~60s
   reconciliation provides. The timer firing and driving `reconcile` to completion
   is smoke-verified (ran past the 60s tick with no bridge-thread panic — a tokio
   `interval` panics rather than silently no-ops if the time driver is off — and
   no spurious churn in steady state); `reconcile`'s own add/remove detection is
   covered by the `reconcile_windows` unit tests plus the reactive-path proof
   (`window_lifecycle`). By composition, a window that becomes Showing+Visible
   *without* re-signaling — or an app that dies without a root `remove` — is caught
   at the next tick rather than missed indefinitely. The timer-driven *detection*
   was not isolated in test (the reactive path catches window changes first), and
   the residual window is the ≤60s gap between ticks. See `P:\a11y\orca`.
2. **Node-level focus**: ~~deferred from passive events~~ **done** — a
   `state-changed:focused` gain now emits a focus-only delta with no re-walk
   (see the focus/caret milestone above). The re-walk-storm concern is avoided
   because the handler filters in O(1) and never re-walks on state changes.
3. **Coalesce children-changed bursts**: New Tab fires ~28 full re-walks in 8s
   before settling. Full-tree re-walks are convergent, so this is a cost, not
   a correctness, issue; debounce is a future optimization.
4. **`probe` example**: ~~latent hvsocket receive-timeout bug~~ **done** — probe
   now returns the hvsocket flag from `connect` and retries `ConnectionAborted`
   like the `viewer` fix (18542fd). Commit ad24120.
5. **Focus/caret follow-ups**: (a) ~~subscribe
   `object:active-descendant-changed`~~ **done** (see the milestone above);
   (b) ~~map UIA `Action::SetTextSelection` → AT-SPI `set_caret_offset`/selection~~
   **wired** (unit-tested; live verification of the AT-SPI write deferred to the
   RAIL path, as GTK returns `NotSupported` headlessly); (c) ~~give
   `Role::Label`/`Document`/`Terminal` text runs too~~ **done** (see the static
   text runs milestone above — leaf-gated, caret suppressed for the caret-less
   static roles; Terminal caret and selectable-Document caret noted there);
   (d) geometry (`character_positions`/`widths`) for
   magnifiers; (e) a `GetForegroundWindow` gate on `post_focus(true)` if RAIL
   testing shows Narrator/NVDA focus theft.

## Notes / corrections

- The `Failed to create peer … Invalid address string` warning is NOT fixable
  from our Cargo.toml: `atspi`'s `connection` feature pulls `atspi-connection`
  with *its* default features (which include `p2p`) regardless of atspi's own
  `p2p` feature, and it has no `default-features = false`. The warning is a
  debug-only `eprintln` in `atspi-connection`, silently ignored in release
  builds. Left as is (the doc's "drop p2p" plan was moot).

## After that — DVC plugin follow-ups (full E2E done above)

1. **Idle-window attach gap**: the hook attaches on the first in-range event
   from an unattached RAIL window, but an idle window emits none — in the live
   run the attach only fired after a minimize/restore nudge (event 0x8002).
   Fix ideas: when the pump learns a new remote window, trigger events on
   candidate RAIL HWNDs (e.g. a harmless `SetWindowPos` frame-change from the
   hook thread), or sweep `EnumThreadWindows` from the hook proc on every event,
   or widen the hook range. Until then, first focus/interaction attaches.
2. **`app_id` is None from `AtspiSource`** (`remote window added … app=None`):
   the mirror fills pid/toolkit but not the desktop-file id, so association
   disambiguation-by-app-id never engages; same-title windows across apps stay
   unmatched. Plumb the app id through `AppInfo` on the Linux side.
3. **Window focus → UIA**: **done** — `ClientEvent::FocusChanged` now routes
   through `Registry::focus_changed` to `post_focus` on the bound RAIL windows
   (see the focus/caret milestone above). Local WM_SETFOCUS/KILLFOCUS handling
   stays (last-writer-wins). Still to exercise interactively on the RAIL path,
   where GTK actually emits focus events (headless WSL does not).
4. **Registration UX**: ~~production install still manual~~ **done** —
   `regsvr32 <dll>` / `regsvr32 /u <dll>` auto-register with **no elevation**.
   `DllRegisterServer`/`DllUnregisterServer` (basic pattern — no `/i` command
   line, no COM CLSID/`InprocServer32`, just the path) write/remove the **HKCU**
   `OptionalAddIns\WSLDVC_PRIVATE` `Name`=DLL-path entry (via `windows-registry`)
   **and** the `%USERPROFILE%\.wslgconfig` `[system-distro-env]
   WSLG_USE_WSLDVC_PRIVATE=true` flag (a surgical, atomic, idempotent single-line
   editor — `wslgconfig.rs` pure core, 12 tests; no INI crate, `rust-ini` drops
   comments). **Findings that enabled this:** msrdc loads
   `OptionalAddIns\WSLDVC_PRIVATE` from **HKCU** (empirically verified — HKLM
   deleted + HKCU-only → identical load), so no admin/HKLM needed; and
   `WSLG_USE_WSLDVC_PRIVATE` **cannot** come from the registry — WSLGd reads it
   only from the `.wslgconfig` file (`P:\Microsoft\wslg` `WSLGd/main.cpp:153-185,
   483-489`), so full automation still needs the (per-user, no-admin) file write.
   **Crash fix:** installing the global `tracing` `fmt` subscriber in `DllMain`
   aborts regsvr32 at its clean exit (`0xC0000409`, `__fastfail` subcode 7 =
   FATAL_APP_EXIT, faulting module = our DLL — a Rust-DLL teardown footgun);
   tracing init moved to a lazy `Once` on the DVC path
   (`VirtualChannelGetInstance`), which msrdc always hits but regsvr32 never does,
   so msrdc logging is unchanged (verified live). The debug DLL stays registered
   on this machine via the **HKLM** key + `.wslgconfig` (dev); `regsvr32 <dll>`
   switches to the per-user path. Commit 8c108fd.
5. **Test — `WSL2_WESTON_SHELL_DESKTOP=1` (full desktop shell).** Set it under
   `[system-distro-env]` in `.wslgconfig` so weston runs its **desktop shell** (a
   real window manager) instead of `rdprail-shell`. *Hypothesis:* headless WSL
   has no WM, so GTK4 emits no `state-changed:focused`/`window:activate` — a
   desktop-shell session *does* manage window focus, so GTK should emit focus +
   window-lifecycle events over AT-SPI. If so, the deferred live verification of
   **node focus, active-descendant, and caret drive** (wired + unit-tested; only
   exercisable where a WM delivers focus events — Remaining 5a/5b + the
   focus/caret milestone) can run against the mirror
   (`dump_tree`/`caret_reflect`/focus examples) **without** the full interactive
   msrdc/RAIL Windows round-trip. Also characterize the Windows side: desktop
   mode has no per-window `RAIL_WINDOW`s, so the RAIL `SetWinEventHook` +
   visible-adapter attach (`rail.rs`, `association.rs`) won't bind the same way —
   record whether the plugin still loads and what UIA surface the single desktop
   window exposes.

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
