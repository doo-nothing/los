# The DLD — a dual looping delay (after the 4ms Dual Looping Delay)

The existing delay (the 288 homage) is a *textural* instrument: eight
taps, shimmer, wash — a room. This module is the opposite pole, modeled
on the 4ms DLD (Gary Hall's design): **a crystal-clean, clock-locked
looping delay**. No character, no diffusion — repeats come back exactly
as they went in, exactly on the beat, and the buffer is an instrument
you can hold, window, and reverse.

Module name: `dld` (aliases `looper`, `dual`). Two instances = four
channels.

## What it is in los terms

Two identical channels (A, B) around one time base:

- **Ping = the transport beat by default.** One beat = `60/bpm`
  seconds, read live from the transport — delay times are *musical* and
  stay locked when macros move the tempo. A `ping` row can be set to a
  free time in ms instead (unquantized mode), decoupling the module
  from the song clock (resonant/Karplus territory).
- **Channel input**: channel A consumes the module's claimed input
  (any source or bus, the established fx pattern). Channel B is
  **normalized from A** exactly like the hardware (`In B` unpatched →
  A's input feeds B). v2 may give B its own claim; one claim per
  module is today's manifest contract, and the second instance is the
  honest workaround — also exactly what owners of one DLD do.
- **Output**: the module's stereo ring carries A left, B right when
  both run (the hardware's mono-in/stereo-out patch); a `mono` toggle
  sums both to both sides.

## Per-channel controls (the DLD panel, translated)

| row | range | meaning |
|---|---|---|
| `time` | 1–16 beats | the Time knob; with `switch` applied: `/8`, `=`, `+16` |
| `switch` | `/8` · `=` · `+16` | eighth-notes / beats / bars-scale |
| `fdbk` | 0–110% | feedback; >100% blooms (soft-clipped in the loop) |
| `feed` | 0–100% | Delay Feed — record level into memory; dry is unaffected |
| `mix` | 0–100% | dry/wet on the channel's output |
| `hold` | off/on | Infinite Hold: stop recording, loop `time` worth of memory |
| `rev` | off/on | reverse read/write; **crossfaded**, never a click |
| `win` | 0–100% | windowing: scrolls the held loop's start point (hold mode only); one full sweep = one loop length |

Mod inputs (`@`-bindable, the los convention): `time`, `fdbk`, `feed`,
`win`, plus **trigger bindings** for `hold` and `rev` (a sequencer
track or an envelope `eoc` can toggle them — Quantized Change Mode
falls out of binding them to a track for free).

## Outputs on the modbus

Three claimed channels, in `routing::output_labels("dld")` order:

- `clk` — the Ping clock as a gate (one beat)
- `lpa` / `lpb` — each channel's loop clock: high at the loop/delay
  boundary. The hardware's killer patch — loop clock fires an envelope
  that VCAs something else — works inside los with two `@` presses.

## DSP notes (pure Rust, no Faust needed)

- Per channel: `Vec<f32>` ring, **120 s at the device rate** (stereo
  pairs stay interleaved in the module's output; channel memory is
  mono like the hardware). ~46 MB/channel at 48 k — `MAX_SECS` const.
- Read/write heads as in the manual's tape metaphor. Time changes move
  the *read* head (delay) or resize the loop keeping the start point
  (hold mode); `rev`+`time` in hold mode trims the end point.
- **Clickless by construction**: every discontinuous head move
  (time change, reverse toggle, hold toggle, window scroll, clear)
  crossfades old→new read positions over 256 samples (~5 ms). This is
  v4-firmware behavior and the whole reason the module can be "super
  clean".
- Feedback path: `read → soft_clip(× fdbk) → write` — the 110%
  bloom region saturates gracefully instead of exploding (the
  hardware's documented Dub trick).
- Hold mode: writing disabled, `feed`/`fdbk` repurposed (`win` row is
  the windowing control; the hardware overloads the Feedback knob,
  a TUI can afford an honest dedicated row).
- Memory clear: `X` on a channel — 50 ms fade-out, zero, fade-in.

## What it is NOT

No taps, no shimmer, no wash, no diffusion, no filtering in the loop
(the 288 already does all of that). If the repeats sound like anything
at all, it's a bug.

## Verification

- Unit: beat math (knob × switch × ping), head distance invariants,
  crossfade continuity (no sample step > the crossfade slope), hold
  loop length, window wrap.
- Live: `los render` a 60-second sketch — a pluck pattern into channel
  A at `time=3, fdbk=60%, mix=50%` — and the audit + a click-scan must
  show discrete repeats on the beat grid (autocorrelation peak at
  exactly 3 beats) and zero isolated discontinuities. **Auditioned by
  a human before anything scores for it.**
