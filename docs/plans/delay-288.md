# Delay — a time domain processor (after the Buchla 288)

The first **fx module**: audio in → audio out, sitting between a source
and the console. Inspired by Don Buchla's Model 288 *Time Domain
Processor* (1976 — an 8-tap voltage-controlled digital delay; two
prototypes built, shelved for noise, never produced) and its modern
descendant, the Verbos Multi-Delay Processor (NAMM 2018). We take the
288/MDP architecture and add the things software makes free.

## 1. What the hardware actually is

The 288 (from the Buchla Archives panel + Mark Verbos's notes):

- One A/D, a bank of shift-register memory, **8 visible taps** (16
  internal) in series at nominal **20/40/60/80/100/120/140/160 ms**.
- Per tap: a level slider and a **phase select** (normal / off /
  inverted) feeding the mixed output.
- A **time multiplier**: knob + CV + audio-rate FM scaling all tap
  times together. Sweeping it repitches what's in the line.
- A **3-channel input mixer that doubles as the feedback path** — no
  feedback knob; you patch outputs back in.
- Preset summed outputs (A/B/C) wired to fixed tap subsets.

The Verbos MDP keeps the soul, drops the 288's looping/pitch modes, and
adds: **per-tap individual outs + envelope-follower CV outs** (9
followers: input + 8 taps), per-tap time 8–150 ms (tap 8 up to 1.2 s),
and a DSP on tap 8 generating **+1-octave pitch shift and reverb**,
normalled to input channels 2/3 — raise those faders and you get
shimmer or reverb-washed regeneration.

## 2. What we build

A mono-summed delay line with **8 series taps**, stereo output, per-tap
faders/pans/phase, 9 envelope followers on the modbus, and the MDP's
three-character feedback mixer — regen, shimmer, wash — with the
shimmer/reverb block written in **Faust** (docs/writing-dsp.md; this
module is the worked example of the DSP-language path).

```
            ┌──────────────────────────────────────────────┐
in ──┬──────┤  delay line (fractional, smoothed time)      │
     │      │   ▼t1   ▼t2   ▼t3  …  ▼t8     (t_i = i·time) │
     │      └───┬─────┬─────┬────────┬────────────────────-┘
     │          │     │     │        ├──────────► regen ─┐
     │         lvl   lvl   lvl      lvl   tap8 ► faust ──┤ (shimmer,
     │         pan   pan   pan      pan        ► faust ──┘  wash)
     │         phase …                                   │
     │          └──┴──┴── Σ stereo ──────► out           ▼
     └── dry ────────────────────────────►        back into the line
```

- **time** — per-stage delay 1–250 ms (tap 8 reaches 2 s), smoothed
  ~80 ms so sweeps repitch the line like the hardware's swept clock.
  Mod input accepts audio-rate-ish wobble (modbus is per-block, ~750 Hz
  — chorus/flange territory when wiggled fast).
- **regen / shim / wash** — the three input-mixer channels, software
  edition: tap 8 fed back plain, +1 octave (Faust transposer), or
  reverb-washed (Faust freeverb). A soft clip in the feedback sum keeps
  self-oscillation musical instead of explosive.
- **dry** — the instantaneous input's own fader (the 288's "mixed" out
  includes it; the mixer strip's fader then rides the whole module).
- **taps** — active tap count 1–8 (the "expandable channels" knob).
  Inactive taps publish 0 on their followers and drop out of the mix.
- Per tap: **lvl** (fader), **pan** (software bonus — pan odd left /
  even right and you've rebuilt the MDP's odd/even outs in stereo),
  **phase** (norm/off/inv — the 288's phase select, `off` is the tap
  mute).
- **Followers**: input + 8 taps published as `delay/N/in`,
  `delay/N/t1…t8` — the MDP's killer feature, a rhythm section of CVs
  from any audio. Fixed 2 ms attack / 150 ms release for v1.

**Mod inputs on every continuous param**: time, regen, shim, wash, dry,
taps' lvl and pan — all `@`-bindable (21 sources). Phase and tap count
stay manual (discrete switches).

## 3. The fx architecture (new)

Until now the mixer was the only audio consumer; ringbuffers are SPSC.
An fx module **takes over consumption** of its source — patching a
cable out of the console into the delay's input:

- Manifest entry grows a 32-byte `input_shm` field (entry 96→128 bytes,
  manifest version 3): the audio SHM this module is consuming, empty if
  none. `Manifest::publish_input()` sets it; `entries()` exposes it.
- The **mixer skips** any source ringbuffer that some live entry claims
  as its input — that strip disappears (the cable left the console) and
  the fx module's own output strip carries the signal instead. When the
  fx module dies or unpatches, the manifest entry clears and the mixer
  re-adopts the source within ~500 ms. Dead-fx reaping is the existing
  `reap_dead`; no new failure modes.
- The delay's audio thread is **input-clocked**: producers write
  continuously while alive, so we block briefly on the input ring and
  emit one output block per input block; on timeout (source dead,
  nothing patched) we self-pace with silent input so tails ring out.

Input selection is a row on the global strip: a picker over live
audio-producing manifest entries (anything with an `audio_shm` that
isn't us). Saved as the source's `module/instance` so it survives
restarts; re-resolved through the manifest like mod bindings.

## 4. UI & keys

Console layout, mixer dialect (it *is* a little console): vertical
strips t1…t8 + a GLOBAL strip. Tap strip rows: **pan · phase · lvl**
(fader with the follower as its meter). Global rows: **input · time ·
regen · shim · wash · dry · taps**.

`h/l` strips · `j/k` rows · `-/=` adjust (`_/+`/`H/L` coarse, counts) ·
`0` reset · `@` bind (input row: source picker) · `x` unbind · `m`
cycles phase (mute-ish) on a tap strip · `gg/G` · `u`/`Ctrl-r` · `:`
(`:set time 120ms`, `:set taps 4`, …) · `?` · Space · mouse wheel/click.
Short panes collapse to dense rows + a selected-strip detail line,
mixer-style.

## 5. DSP notes

- Line: mono f32 ring, 2.1 s headroom. Read heads at
  `i · smooth(time)`, linear interpolation; one shared smoother so taps
  stay locked in ratio (that's the 288 sound — the *pattern* sweeps).
- Followers: rectify → one-pole AR per tap, computed per block.
- Feedback: `fb = clip(regen·t8 + shim·faust_shimmer(t8) + wash·faust_verb(t8))`,
  summed into the line input. The Faust block
  (`src/modules/delay/tap8fx.dsp` → committed codegen) is 1-in/2-out,
  zero parameters: pitch+reverb character is fixed, amounts are the
  Rust-side faders — keeps the generated-code interface trivial.
- Everything per-tap (lvl, pan gains) smoothed 1 ms like the console's
  strips; no zipper.

## 6. Persistence

`DelayParams` (state.rs): time, regen, shim, wash, dry, taps, input
(`"voice/0"`), per-tap `{level, pan, phase}`, plus `_src` strings for
every bindable. All optional/defaulted; same snapshot/apply pair drives
SIGUSR1/2, `Ctrl-s`, and `:w`/`:e`.

## 7. Out of scope (v1), noted for later

- The 288's **looping/sampling modes** (write/recirc, arm/next sound) —
  a sampler-flavored v2; the line and the UI grammar leave room.
- **Pitch mode** (env-synced sawtooth sweeping the time) — approximable
  today by binding an envelope to time.
- Preset tap-mix morphing (A/B spectra à la 296e) and tap-time
  *patterns* (non-integer ratios, golden/prime spacings) — cheap and
  wild, v1.5 candidates.
- Audio-rate time FM (true) — needs sample-accurate mod, not modbus.

## 8. Order of work

1. Manifest v3 (`input_shm` + `publish_input`) + mixer skip + tests.
2. Faust prelude (`src/faust.rs`), `tap8fx.dsp`, codegen committed,
   `just dsp` recipe; docs/writing-dsp.md.
3. `delay/dsp.rs` (line, taps, followers, feedback) + unit tests.
4. Module proper (threads, UI, keys, undo, persistence) + registration.
5. Docs: keybindings.md, DESIGN.md §9.2 + §7.5, README.

Next module after this: the **296e spectral processor**
(docs/plans/filterbank-296e.md) — second fx module, first all-Faust
core, reusing this input architecture unchanged.
