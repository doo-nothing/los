# Filterbank — a spectral processor (after the Buchla 296e)

The second fx module (after docs/plans/delay-288.md, whose input
architecture this reuses unchanged) and the first with an **all-Faust
filter core** (docs/writing-dsp.md). Status: **shipped** — implemented
as designed below, with two deltas: the per-band delay spread uses one
shared knob staggering bands linearly (not skewed), and follower decay
is one global (CV-able) knob rather than per-band.

## 1. The hardware

**Buchla 296e Spectral Processor** ($3,399; descendant of the 200-series
296): 16 fixed bandpass filters, each with a VCA and an envelope
follower. Center frequencies tuned to the ear's discrimination curve:
**<100 (LP), 150, 250, 350, 500, 630, 800, 1k, 1.3k, 1.6k, 2k, 2.6k,
3.5k, 5k, 8k, >10k (HP)**. The original 296 builds each band from three
stagger-tuned bandpasses (flat top, steep skirts).

What makes it an instrument rather than an EQ:

- **16 per-band level CV ins** and **16 envelope-follower CV outs**
  (per-band decay programmable, up to ~40 s).
- Bands split **odd/even** into two interleaved 8-band combs with
  separate ins/outs (comb outs are pre-fader).
- **Spectral transfer** (vocoder): odd bands' followers drive even
  bands' VCAs and/or vice versa — analysis on one comb, synthesis on
  the other.
- **A/B spectra with morph**: two stored response curves, crossfaded
  under CV.
- **Freeze**: latch the followers — a spectral hold of the instant's
  footprint.
- 296-only extras: per-band audio outs, and the **programmed spectrum**
  — a frequency/bandwidth window swept across the bands by CV.

## 2. What we build

`filterbank` (alias `296`, `bank`): 16 bands, everything above except
per-band audio outs, plus software freebies:

- **Per band**: level fader (mod-bindable), follower out on the modbus
  (`filterbank/N/b1…b16` — 16 claimed channels), decay control.
- **A/B morph**: two stored fader spectra, `morph` param (bindable —
  an envelope sweeping morph is the classic move). Edit either bank;
  `v`-style visual to paint across bands with h/l sweeps.
- **Spectral transfer**: off / odd→even / even→odd / both. Plus
  **freeze** (key + bindable trigger).
- **Programmed-spectrum window** (from the 296): center + width params,
  both bindable — a CV-swept bandpass/notch over the bank. Cheap in
  software, huge fun.
- **Per-band delay** (the software-only wildcard, delay-288 meets
  296e): one shared `spread` param 0–250 ms staggering each band's
  output in time (linear or skewed by band index). Spectral smearing;
  off by default.
- Stereo: odd bands panned by `split` (0 = mono, 1 = odd hard left /
  even hard right — the comb outs as a stereo field).

## 3. DSP plan (Faust core)

One `.dsp` file, committed codegen like tap8fx:

- 16 × (3 cascaded `fi.resonbp` stagger-tuned, or `fi.svf.bp`) at the
  296e centers; band 1 LP, band 16 HP.
- Per-band VCA + `an.amp_follower_ud` (attack 2 ms, decay = param).
- Faust params: 16 gains, 16 decays, transfer mode/amount, freeze
  (gate). Param plumbing via `ParamIndex` constants asserted by a test
  that walks `build_user_interface` (the index↔label contract).
- Rust side owns: morph interpolation (A/B → 16 gain params), window
  shaping (center/width → gain multipliers), spread delays (reuse
  `delay::dsp` lines), followers → modbus, all smoothing.

Sixteen fixed bands of biquads is exactly what Faust compiles well;
this is the module where the DSP-language path earns its keep.

## 4. UI sketch

16 vertical band strips (the 296e's iconic slider wall) + GLOBAL strip
(input · morph · transfer · freeze · window c/w · spread · split ·
decay). Follower meters live in the faders. Mixer dialect throughout;
`A`/`B` keys (or `:bank a|b`) switch the edited spectrum; short panes
collapse to a spectrum line + detail row.

## 5. Open questions for build time

- Modbus budget: 16 follower channels is a quarter of the bus — fine
  today (sequencer 8 + envelope 12 + delay 9 + 16 = 45/64), revisit if
  a second instance matters before the bus grows.
- Vocoder latency/level scaling between combs — tune by ear against
  Softube's 296e notes (pre-emphasis may want a one-knob tilt).
- Whether morph also interpolates decays or gains only (hardware:
  gains only — start there).
