# The Sequencer

8 tracks × up to 128 steps, edited entirely with vi grammar. This is the
full tour: layers, probability, cycle modes, scales (microtonal included),
per-step modulation cables, pattern slots, and macros — the sequencer that
sequences sequencers. For the one-line-per-key reference see
[keybindings.md](keybindings.md); for the design rationale see
[plans/sequencer-v2.md](plans/sequencer-v2.md).

A *word* below means a run of consecutive active steps — `w`/`b`/`e` and
`dw`/`ye` treat rhythm phrases the way vi treats words.

## The screen

```
 SEQ                          t2/8           ♪120 ▶ 05/16
  @  a··· b···                                  8 lane     ← macro lane
 t1  ●·●· ●·●· ●·●· ●·●·                       16 P5 R0
▶t2* ●●·· ··●· ●··· ·●··                       16 P5 R0 "b ↔ ♪dori
 t3  ◆◇◇◇ ◇◇◆◇                                  8 P2 R0 ⌁
 ...
 ─────────────────────────────────────────────────────────
  01   02   03   04   05  …                    ← step numbers
  r   r+2  ·    r+4  ·                         ← pitch (degrees when scaled)
 100%  60%  ·   25%   ·                        ← active layer's values
 ─────────────────────────────────────────────────────────
 NORMAL 'p                       …@a · Undo: Edit tracks
```

The track info column reads: length, Euclidean pulses/rotation, then only
what's non-default — `"b` active pattern slot, a cycle-mode glyph
(`← ↔ ? ~ ½ ◎ #`), `♪dori` scale tag, `⌁` modulation mode, `M` muted.
A `*` after the track number is a mark; a `▸` means something in the rig
is **listening** to that row (a voice playing its notes, an envelope
triggered by its output) — if a listened row goes quiet, the music
sitting on it is the reason. Steps with a mod cable patched in are
underlined in the cable's color. The playhead trail follows the steps a
track actually visited, so reverse/spiral/random motion reads correctly;
`?` has the cycle-glyph legend.

## Value layers

A step carries a trigger, a pitch, a velocity, a probability, and a mod
value. Instead of one keymap per field, the pane has an **active layer**
that decides what the grid displays and what the value keys edit:

| chord | layer | grid shows | `k`/`j` fine | `K`/`J` coarse |
|-------|-------|------------|--------------|----------------|
| `'n` | note (default) | pitch-class colors | ±1 semitone / degree | ±1 octave / period |
| `'v` | velocity | meters ▁▃▅█ | ±4 | ±16 |
| `'p` | probability | meters, clock-hued under 100% | ±5 | ±25 |
| `'m` | mod value | cv ramp | ±0.01 | ±0.1 |
| `'d` | delay | meters, clock-hued when pushed | ±1 ms or % | ±10 |
| `'D` | delay prob | meters | ±5 | ±25 |
| `'r` | repeats | the count itself: ·2345678 | ±1 | min/max |
| `'R` | repeat prob | meters | ±5 | ±25 |

Everything else is layer-independent: motions, operators, visual mode,
yank/paste all move whole steps. The modeline shows `'v`/`'p`/`'m` when
you're off the note layer. In insert mode `N` prompts a literal value for
the active layer, and on the probability layer the digits are Orca-style
direct entry: `1`–`9` set 10–90%, `0` sets 100%.

## Probability

Each step fires with its probability (default 100%). A failed roll on a
note track behaves like an inactive step — gate closes, the track's mod
channel reads 0. On a modulation track a failed roll **holds** the bus at
its last value: set a mod track's steps to 50% and you have a free
sample-and-hold. Rolls are deterministic per (track, global step), so
pausing and resuming replays identically.

## Timing — delay, swing, ratchets (the Varigate pass)

Every step can leave the grid line, and every leaving has its own dice.

**Per-step delay** (`'d` layer) pushes a step late. The value is literal
milliseconds by default — change the tempo and the feel changes, which
is a feature — or `%` flips that step to **percent of the step window**
so the groove survives tempo changes (the value converts in place; `N`
accepts `30ms` or `25%` directly). **Delay probability** (`'D`) makes it
rubbery: at 100 the delay is exact every cycle; below that, it's the
chance the step gets a *random push up to the set delay* this cycle,
else it plays straight.

**Repeats** (`'r` layer) ratchet a step 1–8 times, evenly subdividing
the step from wherever its (possibly delayed) start landed. **Repeat
probability** (`'R`) is a coin flip per repeat beyond the first — 100
always plays all of them, 50 gives you a different burst length every
bar, 0 is a single hit. `:decay 60` fades successive repeats out like a
real roll; `:decay -40` swells them into the next beat. Repeats are
pure retriggers (no gate-off between), which the envelope's
anti-click continuity was built for.

**The track-level groove section** (all `:` commands, all auditioning
live from the menu — arrow through values and *hear* them):

- `:swing 66` — classic MPC swing; every odd global 16th pushed.
  50 = straight, 66 = triplet shuffle, 75 = maximum drag.
- `:groove lilt` — per-bar timing templates (`straight`, `lilt`,
  `drag3`, `push24`, `sway`, `limp`, `rushin`, `molasses`). A groove is
  16 per-16th offsets applied on top of swing.
- `:humanize 8` — ±8ms of jitter on every fire, re-rolled per cycle
  but deterministic (pause and resume replays the same "performance").
- `:decay 60` — the ratchet velocity shape above.

Non-default settings show in the track info column (`S66 ≈lilt ±8`).
All of it lands on top of the existing step probability: step prob
decides *if*, repeat prob decides *how many*, delay prob decides
*when*. Every per-step timing value is also a `B` cable target — CV
over ratchet count is exactly as unreasonable as it sounds.

All the dice are deterministic splitmix keyed by (track, global step,
purpose): the same session seed replays the same groove, cycles differ
because the global step does.

## Cycle modes

Per track, the playhead can run: **forward**, **reverse**, **pingpong**
(no double-hit at the ends), **random** (deterministic — same bar, same
chaos), **drunk** (±1 stumble), **everyother** (skips by two; even-length
patterns only ever visit half — that's the charm), **spiral**
(outside-in: 0, 15, 1, 14, …), **primejump** (steps by a fixed coprime
prime — a strange permutation that provably visits every step).

`gc` / `gC` cycle through modes; `:set cycle pingpong` is direct. All
modes derive from the global clock, so tracks stay phase-locked to each
other no matter how weird their paths are.

## Scales & microtonality

`:scale <name>` retunes a track. The engine is cents-based — a scale is a
list of cents offsets within a repeating period, and the period doesn't
have to be an octave — so the 139-scale built-in library includes:

- every common 12-TET scale and mode (majors, minors, bebop, blues,
  pentatonics, Japanese scales, Messiaen modes, double harmonic, …)
- maqamat in 24-EDO quarter-tones (rast, bayati, hijaz, saba, sikah, …)
- equal temperaments from 5edo to 72edo, plus meantone/superpyth subsets
- just intonation: Ptolemy, Pythagorean, 7-limit Centaur, harmonic and
  subharmonic series, **Partch's 43-tone scale**, La Monte Young's
  Well-Tuned Piano
- Bohlen-Pierce (period: a twelfth), Carlos alpha/beta/gamma (period: a
  fifth), 88cET (no octave at all)
- gamelan slendro & pelog from published gamelan measurements

`:scale` alone shows the current tuning. `:scale off` returns to
chromatic. `:scale root a3` (or `root 57`) moves the root.
**`:scale <file>.scl` imports any Scala tuning file**, which opens the
several-thousand-scale Scala archive.

On a scaled track, `note` means *scale degree*: `k`/`j` move one degree,
`K`/`J` one period, and the detail strip reads `r`, `r+3`, `r-2` relative
to the root. Assigning a scale converts existing pitches to the nearest
degree (undoable); removing it converts back. Voices receive raw Hz, so
all of this is true microtonality, not pitch-bend trickery.

Scale names are case-insensitive and forgiving about spaces/hyphens.
Multi-select applies: mark three tracks, `:scale slendro` retunes all
three as one undo entry.

## Per-step mod cables (step states)

Any step can have one modulation source patched into one of its
parameters. The active layer names the parameter; `B` opens the source
picker (the same one voice uses) and plugs the cable:

- on `'n`: the source bends the step's **pitch** (± up to 12 degrees)
- on `'v`: it scales the **velocity**
- on `'p`: it sways the **probability**
- on `'m`: it offsets the **mod value**

`gB` unplugs. `(` / `)` dial the amount (±0.05 steps, counts work,
clamped ±2). The cable is **visible**: bound steps wear an underline in
the source's cable color, the detail strip's value row breathes with the
live source (`50→82%` updating in real time on the matching layer), and
parking the cursor on a bound step spells it out in the modeline
(`← envelope/0/ch1 ×0.75`). The source is read at trigger time from the
modbus — an envelope, another sequencer track's output, anything in the
manifest. Yes, a track can
modulate its own steps' probability with its neighbor's output. The
sequencer is now both a producer *and consumer* of modulation.

## Pattern slots

Every track has 8 pattern slots, `a`–`h`, like registers for patterns:

- `"b` switches the current track to slot **b** (empty slots are silent
  16-step patterns waiting to happen)
- `"B` *saves* the current pattern into slot **b** without switching
- with tracks marked or a `V` selection, slot switches fan out — that's a
  scene change without the word "scene"

A pattern is steps + length + pulses + rotation; the scale and cycle mode
belong to the *track* and apply to all its slots. Switching is undoable
and macro-recordable.

The detail strip's number row carries a **slot chip** — `a b c d e f g h`
with the active slot highlighted and slots holding content lit — so an
empty slot reads as "empty slot," not as your pattern vanishing.

## Multi-select

Two mechanisms, freely combined:

- `V` — visual-line over **tracks**: `j`/`k` extend, then `d` deletes the
  span, `c` clears it, `~` toggles every step, `m`/`M` mute/mode it,
  `y` yanks the current track (one register — vi rules).
- `X` — toggles a persistent **mark** on the current track (`t3*` in the
  gutter); `gX` clears all marks. Non-consecutive selection, dired-style.

Every edit fans out: **V-line span beats marks beats current track**,
and a fanned-out edit is ONE undo entry. **Yank fans out too**: `y` over
three marked tracks reports `3 tracks yanked`; `p` then **block-pastes**
them down successive rows from the cursor, and `gp` re-inserts all three
as new tracks. A multi-track delete leaves everything it removed in the
register, row order intact. (A step-range delete across a multi-select
still keeps only the last target's steps.)

## Auto-fill

`:fill <kind> [arg]` rewrites the current (or marked) track's pattern:

| kind | what it does |
|------|--------------|
| `mutate [0..1]` | small evolutionary nudges — flip a trigger, slide a note, shuffle velocities. Capped at 85% density so spamming drifts instead of congealing |
| `density <0..1>` | clear and refill to a target density, downbeat-weighted; melodic content of the steps survives |
| `markov` | learns trigger/note transitions from your *other* tracks and generates something that fits the song |
| `cantor` / `thuemorse` / `fibonacci` / `sierpinski` | deterministic fractal rhythm masks (notes preserved) |

`F` re-runs the last fill with a fresh seed — the spam-until-it-grooves
gesture. `.` repeats it with the *same* seed, exactly. All fills are
plain undoable edits.

## The command bar

`:` is more than a prompt. Typing hints commands (`:sc` shows `scale`),
`Tab` completes — longest common prefix first, then cycling — and
**value positions get a menu**: `:scale ` lists all 139 scales,
`:fill ` its seven generators, `:set cycle ` the eight playhead modes,
`:e ` your patches.

The arrow keys are the explorer: `↓` enters the menu and **auditions**
the highlighted value live — scales retune the playing track as you
scroll, fills regenerate the pattern (same seed while you browse), cycle
modes flip. `Enter` keeps what you hear (one undo entry), `Esc` or more
typing reverts it exactly. Typing, `Tab`, and `Enter` never audition —
if you know what you want, nothing plays under you.

`↑` with no menu row selected recalls command history.

## Macros — sequencing the sequencer

vi's macro surface, pointed at performance:

- `qa` … `q` records macro **a**. **Everything you can undo records** —
  step toggles, value sweeps (a held `k` records once, as its final
  value), euclid changes, mutes, pattern switches, cycle modes, scales,
  fills, bpm. Recording captures *absolute results*, not keystrokes, so
  replays are exact. The modeline counter ticks on every captured
  command: `rec @a [4]`.
- **The one thing to understand:** recording performs your actions live.
  After `qa m q`, track 1 IS muted — so firing `@a` right away changes
  nothing, and the modeline says so: `@a: no change`. The idiom: record
  the gesture, hit `u` a few times to rewind its live effects, keep the
  macro for later. (And don't record a toggle twice — `mute, unmute`
  nets to nothing forever.)
- `@a` fires it, **quantized**: the modeline shows `…@a` until the
  boundary lands, then flashes `@a`. `@@` refires the last one. Fired
  macros are one undo entry — `u` takes the whole gesture back — and a
  macro fired while you're recording another does NOT leak into it.
- Each macro has a quantize: `now`, `beat`, `bar` (default), or `end`
  (the first affected track's pattern boundary).
- When the transport is stopped, macros fire immediately.
- Not recorded, on purpose: making/deleting/pasting tracks, lane edits,
  slot save-as. Macros perform on the music that exists.

`:macro` lists what's defined. `:macro a` shows a macro as text.
`:macro a = pat 2 b | mute 3 | xpose 1 +7 | quant beat` writes one by
hand — tracks are 1-based, `|` separates commands:

```
pat <t> <a-h>      switch track t to a pattern slot
mute <t> / unmute <t>
cycle <t> <mode>   set a cycle mode
xpose <t> <±n>     transpose the whole track
rot <t> <±n>       rotate the pattern
scale <t> <name>   retune (library names; "off" = chromatic)
fill <t> <kind> [arg]
bpm <n>
step <t> <n> on|off
euclid <t> <pulses> <length> <rotation>
mode <t> note|mod
quant now|beat|bar|end
```

Recorded step edits display as `edit 2 @5 ×3` (absolute snapshots) and
can only come from `q` — they're not hand-writable.

### The macro lane

The strip above the tracks is a sequence of macro firings — one slot per
bar. `k` from track 1 reaches it; it behaves like a tiny track:

- `@a` assigns macro **a** to the slot under the cursor
- `x`/`d` cut a slot, `y`/`p` yank/paste, and counts tile: yank a slot,
  `4p` repeats it four bars — **the grammar is the repeat-count UI**
- `D` wipes the lane, `#L` sets its length in bars (up to 128)
- `j` goes back down to the tracks; transport and undo work from up there

Macros are the atoms, the lane plays them, pattern slots are what they
act on. Eight slots per track × 26 macros × a 128-bar lane composes into
full arrangements — and because the lane wraps, it's a *loop* of
arrangements. Songs fall out; the word never enters the UI.

## Mute

`m` silences a track completely: note gates close (mid-note notes are
released, no hanging drones), and the track's modbus channel is pinned to
0 — a muted modulation track stops modulating instead of freezing its
last value. The mixer owns per-channel solo; the sequencer deliberately
doesn't duplicate it.

## Paste grammar

- `p`/`P` paste the register **into** the current track at the cursor —
  steps overwrite on the fixed grid; a yanked *track* contributes its
  pattern's steps the same way.
- `gp`/`gP` materialize the register as a **new track**. A 3-step yank
  `gp`'d becomes a 3-step track — instant polymeter.
- Counts tile (`3p`) or multiply tracks (`2gp`).

## Everything is undoable

Steps, ranges, tracks, Euclidean rewrites, scale changes (full pitch
conversion included), cycle modes, slot switches, lane edits, fills,
fired macros, multi-track fan-outs (one entry each), BPM. 100 entries,
sweep-coalescing so a held `k` reverts with one `u`. Navigation, layer
choice, marks, and recording state are view state and stay out of
history. Patches save as readable TOML — macros and the lane included —
and **every pre-v2 save file loads unchanged** (new fields default:
probability 100, forward cycle, chromatic, slot a, empty lane).

## What's deliberately not here

- **A separate macro-sequencer pane** — the lane ships the same power
  in-pane; the engine doesn't know about the UI, so a dedicated pane can
  come later without rework.
- **Keystroke-replay macros** — semantic commands survive keymap changes
  and quantize safely; replayed keystrokes can land mid-edit and corrupt.
- **Solo** — lives in the mixer, on purpose.
- **Ratchets / micro-timing / swing** — next pass; the step model has
  room for param-lock-style extensions.
