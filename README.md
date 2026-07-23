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

Early development. Nothing works yet.

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

Naming convention (mirroring upstream AccessKit): a platform-name suffix
exposes to that platform's accessibility API; an AT-API-name suffix consumes
that API as a tree source.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
