# AccessKit Remote

Making Linux GUI applications accessible to Windows screen readers.

A Linux-side service consumes the AT-SPI accessibility tree (like a screen
reader would), translates it into [AccessKit](https://accesskit.dev)'s
serializable schema, and streams it over a DVC-shaped protocol to a plugin
loaded inside the Windows RDP client, which exposes the trees to UI Automation
via AccessKit's Windows adapter. The first target is WSLg, where each Linux app
window appears as a native RAIL window on the Windows desktop; the protocol is
designed so that full remote Linux desktops (and other producer/consumer
combinations) fit the same skeleton.

## Status

Working end to end on WSLg, with gaps. A Windows UI Automation client can read
a Linux app's accessibility tree on that app's own RAIL window, and invoke
controls in it: the DVC plug-in loads into `msrdc.exe`, connects to the daemon
over hvsocket, attaches an AccessKit adapter to each remoted window, and keeps
it live as the app changes.

Mirrored today:

- Tree structure, with window add/remove and debounced re-walks on change.
- Roles and widget state — toggled, expanded, selected, has-popup, disabled,
  read-only, required, invalid, modal, busy, orientation.
- Numeric value with range and step, table row/column counts, cell
  coordinates and spans, placeholder text, and label/control/description
  relations.
- Focus, at window and node level, including active-descendant; host-side
  window switches arrive as remote focus transitions.
- Text: per-line runs with caret and selection, per-character geometry (so
  UIA `TextPattern` bounding rectangles resolve for magnifiers), and the
  widget's base text direction.
- Semantic changes arrive as single-node deltas rather than full re-walks.
- Same-titled windows are told apart by their Weston window ids, which the
  daemon reads from the WSLg weston log and the plug-in checks against each
  RAIL window's `WslgServerWindowId` property (per RDP session).

Action routing turns each UIA gesture into an ordered list of AT-SPI calls
tried until one succeeds: named or index-0 actions, selection of options and
tabs, clamped value setting and stepping, editable-text replacement, focus and
caret. Verified end to end on the real RAIL window for toggle, invoke, tab
selection and range-value setting.

Known gaps:

- **GTK4 leaves some widgets undrivable by design of its AT-SPI bridge**:
  most check/radio buttons expose no action, combo boxes expose no expanded
  state (their UIA `Expand()` is refused client-side), popover menu items are
  only sometimes exposed after opening, and `GrabFocus`/`SetCaretOffset`
  return `NotSupported`. LibreOffice under gtk3 (the ATK bridge) supports all
  of these routes.
- Verified with UIA test clients; not yet exercised with a real screen reader.
- Exercised only against WSLg on Windows 11, with GTK4 and LibreOffice/VCL apps.

`docs/next-steps.md` tracks current state, open work and the toolkit findings
behind the design; `docs/spikes.md` records the environment and RDP-plumbing
findings; `docs/newton.md` records why the AT-SPI mirror stays the source
path for now and how a Newton source would slot in behind the same seam.

## Installing

The installer is per-user and needs no elevation. It:

- registers the DVC plug-in for the WSLg RDP client (HKCU
  `OptionalAddIns\WSLDVC_PRIVATE`) and sets `WSLG_USE_WSLDVC_PRIVATE` in
  `%USERPROFILE%\.wslgconfig`. The plug-in chain-loads the stock
  `WSLDVCPlugin.dll`, so WSLg's own app-list integration keeps working;
- provisions a WSL distribution of your choice — installs `accesskit_remoted`
  into `~/.local/bin` and enables it as a `systemd --user` service.

Run `wsl --shutdown` afterwards so the next WSLg session loads the plug-in.

No release is published yet. To build the installer locally you need NSIS and
release builds for the four shipped targets; `cargo xtask dist` then assembles
it, which is what the release workflow does on a tag.

## Building

Never build the workspace as a whole — the Windows-only and Linux-only members
do not compile on the other platform. On Windows the DVC plug-in is a cdylib
that its own integration tests load at runtime, so it must be built first:

```
cargo build -p accesskit_remote_dvc_plugin
cargo test -p accesskit_remote_dvc_plugin -p accesskit_remote_windows
cargo xtask build-windows    # release DLL for both Windows arches
```

Inside a Linux distro (from Windows, wrap in `wsl -e bash -lc '...'`):

```
cargo test -p accesskit_remote -p accesskit_remote_transport \
           -p accesskit_remote_server -p accesskit_remote_client \
           -p accesskit_remote_source -p accesskit_remote_atspi \
           -p accesskit_remoted -p xtask
```

On macOS, the same list without `accesskit_remote_atspi`.

`.github/workflows/ci.yml` is the authoritative form of all three.

## Workspace layout

| Crate | Platform | Purpose |
|---|---|---|
| `accesskit_remote` | any | Wire protocol: message schema, DVC-compatible framing, handshake. No I/O. |
| `accesskit_remote_transport` | any | Byte-pump trait with DVC semantics + socket implementations. |
| `accesskit_remote_server` | any | Session/window registry, tree multiplexing, action routing over a `TreeSource` trait. |
| `accesskit_remote_source` | any | Source-agnostic tree-source building blocks: re-walk debounce, per-node refresh limiting, focus tracking, window reconciliation. |
| `accesskit_remote_atspi` | Linux | AT-SPI tree source: mirrors AT-SPI into AccessKit trees. |
| `accesskit_remoted` | Linux | The daemon: AT-SPI source + server + socket transport. |
| `accesskit_remote_client` | any | Client core: receives the protocol, maintains per-window tree stores, routes actions. |
| `accesskit_remote_windows` | Windows | Exposes remote trees to UIA on RAIL window HWNDs. |
| `accesskit_remote_dvc_plugin` | Windows | The DVC plugin DLL loaded by the RDP client. |
| `xtask` | any | Workspace automation: cross-arch builds and installer assembly. |

Naming convention (mirroring upstream AccessKit): a platform-name suffix
exposes to that platform's accessibility API; an AT-API-name suffix consumes
that API as a tree source.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
