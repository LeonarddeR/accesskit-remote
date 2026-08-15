# Continuation notes

State as of 2026-07-29. Environment and RDP-plumbing spike findings live in
`docs/spikes.md`; the Newton design note is `docs/newton.md`; per-commit
history is in `git log`, summarized in the changelog at the bottom.

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
| `accesskit_remote` | Framing, JSON codec, sans-I/O `Session` handshake; `WindowAdded` carries an optional `nativeWindowId` |
| `accesskit_remote_transport` | TCP + vsock listener (Linux) + hvsocket connector (Windows), verified across the WSL boundary |
| `accesskit_remoted` | `--tcp\|--vsock [PORT]`; `--atspi`/`--ax` swap `DemoSource` for `AtspiSource`/`AxSource`; WSLg window-id enrichment off the weston log; tracing via `ACCESSKIT_REMOTED_LOG` |
| `accesskit_remote_ax` | macOS AX source: role+subrole map, batched per-element reads, window discovery, window-relative geometry, `AxSource` implementing `TreeSource`. Live end to end: `accesskit_remoted --ax --tcp` serves real Mac windows to the platform-agnostic `probe` client |
| `accesskit_remote_source` | Source-agnostic and platform-neutral: `RewalkCoalescer`, `NodeRefreshLimiter<K>`, `FocusTracker`, `reconcile_windows<K>`. Shared by every tree source; tested on Linux, Windows and macOS |
| `accesskit_remote_atspi` | Roles, states, text runs + geometry + widget-level direction, focus, caret, numeric value, table geometry, cell coordinates, placeholder/level/posinset/setsize, relations, window lifecycle, per-node refresh, debounced re-walks |
| `accesskit_remote_windows` | Visible-window adapter (no visibility precondition), `post_delta` / `post_focus` via a registered window message |
| `accesskit_remote_dvc_plugin` | Chain-loads the stock `WSLDVCPlugin.dll`, per-connection hvsocket pump, RAIL hook + idle-attach nudge, `regsvr32` install, Weston-id-narrowed same-title matching |
| Drive-back | `plan_action` (`drive.rs`) turns each request into an ordered call list tried until one succeeds: named-or-index `DoAction`, `Selection.SelectChild`, `Value.SetCurrentValue` (clamped, zero-step synthesis), `EditableText.SetTextContents`, `GrabFocus`, `SetCaretOffset` |

**Proven live.** Initial tree over the wire; window add/remove; passive
re-walks; window and node focus reflection; caret/text/selection reflection;
text geometry through to real UIA TextPattern rects on the RAIL window; state
deltas (checkbox, radio, toggle, tab selection) on both GTK4 and VCL; the Calc
active-descendant splice; `app_id` resolution; idle attach with zero
interaction; focus and caret drive-back against LibreOffice/gtk3. From the
2026-07-29 E2E block on the real RAIL window: RangeValue read + SetValue
(spinner 50→400), TogglePattern flip, TabItem selection through
`SelectChild`, InvokePattern, `accessible-value` 1-node deltas, two
same-titled windows attaching with distinct Weston ids, and host window
switches arriving as remote focus transitions on the right HWNDs (item 6).

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

The 2026-07-29 session closed the previous list (interface-gated reads,
attributes/relations, action drive-back, same-title disambiguation, the
interactive RAIL focus exercise, widget-level RTL, the Newton note). What
remains is small, conditional, or blocked on the toolkit:

1. **Real screen-reader validation.** Everything is verified with UIA test
   clients; NVDA/Narrator have not been run against a RAIL window yet.
2. **Release workflow.** `.github/workflows/release.yml` has never executed;
   it gets its first run on the first `v*` tag (deliberate).
3. **Toolkit-blocked, watch upstream GTK.**
   - Most GTK4 check/radio buttons expose no AT-SPI action — `DoAction(0)`
     answers `No action with index 0`, so they cannot be toggled remotely
     (two widget-factory checkboxes expose a named `Toggle` and work).
   - GTK4 combo boxes report no expanded state, so their ExpandCollapse
     pattern reads `LeafNode` and the UIA client refuses `Expand()` before it
     reaches the wire. Selection-based opening works.
   - GTK4 popover *items* materialize for some widgets (widget-factory's menu
     button, surfaced by the post-action re-walk) and never for others
     (gnome-text-editor's hamburger); `menu.popup` opens either way.
   - `Component.GrabFocus`/`Text.SetCaretOffset` stay `NotSupported` on GTK4.
4. **Small and conditional.**
   - A `GetForegroundWindow` gate on `post_focus(true)` — condition (focus
     theft) not observed in the live block; unbuilt.
   - The empty-field caret anchor spans the full container height; widget
     padding is ignored (approximate v1).
   - Watch for `chain=false, stock=0` recurrence — seen once during a
     double-msrdc state, not reproduced since.
   - Same-title disambiguation is per-msrdc-session: Weston reassigns RAIL
     window ids on a new peer session without re-logging, so ambiguous sets
     resolve again only when their windows are recreated.
   - Per-run RTL is unachievable over AT-SPI (GTK reports only the widget's
     base direction); the mirror forwards that widget-level direction.

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
- **Attributes:** GTK4 emits `placeholder-text` (real values on text inputs)
  and `toolkit=GTK` on every node; `keyshortcuts` exists but is empty on every
  node; `level`/`posinset`/`setsize`/`xml-roles` are emitted by neither GTK4
  nor LO here — `parse_attributes` still parses the set for toolkits that do.
- **Relations:** LabelledBy/LabelFor/ControllerFor/ControlledBy pairs, hung
  mostly on labeled *Panels* (which is why Panel/Grouping are in the relation
  role gate); we consume forward directions only.
- **Action names are capitalized** (`Click`, `Toggle`, `Activate`) and dotted
  names appear at any index in namespaced lists (`menu.popup` at index 5 of a
  GtkText's list) — named matching is case-insensitive and index-agnostic.
  Most CheckBoxes/RadioButtons/Sliders/SpinButtons/ComboBoxes list no actions
  at all.
- **Popovers are opened blind.** `menu.popup` returns true and the popup maps,
  but GTK4 emits no events for it; for some widgets the items exist in a
  fresh walk afterwards (widget-factory's menu button — the post-action
  debounced re-walk is what surfaces them, +33 nodes), for others they never
  materialize (gnome-text-editor's hamburger). No popover ever appears as a
  separate toplevel, so no window suppression/grafting is needed.
- **Text direction is widget-level only**: the `direction` attribute in
  `Text.GetDefaultAttributes` reports the widget's base direction ("ltr" even
  inside Hebrew text, one uniform attribute run per buffer). The mirror
  forwards it per text node; per-run RTL cannot exist on this bridge.

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
- **LO advertises `Interface::Value` on menu items and table cells** — a
  numeric range is meaningless there, which is why the Value read is
  role-gated (`role_reads_value`: slider/spin/progress/level-bar/scroll-bar/
  dial). Ungated it would have put a RangeValue pattern on ~1300 Writer menu
  items.
- **at-spi2-atk answers `GetRowColumnSpan` with `(iiii)`** — no leading
  validity flag — while the spec shape is `(biiii)`; `read_cell_state`
  decodes both. The TableCell *properties* (`Position`/`RowSpan`/
  `ColumnSpan`) also work over `Properties.GetAll` on the ATK bridge.
- LO menu items expose one empty-named action (`actions=[""]`) — the
  index-0 fallback is their only drive route, verified live in 50 ms
  (`click_probe Bold`).

### macOS / AX

Measured 2026-08-15 with `ax_probe` against Finder, TextEdit, Safari, System
Settings (Catalyst) and 1Password 8 (Electron) on one desktop. Every number
below is from that run; nothing here is inherited from the AT-SPI findings.

- **Element identity is stable, which is the finding the design rested on.**
  `CFEqual` retention of pre-existing elements across a re-walk is **100%** for
  AppKit, Catalyst and WebKit alike — idle, and also across a real content
  mutation (`--churn` writes `AXValue`, then re-walks). The LibreOffice failure
  mode on Linux, where a toolkit mints a fresh accessible per visit and defeats
  id reuse, does not reproduce here. `ElementKey` stays identity-by-reference;
  the positional-key fallback is not needed.
  Caveat: measured on small trees (14-143 nodes). Re-measure on a big
  document or a lazy table before relying on it there.
- **AX writes work.** `AXUIElementSetAttributeValue(AXValue)` on a TextEdit
  text field succeeds. This is the capability GTK4 never had over AT-SPI, where
  `GrabFocus` and `SetCaretOffset` answered `NotSupported` in every
  environment and left two whole drive routes untestable.
- **Walking is roughly 3× cheaper than AT-SPI, unbatched.** 267 nodes across 5
  applications in ~204ms — about 0.75ms/node against AT-SPI's ~1.9ms/node
  *after* its 41% batching win. `AXUIElementCopyMultipleAttributeValues` is not
  used by the probe at all, so the per-node cost has room to fall further.
- **Menu bars are free.** Each application publishes 233-631 menu nodes
  (Safari 631), and **none is reachable from a window-rooted walk** —
  `AXMenuBar` is a sibling of `AXWindows` on the application element. The
  equivalent on Linux was not free: LibreOffice Writer's ~770 MenuItems were
  inside the window tree and had to be walked.
- **`AXWindows` is not exclusively windows.** Finder publishes the desktop
  there as an `AXScrollArea` with no title and no `CGWindowID`. Discovery
  therefore gates on `AXRole == "AXWindow"`, mirroring the AT-SPI source's
  Frame/Window/Dialog filter. Dialogs share that role and are told apart by
  subrole (`AXStandardWindow` vs `AXDialog`/`AXSystemDialog`).
- **`_AXUIElementGetWindow` resolves and works** (Safari window → `305`). It is
  private SPI, so it is loaded by `dlsym` and a miss degrades to
  `native_window_id: None`, which the wire already models. This one call
  replaces the entire WSLg Weston-log ledger (`wslg.rs`, 744 lines) for the
  same field.
- **`AXManualAccessibility` is accepted by Electron and moves no windows.**
  1Password 8 accepted the write; every native application answered
  `AttributeUnsupported`; **no application's window frame changed** in a
  before/after comparison. That is the property that makes it safe to set on a
  machine being screen-shared, and it is why `AXEnhancedUserInterface` — which
  does move windows — is never written.
- **The opt-in does not read back.** After a write that returned success,
  `AXManualAccessibility` still reads `false`. Chromium treats it as a
  write-only signal, so gating the request on a read would mean never enabling
  accessibility at all. `opt_in::answers_opt_in` is documented as diagnostic
  only; its real use is telling a Chromium application (answers) from a native
  one (does not).
- WebKit publishes page content (`AXHeading`, `AXLink`, `AXStaticText` under an
  `AXWebArea`) with no opt-in of its own.
- **Development needs the grant, and SSH does not inherit one.** macOS
  attributes the Accessibility grant to a responsible GUI application; a
  process tree rooted in `sshd` has none, so granting the binary does nothing.
  Either grant `/usr/libexec/sshd-keygen-wrapper` (which widens access to every
  later SSH session) or run from a granted terminal on the Mac.

- **The role map covers a real desktop.** `ax_probe --roles` over Finder,
  TextEdit, Safari, System Settings and 1Password: 422 elements, every
  (role, subrole) pair mapped deliberately, none reaching the catch-all. 348
  reach the consumer and 74 (17.5%) are filtered as structural containers —
  that ratio is the tree-inflation metric to watch when broadening the map.
  Subroles genuinely carry information the role does not: `AXSearchField` and
  `AXSecureTextField` both sit under `AXTextField`, and `AXSwitch` under
  `AXCheckBox`.
- **A freshly launched application reports no windows for a second or two.**
  Same code and same applications went from 0 windows to 1 each with nothing
  but time passing. So an app appearing in discovery with no usable tree is
  normal and must be retried rather than announced broken — which is what the
  AT-SPI source already does by returning `Ok(None)` from `add_discovered`.
- ~4% of elements in a deep walk will not answer `AXRole` at all (17 of 439),
  having vanished between being enqueued and being read. Not a mapping gap;
  the walk must tolerate it silently rather than log per element.

- **A locked screen makes AX lie, quietly.** With the login session locked,
  applications launch but report zero windows and `AXWindows` returns the
  *application element itself*. It presents exactly like a bug in the window
  filter or in CF array handling and is neither. Cross-check against
  `osascript`/System Events: if both agree on zero, it is the environment.
- **`AXChildren` of a window can name the application element**, unlocked too
  (TextEdit, System Settings). That is a cycle back to the root of everything
  and puts `Role::Application` inside a `Role::Window`, so the walk drops it
  and does not descend. AT-SPI needs no such guard; its hierarchy is strict.
- **`AXFrame` is present on 100% of elements** and carries origin and size
  together, so geometry costs one read per node rather than two
  (`AXPosition` + `AXSize`). It is screen-space with a top-left origin;
  `build_container` subtracts the window origin, and a node read without a
  window context carries no bounds at all rather than bounds in the wrong
  space.
- **Neither `AXTitle` nor `AXDescription` alone names the tree.** Of 313
  elements surveyed, 29% carried a title and 54% a description; the name falls
  back from one to the other. Other frequencies worth knowing: `AXRole` 100%,
  `AXParent` 99%, `AXChildren` 82%, `AXFocused` 71% (38 settable), `AXEnabled`
  61%, `AXSubrole` 50%, `AXSelected` 43% (40 settable), `AXValue` 31% (17
  settable). `AXEnabled` being absent on 39% is why absence is not disablement.
- **Catalyst is an order of magnitude slower per node.** System Settings walks
  at ~7ms/node (133 nodes in 946ms) against 0.3ms/node for Electron and
  0.85-1.3ms/node for AppKit and WebKit. A desktop-wide eager walk is
  affordable today, but this is the number that would force lazy walking if a
  session had several Catalyst apps open.
- Post-filter ratio on a real desktop: 311 nodes walked, 240 reaching the
  consumer (77%). That is the tree-inflation metric to watch when the role map
  is broadened.

Still unmeasured: a large document or lazily-populated table (identity and walk
cost at scale), a Java or Qt application, and an *unlocked* Electron window —
1Password was at its lock screen, so the opt-in was verified by acceptance and
by the presence of `AXWebArea` rather than by a tree that grew.

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
- **Same-title disambiguation** rides `/mnt/wslg/weston.log`: Weston logs
  `ClientGetAppidReq: pid:<n> appId:<s> WindowId:0x<hex>` per mapped toplevel,
  the daemon tails it (appId-keyed ledger, FIFO, window-id dedupe), ships
  `nativeWindowId`, and the plugin narrows ambiguous title sets against
  `GetPropW(WslgServerWindowId) & 0xFFFF_FFFF`. **The logged pid is a
  global-VM-namespace pid** (weston saw 10372 for user-distro pid 10311) —
  useless for correlation; appId is the join key, and reactive pairing only
  uses entries fresher than 10 s because the log accumulates entries for every
  window ever mapped. LO owns no app id daemon-side while Weston resolves
  `libreoffice-writer` — covered by the sole-fresh-entry fallback.
- **Weston reassigns RAIL window ids on a new msrdc peer session and does not
  re-log** `ClientGetAppidReq` — so claims go stale across an msrdc restart,
  a lone title match must bind despite a conflicting claim, and ambiguous
  same-title sets resolve again only when their windows are recreated.
- **UIA gesture → AccessKit action mapping, measured on msrdc** (2026-07-29):
  every pattern gesture is preceded by `Focus`; `Toggle`, `Invoke` and
  `SelectionItem.Select` all arrive as `Click`; `RangeValue.SetValue` arrives
  as `SetValue` with numeric data; `ExpandCollapse.Expand` is refused by the
  UIA *client* layer when the provider reports `LeafNode` (GTK4 combos have
  no expanded state) and never reaches the wire.
- `SetForegroundWindow` from a background process is blocked by foreground
  rights; `SwitchToThisWindow` drives real RAIL activation (verified: host
  switches arrived as `remote focus: Some(3) → None → Some(2)`).
- After the distribution installer's uninstall test, the dev machine has no
  plugin registration — `regsvr32 /s target\debug\accesskit_remote_dvc_plugin.dll`
  restores HKCU `OptionalAddIns` + `.wslgconfig`, elevation-free.

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
  `ACCESSKIT_DVC_PORT`. Daemon log level via `ACCESSKIT_REMOTED_LOG`
  (default info; `debug` shows every executor step); weston-log path override
  via `ACCESSKIT_REMOTED_WESTON_LOG` (empty disables the enrichment).
- `scripts/uia.ps1` (Windows PowerShell 5.1, run with `powershell.exe`)
  drives the provider on RAIL windows: `list`, `tree`, `invoke`/`toggle`/
  `expand`/`collapse`/`select -Name <substr>`, `setvalue -Name <substr>
  -Value <n>`, `range`, `focus [-Seconds n]`, `activate -Window <substr>` —
  all accept `-Window <title-substr>`. Pattern-state re-reads wait ~3.5 s for
  the debounced re-walk; unnamed targets (GTK spinners) need a direct
  `FindFirst` on the pattern-available property instead of `-Name`.
- A fresh `wsl --shutdown` wipes `/tmp` (staged scripts included) and apps
  launched before msrdc is up never get RAIL windows — launch apps only
  after the first window of the boot has mapped, or relaunch them.
- Two same-titled windows for testing: two `python3` GTK4 processes
  (`gir1.2-gtk-4.0`) with distinct `Gtk.Application` ids and one fixed
  window title; D-Bus activation blocks duplicate instances of the desktop
  apps themselves.

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
| 13532cc | Numeric value, table geometry, cell coordinates (dual-shape `GetRowColumnSpan` decode, `role_reads_value` gate) |
| f65f401 | Object attributes and relations, role-gated; measured scope |
| ede76ea | `drive.rs` pure action planner + declared actions |
| d614bf4 | Drive-back glue: perform-time context, ordered execution, `action_drive` |
| 13d7749, 3114163, e4522e8 | Same-title disambiguation: wire `nativeWindowId`, weston-log ledger daemon-side, Weston-id-narrowed matcher |
| 0584b28, c574751 | Pump action log; E2E block — gesture mapping measured, veto relaxed, `uia.ps1`, daemon tracing |
| 830f2bc | Widget-level text direction (per-run RTL is toolkit-impossible) |
| 0b24b35 | `accesskit_remote_source` — debounce, refresh limiter, focus tracker and window diff extracted out of the Linux-only crate, generic over node/window identity; macOS CI job |
