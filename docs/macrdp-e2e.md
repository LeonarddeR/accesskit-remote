# Reading a Mac from Windows, over RDP

The real thing: a screen reader on Windows reading and driving a Mac through an
ordinary RDP session, with the accessibility tree riding the connection itself.

This is the arrangement the project exists for, and it differs from
[`macos-e2e.md`](macos-e2e.md) in the way that matters. There, an SSH tunnel
carries the trees and the viewer stands in for a client. Here there is no tunnel
and no stand-in: **the dynamic virtual channel is the only path there is**, which
is exactly why it had to be built. (In WSLg the plug-in never used the DVC — the
trees came over an hvsocket into the local VM, a side channel that only exists
because the VM is on the same machine.)

```
  Mac                                         Windows
  ───                                         ───────
  applications                                screen reader
      │ AXUIElement                                │ UI Automation
  accesskit_remote_ax                         one composed tree
      │ TreeSource                                 │ accesskit_remote_windows
  accesskit_remote_server                     accesskit_remote_dvc_plugin
      │ AccessKitBackend                           │ IWTSVirtualChannel
  macrdp  ═══════ RDP, "AccessKit" DVC ═══════ mstsc
```

## Before starting

Three things fail silently and look like bugs:

- **The Mac must be unlocked and the Accessibility grant live.** A locked screen
  makes every application report zero windows, which is indistinguishable from
  a machine with nothing open. macrdp logs which permission is missing if the
  grant is absent.
- **macrdp must be the build you actually granted.** The grant is keyed to the
  binary; a fresh `cargo build` in a different location is a different binary as
  far as TCC is concerned. Build through the usual helper script and replace the
  binary in place.
- **Start it through LaunchServices, not from a shell.** Run from a terminal,
  the same granted binary is refused Screen Recording — TCC attributes the
  request to the responsible parent process, which is the terminal or the SSH
  session, not the app. It presents identically to a stale grant, so it is easy
  to spend an afternoon re-toggling switches that were never the problem.

## 1. On the Mac

```
open -a /Applications/macrdp.app --args --enable-accesskit --log-dir <dir> \
  [your usual flags]
```

`--log-dir` because a LaunchServices-started process has no terminal to write
to. Started by hand from a shell, `macrdp --enable-accesskit` is the same
command minus both.

Off by default, deliberately: the tree exposes the text and structure of every
window on the desktop, which is strictly more than the screen shows. Nothing
touches the accessibility API until a client opens the channel.

`RUST_LOG=debug` shows the channel opening and the tree worker starting.

## 2. On Windows

Build and register the plug-in:

```
cargo build -p accesskit_remote_dvc_plugin --release
regsvr32 target\release\accesskit_remote_dvc_plugin.dll
```

`regsvr32` needs no elevation — it writes `HKCU`. It registers under **two**
keys: WSLg's private slot, and `...\Terminal Server Client\Default\AddIns\AccessKit`,
which is the ordinary route mstsc reads for every connection and the one that
matters here.

Then connect to the Mac with mstsc as usual.

## 3. Read it

The whole Mac desktop arrives as **one** UIA tree on the session window, with
each Mac window grafted into it as a subtree. That is not how the WSLg path
works, and it is not an implementation detail: a full-desktop RDP session has
one window showing a picture of everything, so there is no per-window HWND to
attach to.

What to check:

- the session window reports a desktop root with each Mac window under it
- window contents are announced — titles, roles, labels, text
- opening and closing a window on the Mac adds and removes it live
- focus follows the Mac's focus
- **invoking a control actually presses it on the Mac** — that is the whole
  round trip, and it goes back over the same channel

## When it does not work

The failure modes are in a known order, and each says something different in the
log.

**The channel never opens.** The plug-in logs `waiting for the remote desktop to
open the AccessKit channel` and nothing follows. Either mstsc did not load the
plug-in (check the `AddIns\AccessKit` key points at the DLL that exists) or
macrdp was started without `--enable-accesskit`.

**The channel opens but nothing is announced.** Look for
`hosting the remote desktop on class=…` in the plug-in log. If it is absent, the
session window was not found — the log lists **every** window it considered with
its class and size, so pick the right one and pin it:

```
set ACCESSKIT_SESSION_WINDOW_CLASS=IHWindowClass
```

The heuristic is the largest visible top-level window in the process, which is
a guess about a client's window hierarchy that cannot be verified from anywhere
but a real client.

**Actions do nothing.** Check the plug-in log for `forwarding an action` and the
Mac's log for `action performed`. If the first appears without the second, the
action crossed the wire and the Mac found no route for it — that is a provider
mapping gap, not a transport problem.

## What has been proven without a Windows box

Worth knowing, so a failure here is attributed to the right half:

- macrdp's channel carries bytes both ways against a real IronRDP client
  (`macrdp: src/conn_test.rs::accesskit_dvc`).
- The tree composition satisfies every rule `accesskit_consumer` enforces —
  which it enforces with panics, inside the screen reader's process
  (`accesskit_remote_client::desktop`).
- The provider's trees are correct against the real consumer on the Mac itself
  (`accesskit_remote_ax --example hit_probe`).

What has **not** been proven anywhere is everything between: that mstsc loads
the plug-in, opens the channel, and that the session window can be found and
hosted. That is what this runbook tests.
