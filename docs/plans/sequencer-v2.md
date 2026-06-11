# Sequencer v2 — layers, scales, cycles, probability, macros

The last feature pass before the link goes out. Everything here was decided
in a design interview on 2026-06-09; deviations from that interview are
called out inline with reasons.

Goal: make the sequencer weird, surprising, and joyful while staying vi.
Vi is foundational — every new feature must be reachable through the
existing grammar (counts × operators × motions × registers), not bolted on
as chorded UI.

## Contents

1. Value layers (edit velocity / probability / mod with one grammar)
2. Probability per step
3. Cycle modes per track (reverse, pingpong, random, drunk, exotics)
4. Scales — cents engine, huge library, Scala `.scl` import (microtonal)
5. Per-step mod-in routing ("patch a cable into step 7")
6. Pattern slots a–h per track (vim-register-style)
7. Macros — record/replay semantic commands with clock quantize
8. The macro lane — the sequencer that sequences sequencers
9. Paste grammar change (p = into track, gp = new track)
10. Multi-select: V-line over tracks + persistent track marks
11. Keybinding completeness survey (Y/D/C, composition everywhere)
12. Auto-fill generators (mutate, density, markov, l-systems)
13. Mute audit
14. Data model & persistence
15. Undo coverage
16. What was deliberately NOT done

---

## 1. Value layers

A step now carries: trigger, note, velocity, probability, mod value, and
an optional mod-in binding. Instead of one keymap per field, the pane has
an **active layer** that selects what the grid displays and what the
editing keys touch:

| key  | layer        | grid shows                  | +/- granularity |
|------|--------------|-----------------------------|------------------|
| `'n` | note (default) | pitch-class colors (today's view) | k/j ±1 semitone-or-degree, K/J ±octave-or-period |
| `'v` | velocity     | meter glyphs ▁▃▅█           | k/j ±4, K/J ±16 |
| `'p` | probability  | meter glyphs, dimmed by %    | k/j ±5, K/J ±25 |
| `'m` | mod value    | cv ramp colors (today's ⌁ view) | k/j ±0.01, K/J ±0.1 |

- The modeline shows the active layer when it isn't `note`.
- Operators/motions/visual are layer-independent (they move whole steps);
  what changes is the *display* and the *value keys* (k/j/K/J, `N` prompt).
- `N` (insert mode) prompts a literal value for the active layer
  (note 0–127, vel 0–127, prob 0–100, mod −1…1).
- In insert mode on the prob layer, digits 1–9 set 10%–90% and 0 sets
  100% (Orca-style direct entry). Normal mode digits stay counts.

Rationale: one grammar over every lane — you can `v3l` a probability
phrase and the same keys you already know edit it.

## 2. Probability per step

`Step.prob: u8` (0–100, default 100). At each step transition the playback
thread rolls once; a failed roll makes the step behave as inactive for
BOTH note output and modbus output that tick. Deterministic splitmix64
seeded per (track, global-step) so a paused/resumed transport doesn't
replay differently than a running one.

## 3. Cycle modes per track

`Track.cycle: CycleMode` — `Forward | Reverse | PingPong | Random | Drunk |
EveryOther | Spiral | PrimeJump`.

The playhead becomes a pure function `pos = f(gstep, len, mode, seed)` of
the monotonic global step counter wherever possible (resync-safe across
pause/resume and module restarts):

- Forward: `gstep % len`
- Reverse: `len-1 - gstep % len`
- PingPong: triangle over period `2·len−2`
- EveryOther: `(2·gstep) % len` — even lengths only cover half the
  pattern; that's the charm, not a bug
- Spiral: outside-in interleave `0, len−1, 1, len−2, …`
- PrimeJump: `(gstep · m) % len` where m is the smallest prime ≥ len/3
  coprime with len — a fixed strange permutation per length
- Random: `splitmix(seed ⊕ gstep) % len` — stateless, deterministic
- Drunk: stateful ±1 walk (the one stateful mode; resets on stop)

Keys: `gc` / `gC` cycle forward/back through modes; `:set cycle <name>`
sets directly. Shown as a glyph in the track info column
(→ ← ↔ ? ~ ½ ◎ ✗).

## 4. Scales (the cents engine)

New crate group `src/theory/`:

- `theory::scales` — `Scale { name, degrees: Vec<f64> /* cents */, period }`
  with `degree_to_hz(degree, root_hz)`. A built-in library of 137 scales
  (as shipped; counted by the library invariant test):
  12-TET modes & exotica, Messiaen modes, 24-EDO maqamat, gamelan
  pelog/slendro from published measurements, Bohlen-Pierce, Carlos
  alpha/beta/gamma, EDOs 5–72, just intonation (Ptolemy, Pythagorean,
  Partch 43, harmonic/subharmonic series). Cents from ratios are computed,
  never hand-typed.
- `theory::scl` — Scala `.scl` import, so the entire Scala archive works.

Per track: `scale: Option<Scale>` + `root: u8` (MIDI root, default 60).

**Pitch representation.** When a track has no scale (default), `Step.note`
is a MIDI note — exactly today's behavior, 12-TET via the existing
`note_on` constructors. When a scale is assigned, `note` is reinterpreted
as **scale degree biased at 60** (60 = root, 61 = one degree up, …).
Assigning a scale converts existing notes to nearest degrees (one undoable
EditSteps); removing it converts back. This keeps k/j = ±1 degree and
K/J = ±1 period meaningful in any tuning (a 22-EDO track can reach all 22
degrees), which quantize-on-playback cannot.

Playback sends `AudioEvent::note_on_hz` (new constructor: explicit Hz +
stable u8 note id for note-off matching). Voices already consume Hz —
nothing changes downstream.

Ex commands: `:scale <name>` (tab-completed from the library),
`:scale root <note>`, `:scale off`, `:scale <path>.scl` to import.
Applies to all marked/selected tracks when a multi-select is active.
Track info column shows a short scale tag.

## 5. Per-step mod-in routing

The sequencer becomes a modulation *consumer*. Each step may carry one
binding: `{ param: Note|Velocity|Prob|Mod, source: "module/inst/output",
amount: f32 }`.

At trigger time the playback thread reads the source's modbus channel
(resolved through the manifest, cached and re-resolved ~1 Hz like voice
does) and offsets the step's parameter:

- note: ± round(value · amount · 12) degrees/semitones
- velocity: base + value · amount · 127, clamped
- prob: base + value · amount · 100, clamped
- mod: base + value · amount, clamped −1…1

UI: the active layer names the param; `B` on a step opens the source
picker (same component as voice param binding) and patches the cable into
the *current layer's* param. `gB` clears the binding. `(`/`)` adjust the
amount ±0.05 (with counts). Bound steps wear a cable-colored underline in
the grid; the detail strip shows `src×amt`.

## 6. Pattern slots a–h

Every track has 8 pattern slots, named like registers: `a`–`h`. A pattern
= steps + length + pulses + rotation. The active slot's data lives inline
in `Track` (zero churn for everything that already touches
`track.steps`); inactive slots are stored aside and swapped in/out.

- `"a` … `"h` — switch the current track (or every marked/V-line track)
  to that slot. Mnemonic: slots are registers for patterns.
- `"A` … `"H` — *copy* the current pattern into that slot (save-as).
- Track info column shows the active slot letter; non-empty slots show in
  the detail strip header.
- Switching is undoable and macro-recordable.

Empty slots start as silent 16-step patterns.

## 7. Macros

`q{a-z}` starts recording, `q` stops, `@{a-z}` fires, `@@` refires the
last one. vi's exact surface — but what's stored is **semantic commands**,
not keystrokes (robust to UI changes, quantizable, one undo group):

```
MacroCmd: SwitchPattern{track,slot} | Mute{track} | Unmute{track}
        | SetCycle{track,mode} | TransposeTrack{track,±n}
        | RotateTrack{track,±n} | SetScale{track,name}
        | Fill{track,kind,arg} | SetBpm{f64}
```

While recording, the modeline shows `rec @a`; macro-recordable actions
append as they execute. Non-recordable actions (navigation, step edits)
simply don't record — a macro is a *performance* gesture, not an edit
script (step-level edits already have dot-repeat).

**Quantize.** Each macro has `quant: Now | Beat | Bar | PatternEnd`
(default Bar). Firing `@a` live shows a pending `…@a` in the modeline
until the boundary lands; the playback thread applies it on the tick.
When the transport is stopped, macros fire immediately.

**Ex surface.** `:macro` lists, `:macro a` shows the command list as
text, `:macro a = pat 2 b | mute 3 | xpose 1 +7 | quant beat` edits or
writes a macro by hand. Macros save with the patch.

## 8. The macro lane

A lane strip rendered above the track rows (always present — the
conductor's fixed pane height grows by one line via `CONTENT_LINES`).
The lane is a sequence of slots, one **bar** each; each slot optionally
fires a macro.

- Reach it with `k` from track 1 (it behaves as "track 0" for
  navigation); `j` returns to the tracks.
- On the lane: `@a` assigns macro a to the slot under the cursor, `x`/`d{motion}`
  clears, `y`/`p` yank/paste lane slots, counts work (`4p` = paste 4×).
  **There is no repeat-count UI: the vi grammar is the repeat mechanism**
  (yank a slot, `3p` it).
- `#L` sets lane length in slots, exactly like a track.
- The lane only advances while playing; its playhead renders like a
  track's.
- Lane-fired macros land exactly on their bar line (their own quant is
  bypassed — the lane IS the quantizer).

A track's pattern slots + macros + the lane compose into
sequencing-of-sequences: macros are the atoms, the lane plays them,
patterns are what they switch. Scenes/songs fall out of it without
either word entering the vocabulary.

Deviation from interview: the strip is "track 0" rather than a separate
pane — lowest-risk path that keeps the door open for a real macro pane
later (the engine is UI-independent).

## 9. Paste grammar

- `p`/`P` paste the register's *contents into* the current track at the
  cursor — steps registers exactly as today; a **track register's steps
  now overwrite into the row** (truncated at the pattern end) instead of
  inserting a track.
- `gp`/`gP` insert the register as a **new track** after/before — for a
  steps register, the new track is built from those steps.
- Counts: `3p` still tiles the register 3×; `3gp` inserts 3 tracks.

## 10. Multi-select

- `V` enters visual-line over **tracks**; `j`/`k` extend (this replaces
  the old V = current-track-only visual line, which V-line over one track
  still gives you).
- `X` toggles a **persistent mark** on the current track (gutter `*`);
  `gX` clears all marks. *(Interview said `x`/Space; both are taken — `x`
  cuts a step, Space is transport. `X` was free and adjacent.)*
- Operator targeting: **visual selection if active, else marked tracks,
  else current track** — applies to track-level ops (mute, mode, cycle,
  scale, slot switch, euclid, rotate, fill) and to step-range edits
  (d/c motions, toggle, transpose, layer value keys), which hit the same
  step range on every selected track.
- Yank stays single-track (one register, vi semantics).
- A multi-track edit is ONE undo entry (`Command::Group`).

## 11. Keybinding completeness

Full survey against vim. Added:

- `Y` = `y$`, `D` = `d$`, `C` = `c$` (yank/delete/change to end of
  pattern). *(Counted `#P/#L/#R` Euclid setters keep priority; `D`/`Y`
  with a pending count is still Euclid-free since those use P/L/R only.)*
- Operator + find/till composition `yt3`, `df5`, `ct12` already existed;
  now documented and tested. `yf#`, counts (`2yt8`) verified.
- `@` is now the macro-fire prefix (vi-correct). **Track mode toggle
  moves `@` → `M`** (mnemonic: `m` mutes, `M` switches Mode).
- `"a`–`"h`/`"A`–`"H` pattern slots, `'n/'v/'p/'m` layers, `X`/`gX`
  marks, `gc`/`gC` cycle mode, `gp`/`gP` paste-as-track, `B`/`gB`/`(`/`)`
  step bindings, `q`/`@` macros, `F` fill repeat.
- Every operator works in visual and V-line; `~` toggles spans in both.

The complete reference lives in docs/keybindings.md (updated) and the
user-facing tour in docs/sequencer.md (new).

## 12. Auto-fill generators

`theory::gen`, all deterministic given a seed, all undoable as EditSteps,
all dot-repeatable:

- `:fill mutate [0..1]` — evolutionary nudge (flip/swap/nudge within the
  track's scale); spam it until it grooves. Capped at 85% density.
- `:fill density <0..1>` — downbeat-weighted fill to a target density,
  preserving melodic content of reactivated steps.
- `:fill markov` — learns trigger/note transitions from the *other*
  tracks and generates something that fits the song.
- `:fill cantor | thuemorse | fibonacci | sierpinski` — rewrite-rule
  rhythm masks (notes preserved).
- `F` in normal mode repeats the last fill with a fresh seed (the
  performance gesture); `.` repeats it with the same seed.

## 13. Mute audit

`m` already silences note output and stuck notes release correctly. Bug
found and fixed in this pass: **muting a modulation-mode track left its
last value frozen on the modbus**. Mute now zeroes the track's mod
channel in the same sweep that releases stuck notes. Covered by a test.

## 14. Data model & persistence

`Step` gains `prob: u8` and `bind: Option<StepBind>` (loses `Copy` — the
binding carries a source string; `copy_from_slice` call sites become
`clone_from_slice`). `Track` gains `cycle`, `scale: Option<Scale>`,
`root: u8`, `slots: [Option<PatternData>; 8]` + `active_slot: usize`.
`SequencerState` gains `macros`, `lane`, `marks`, `layer`, pending macro
queue.

Persistence (`state::SequencerParams`): every new field is
`#[serde(default)]` / `Option` so every existing patch and session file
loads unchanged (defaults: prob 100, cycle forward, no scale, slot a,
no macros, empty lane). Scales persist as name + root + raw cents (cents
are authoritative for `.scl` imports; names re-resolve against the
library when present). `CycleMode`, `MacroCmd`, `Quant` live in
`state.rs` next to `TrackMode` so the runtime and the save file share one
type.

`shm::AudioEvent` gains `note_on_hz(note_id, velocity, hz, source, step)`
— same wire layout, explicit frequency.

## 15. Undo coverage

New commands: `SwitchPattern`, `CopyPattern`, `SetCycle`, `SetScale`
(stores full step rewrite — degree conversion included), `SetLane`,
`SetMacro`, `Group(Vec<Command>)` (multi-track edits, macro firings).
Everything a key or `:` command mutates is undoable; navigation, layer
switching, marks, and recording state are not (vi semantics: view state,
not document state).

## 16. Deliberately not done (with reasons)

- **Separate macro-sequencer tmux pane** — the lane strip ships the same
  power with none of the conductor layout risk; the macro engine is
  UI-independent so a pane can be added later without rework.
- **Raw-keystroke macros** — semantic commands are quantize-safe and
  survive keymap changes; keystroke replay can corrupt mid-edit state.
- **Solo** — the mixer owns solo (it has per-channel solo already);
  duplicating it at the pattern level invites state fights.
- **Per-step ratchets/micro-timing** — param-lock style features layer
  cleanly on the new Step struct later; out of scope tonight, listed in
  roadmap.
- **La Monte Young WTP tuning** — only ships if accurately sourced;
  no invented cents values anywhere in the library.
