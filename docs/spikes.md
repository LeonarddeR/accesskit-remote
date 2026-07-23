# Phase 0 spike findings

Live findings from the WSLg environment (Windows 11, WSL2 Debian 13/trixie,
2026-07-23). See the design plan for the spike definitions.

## Spike 3 — AT-SPI in stock WSL: works out of the box

- Debian 13 WSL with systemd as PID 1 and an active user D-Bus session.
- at-spi2-core 2.61 (trixie-backports) preinstalled incl. ATK bridge,
  `python3-pyatspi`, GI bindings. Orca itself runs in the distro, so the
  screen-reader-enabled flag is set and toolkits publish full trees.
- a11y bus address obtainable via `org.a11y.Bus.GetAddress` →
  `unix:path=/run/user/1000/at-spi/bus_0`.
- `gnome-text-editor` (GTK4) registers on the bus; tree walkable via pyatspi
  (frame title, nested panels; text view sits below depth 4).
- Gotcha: GTK4's default GL renderer (ngl/zink) can fail entirely under WSLg
  software rendering ("ZINK: failed to choose pdev") — the app then runs with
  a live accessibility tree but **no mapped window**. `GSK_RENDERER=cairo`
  (with `LIBGL_ALWAYS_SOFTWARE=1`) fixes it. Test scripts must account for
  this.

## Spike 1 — RAIL window association: strong linkage exists

Windows side (enumerated inside msrdc, PID of the `/wslg` instance):

- Each remoted Linux toplevel is a **visible top-level HWND of class
  `RAIL_WINDOW`** in msrdc.exe. The RAIL container (`TscShellContainerClass`,
  title "RemoteApp") stays hidden.
- **msrdc stamps the server-side RAIL window ID onto each RAIL HWND as window
  properties**: `WslgServerWindowId=0x100000005`,
  `RailWindowIdForDebugOnly=5` for Weston's window ID `0x5`. Readable
  cross-process via `GetPropW`; trivially readable in-process from our plugin.
- HWND title = `[WARN:COPY MODE] <Wayland toplevel title> (<distro>)`.
  The warn prefix appears in RAIL copy mode (e.g. inside an RDP session where
  VAIL shared-memory presentation is unavailable); suffix is the distro name.
  Stripping both yields the exact toplevel title, which equals the AT-SPI
  frame name for GTK apps.
- Weston (system distro) logs the association flow in `/mnt/wslg/weston.log`:
  `rdp_rail_notify_app_list()` entries carry `appWindowId`, and
  `ClientGetAppidReq: pid:<n> appId:org.gnome.TextEditor WindowId:0x5` shows
  msrdc resolving a RAIL window to the Linux app ID (which becomes the HWND's
  AppUserModelID → taskbar grouping).
- Open question for the Linux side: the user-distro bridge cannot directly
  observe Weston's surface→RAIL-window-ID mapping (it lives in the system
  distro). Phase 1 association therefore: normalize titles (strip WSLg
  decorations) + app identity + timing, keyed off the `WslgServerWindowId`
  props on the Windows side. Phase 3's Weston relay could export the exact
  mapping later.
- Still to verify: what UIA/MSAA content msrdc itself exposes on
  `RAIL_WINDOW` HWNDs today (an `MSAA_*` window property exists on the
  container, so msrdc does some accessibility annotation), and whether
  subclass-first wins cleanly.

## Spike 2 — hvsocket: VM ID discovery solved

msrdc's WSLg instance command line (readable by our plugin in-process):

```
msrdc.exe /wslg /silent /v:FBDDE2F2-6CC4-4A2A-AC4D-CE69559CADC5
  /hvsocketserviceid:00000001-FACB-11E6-BD58-64006A7986D3
  /plugin:WSLDVC_PACKAGE /wslgsharedmemorypath:WSL\<vmid>\wslg
  "C:\Program Files\WSL\wslg.rdp"
```

- `/v:<GUID>` is the **WSL VM ID** needed for `AF_HYPERV` connect — no HCS
  API spelunking required, just parse our own process command line.
- The service ID confirms the standard Linux hvsocket GUID template:
  AF_VSOCK port N in the guest ↔ `<N as 8 hex digits>-FACB-11E6-BD58-64006A7986D3`
  on the host (WSLg's RDP transport itself uses port 1).
- `/plugin:WSLDVC_PACKAGE` shows WSLg's own DVC plugin loads via command
  line, not registry. `wslg.rdp` sets `remoteapplicationmode:i:1`,
  `hvsocketenabled:i:1`; WSLg ships in `C:\Program Files\WSL`
  (`WSLDVCPlugin.dll`, `msrdc.exe`, `rdclientax.dll`, `system.vhd`).

## Spike 2b — plugin loading into the /wslg msrdc: AddIns is dead, WSLDVC_PRIVATE is the way

Verified empirically (2026-07-23) and against the local WSLg clone at
`P:\Microsoft\wslg`:

- **The `/wslg` msrdc instance does NOT enumerate the regular
  `Terminal Server Client\Default\AddIns` hive — neither HKCU nor HKLM.**
  Test: rd_pipe has a fully valid HKCU AddIns + COM registration on this
  machine, and a temporary HKLM AddIns entry was added; after cycling msrdc,
  only `WSLDVCPlugin.dll` was loaded either way. The instance loads exactly
  the one plugin named by `/plugin:<name>`, resolved via
  `HKLM\SOFTWARE\Microsoft\Terminal Server Client\Default\OptionalAddIns\<name>`
  (`Name` value = DLL path).
- **Supported override**: `WSLGd/main.cpp:484` — if env
  `WSLG_USE_WSLDVC_PRIVATE` is truthy, WSLGd passes
  `/plugin:WSLDVC_PRIVATE` instead of `/plugin:WSLDVC_PACKAGE`. WSLGd reads
  `.wslgconfig` (`C:\ProgramData\Microsoft\WSL\.wslgconfig` or
  `%USERPROFILE%\.wslgconfig`). Also useful: `WSLG_USE_MSTSC=true` swaps
  msrdc for mstsc (debugging).
- **Deployment design**: register our shim DLL under
  `OptionalAddIns\WSLDVC_PRIVATE` + set `WSLG_USE_WSLDVC_PRIVATE=true` in
  `.wslgconfig`. The DVC entry point
  `VirtualChannelGetInstance(REFIID, ULONG* pNumObjs, VOID** ppObjArray)`
  returns an **array** of plugin objects — our shim instantiates the stock
  `C:\Program Files\WSL\WSLDVCPlugin.dll` via its own
  `VirtualChannelGetInstance` (see `WSLDVCPlugin.def`,
  `WSLDVCPlugin/dllmain.cpp:31`) and returns both its plugin and ours. No
  call forwarding, stock app-list integration preserved.
- Consequence for non-WSLg targets (mstsc, real RDP): regular AddIns
  registration presumably still applies there (rd_pipe's daily mechanism);
  the plugin crate should support both registration modes.

## Spike 1b — msrdc's own UIA on RAIL windows: none (field is clear)

Inspected the live RAIL window with System.Windows.Automation
(2026-07-23): `UiaHasServerSideProvider(hwnd) == FALSE`, no UIA children,
bare `ControlType.Pane` with `FrameworkId=Win32` (the default MSAA proxy)
and only the style-derived Transform pattern. msrdc implements no
accessibility on RAIL windows — which is why WSLg apps are silent for
screen readers today. Our subclassing adapter therefore has no
`WM_GETOBJECT` competition; anything we expose is pure addition.

## Spike 2c — hvsocket end-to-end: verified working

Live test (2026-07-23): python `AF_VSOCK` listener on port 52000 in the
Debian **user** distro; Windows side connected with a .NET socket
(`AddressFamily` 34, `HV_PROTOCOL_RAW`=1, sockaddr_hv = family(2) +
reserved(2) + VmId GUID(16) + ServiceId GUID(16)), service GUID from the
template `<port as 8 hex digits>-facb-11e6-bd58-64006a7986d3`. Echo
round-trip succeeded; guest saw the peer as CID 2 (host).

- **No `GuestCommunicationServices` registry registration was needed** for
  the host→guest direction.
- **The WSL VM ID changes on every VM boot** (observed two different GUIDs
  in one day), so it must be parsed from the msrdc command line (`/v:`) at
  runtime — never cached.
- The user distro can bind vsock directly; the system distro is not
  involved. This validates the phase 1 out-of-band transport exactly as
  designed.

## WSLg reliability quirks observed

- msrdc exits when the last GUI app closes; WSLGd restarts it on demand.
  A toplevel created while the RDP connection is down/cycling may never be
  remoted (no RAIL window until the app recreates its window), while the
  same app started with the connection up maps fine — and then in VAIL
  mode (no `[WARN:COPY MODE]` title prefix). The client plugin must treat
  "AT-SPI window exists but no RAIL HWND (yet)" as a normal state.

## Environment notes

- The WSL distro cold-starts on first `wsl.exe` call; WSLg (weston + msrdc)
  starts with it. Everything above was exercised inside the user's active RDP
  session (console locked) — WSLg works there, in RAIL copy mode.
- Rust is not yet installed in the distro (rustup needed before Linux-side
  builds).
