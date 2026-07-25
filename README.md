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
- Focus, at window and node level, including active-descendant.
- Text: per-line runs with caret and selection, plus per-character geometry, so
  UIA `TextPattern` bounding rectangles resolve for magnifiers.
- Semantic changes arrive as single-node deltas rather than full re-walks.

Known gaps:

- **Action drive-back covers invoke, focus and caret only.** Menus, combo
  boxes, expand/collapse and value setting are not routed yet.
- **GTK4 does not implement the AT-SPI write methods** — `Component.GrabFocus`
  and `Text.SetCaretOffset` return `NotSupported` — so driving focus and caret
  reaches only toolkits on the ATK bridge, such as LibreOffice under gtk3.
- Numeric value, table coordinates, object attributes and relations are not
  mirrored yet.
- Verified with UIA test clients; not yet exercised with a real screen reader.
- Exercised only against WSLg on Windows 11, with GTK4 and LibreOffice/VCL apps.

`docs/next-steps.md` tracks current state, open work and the toolkit findings
behind the design; `docs/spikes.md` records the environment and RDP-plumbing
findings.

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
           -p accesskit_remote_atspi -p accesskit_remoted -p xtask
```

`.github/workflows/ci.yml` is the authoritative form of both.

## Workspace layout

| Crate | Platform | Purpose |
|---|---|---|
| `accesskit_remote` | any | Wire protocol: message schema, DVC-compatible framing, handshake. No I/O. |
| `accesskit_remote_transport` | any | Byte-pump trait with DVC semantics + socket implementations. |
| `accesskit_remote_server` | any | Session/window registry, tree multiplexing, action routing over a `TreeSource` trait. |
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
