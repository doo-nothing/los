# Maths — Make Noise–grade Function Generator

Goal: evolve `los envelope` (alias `maths`) into a faithful clone of the Make
Noise Maths (2013 panel) — expressive range and *sound* included.
Reference: https://www.makenoisemusic.com/wp-content/uploads/2024/03/MATHSmanual2013.pdf

## What Maths actually is

Four channels. Ch 1 & 4 are full **function generators**: Rise time, Fall
time, **Vari-Response** (one knob morphing the curve continuously
log → linear → exponential), Cycle (self-looping = LFO up to audio rate),
a **signal input** (turns the channel into a slew limiter / portamento /
filter), and a **trigger input** (fires a full rise+fall regardless of gate
length). Ch 2 & 3 are offset/attenuverter utility channels. Outputs: each
channel, **SUM** (attenuverted mix), **OR** (analog max), and gates **EOR**
(End of Rise, ch 1) / **EOC** (End of Cycle, ch 4). Times span **~0.5 ms to
~25 minutes**. Much of its magic is self-patching (EOC → trigger, ch 1 out →
ch 4 rise CV, …).

## Where we are vs. the dream

| Maths | los envelope today | Gap |
|---|---|---|
| 0.5 ms – 25 min | 1 ms – 10 s | ~5 decades missing |
| Vari-Response log↔lin↔exp | x^k exponent curve | not analog-shaped |
| EOR / EOC gate outputs | internal flags only | not routable |
| Signal-in slew limiting | — | missing entirely |
| Trigger from anything | seq-track notes / off / any | no edge-triggered CV |
| Cycle to audio rate | block-rate modbus out (≈750 Hz updates) | can't be *heard* directly |
| Ch 2/3 offset+atten | 4 full generators (superset) | no offset param |
| Sustain (gate) + AD (trig) | gate-follow via note on/off | ✅ close |

## Design

### M1 — Range & response fidelity
- **Time taper**: param 0–1 → `t = 0.0005 · 3_000_000^p` seconds
  (0.5 ms → 25 min, exponential through the whole travel). Display
  auto-units (ms/s/min). `:set rise 100ms`, `:set fall 2s`, `:set rise 1.5m`
  for exact dial-ins (unit parser; bare number = param 0–1).
- **Vari-Response**: replace the exponent hack with the analog model —
  log side is an RC charge curve `f(x) = (1 − e^(−τx)) / (1 − e^(−τ))`,
  exp side its mirror, τ sweeping ±6 through 0 (= exactly linear). One
  shape param 0–1, same curvature applied to rise and fall, like the
  hardware. This is the "sounds right" part: attacks bloom, decays snap.
- **EOR/EOC outputs**: claim 10 modbus channels — `ch1..ch4, sum, or, and,
  inv, eor, eoc`. EOR high while ch 1 is past its rise; EOC pulses at ch 4
  cycle end (gate semantics per the manual).
- **Offset per channel**: post-attenuverter DC offset (−1..+1), giving ch 2/3
  their Maths role while every channel stays a full generator (strict
  superset of the hardware).

### M2 — Slew & self-patching
- **Signal input** per channel: a `signal_src` binding. When bound, the
  channel stops generating and **slews** the source toward its value using
  rise/fall times + vari-response (portamento, lag, CV smoothing of stepped
  sequencer tracks). Cycle + no signal = LFO, exactly like the hardware.
- **Edge-triggered trigger bindings**: trigger bound to a *non-note* source
  (e.g. `envelope/0/eoc`, a mod-mode sequencer track) fires on rising edge
  (>0.5). Unlocks the classic self-patches: EOC→trigger ping-pong, channel
  cascades, clock divisions.
- Trigger modes already shipped: any note / **off** / specific seq track.

### M3 — Audio-rate (the "even sound wise" mile)
- Cycling at audio rate can't be heard through a 750 Hz modbus. Give the
  module an optional **audio output**: register an AudioRingbuf like a
  voice; cycling channels render per-sample into it (mixer auto-discovers).
  Then a cycled ch 1 at 200 Hz *is* a sound source with vari-response as
  waveshape — the Maths-as-oscillator trick.
- Per-sample internals already run at 48 kHz in 64-sample blocks, so M3 is
  output plumbing, not an engine rewrite.

## Open questions (interview)
1. Ch 2/3: strict-Maths (offset/atten only) or keep 4 full generators +
   offsets (superset)?
2. M3 audio output: in the first build or after M1/M2 land?
3. Display: keep gauge rows, or move toward a Maths-style per-channel
   "panel" view (rise/fall/shape/cycle per column)?
