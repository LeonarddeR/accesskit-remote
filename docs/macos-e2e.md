# Reading a Mac from Windows, without RDP

The shortest path from the macOS provider to a real Windows screen reader. No
RDP client, no DVC plug-in, no registry work, nothing to install on either side
beyond a Rust toolchain — an SSH tunnel and two commands.

It exists because the macrdp DVC integration still has real unknowns, and none
of them are in the part being tested here. This path exercises the whole stack
that matters: the AX source, the wire protocol, the client, and AccessKit's
Windows adapter presenting the trees to UI Automation.

```
  Mac                                    Windows
  ───                                    ───────
  applications                           viewer  (one HWND per Mac window)
      │ AXUIElement                          │ accesskit_windows
  accesskit_remote_ax                    accesskit_remote_client
      │ TreeSource                            │
  accesskit_remoted ──── tcp/127.0.0.1 ──── ssh -L tunnel
```

## What you need

- **The Mac unlocked, and the Accessibility grant live.** Both fail silently
  and look like bugs — a locked screen makes applications report zero windows
  and `AXWindows` return the application element. Check with
  `cargo run -p accesskit_remote_ax --example ax_probe`, which reports the
  grant on its first line and lists real windows if all is well.
- A Rust toolchain on the Windows box, and this repository checked out there.
- SSH from Windows to the Mac (macOS: System Settings → General → Sharing →
  Remote Login).

## 1. On the Mac: serve the desktop

```
cargo run -p accesskit_remoted -- --ax --tcp 4750
```

It prints `listening on tcp 127.0.0.1:4750`. **Loopback only** — that is
deliberate, and why the tunnel below exists rather than a `--bind` flag: the
accessibility tree of a whole desktop is not something to put on a network
interface.

For a legible first test, quit what you do not need. Every window on the Mac
becomes a window in the viewer, and a desktop with Safari, System Settings and
a password manager open is a lot to read through. TextEdit with a document is
the clearest single target.

## 2. On Windows: tunnel, then view

```
ssh -N -L 4750:127.0.0.1:4750 <you>@<mac-hostname>
```

Leave that running. Then, in another terminal:

```
cargo run -p accesskit_remote_windows --example viewer -- --tcp 4750
```

The viewer creates one ordinary top-level window per Mac window, each carrying
the remote tree as its UIA provider. It prints nothing useful; the windows are
the output.

### Or: one window for the whole desktop

```
cargo run -p accesskit_remote_windows --example viewer -- --desktop --tcp 4750
```

**This is the shape macrdp needs**, and worth testing on its own. A window per
remote window is what RAIL gives you — the RDP client really does create a local
HWND for each remote window. A full-desktop session does not: there is one
session window showing a picture of everything, so every Mac window has to reach
the reader through a single host.

`--desktop` does that here, with no RDP involved: one window whose UIA tree is
the whole Mac, each Mac window grafted into it as an AccessKit subtree. Node ids
are namespaced per subtree, so the trees cross the wire and are grafted
unchanged — nothing is renumbered.

What to check, beyond "it reads": that every Mac window is reachable under the
one desktop node, that opening and closing windows on the Mac adds and removes
them live, and that activating a control still drives the real Mac application
(actions are routed by the subtree the request names).

## 3. Read it

**With a screen reader.** Alt-Tab to a viewer window and navigate it. This is
the interesting test, and worth being clear about: *no screen reader has ever
been run against this project on any platform* — `docs/next-steps.md` lists
that as open work for the Linux side too. Whatever it reports is new
information.

What should work: window titles, control roles and names, focus following the
Mac's focus, text content in a document with a caret, and activating a control
(the action travels back and drives the real Mac application).

Per-character geometry is implemented, so a range's rectangles and hit-testing
a point both answer. Before blaming the wire for a text result that looks wrong,
check it against the consumer on the Mac itself:

```
cargo run -p accesskit_remote_ax --example hit_probe -- --empty
```

It feeds real walked trees to the same `accesskit_consumer` the Windows adapter
runs behind UIA and reports whether the centre of every character rectangle
resolves to that character. A UIA finding that survives `hit_probe` passing is a
consumer- or transport-side problem, not a provider one.

**With inspect.exe** (Windows SDK), if you want the raw provider view:
point it at a viewer window; the AccessKit subtree reports `FrameworkId`
`AccessKit`.

`scripts/uia.ps1` does *not* work here — it looks for `RAIL_WINDOW` inside
`msrdc.exe`, which is the WSLg arrangement, not this one.

## Reading the result

Both sides log. On the Mac, `ACCESSKIT_REMOTED_LOG=debug` shows each action
arriving and whether a route was found:

```
DEBUG accesskit_remote_ax::ax: no route to perform this action action=Click role=Toolbar
INFO  accesskit_remote_ax::ax: action performed action=Click call=Perform("AXPress")
```

To watch what the provider emits without involving Windows at all — useful for
telling "the Mac side sent nothing" apart from "the Windows side dropped it":

```
cargo run -p accesskit_remote_ax --example reflect -- --seconds 20 --verbose
```

## Known rough edges

- A window that appears while the viewer is connected shows up within ~3s (the
  reconcile tick), not instantly.
- Geometry is correct until the user scrolls. Scrolling emits no accessibility
  notification on macOS at all, so bounds are only corrected on the next walk
  of that window.
- A single large web page reaches the 5000-node cap and is truncated.
- System Settings' sidebar reports a stream of changed nodes even when idle.
