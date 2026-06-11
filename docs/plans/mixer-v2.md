# Mixer v2 — the console

Redesign of the mixer pane, decided in a design interview on 2026-06-10.
The old mixer was horizontal rows (name · meter · %) with level/pan/mute/
solo; "so confusing" was the verdict. This pass makes it a console.

## The shape

**Vertical channel strips**, one per audio source (a channel per voice;
the envelope's function out sits at fader 0 by default — fader-as-patch-
cable). Strip top to bottom, console-true signal order:

```
┌VOICE 0┐
│ drv 0 │   drive — soft saturation, input-gain position
│ hi +2 │   high shelf        ±15 dB
│ mid-3▸│   mid bell gain     ±15 dB   (▸ = mod cable, its color)
│ 1.2kHz│   mid frequency     200 Hz – 5 kHz, log sweep
│ lo +4 │   low shelf         ±15 dB
│ pan · │   equal-power (−3 dB center)
│  ▕█▏  │
│  ▕█▏◂ │   fader with the meter IN the track; ◂ ghost = live
│  ▕▆▏  │   modulated position (voice-slider language)
│  80%  │
│  M S  │   mute · solo
└───────┘
```

Per-channel chain: **drive → 3-band EQ → pan → fader → sum**. The MASTER
strip gets the same EQ + drive plus a **width** control (M/S scale),
then the master fader.

## Mod inputs everywhere

Every strip param — drive, hi, mid gain, mid freq, lo, pan, level — and
the master's params are bindable to any modulation source via the shared
`@` picker, with the both-ends cable-color law and a ghost marker at the
live modulated position. Bindings resolve through the manifest like
voice params (cached, ~1 Hz re-resolve) and publish into the manifest's
consumes bitmap (who's-listening markers).

## Keys (doctrine: navigate the layout axes, adjust on a key)

- `h`/`l` — select strip (channels, then MASTER)
- `j`/`k` — select param within the strip
- `-`/`=` — adjust the selected param (Shift = coarse, counts work)
- `0` — reset the selected param to default
- `@` — bind picker on the selected param · `x` unbinds
- `m`/`s` — mute / solo the selected strip
- unchanged globals: Space transport, `u`/`Ctrl-r` (ParamUndo), `:`
  (with the shared completion menus), `?` help, mouse

## Adaptive layout

No conductor/layout surgery. The strip degrades to the pane it's given:

- **Tall enough (≥ ~13 rows):** full console as drawn above.
- **Short:** strips collapse to name + fader/meter + % + M S, and the
  SELECTED strip's drive/EQ/pan detail renders in a side panel with its
  mod indicators (the sequencer detail-strip pattern).

## DSP

- EQ: RBJ biquads — low shelf, peaking, high shelf; ±15 dB; mid freq
  200 Hz–5 kHz log; coefficients recomputed only when params move.
- Drive: soft clip (`tanh`-shaped) with output gain compensation, 0 =
  bypass-transparent.
- Pan: equal-power (−3 dB center). Width on master via M/S scaling.
- **Anti-click discipline from day one** (lessons of the click hunt):
  every gain-class param (level, pan, drive, EQ gains, width) is slewed
  ~1 ms per-sample at application; mod values are read per slot block
  from the modbus and smoothed the same way. The click-scan measurement
  (sine voices + `los record` + delta scan) is part of verification.

## State & persistence

`MixerTrackParam` grows: `drive`, `eq_hi`, `eq_mid`, `eq_mid_freq`,
`eq_lo`, plus `*_src: Option<String>` per bindable param. `MixerParams`
master side grows EQ/drive/width + srcs. All serde-defaulted — existing
saves load unchanged. All edits flow through the shared ParamUndo.

## Out of scope (later passes)

Sends/aux buses, compression, per-channel metering history, FX slots.

## Verification

- Unit: biquad gain at band centers/shelves (±15 dB within tolerance,
  flat at 0), pan law (−3 dB center, full at edges), drive monotonic +
  bounded + transparent at 0, adaptive row math, persistence round-trip,
  param undo, binding resolve.
- Rig: fresh session — strips render, keys per doctrine, EQ audibly
  works (record with a low-shelf boost and verify spectral tilt in the
  WAV), mod-bind a fader to an envelope and watch the ghost breathe,
  click-scan stays at zero discontinuities.
