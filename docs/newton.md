# Newton: why not yet, and what it would look like

A design note, not a plan. No code exists or is proposed here.

## What Newton is

Newton is Matt Campbell's experimental replacement for AT-SPI on Linux,
built out of the same Sovereign Tech Fund work that produced AccessKit. It
inverts AT-SPI's pull model: toolkits push AccessKit tree updates to the
Wayland compositor as part of a surface's committed state (synchronized with
the visual frame, not polled afterward), through a Wayland protocol that as
of the last check (mid-2024 GNOME blog post, a September 2025 status report)
still lives in an unmerged branch of `wayland-protocols`. The compositor does
not interpret the payload; it only forwards it to assistive technologies —
over D-Bus, with a file descriptor (currently a pipe, shared memory floated
as a future option) passed alongside so the AT reads the update directly
rather than through the bus. The payload riding that pipe is AccessKit
`TreeUpdate`s serialized as JSON — the serialization format was still
explicitly unsettled in the sources found. Only Mutter has a Newton branch;
GNOME Shell itself does not yet speak it, so it still rides AT-SPI. Orca
consumes the Newton side through a Python compatibility library that "roughly
emulates the AT-SPI API," letting Orca's existing command layer run mostly
unchanged against a Newton source instead of a real AT-SPI bus.

No confirmed 2026 upstream-merge announcement turned up in research for this
note; treat "unmerged, compositor support experimental" as the last known
state rather than a currently re-verified one.

## What it means for this project

`accesskit_remote`'s wire protocol already carries the same payload shape
Newton pushes: serialized AccessKit `TreeUpdate`s (see the `Codec` in
`crates/accesskit_remote/src`). That is the load-bearing fact here. Today,
`AtspiSource` (`crates/accesskit_remote_atspi`) is a *lossy* AT-SPI→AccessKit
translator: role maps, state distillation, and interface-gated reads recover
an approximation of the tree the toolkit actually has in memory. A Newton
pipeline would not need to recover anything — it would forward the toolkit's
own AccessKit output close to verbatim, subject only to whatever the Wayland
protocol's JSON schema drops relative to full `accesskit::Node`.

Both would sit behind the same seam: `TreeSource`
(`crates/accesskit_remote_server/src/lib.rs`), the trait `accesskit_remoted`
already parameterizes over (`initial_state`, `perform`, `poll_events`). A
`NewtonSource` implementing `TreeSource` would plug in next to `AtspiSource`
with no change above that line. Once it exists and is exercised, `AtspiSource`
becomes the legacy path — kept for toolkits and compositors that never speak
Newton, not the primary target.

Seam audit (2026-07-29): no crate above `TreeSource` references AT-SPI in
code or depends on `accesskit_remote_atspi` — the wire, server, client,
transport, Windows adapter, and DVC plugin are all source-agnostic. The one
documentation mention found above the seam was reworded the same day.

## Why not now

Three independent blockers, any one of which is sufficient on its own:

- **The protocol is unmerged.** There is no stable Wayland accessibility
  protocol to implement against; building on a draft branch means rebuilding
  when it changes shape, if it lands at all.
- **It needs compositor support this project's environment doesn't have.**
  WSLg runs Weston with the `rdprail-shell` (see `docs/spikes.md`), not
  Mutter — the only compositor with a (branch-only, unmerged) Newton
  implementation. There is nothing to forward the pipe on the producer side
  even if the toolkit emitted Newton updates.
- **GTK's own AccessKit backend doesn't target this path yet.** GTK 4.18
  merged an AccessKit backend, but it targets Windows and macOS; on Linux,
  GTK still defaults to its own AT-SPI bridge (opt-in via `GTK_A11Y=accesskit`
  otherwise). The toolkit side of "GTK pushes AccessKit to Newton" is itself
  optional and off by default.

Given all three, the AT-SPI mirror (`AtspiSource`) stays the correct path for
the foreseeable future. This note exists so that judgment isn't re-derived
from scratch later: the seam is already shaped correctly for a `NewtonSource`
to arrive without disturbing anything above `TreeSource`, but nothing here
depends on Newton landing on any particular schedule.
