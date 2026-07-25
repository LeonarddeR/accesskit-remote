# Continuation notes

State as of 2026-07-25. Environment and RDP-plumbing spike findings live in
`docs/spikes.md`; the broad-GTK4 plan (phases 0-10) is at
`C:\Users\LeonarddeRuijter\.claude\plans\i-want-to-continue-quiet-puffin.md`;
per-commit history is in `git log`, summarized in the changelog at the bottom.

## Where things stand

Two end-to-end milestones are proven live:

- **A GTK app's tree read from Windows via UIA.** gnome-text-editor (GTK4/AT-SPI,
  WSL) → `AtspiSource` → `accesskit_remoted --atspi --vsock 4750` → hvsocket →
  Windows `viewer` → `RemoteWindowBinding`. A PowerShell 5.1
  `System.Windows.Automation` client read the tree (FrameworkId `AccessKit`).
- **Full RAIL E2E inside msrdc.** The DVC plugin runs the whole consumer stack.
  UIA on the real `RAIL_WINDOW` read the tree, InvokePattern'd New Tab, GTK
  opened a tab, deltas came back, UIA re-read the grown tree.

| Subsystem | State |
|---|---|
| `accesskit_remote` | Framing, JSON codec, sans-I/O `Session` handshake |
| `accesskit_remote_transport` | TCP + vsock listener (Linux) + hvsocket connector (Windows), verified across the WSL boundary |
| `accesskit_remoted` | `--tcp\|--vsock [PORT]`; `--atspi` swaps `DemoSource` for `AtspiSource` (Linux-only, compiled out elsewhere) |
| `accesskit_remote_atspi` | Roles, states, text runs + geometry, focus, caret, window lifecycle, per-node refresh, debounced re-walks |
| `accesskit_remote_windows` | Visible-window adapter (no visibility precondition), `post_delta` / `post_focus` via a registered window message |
| `accesskit_remote_dvc_plugin` | Chain-loads the stock `WSLDVCPlugin.dll`, per-connection hvsocket pump, RAIL hook + idle-attach nudge, `regsvr32` install |
| Drive-back | `Click`→`DoAction(0)`, `Focus`→`grab_focus`, `SetTextSelection`→`set_caret_offset`. Only these |

**Proven live.** Initial tree over the wire; window add/remove; passive
re-walks; window and node focus reflection; caret/text/selection reflection;
text geometry through to real UIA TextPattern rects on the RAIL window; state
deltas (checkbox, radio, toggle, tab selection) on both GTK4 and VCL; the Calc
active-descendant splice; `app_id` resolution; idle attach with zero
interaction; focus and caret drive-back against LibreOffice/gtk3.

**Wired and unit-tested, never exercised live.** The
`object:selection-changed` route (GTK4 routes selection through state changes
instead); the `window:create`/`destroy` fallback (GTK4 never emits them);
Terminal and Document text runs (no such node in the test apps);
active-descendant against a GTK app (only LO Calc exercises it).

**Toolkit-blocked, cannot pass here.** GTK4 answers `NotSupported` for
`Component.GrabFocus` and `Text.SetCaretOffset` in every environment.
`focus_drive` and `caret_drive` are the ready-made instruments: both PASS
against LO/gtk3 (ATK bridge) and fail against GTK4 by design.

## Open work

1. **Phase 4 — interface-gated reads.** Numeric value and table coordinates,
   read only where the object advertises the interface.
2. **Phase 5 — object attributes and relations.** Role-gated. Includes the
   measurement that settles whether GTK4 emits `level`/`posinset`/`xml-roles`
   at all — `Switch` support depends on `xml-roles`, since `atspi-common 0.14`
   has no such role variant.
3. **Phases 8-9 — GTK4 action drive-back** (the "operate menus and combo boxes"
   half). Two measurements constrain the router: a GTK4 check button has **no
   action at index 0** (`DoAction(0)` → `No action with index 0`), while VCL's
   toolbar toggles have **empty action names** — so `plan_action` must try a
   named action *and* an index, in order; neither alone suffices. GTK4 does
   implement every *action* interface (`Action.DoAction`,
   `Selection.SelectChild`, `Value.SetCurrentValue`, `EditableText`, all
   present in `atspi-proxies 0.14`). Plan: (a) a pure
   `plan_action(role, interfaces, action) -> AtspiCall` in `drive.rs`, mapping
   `Click`→`DoAction`; `Click`/`Focus` on a selectable option
   →`Selection.SelectChild(index)`; `Expand`/`Collapse`→ named `DoAction`
   (`menu.popup`/`Press`, falling back to an index);
   `Increment`/`Decrement`/`SetValue`→`Value.SetCurrentValue`; `SetValue` on
   editable text →`EditableText.SetTextContents`. (b) Thin glue in `perform`
   building `SelectionProxy`/`ValueProxy`/`EditableTextProxy`. (c) Expose
   Expand/Collapse on Expandable nodes and Increment/Decrement/SetValue on
   Value-interface nodes in `build_node` so UIA offers the patterns.
   The Linux side is TDD-able and live-verifiable here. The **unverified hop is
   UIA→AccessKit** — which action msrdc's UIA sends per gesture. Resolve it by
   logging incoming `msg.action` in `handle_action` (the drive-back diagnostic
   is the instrument) while driving the RAIL window from
   `System.Windows.Automation` (ExpandCollapse/Toggle/SelectionItem/Invoke/
   RangeValue). Full validation needs an active Windows/msrdc session.
4. **Phase 10 — Newton design note.** Documentation only.
5. **Same-title window disambiguation.** AUMID equality is dead on WSLg (see
   findings); the appId↔`WslgServerWindowId` association PDU rides the *stock*
   plugin's channel, so a different signal is needed.
6. **Interactive RAIL focus exercise.** `FocusChanged` → `post_focus` is wired
   and unit-tested but only ever driven headlessly; the RAIL path is where GTK
   actually delivers `window:activate`.
7. **Small and conditional.**
   - RTL text direction (`TextRun` currently hardcodes LTR).
   - A `GetForegroundWindow` gate on `post_focus(true)` if RAIL testing shows
     Narrator/NVDA focus theft.
   - The empty-field caret anchor spans the full container height; widget
     padding is ignored (approximate v1).
   - Watch for `chain=false, stock=0` recurrence — seen once during a
     double-msrdc state, not reproduced since.

## Design constraints and toolkit findings

### AccessKit and the consumer

- **`accesskit_windows` is what to mirror against, not `accesskit_atspi_common`.**
  AccessKit's own AT-SPI adapter is deliberately lossy (no Table/TableCell
  interface, one relation type, five attributes, a single `"click"` action);
  `platforms/windows/src/node.rs` consumes the full property set.
- **The `GenericContainer` fallback is load-bearing.**
  `accesskit_consumer::common_filter` excludes exactly `GenericContainer` and
  `TextRun`, so promoting `Grouping`/`Panel`/`Viewport`/`SplitPane` would
  surface every GTK layout box. Structural roles stay transparent;
  `refine_role` promotes only a *named* `Grouping` (ARIA `group`).
- **`accesskit::Node` compares property values in insertion order** (a
  `Vec<PropertyValue>` plus an index array, `common/src/lib.rs`), so a refresh
  must set properties through the same path and order as the walk or an
  unchanged read emits spuriously. Sharing `build_container` between walk and
  refresh is what makes "unchanged ⇒ `None`" hold.
- **A refresh changes semantics only, never structure.** Children come from the
  paths the client currently holds plus the node's cached run ids, so a lazy
  grid's fresh 5000-cell child list can neither bloat the tree nor allocate
  ids, and the last-emitted `TextSelection` carries over verbatim.
- The UIA Text pattern needs a role in `supports_text_ranges` —
  `{text inputs} ∪ {Label, Document, Terminal}`, plus `Paragraph` through our
  static-run set — each with ≥1 `TextRun` child. `has_text_selection()` false
  → `SupportedTextSelection = None` in the consumer.
- **`Showing`/`Visible` are deliberately not mapped to `hidden`.**
  `is_hidden()` makes the consumer drop the whole subtree, and GTK reports
  `Showing=false` for scrolled-out rows a screen reader still needs. Pinned by
  `no_node_is_ever_hidden`.
- Structure and semantics stay orthogonal: the refresh paths never mark the
  `RewalkCoalescer`.
- `bridge_main` resolves the window and cache slot *after* the reads (a re-walk
  in between prunes the path, and `refresh_node` then declines it). Safe only
  because one `select!` branch runs to completion — a future `tokio::spawn`
  would break it.
- The 60s `reconcile` tick is a safety net, not the detection path (the
  reactive root-`children-changed` route catches window changes first).
  Residual exposure is the ≤60s gap between ticks.

### GTK4

- **No `window:create`/`window:destroy`** in this environment — a raw a11y-bus
  monitor shows no `window:*` signals at all. Toplevel lifecycle arrives as
  `children-changed` on the AT-SPI root path
  (`/org/a11y/atspi/accessible/root`), disambiguated by sender; deeper
  `children-changed` stays a same-window re-walk.
- **No `object:selection-changed`** for a tab switch — a bus monitor over the
  click shows only `state-changed:selected` ×2 and `state-changed:selectable`
  ×2, so selection reaches us through the *state* route.
- **The AT-SPI write methods are not implemented, period.**
  `Component.GrabFocus` and `Text.SetCaretOffset` return
  `org.freedesktop.DBus.Error.NotSupported` headless, under the desktop shell,
  and X11-focused alike. This is a toolkit gap, not an environment gap.
- **`disabled` is derived, not mirrored.** GTK4 never emits `State::Enabled`
  and omits `Sensitive` only when the widget is explicitly disabled
  (`collect_states` in `gtk/a11y/gtkatspicontext.c`), while at-spi2-atk emits
  both — so absence of *either* marks a control disabled, gated by
  `is_control()` so layout boxes are not announced disabled.
- **A check button advertises the `Action` interface but has no action at index
  0** — `DoAction(0)` fails with `No action with index 0`.
- **No Menu/MenuItem/PopupMenu roles even with a menu open.** A "menu" is a
  `menu.popup` `DoAction` on a `Grouping`/`ToggleButton[has_popup]` node; items
  surface as Button/Label in a transient popover. Classic menu-role mapping is
  moot for GTK4 and matters for LibreOffice/gtk3. `DoAction` itself genuinely
  activates (`menu.popup` returned true).
- `ComboBox` exposes the `Selection` interface (not `Value`) and holds its
  displayed value in a child `Text` node the mirror already reads — value is
  structural, no special casing. `Slider`/`SpinButton`/`ScrollBar` expose
  `Value`.
- GTK reports `caret_offset = 0` (not −1) on non-selectable labels, which is
  why `has_text_caret(role)` keeps caret/selection to editable fields and
  terminals — otherwise every label gets a degenerate caret-at-0, including on
  live `text-changed` re-reads.
- GTK reports `(0,0,0,0)` extents for newline characters; those are synthesized
  from the predecessor's right edge (leading ones from the first real
  character's left edge).
- GTK4 publishes its tree only when accessibility is enabled — see the workflow
  notes for the `busctl` incantation.

### LibreOffice / VCL

- gtk3 is the ATK bridge and **implements the writes** (`GrabFocus`,
  `SetCaretOffset` → `Ok(true)`); gtk4 VCL inherits GTK4's gap. Same app,
  controlled A/B — details in `docs/spikes.md`.
- **LO mints a transient accessible object (a fresh path) per cell selection**,
  so revisits re-splice under fresh ids and the stable-path fast path is
  unreachable against LO by design. Spliced ids accumulate between re-walks
  (append-only `NodeIdMap`) and each re-walk prunes the spliced nodes, though
  the path→id entries persist for the window's lifetime.
- **VCL exposes its toolbar toggle actions with empty names** (`actions=[""]`).
- **VCL omits `Checkable` on an unchecked dialog check button** — observed live
  as a delta going `toggled Some(True) → None`, which would drop the node's UIA
  Toggle pattern exactly while unchecked. `build_container` therefore floors
  the five inherently-toggleable roles at `Toggled::False`; a read state still
  wins and a plain `Button` stays toggle-less.
- Not a GApplication: it owns no reverse-DNS session-bus name, so
  `AppInfo.app_id` is `None` — the `AppIdResolver`'s real negative case, with
  discovery unaffected.
- **AT-SPI calls serialize on the app's main loop.** The same Writer tree
  walked in 6-7s idle but 79s while LO was busy right after X11 typing. Time
  walks against an idle app.
- Scale reference: Writer publishes ~2100 nodes headlessly under gtk3, of which
  menus contribute ~770 MenuItems and ~520 CheckMenuItems.

### AT-SPI and mirror plumbing

- **The event stream needs its own connection.** Sharing one with method calls
  deadlocks: a full event broadcast stalls that connection's socket reader and
  starves in-flight replies.
- **AT-SPI has no desktop-id property** — Application exposes only
  toolkit/version and a deprecated numeric id. Hence the session-bus
  `AppIdResolver` sideband: a GApplication owns its application id as a bus
  name. (An env-var probe is impossible: atspi finds the a11y bus *via* the
  session bus.)
- `property_is_mirrored` routes off the signal's property *string*, not atspi's
  `Property` enum — `accessible-value` deserializes to `Property::Other`.
- `BoundsChanged` is deliberately not registered: it fires on every scroll, and
  bounds ride the re-walk.
- Batching the per-node walk reads measured **8.1s → 4.7s on a 2446-node Writer
  tree, 41% faster while adding a read**: five calls as one `tokio::join!`,
  Name+Description from one `Properties.GetAll`, and
  `cache_properties(CacheProperties::No)` on the one-shot Text/Component
  proxies — zbus's default lazy cache otherwise costs an `AddMatch` + `GetAll`
  on first property access and a `RemoveMatch` on drop. Time this from
  `AtspiSource::new()`; the bridge thread starts enumerating there.
- Text geometry is capped at `MAX_GEOMETRY_CHARS = 512` per node (one bus call
  per code point); above the cap it emits no geometry rather than partial
  arrays. An empty text field has no anchor of its own, so its run takes a
  zero-width caret rect at the container's left edge.
- The `Failed to create peer … Invalid address string` warning is **not**
  fixable from our `Cargo.toml`: `atspi`'s `connection` feature pulls
  `atspi-connection` with *its* default features (including `p2p`) regardless,
  and offers no `default-features = false`. It is a debug-only `eprintln`,
  silent in release.

### WSLg, msrdc and the DVC plugin

- **msrdc's RAIL AUMID is an opaque hashed RemoteApp id**
  (`Microsoft.RemoteApp.R1qEaHzQr/…=`), *not* the Linux app id. AUMID equality
  with `app_id` can never hold on WSLg, and `normalize_aumid` is moot — it is a
  hash, not a decoration. The real appId↔`WslgServerWindowId` association PDU
  flows through the *stock* plugin's channel (`WSLDVCCallback.cpp`
  `OnAssociateWindowId`).
- **msrdc allocates a 2-element array**, so returning count > 1 from
  `VirtualChannelGetInstance` is confirmed safe — the buffer-overflow linchpin
  is resolved, not merely mitigated. The plugin is exposed via the **instance
  method**: the DLL exports `VirtualChannelGetInstance`, with no class factory,
  no CLSID and no `DllInstall`. It returns both plug-ins only when it occupies
  the `WSLDVC_PRIVATE` slot (gated on `/plugin:WSLDVC_PRIVATE` in the host
  command line).
- **On hvsocket a receive timeout surfaces as `ConnectionAborted`** (not
  `WouldBlock`/`TimedOut`) while the connection stays usable — retry it on
  hvsocket only, or the first idle read drops the connection. Applies to
  `viewer`, `probe` and the plugin pump alike.
- **The WSL VM id changes on every VM boot** — parse it from msrdc's `/v:` at
  runtime, never cache it.
- The RAIL hook is a process-wide **in-context** `SetWinEventHook`
  (CREATE..NAMECHANGE, own pid); its proc runs synchronously on the emitting
  window's owning thread — empirically confirmed — which is where the visible
  adapter gets installed. `try_attach` has an owning-thread guard that turns
  any wrong-thread delivery into a logged skip.
- The idle-attach nudge works because `SetWindowTextW`'s `WM_SETTEXT` executes
  on the *owning* thread, whose `DefWindowProc` raises
  `EVENT_OBJECT_NAMECHANGE` there, re-running the whole `try_attach` path on
  exactly the right thread.
- A RAIL window never receives its own `WM_SETFOCUS`, and the consumer only
  raises UIA focus events while it believes the window is host-focused — hence
  `post_focus` driving `update_window_focus_state` on the window's own thread.
- **Installing a global `tracing` subscriber in `DllMain` fast-fails
  `regsvr32`** at its clean exit (`0xC0000409`, `__fastfail` subcode 7 =
  FATAL_APP_EXIT, faulting module = our DLL). Init tracing lazily on the DVC
  path instead, which msrdc always hits and regsvr32 never does.
- **`windows` 0.62's `PROPVARIANT` is not POD** — the crate's `extensions/`
  module adds `Drop` (`PropVariantClear`) and `From<&str>`, so a hand-built
  `VT_LPWSTR` pointing at Rust memory dies with `STATUS_HEAP_CORRUPTION` when
  Drop frees it.
- Install is elevation-free: `OptionalAddIns\WSLDVC_PRIVATE` resolves from
  **HKCU**, but `WSLG_USE_WSLDVC_PRIVATE` **cannot** come from the registry —
  WSLGd reads it only from `%USERPROFILE%\.wslgconfig`
  (`WSLGd/main.cpp:153-185, 483-489`).
- Default dev port is **4750**; 52017 sits inside a Hyper-V excluded TCP range
  (`netsh interface ipv4 show excludedportrange protocol=tcp`).

## Workflow notes

### Building and testing

- Rust is installed in the WSL distro (rustup, cargo, rustc 1.97.1). Build and
  test the Linux crates from Windows via `wsl -e bash -lc '...'`; single-quote
  the PowerShell argument so `$` reaches bash unexpanded. Keep
  `CARGO_TARGET_DIR=~/target-accesskit-remote` (native; drvfs is slow) and
  build one crate (`-p accesskit_remote_atspi`) — never `--workspace` on Linux,
  the Windows-only members won't build.
- DVC plugin:
  `cargo build|test -p accesskit_remote_dvc_plugin --target x86_64-pc-windows-msvc`.
  The cdylib is loaded at runtime, so `build` must precede `test`; never
  `--workspace`.
- Read vendored crate source with the Read tool over the UNC path
  `\\wsl.localhost\Debian\home\leonard\.cargo\registry\src\index.crates.io-*\<crate>-<ver>\src\...`
  — invaluable for verifying an API instead of guessing.
- Commit each tested component without asking; stop only for real obstacles.
- Background processes started via `Start-Process` die with the sandbox job —
  use the harness `run_in_background`. The sandbox blocks HKLM writes; Windows
  `sudo` works when the user grants elevation.

### Live testing on Linux

- **Enable a11y first** or GTK4 publishes nothing:
  `busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true`.
  Then `GSK_RENDERER=cairo LIBGL_ALWAYS_SOFTWARE=1 setsid gnome-text-editor &`,
  sleep ~7s, run `dump_tree`.
- LibreOffice:
  `SAL_USE_VCLPLUGIN=gtk3|gtk4 LIBGL_ALWAYS_SOFTWARE=1 setsid soffice --writer --norestore`;
  kill with `pkill -x soffice.bin`. gtk3 is the ATK-bridge path (write methods
  work); the first launch shows a Welcome dialog. `SAL_USE_VCLPLUGIN=gtk3` is
  set on this machine via `~/.config/environment.d/accesskit-libreoffice.conf`.
- **Focus and caret input without Windows**, under the stock rdprail-shell: add
  `GDK_BACKEND=x11` to the launch env (kill any Wayland-backed instance by pid
  first — single-instance D-Bus activation), then drive real input with the
  preinstalled `xdotool`: `windowfocus --sync <id>` toggles between two windows,
  an XTEST click into the text area sets the focus widget, `key Left/Home`
  moves the caret, `type` edits. Watch with `window_lifecycle` (focus-only
  deltas print as `TreeUpdate n (0 nodes)`); `state_probe` prints states plus
  verbatim GrabFocus/SetCaretOffset results.
- Raw signal check:
  `busctl --address $(busctl --user call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress | sed -e 's/^s "//' -e 's/"$//') monitor`.
- **xdotool XTEST regression (2026-07-24)**: clicks still deliver focus events,
  but keys and typing no longer reach the GTK4 text widget in this headless X
  session. GTK4-specific — LO/gtk3 accepts XTEST typing and arrow keys fine
  (Writer body typing and Calc cell navigation both verified). `xdotool
  windowfocus` can throw BadMatch on unmapped candidate ids from `search`; try
  each id and keep the ones that focus.
- Clean-slate gnome-text-editor also needs
  `rm -rf ~/.local/share/gnome-text-editor` — session/draft restore changes
  node counts.
- Instruments: `dump_tree` (roles, states, geometry), `charprobe` (raw
  role/state/interface/action dump straight off the bus, `--open <action-substr>`
  fires a `DoAction` first to capture transient popups), `window_lifecycle`
  (each update's focus id and leading node ids), `passive_reflect`,
  `caret_reflect`, `state_reflect`, `click_probe` (update shape and latency),
  `state_probe`, `focus_drive`, `caret_drive`. Note `passive_reflect`'s PASS
  gate compares later walks against the immediate post-action walk, and on
  current gnome-text-editor the settled two-tab tree is *smaller* than the fresh
  one-tab tree — grow-past-initial is unsatisfiable there.

### Windows, RAIL and DVC

- Cross-machine recipe: on WSL enable a11y, then launch the app and daemon
  *detached* so they outlive the launching command —
  `... setsid gnome-text-editor >/tmp/gte.log 2>&1 </dev/null &`, sleep ~9s,
  `setsid accesskit_remoted --atspi --vsock 4750 >/tmp/daemon.log 2>&1 </dev/null &`.
  On Windows take the VM id from
  `Get-CimInstance Win32_Process -Filter "Name='msrdc.exe'"` `/v:` (a single
  msrdc runs while a WSLg app is up, so no GUID ambiguity), run
  `viewer --hvsocket <vm-id> 4750` in the background, then inspect via
  **powershell.exe** (Windows PowerShell 5.1, `System.Windows.Automation`):
  find the RootElement child whose FrameworkId is `AccessKit`. The provider
  collapses filtered and generic nodes, so an 83-node AT-SPI tree shows as ~13
  UIA descendants.
- The debug DLL is locked while any msrdc holds it (`cargo build` fails with os
  error 5). `wsl --shutdown` (watch for a *second* lingering msrdc and kill it
  too), rebuild, and the next WSLg boot loads the fresh DLL. Combined-run order
  that exercises the nudge: build DLL → build daemon in WSL → a11y enable +
  launch app (msrdc boots, RAIL window maps, registry still empty) → start
  daemon → read `%TEMP%\AccessKitDvc.log`.
- Plugin log level via `ACCESSKIT_DVC_LOG`; port override via
  `ACCESSKIT_DVC_PORT`.

### Traps

- Never `pkill -f gnome-text-editor` (or `-f accesskit_remoted`) from a
  `bash -lc '<script>'` whose text contains that string: `-f` matches the
  script's own bash cmdline and SIGTERMs it (exit 15). Kill by pid, or guard.
  `pgrep -a NAME` also fails for names longer than 15 chars — use `pgrep -af`.
  Safe form:
  ```sh
  me=$$; pgrep -af gnome-text-editor | while read pid cmd; do
    [ "$cmd" = "gnome-text-editor" ] && [ "$pid" != "$me" ] && kill "$pid"
  done
  ```
- A bare `wait` hangs a `bash -lc` script that `setsid`-launched GUI apps
  earlier in the same script (they stay children and never exit) — `wait` on
  the explicit watcher pids instead.
- `wsl -e bash -lc '<script>'` exits 15 in this harness even on success; check
  the actual output, not the exit code.
- If the harness timeout kills a long `wsl.exe` invocation, the WSL VM can
  idle-terminate with it (no remaining client), taking `/tmp` logs and the
  launched apps along. Keep driver scripts comfortably under the tool timeout
  and dump logs in the same script that produced them.

## Changelog

Oldest first. `git log` carries the detail.

| Commits | What landed |
|---|---|
| 58e78c2 | `TreeSource` seam; `serve`/`pump`/`dispatch` over `&mut dyn TreeSource` |
| 9777500 | `accesskit_remote_atspi` v0 — pure mapping core, async bus layer, `AtspiSource` |
| fdc6879 | `accesskit_remoted --atspi` serves the live mirror |
| 66e0a0e | Passive event reflection on a dedicated event connection |
| cf0c413 | `AppInfo` carries pid + toolkit |
| 18542fd | **Milestone**: GTK tree visible from Windows via UIA (+ viewer hvsocket retry) |
| 16b10e6 | Window lifecycle — `WindowAdded`/`WindowRemoved`, pure `reconcile.rs` |
| ba4c457, 0963a4b, 6c7ab5c | DVC plugin — Spike 2d, COM scaffolding, stock-plugin chain-load |
| 0bce2b3 + step 3/4 | **Milestone**: full RAIL E2E — pump, visible adapter, RAIL hook |
| 150f4cb, 6becbb4, c9c8ee8 | Focus forwarding — mirror emit, `post_focus`, plugin route |
| 4d5933a, cd5c039, 43a2923 | Caret/text — TextRun synthesis, Text reads, text events |
| ad24120, a1e8b46 | `probe` hvsocket retry; active-descendant, caret drive, periodic reconcile |
| 438a852 | Static text runs for Label/Terminal/Document |
| 055e3cb | `focus_drive`/`caret_drive`/`state_probe`; the X11 + xdotool recipe |
| cdd75de | `RewalkCoalescer` debounce — 24 → 3 updates per burst |
| 9e1a3bc, cafe28b | `AppIdResolver`; AUMID read (and the hashed-id finding) |
| 72812e4, d694504 | Text geometry — bounds, character positions/widths, direction |
| 7ceb8cf, c2af53e, c2ed8b3 | Idle-attach nudge (attaches with zero interaction) |
| 8c108fd | `regsvr32` install — HKCU `OptionalAddIns` + `.wslgconfig` |
| 4dca659, 2152c3b, 9412445, 106daee, f265d57 | LO Paragraph runs; Calc active-descendant splice |
| 9ed9a7f, 95051fb | Container bounds; empty-field caret anchor |
| 45f4659, e6bbc2d, 7296594 | Widget states; PopupMenu/PushButtonMenu roles; drive-back diagnostic |
| 5df5c1c, 7aef483, aa35493, afa6527 | Breadth phases 0-3 — data reshape, roles, states, bus budget |
| e169e92, aaabeb0, 399c2b6, f432d07 | Phases 6-7 — pure `refresh_node`, live-update routing, post-action debounce, toggle floor |
