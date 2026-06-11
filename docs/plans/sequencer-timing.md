# Sequencer timing — delay, swing, ratchets, probability-of-everything

Varigate-inspired micro-timing pass, decided in a design interview on
2026-06-10. Turns the step grid from "fires on the grid line" into a
scheduler: every step can be late (delay), shuffled (swing/groove),
loose (humanize), and multiplied (repeats) — and each of those has its
own probability, layered on top of the existing step on/off prob.

## Per-step values (4 new value layers)

| Layer | Key | Range | Meaning |
|-------|-----|-------|---------|
| delay | `'d` | 0–95% of the step window | push the step late; stored per step with a **unit flag** — literal ms (default, tempo changes the feel: quirk) or step-% (groove survives tempo). `%` in the delay layer toggles the unit; `N` accepts `30ms` or `25%`. |
| delay prob | `'D` | 0–100% | chance the delay applies **as a random amount up to the set value** this cycle (else the step plays straight). 100% = always the exact set delay. |
| repeats | `'r` | 1–8 | ratchet count; the step's window is subdivided evenly from its (post-delay) start. Note tracks only — modulation tracks ignore repeats. |
| repeat prob | `'R` | 0–100% | **coin flip per extra repeat**: each repeat beyond the first fires with prob p. Count varies 1..=N with a bell curve; 100% = always all N, 0% = single hit. |

These ride the existing value-layer machinery (`state::BindTarget`
grows `Delay`, `DelayProb`, `Repeats`, `RepeatProb`): the grid shows
the layer's value per cell, `k/j/K/J` adjust, `N` sets outright, the
third row shows the cursor step's value (`12ms`, `25%`, `x4`), and —
because layers and bind targets are the same enum — every one of them
is **cable-addressable per step** via `B`, like note/velocity/prob/mod
already are (CV over ratchet count is extremely Varigate).

## Track-level timing (the groove section)

- **swing** — classic MPC: every odd *global* 16th (gstep parity, so
  it lines up across tracks regardless of length/cycle mode) is pushed
  by `(2·s/100 − 1)` steps. 50% = straight, 66% = triplet shuffle,
  75% = max drag. `:swing 66` with the standard menu/audition.
- **groove** — a per-bar template of 16 per-step offsets (±half a
  step), applied like swing but shaped: `straight`, `lilt` (sine
  push/pull across the bar), `drag3` (beat 3 lays back), `push24`
  (2 and 4 rush), `sway` (8th-note lean)… `:groove` gets the full
  explorer treatment — menu, Tab cycle, ↓/↑ audition.
- **humanize** — ±N ms gaussian-ish jitter on every fire, re-rolled
  per cycle (keyed by gstep, which is cycle-unique), deterministic
  like the prob gates. `:humanize 8`.
- **ratchet decay** — one knob for repeat velocity shape: 0 = flat,
  positive = each repeat quieter (classic roll), negative = crescendo
  into the next beat. `:decay 30`.

All four persist in `TrackParam` (serde-defaulted), travel with
`yy`/`gp` like every other track setting, and show as a compact badge
in the track settings cluster when non-default.

## How a step actually fires (composition order)

At each grid crossing, per track:

1. **Step prob** (existing, unchanged) — does the step happen at all?
   Failed = inactive: gate closes, bus reads 0. Gates everything below.
2. **Repeat count** — `1 + Σ coin(p_repeat)` over the N−1 extras,
   deterministic splitmix keyed `(track, gstep, k, REPEAT)`.
3. **Base offset** — swing(gstep) + groove(gstep%16) + delay, where
   delay = `roll(0..=set)` if `coin(p_delay)` else 0, keyed
   `(track, gstep, DELAY)` — plus humanize jitter. Clamped to ≤95% of
   the step window so a step's note-on always precedes the next
   boundary's note-off.
4. **Schedule** — repeat k of n lands at
   `base + k · (window − base)/n`; velocities follow the ratchet
   decay curve from the step's velocity. Repeats are pure retriggers
   (no note-off between — the envelope's `vari_inverse` continuity is
   exactly what makes this clickless).

The playback loop grows a pending-fire queue (`Vec<(phase: f64,
Fire)>`): boundary crossings *schedule*, the loop *emits* whatever has
come due. Poll tightens from 10ms to 2ms while the queue is non-empty
(ratchets at /8 need it), relaxes when idle. Clock regression — mixer
respawn, session reload — **flushes the queue** and rebases, same
contract as `advance_phase` (the permanent-mute bug taught us this).

Modulation tracks: delay + delay-prob apply (the bus write is
scheduled), repeats are ignored.

## Determinism

Every roll is splitmix keyed `(track, gstep, purpose[, k])` — same
session, same seed, same groove; different cycles differ because
gstep does. Identical to the step-prob contract today.

## Persistence & ecosystem

- `StepParam` grows `delay`, `delay_unit`, `delay_prob`, `repeats`,
  `repeat_prob`; `TrackParam` grows `swing`, `groove`, `humanize`,
  `ratchet_decay`. All serde-defaulted — old saves load unchanged.
- Yank/paste: steps carry the new fields automatically (they live in
  `Step`); routing still never travels (pinned contract test stays).
- Undo: all edits flow through the existing `Command`/`Adjust` path —
  Group undo, sweep coalescing, and macro recording (`macro_cmds_of`)
  pick the new layers up for free.
- Pattern slots: `PatternData` holds steps, so slots already carry
  the new per-step values.

## Out of scope (later)

Per-step glide (the other Varigate trick — needs voice portamento
first), groove template editing/import, per-step repeat *spacing*
shapes (accel/decel ratchets), swing subdivision other than 16ths.

## Verification

- Unit: scheduler emit order + clamp (delayed on before next off, no
  reordering), swing math (66% ≈ +1/3 step on odd gsteps only),
  repeat-count distribution (p=0 → 1, p=100 → N, mid-p between),
  delay-prob roll bounds, determinism (same key → same roll),
  queue flush on clock regression, ratchet decay velocities,
  persistence round-trip incl. legacy saves, unit-flag toggle.
- Rig: audible swing sweep 50→75 on the default session; ratchet a
  step to 4 with decay and *hear* the roll; delay-prob at 50 makes a
  step rubbery; recorded-WAV onset analysis confirms odd-step onsets
  move by the swing fraction; click-scan stays at zero.
