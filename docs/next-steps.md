# Continuation notes

State as of 2026-07-24. Everything below "works and is committed" was
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

- **Focus & caret reflection live-verified; drive paths are GTK4-blocked, not
  environment-blocked.** The deferred live verifications from the focus/caret
  milestone ran headlessly via a new recipe — `GDK_BACKEND=x11` + `xdotool`
  (see Spike 5 in `docs/spikes.md` and Workflow notes) — under the **default
  rdprail-shell**, no interactive RAIL session needed:
  - *Window focus*: `xdotool windowfocus` toggles between two editor windows
    produced raw `window:activate`/`deactivate` and clean deduped
    `FocusChanged None → Some(new)` pairs from the mirror.
  - *Node focus*: `state-changed:focused` gains arrived and `handle_focus_change`
    emitted the focus-only 0-node delta on the right window — the exact
    `TreeUpdate n (0 nodes)` signature, twice across environments.
  - *Caret/text/selection reflection*: real arrow keys / typing / shift-select
    produced `text-caret-moved` ×4, `text-changed` ×5, `text-selection-changed`
    ×3, each reflected as the minimal 1–2-node `refresh_text` deltas (no
    re-walk).
  - *Drive paths cannot pass against GTK4*: `state_probe` shows GTK4's AT-SPI
    bridge answers `org.freedesktop.DBus.Error.NotSupported` for
    `Component.GrabFocus` **and** `Text.SetCaretOffset` in *every* environment
    (headless, desktop shell, X11-focused). `focus_drive`/`caret_drive` (new
    examples, PASS-gated on the real deltas) are the ready-made instruments for
    a toolkit that implements the write methods; against GTK4 they fail by
    toolkit design. The `WSL2_WESTON_SHELL_DESKTOP=1` hypothesis itself was
    **disproven**: WSLg's desktop mode renders black with dead input (full
    findings in Spike 5). New examples committed as 055e3cb.

- **Re-walk debounce, app_id, idle-attach nudge, AUMID, text geometry — all
  four remaining items landed and live-verified in one combined RAIL run.**
  - *Debounce* (was Remaining #3): deep `children-changed` no longer re-walks
    inline; `RewalkCoalescer` (pure, clock-injected; quiet 250ms, hard cap 2s
    from a burst's first event) marks the sender's windows dirty and a fourth
    `select!` arm in `bridge_main` re-walks once the burst settles. Lifecycle
    (root-path), focus, active-descendant, and text paths stay immediate.
    Measured on New Tab: **24 re-walk updates in 8s → 3**, passive reflection
    intact, `WindowAdded` still immediate. Note: `passive_reflect`'s PASS gate
    now compares later walks against the immediate post-action walk — on
    current gnome-text-editor the settled two-tab tree (122 nodes) is *smaller*
    than the fresh one-tab tree (130; TextRun era), so grow-past-initial is
    unsatisfiable and the pre-change code failed it identically (stash A/B).
    Commit cdd75de.
  - *app_id* (was DVC follow-up #2): `AppIdResolver` sweeps the session bus for
    well-known names owned by the app's pid (a GApplication owns its
    application id as a bus name; candidate filter = reverse-DNS ≥2 dots minus
    org.freedesktop./org.a11y./org.gtk.; sorted first pick; per-pid cache incl.
    negatives; lazy session connect, never blocks discovery). AT-SPI itself has
    **no** desktop-id property (Application exposes only toolkit/version and a
    deprecated numeric id), hence the sideband. Verified live E2E: the DVC log
    shows `remote window added: … app=Some("org.gnome.TextEditor")`;
    LibreOffice (non-GApplication) yields `app_id=None` with discovery intact —
    the real negative probe (an env-var one is impossible: atspi finds the a11y
    bus *via* the session bus). Commit 9e1a3bc.
  - *Text geometry* (was 5d): every synthesized TextRun now carries the four
    properties `accesskit_consumer` requires for range rects — `bounds`
    (union of the run's char rects), run-relative `character_positions`,
    `character_widths`, `text_direction` (LTR; RTL future work) — read via
    `Text.GetCharacterExtents(CoordType::Window)`, one bus call per code point,
    capped at `MAX_GEOMETRY_CHARS = 512` per node (above the cap: no geometry,
    never partial arrays). Unreported `(0,0,0,0)` extents (GTK newlines) are
    synthesized from the predecessor's right edge (leading ones from the first
    real char's left edge); the trailing empty run gets a zero-width caret rect;
    an all-unreported node or length mismatch carries none. A consumer
    round-trip test (dev-dep accesskit_consumer 0.38) pins the contract.
    Refresh path: `text-changed` re-reads extents; caret/selection moves reuse
    the cache when the text length is unchanged, drop it otherwise. Verified
    live at every layer: `dump_tree` shows plausible window-relative rects from
    **both** GTK4 and LibreOffice/gtk3 headlessly; `caret_reflect`'s minimal
    3-node text-changed delta carries geometry on 2/2 runs; and a Windows
    PowerShell UIA client read **real TextPattern bounding rectangles off the
    RAIL window** (`'Open'` → 43×17px at screen coords) — the magnifier path
    works end to end. Known gap: an empty text field has no anchor, so its runs
    carry no geometry (rects=0 on the empty document). Commits 72812e4, d694504.
  - *Idle-attach nudge* (was DVC follow-up #1): on `WindowAdded`, after the
    registry insert releases its lock, the pump enumerates the process's
    `RAIL_WINDOW` toplevels and fires a same-title `SetWindowTextW` at each
    unattached one — the `WM_SETTEXT` executes on the *owning* thread, whose
    `DefWindowProc` raises `EVENT_OBJECT_NAMECHANGE` there, re-running the
    whole existing `try_attach` path on exactly the right thread. A new
    owning-thread guard in `try_attach` turns any wrong-thread delivery into a
    logged skip (none observed). Verified live with **zero interaction**:
    editor launched first (registry empty through its creation burst), daemon
    second → `nudge sweep: 1 RAIL hwnd(s)` → `nudging …` → `attaching … event
    0x800c, owning thread == current thread` → `visible adapter installed`,
    then UIA read the tree (FrameworkId AccessKit, 12 descendants). The old
    minimize/restore workaround is gone. Commits 7ceb8cf, c2af53e, c2ed8b3.
  - *AUMID* (was the Windows half of DVC #2): `try_attach` reads
    `PKEY_AppUserModel_ID` from the RAIL HWND's shell property store and logs
    it. **Format finding: msrdc sets an opaque hashed RemoteApp id**
    (`Microsoft.RemoteApp.R1qEaHzQr/…=`), *not* the Linux app id — the
    `RailWindow.app_user_model_id` doc-comment's assumption was wrong, AUMID
    equality with `app_id` can never hold on WSLg, and `normalize_aumid` is
    moot (it's a hash, not a decoration). Same-title disambiguation needs a
    different signal — the `WslgServerWindowId` ↔ appId association PDU flows
    through the *stock* plugin's channel (WSLDVCCallback.cpp
    `OnAssociateWindowId`), out of scope for now. windows-rs footgun recorded:
    windows 0.62's `PROPVARIANT` is **not** POD — the crate's `extensions/`
    module adds `Drop` (PropVariantClear) and `From<&str>`; a hand-built
    `VT_LPWSTR` pointing at Rust memory dies with STATUS_HEAP_CORRUPTION when
    Drop frees it. Commit cafe28b.
  - Session note: the first WSLg boot of the day showed `chain=false, stock=0`
    (stock plugin not chain-loaded) during a double-msrdc state; the next boot
    chain-loaded normally (`reporting 2, chain=true`, stock from
    `C:\Program Files\WSL\WSLDVCPlugin.dll`). Watch for recurrence; not
    reproduced since.

- **LibreOffice (Writer + Calc, gtk3 AND gtk4 VCL backends) is installed in
  the distro as the rich a11y test target.** Under `SAL_USE_VCLPLUGIN=gtk3`
  Writer publishes a 2093-node tree headlessly (plus the Welcome dialog);
  status-bar labels carry text runs with geometry at real scale. Launch:
  `SAL_USE_VCLPLUGIN=gtk3 LIBGL_ALWAYS_SOFTWARE=1 setsid soffice --writer
  --norestore`. Two new follow-ups this exposed: (i) Writer's document body
  (`document text` → `paragraph`) gets **no TextRuns yet** — AT-SPI `Paragraph`
  is not in `reads_text_runs`'s role set; mapping Paragraph (and gating it into
  the static-run path) is needed to mirror LO document content. (ii) Calc fires
  `object:active-descendant-changed` on cell navigation (verified on the raw
  bus), but the mirror can't surface it: the selected cell isn't in the walked
  tree (grid exceeds the walk; cells are lazy), so `resolve_focus_target` finds
  nothing and `handle_active_descendant` emits nothing. Surfacing
  active-descendant for large grids (walk-on-demand around the active cell, or
  resolve the descendant path directly rather than via the walked `objects`
  map) is mirror follow-up work. **Both follow-ups are now done — see the
  mirror-follow-ups milestone below.**

- **MILESTONE — mirror follow-ups: LO Paragraph text runs, Calc
  active-descendant splice, container bounds + empty-field caret anchor.**
  All three landed red-green TDD per stage; the crate suite grew 70 → 90
  tests. Plan: `docs/superpowers/plans/2026-07-24-atspi-mirror-follow-ups.md`.
  - *Paragraph runs* (LO follow-up (i)): `Role::Paragraph` joined the static
    text roles — leaf-gated like Label/Document, caret-less (a paragraph with
    inline element children keeps its structure). Verified live: typing into
    the Writer body produced a body-paragraph `TextRun` carrying the typed
    text with per-char geometry, `sel=None`. Commit 4dca659.
  - *Calc active-descendant* (LO follow-up (ii)): an unresolved descendant now
    splices on demand. `read_node` factored out of the walk (2152c3b). Pure
    `splice_chain_update`/`merge_update`/`emitted_children` (+ `SpliceResult`)
    build a partial update from a freshly read ancestor chain; the re-emitted
    ancestor keeps the *client tree's* children (plus the chain child), never
    the fresh bus child list, so a lazy grid can neither bloat nor orphan the
    tree; re-splicing is idempotent via `NodeIdMap` stability (9412445).
    `handle_active_descendant` escalates via `None` to async
    `splice_active_descendant` — event-sender-addressed;
    `read_chain_to_known` climbs ≤16 parent hops to a known path;
    `apply_spliced_chain` folds ids/objects/children/focus in, keeping
    `objects` exactly equal to the client tree (106daee). A re-walk that
    cannot see the focused spliced node re-splices it and merges before
    emitting; on failure the walk's own focus stands — a stale focus id is
    never retained (f265d57). **Verified live on Calc/gtk3** (`xdotool`
    clicks + arrows): every cell move emitted the splice signature
    `TreeUpdate (2 nodes, focus <new>) ids=[<grid>, <new>]`, Name-Box text
    deltas carried the spliced focus, and a typing-induced debounced re-walk
    emitted `(2005 nodes, focus 2004)` — the 2004-node walk plus the
    re-spliced cell, focus preserved (without the guard: `(2004, focus 0)`).
    gnome-text-editor regression clean, including live `(0 nodes)` fast-path
    focus deltas and deduped `FocusChanged` pairs. **Toolkit finding**: LO
    mints a *transient* accessible object (fresh path) per cell selection, so
    revisits re-splice under fresh ids — the fast path is unreachable against
    LO by toolkit design (it engages on GTK's stable paths); spliced ids
    accumulate between re-walks (append-only `NodeIdMap`), and each re-walk
    prunes the spliced nodes from the tree — though the `NodeIdMap`'s path→id
    entries persist for the window's lifetime by design.
  - *Container bounds + empty-field caret anchor* (the 5d tail): the walk
    reads `Component.GetExtents(CoordType::Window)` for every node exposing
    the Component interface — measured +1s on the Writer walk (7s vs 6s
    baseline, ~+17%, within the predicted +20%; no gating needed); zero-area
    rects are dropped in the pure layer; `build_node` sets container bounds
    (9ed9a7f). An empty text field's single run takes a zero-width caret rect
    at the container's left edge (`build_text_runs` gained a
    `container_bounds` parameter; the rect is cached on `TextNodeCache` and
    NOT re-read on text events — same staleness class as cached char
    extents). Verified live: clean-slate gnome-text-editor's empty document
    run shows `geom=(0,46)-(0,520)` under container `(0,46)-(700,520)`; GTE
    buttons/window all carry plausible rects. Caveat: the anchor spans the
    full container height (approximate v1; widget padding ignored).
    Commit 95051fb.
  - `window_lifecycle` now prints each update's focus id and leading node ids
    (the instrument the splice/guard verifications read). Walk-cost variance
    note: the same Writer tree walked in 6-7s idle but 79s while LO was busy
    right after X11 typing — AT-SPI calls serialize on the app's main loop,
    so time walks against an idle app.
- **Widget-state forwarding + menu roles + drive-back diagnostic**
  (2026-07-24, later same day). Grounded in a live AT-SPI characterization
  pass over gtk4-widget-factory and LibreOffice via the new
  `examples/charprobe` (raw role/state/interface/action dump straight off the
  bus, no mirror in between; `--open <action-substr>` fires a `DoAction`
  first to capture transient popups). Four changes:
  - *States* (commit 45f4659): the mirror distilled only Focusable/Focused
    from each `StateSet`, so check/radio/menu items, toggle buttons, and
    combo/menu buttons reached AccessKit with no state. The dead `node_flags`
    became a pure `node_states(StateSet) -> NodeStates` distiller
    (`Checked`/`Pressed`→`Toggled::True`, `Indeterminate`→`Mixed`,
    `Checkable`→`False`; `Expandable`/`Collapsed`/`Expanded`→`expanded`;
    `Selectable`/`Selected`→`selected`; `HasPopup`→`has_popup`), carried on
    `MirrorNode`, populated in `read_node`, set in `build_node` via
    `set_toggled`/`set_expanded`/`set_selected`/`set_has_popup`. Live-verified
    through the mirror on gtk4-widget-factory: `CheckBox tog=True/False/Mixed`,
    `RadioButton` same, `ToggleButton` pressed=`True`, menu button `pop=Menu`
    (`dump_tree` gained a state column). Core fix for "menu items and combo
    boxes don't announce state"; applies to both toolkit paths.
  - *Roles* (commit e6bbc2d): `Role::PopupMenu`→`Menu`,
    `Role::PushButtonMenu`→`Button` (popup rides `has_popup`); both had fallen
    through to `GenericContainer`. Classic
    MenuBar/Menu/MenuItem/CheckMenuItem/RadioMenuItem were already mapped
    (LibreOffice/gtk3 emits ~770 MenuItems + 520 CheckMenuItems, now stateful).
  - *Drive-back diagnostic* (commit 7296594): `handle_action` discarded
    `perform`/`set_text_selection` errors with `.ok()?`, so a GTK4
    `grab_focus`/`set_caret_offset` returning NotSupported vanished silently.
    It now logs `action`+`path`+error at `warn` (added `tracing` dep); visible
    under the daemon's subscriber.
  - *LibreOffice gtk3*: `SAL_USE_VCLPLUGIN=gtk3` set locally via
    `~/.config/environment.d/accesskit-libreoffice.conf` (this-system, not the
    installer; takes effect next WSL session). Re-confirms the A/B — same
    soffice, gtk4 `grab_focus`→`Err(NotSupported)` vs gtk3 (ATK bridge)
    `text grab_focus`→`Ok(true)`.
  - *GTK4 menu/combo shape* (characterization that drives the Remaining item
    below): GTK4-native apps expose **no** Menu/MenuItem/PopupMenu roles even
    with a menu open — a "menu" is a `menu.popup` `DoAction` on a
    `Grouping`/`ToggleButton[has_popup]` node, items surface as Button/Label in
    a transient popover (classic menu-role mapping is moot for GTK4; it matters
    for LibreOffice/gtk3). `ComboBox` exposes the `Selection` interface (not
    Value) and holds its displayed value in a child `Text` node the mirror
    already reads (value is structural — no special casing);
    `Slider`/`SpinButton`/`ScrollBar` expose `Value`. `DoAction` genuinely
    activates on GTK4 (`menu.popup` returned true).

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
3. **Coalesce children-changed bursts**: ~~debounce is a future optimization~~
   **done** — `RewalkCoalescer` + debounce arm, 24 → 3 updates per burst (see
   the combined milestone above). Commit cdd75de.
4. **`probe` example**: ~~latent hvsocket receive-timeout bug~~ **done** — probe
   now returns the hvsocket flag from `connect` and retries `ConnectionAborted`
   like the `viewer` fix (18542fd). Commit ad24120.
5. **Focus/caret follow-ups**: (a) ~~subscribe
   `object:active-descendant-changed`~~ **done** (see the milestone above);
   (b) ~~map UIA `Action::SetTextSelection` → AT-SPI `set_caret_offset`/selection~~
   **done and live-verified on LibreOffice/gtk3** — `caret_drive` and
   `focus_drive` both PASS against VCL (the toolkit that implements the writes),
   and a same-app gtk3-vs-gtk4 A/B proves the block is the GTK4 AT-SPI bridge,
   not the environment (see the LibreOffice section in `docs/spikes.md`); (c)
   ~~give
   `Role::Label`/`Document`/`Terminal` text runs too~~ **done** (see the static
   text runs milestone above — leaf-gated, caret suppressed for the caret-less
   static roles; Terminal caret and selectable-Document caret noted there);
   (d) ~~geometry (`character_positions`/`widths`) for
   magnifiers~~ **done** (see the combined milestone above — TextRun bounds +
   positions + widths + LTR direction, 512-char cap, UIA TextPattern rects
   verified on the RAIL window; remaining geometry follow-up: RTL direction —
   container/element bounds via `Component.GetExtents`, the empty-field caret
   anchor, and LO `Paragraph` runs are all **done**, see the mirror-follow-ups
   milestone); (e) a `GetForegroundWindow` gate on
   `post_focus(true)` if RAIL testing shows Narrator/NVDA focus theft.
6. **GTK4 action drive-back** (the "operate menus/combo boxes" half — not yet
   started). Today `mirror::perform` wires only `Click`→`DoAction(0)` and
   `Focus`→`grab_focus` (dead on GTK4 by design — see spikes.md). GTK4 keeps
   every *action* interface (verified in upstream `gtk/a11y/`):
   `Action.DoAction`, `Selection.SelectChild`, `Value.SetCurrentValue`,
   `EditableText` — all present in `atspi-proxies 0.14`. Plan: (a) a pure
   `plan_action(role, interfaces, action) -> AtspiCall` routing fn
   (unit-tested) mapping incoming `accesskit::Action` → `Click`→`DoAction`;
   `Click`/`Focus` on a selectable option → `Selection.SelectChild(index)`;
   `Expand`/`Collapse` → named `DoAction` (`menu.popup`/`Press`, fallback
   `DoAction(0)`); `Increment`/`Decrement`/`SetValue` → `Value.SetCurrentValue`;
   `SetValue` on editable text → `EditableText.SetTextContents`; (b) thin glue
   in `perform` building `SelectionProxy`/`ValueProxy`/`EditableTextProxy`;
   (c) expose Expand/Collapse on Expandable nodes and Increment/Decrement/SetValue
   on Value-interface nodes in `build_node` so UIA offers the patterns. The
   Linux side is TDD-able + live-verifiable here (drive an Expand/Select at a
   live combo). The **unverified hop is UIA→AccessKit** (which action msrdc's
   UIA sends per gesture): resolve it by logging incoming `msg.action` in
   `handle_action` (the drive-back diagnostic above is the instrument) while
   driving the `viewer` RAIL window from `System.Windows.Automation`
   (ExpandCollapse/Toggle/SelectionItem/Invoke/RangeValue). Full end-to-end
   UIA validation needs an active Windows/msrdc session.

## Notes / corrections

- The `Failed to create peer … Invalid address string` warning is NOT fixable
  from our Cargo.toml: `atspi`'s `connection` feature pulls `atspi-connection`
  with *its* default features (which include `p2p`) regardless of atspi's own
  `p2p` feature, and it has no `default-features = false`. The warning is a
  debug-only `eprintln` in `atspi-connection`, silently ignored in release
  builds. Left as is (the doc's "drop p2p" plan was moot).

## After that — DVC plugin follow-ups (full E2E done above)

1. **Idle-window attach gap**: ~~first focus/interaction attaches~~ **done** —
   same-title `SetWindowTextW` nudge on `WindowAdded`, verified attaching with
   zero interaction (see the combined milestone above). Commits 7ceb8cf,
   c2af53e, c2ed8b3.
2. **`app_id` is None from `AtspiSource`**: ~~plumb the app id~~ **done** on
   the Linux side (session-bus ownership; `app=Some("org.gnome.TextEditor")`
   live) and the AUMID is now read on the Windows side — but **disambiguation
   by AUMID equality is dead on WSLg by design**: msrdc's RAIL AUMID is an
   opaque `Microsoft.RemoteApp.<hash>`, not the Linux app id. Same-title
   cross-app windows still need a different signal (the appId↔windowId
   association PDU rides the *stock* plugin's channel). Commits 9e1a3bc,
   cafe28b.
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
5. **Test — `WSL2_WESTON_SHELL_DESKTOP=1` (full desktop shell)**: ~~hypothesis:
   a WM makes GTK emit focus events~~ **done — hypothesis disproven, goal
   achieved anyway** (see the live-verification milestone above and Spike 5 in
   `docs/spikes.md`). WSLg's desktop mode runs (weston `desktop-shell.so`,
   msrdc shows one `wslg_desktop` window) but renders **black with dead RDP
   input**, so it verifies nothing; GTK4-Wayland stays `Active`-less under it.
   The working substitute is `GDK_BACKEND=x11` + `xdotool` under the *default*
   rdprail-shell, which delivered every reflection-path verification (window +
   node focus, caret/text/selection). Drive paths (5a live-drive, 5b) are
   toolkit-blocked: GTK4 answers `NotSupported` for `GrabFocus`/`SetCaretOffset`
   everywhere. Active-descendant remains deferred (gnome-text-editor has no
   such widget; needs a list/tree app). Windows-side desktop-mode record: the
   plugin loads, chain-load reports 2, the `AccessKit` channel connects, the
   RAIL hook installs with nothing to bind, and the desktop window's UIA
   surface is plain RDP chrome (`TscShellContainerClass` → BBar + panes →
   `UIMainClass`). `.wslgconfig` restored to rdprail-shell afterward. Examples
   055e3cb.

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
- Focus/caret live testing without Windows (works under stock rdprail-shell):
  launch the app with `GDK_BACKEND=x11` added to the usual env (kill any
  Wayland-backed instance by pid first — single-instance D-Bus activation),
  then drive real input with the preinstalled `xdotool`:
  `xdotool windowfocus --sync <id>` toggles between two windows (focus events),
  an XTEST click into the text area sets the focus widget, `xdotool key
  Left/Home` moves the caret, `type` edits. Watch with `window_lifecycle`
  (focus-only deltas print as `TreeUpdate n (0 nodes)`); `state_probe` prints
  states + verbatim GrabFocus/SetCaretOffset results. Raw-signal check:
  `busctl --address $(busctl --user call org.a11y.Bus /org/a11y/bus
  org.a11y.Bus GetAddress | sed -e 's/^s "//' -e 's/"$//') monitor`.
- A bare `wait` hangs a `bash -lc` script that `setsid`-launched GUI apps
  earlier in the same script (they stay children and never exit) — `wait` on
  the explicit watcher pids instead.
- Killing gnome-text-editor safely from a script whose text mentions it:
  `me=$$; pgrep -af gnome-text-editor | while read pid cmd; do [ "$cmd" =
  "gnome-text-editor" ] && [ "$pid" != "$me" ] && kill "$pid"; done` — an
  `awk /gnome-text-editor$/` filter matches the script's own bash cmdline
  (it ends with the pgrep argument) and self-terminates. Clean-slate launch:
  also `rm -rf ~/.local/share/gnome-text-editor` (session/draft restore
  changes node counts).
- The debug DLL is locked while any msrdc holds it — `cargo build -p
  accesskit_remote_dvc_plugin` fails with os error 5. `wsl --shutdown` (watch
  for a *second* msrdc lingering; kill it too), rebuild, then the next WSLg
  boot loads the fresh DLL. Combined-run order that exercises the nudge: build
  DLL → build daemon in WSL → a11y enable + launch app (msrdc boots, RAIL
  window maps, registry empty) → start daemon → read %TEMP%\AccessKitDvc.log
  untouched.
- LibreOffice: `SAL_USE_VCLPLUGIN=gtk3|gtk4 LIBGL_ALWAYS_SOFTWARE=1 setsid
  soffice --writer --norestore`; kill with `pkill -x soffice.bin`. gtk3 is the
  ATK-bridge path (write methods expected); the first launch shows a Welcome
  dialog window.
- xdotool XTEST input regression (2026-07-24): clicks deliver focus events but
  keys/typing no longer reach the GTK4 text widget in this headless X session
  (Spike 5's caret recipe worked earlier under the same shell). Caret-move
  live checks ride LibreOffice's `SetCaretOffset` (caret_drive) instead.
  The regression is GTK4-specific: under `GDK_BACKEND=x11`, LO/gtk3 accepts
  XTEST typing and arrow keys fine (Writer body typing and Calc cell
  navigation both verified live 2026-07-24). `xdotool windowfocus` can throw
  BadMatch on unmapped candidate ids from `search` — try each id and keep the
  ones that focus.
- If the harness timeout kills a long `wsl.exe` invocation, the WSL VM can
  idle-terminate with it (no remaining client), taking `/tmp` logs and the
  launched apps along. Keep driver scripts comfortably under the tool timeout
  and dump logs in the same script that produced them, or in a quick follow-up
  call.
