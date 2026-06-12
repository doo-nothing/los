# Composing by prompt

Los was built for hands: vi grammar on a live sequencer, tmux panes,
cables patched by keystroke. Then we asked a language model to make a
record with it, and found the opposite was also true — the same design
that serves deep live control turns out to be one of the most
promptable instruments imaginable. A complete song is one TOML file; an
agent that can write a file, run a command, and read numbers back can
compose, audition, and revise without ever touching a key.

This is the guide to that path. It is written for both readers at once:
a human deciding what to ask for, and an agent doing the asking. **If
you are an agent about to compose: read this whole document first** —
the schema section is the difference between a song and a validation
report.

## Why this works

**Everything is text.** The UI is a terminal, the state is TOML, the
control surface is a CLI. There is no plugin GUI to screenshot, no
binary project file to reverse-engineer. A language model's native
medium is the entire interface.

**The whole song is one declarative file.** Patterns, pattern slots,
macros, the bar-by-bar arrangement, tempo moves, every synthesis
parameter and every modulation cable — all of it lives in one
`SessionState` TOML (`src/session/state.rs` is the schema). The model
can hold the complete composition in view and edit any part of it
without simulating an interaction sequence.

**The engine is deterministic where it matters.** Playheads are pure
functions of the step counter; the macro lane fires the same macro at
the same bar every run. Two renders of the same file are the same song
(probability and humanize are the deliberate, bounded exceptions). That
makes iteration meaningful: change one thing, render, and what you
measure is the change.

**The macro lane is form-as-data.** Sections — pattern switches, mutes,
tempo moves — are macros (`a`–`z`), and the arrangement is literally an
array of letters, one per bar: `["a","","","b",…]`. Writing a song's
structure is writing a list. This is the single biggest reason weak
models can produce coherent long-form pieces here: form is not an
emergent property of many edits, it is a field.

**The guardrails are musical.** Scales snap notes into key (139 scales,
microtonal included), Euclidean fills are rhythmic by construction,
probability and ratchets add life that survives crude placement, swing
and grooves humanize a square grid. A model with mediocre note-level
taste still lands inside something that sounds intentional.

**You can't hear, but you can measure.** `los render` turns the file
into a WAV; `los audit` turns the WAV into numbers — an RMS arc per
bar, per-section dynamics, peak and crest. The ears in the loop are
numeric, which is exactly the kind of feedback a model uses well. The
first record made this way was finished through exactly this loop:
the audit said the quiet sections weren't quiet enough (floor only
2.3× under the peak), so the outro got a ghost pattern at velocity 34;
the audit said the "peak" didn't peak, so the bass got a denser riff
and hotter velocities. Measurable critique, targeted fix, render,
measure again.

## The loop

```sh
$EDITOR song.toml                          # write (start from examples/first-song.toml)
los check song.toml                        # every problem at once; fix until clean
los render song.toml take.wav              # spawn detached, record from bar 0, tear down
los audit take.wav --song song.toml        # the take, in numbers
# read the section table, change ONE thing, repeat
```

- `los check` reports **all** errors and warnings in one pass — typo'd
  keys (with did-you-mean), out-of-range values, bad cable addresses,
  lane slots firing macros that don't exist. `los load` and `los
  render` refuse anything check refuses, so a clean check means a
  loadable song.
- `los render` is **realtime and audible**: the engine is a fleet of
  live processes around a device-driven clock, so a 7-minute song takes
  7 minutes and plays through the default output while it records.
  Duration comes from the macro lane automatically, plus `--tail 3` for
  the delay and filterbank to ring out; `--secs N` overrides. It
  refuses to run if a `los` session is already open.
- `los audit --song` maps bars and sections to timestamps using the
  lane and the tempo moves, then reports mean/max RMS per section and
  the delta against the previous section — form legibility at a glance.
  Without `--song` it windows by `--window` seconds.
- One change per iteration. Render time is real time; spend it on a
  hypothesis, not a rewrite.

A fresh take of the annotated example looks like this:

```
sections
  sec  macro  bars     time              mean RMS   max RMS   Δ mean
    1  a      0–3      0.00–9.60s           -27.3     -26.7        —
    2  b      4–7      9.60–20.03s          -31.0     -30.8     -3.7
```

Section `b` sits 3.7 dB under section `a` — the sparse lift is actually
sparser. That's the composition, audited.

## Anatomy of a song file

Copy [examples/first-song.toml](../examples/first-song.toml) (minimal,
annotated) or [examples/house-drone.toml](../examples/house-drone.toml)
(the full ~7-minute house composition). The shape:

```toml
[meta]
name = "my-song"
format = 2                 # required: state format v2

[[windows]]                # windows group panes; layout is cosmetic
name = "modules"

[[windows.panes]]          # one pane = one module process
module = "sequencer"
instance = 0

[windows.panes.patch_inline]   # that module's params — the music
bpm = 96.0
# ...
```

A song needs at minimum: a **sequencer** (the patterns and the form), a
**voice** (sound), an **envelope** (the voice's amplitude lives on an
envelope channel), and a **mixer** — the mixer owns the audio device
and advances the clock; without one, nothing sounds and nothing moves.
A **scope** is free eyes. Add **delay**/**filterbank** for the fx rack
and a second **sequencer** to step fx parameters.

### Sequencer — patterns and form

| field | range | musical meaning |
|---|---|---|
| `bpm` | 20–300 | tempo; macros move it per-section |
| `playing` | bool | ships true in the house file; `los render` forces its own start |
| `lane` | `["a","",…]` | one slot per bar: `""` = keep going, letter = fire that macro |
| `lane_len` | 1–128 | bars before the form wraps |
| `tracks` | ≤ 8 | each track is a voice's pattern or a CV lane |

Per track (`[[…tracks]]`):

| field | range | musical meaning |
|---|---|---|
| `length` | 1–128 | steps per cycle — **lengths that differ across tracks are polymeter** (the house bass runs 12 against 16) |
| `mode` | `"note"` / `"modulation"` | notes fire a voice; modulation writes `mod_value` CV to the modbus (sample-and-hold) |
| `cycle` | forward, reverse, pingpong, random, drunk, everyother, spiral, primejump | playhead direction; drunk + a CV track = a spectrum that strolls |
| `scale` / `root` | name / 0–127 | snap notes into key; `"minor"`, `"dorian"`, `"hirajoshi"`, `"messiaen 3"`… (139 names in `src/theory/scales.rs`) |
| `swing` | 50–75 | 50 straight, ~62 MPC feel, 75 maximal |
| `groove` | straight, lilt, drag3, push24, sway, limp, rushin, molasses | per-16th micro-timing templates |
| `humanize` | 0–30 ms | re-rolled timing jitter — tape wobble at 2–4 |
| `ratchet_decay` | −100–100 | ratchet velocity shape: + decays, − crescendos into the beat |
| `active_slot` / `slots` | 0–7 (`a`–`h`) | the active pattern lives inline in the track; other slots go in `[[…tracks.slots]]` with their own steps/length |

Per step (entries map by position; inactive steps are placeholders):

| field | range | musical meaning |
|---|---|---|
| `active` | bool | does it fire |
| `note` | 0–127 | MIDI; the scale snaps it |
| `velocity` | 1–127 | dynamics — **the** lever for arcs. 100+ peaks, 60–80 body, 30–50 ghosts |
| `prob` | 0–100 | % chance per cycle — 60–85 keeps a line alive without changing it |
| `repeats` / `repeat_prob` | 1–8 / 0–100 | ratchets, and the coin-flip on them |
| `delay` / `delay_unit` / `delay_prob` | ≥0, `"ms"`/`"pct"` | push a note late — a 45 ms late ornament reads as a hand |
| `mod_value` | −1–1 | the CV a modulation-track step emits |

**Held notes (the gate trick):** a note's gate closes at the next
step boundary, so a *held* tone is the same note repeated on
consecutive steps — the off/on flip lands inside one audio block and
nothing retriggers. A swarm pad that should bloom across a whole bar
is 15 held steps and one rest; the rest is what lets the swell (and
any gate-driven envelope) breathe before the next bloom.

**Entrances and exits are pattern slots.** A slot whose steps are all
inactive is a voice's silence; `switch_pattern` into a quiet slot is a
clean entrance, back out is a clean exit — no level automation needed.
For slow builds, keep two copies of a line in different slots (one at
velocity 45–60, one at 85–110) and let the form walk a voice from
soft to hot: entries that *grow* instead of arriving.

### Macros and the lane — the arrangement

Macros are the song's verbs; each is a *section* expressed as absolute
commands (never toggles, so firing one always lands the same state):

```toml
[[windows.panes.patch_inline.macros]]
id = "f"            # fired by "f" in the lane, or live with @f
cmds = [
  { switch_pattern = { track = 0, slot = 1 } },
  { switch_pattern = { track = 2, slot = 1 } },
  { set_mute = { track = 0, muted = false } },
  { set_cycle = { track = 2, mode = "drunk" } },
  { set_bpm = { bpm = 90.0 } },
]
```

The full verb set: `switch_pattern`, `set_mute`, `set_cycle`,
`transpose_track`, `rotate_track`, `set_scale`, `set_bpm`, `set_mode`,
`set_timing` (swing/groove/humanize/decay), `set_euclid`
(pulses/length/rotation), `set_steps` (absolute rewrite), `set_active`,
`fill` (mutate, density, markov, cantor, thuemorse, fibonacci,
sierpinski). Prefer `switch_pattern` over `transpose_track` in a
wrapping lane — relative verbs accumulate every pass.

**Put a macro at lane slot 0.** It makes the opening state explicit,
and `los render` starts the form at bar 0.

The house drone's 128-bar form, as a recipe (74 BPM home tempo):

```
bars   0–11  a  theme        both voices, ping-pong bass        74
bars  12–27  b  build        fuller arp, lifted                 78
bars  28–39  c  shimmer      sparse highs over reversed bass    74
bars  40–51  d  thin         root drones only                   70
bars  52–63  e  bass only    melody muted, bass walks           64
bars  64–71  a  theme        the return home                    74
bars  72–91  f  THE PEAK     dense riff, drunk bass, hot        90
bars  92–103 h  swell        shimmer with the bass back         70
bars 104–115 d  thin         coming down                        70
bars 116–127 g  outro        one ghost note, alone              58
```

Steal the shape: home → build → digress → strip → return → peak →
afterglow → floor. Tempo arcs sell sections more than note changes do.

### Voices

**voice** (STO-style): `shape`/`sub`/`fm`/`lpg`/`level`, all 0–1.
`lpg` is the vactrol low-pass gate — 0 plain VCA, 1 full pluck. Soft
melody: shape ≈ 0.35, lpg ≈ 0.55. Heavy bass: shape ≈ 0.1, sub ≈ 0.85,
lpg ≈ 0.35. Required cables: `amp_src` (an envelope channel) and
`notes_src` (a sequencer track).

**swarm** (CS-80-ish brass): one track becomes chords — seven detuned
saws through a swelling ladder. `chord` ∈ uni, oct, 5th, sus4, min,
maj, min7, maj7; knobs `detune`/`cutoff`/`res`/`swell`/`glide` 0–1; its
bloom is itself a source (`swarm/N/swl`). For *lush* rather than
chordal, `"uni"` and `"oct"` are detuned clouds — held-step roots
(see the gate trick above) with `swell` 0.7+ and `glide` 0.4+ is the
heavenly-pad recipe. The swell only re-blooms after a rest step; a
fixed `chord` spread is diatonic to nothing, so on mixed major/minor
progressions feed it roots and let `uni`/`oct`/`5th` stay neutral.

The raspy Cortini/Buchla texture that suits slow builds lives in the
**voice**, not the swarm: shape 0.5–0.62 (the rasp), lpg 0.5–0.7,
long envelope rises (0.4+) so stacked entries smear into each other.

**envelope** (MATHs, ≤6 channels): `rise`/`fall`/`shape` are 0–1 knobs
over a logarithmic 0.5 ms–25 min range; `loop_mode = true` turns a
channel into an LFO; `pluck` adds vactrol snap; `trigger_src` names the
track that fires it (`"off"` = never, absent = any note). Amp channels
that *speak*: fast rise (≈0.25), longer fall (0.6–0.8).

### FX and the console

**delay** (Buchla 288 homage): `time` 0.001–0.250 s per stage, `regen`,
`shim` (octave-up shimmer in the feedback), `wash` (reverb tail),
`taps` 1–8 with per-tap `level`/`pan`/`phase` (`"+"`, `"·"`, `"−"`).
**filterbank** (296e homage): 16 fixed bands, two stored spectra
(`bank_a`/`bank_b`) morphed by `morph`, a resonant window
(`wcent`/`wwidth`) you can stroll with a CV track, `spread` (per-band
time smear), `split`, `decay`.

Both are wet-only when `dry = 0` and consume an `input`: `"send/0"`
(send A), `"send/1"` (send B), `"mix/0"` (the print bus), or a source
directly (`"voice/1"` — that strip leaves the console). The mixer's
per-strip `send_a`/`send_b` (0–1) feed the buses; the fx module's own
strip is the return. The house wiring: voices → send A → delay; voices
→ send B → filterbank, whose morph breathes on a looping envelope
channel and whose window walks on a drunk CV track.

**dld** (4ms Dual Looping Delay homage): the *clean* delay — repeats
come back exactly as played, locked to the beat. Two channels (B eats
the same input as A); per channel `time` (beats 1–16) × `switch`
(`"/8"`, `"="`, `"+16"`) against the transport BPM (`ping_ms = 0`) or
a free ping in ms; `fdbk` to 1.1 (blooming dub echoes), `feed`
(record level), `mix`, `hold` (freeze into a loop), `win` (scroll the
held loop), `rev` (crossfaded). `hold_src`/`rev_src` take *trigger*
sources — bind a sequencer track and the loop toggles on the grid
(the hardware's Quantized Change Mode for free). Loop clocks publish
as `dld/N/clk·lpa·lpb`: bind an envelope's `trigger_src` to a loop
clock and VCA anything in time with the loop. For audible discrete
repeats, feed it transients (plucks, drums) — legato drones don't
echo, they smear.

**sampler** (reels + Morphagene-ish designer): eight slots loaded
from `~/.config/los/samples/` (prefetch with `los samples pull
"kick" --n 8`, or `--raw` for long found reels). Per slot: `sample`
(the cache path), `mode` ∈ oneshot, loop, gated, hold; `start`/`len`
(the splice), `pitch` ±24, `speed` ±2 (negative = reverse), `gene`
(0 = tape, >0 = grain size), `slide` (grain position — bind a drunk
track and the reel strolls), `atk`/`dec`, `level`. `kit = true` maps
notes C→slot a … G→slot h: ONE track plays a whole drum kit — give
steps note 60 for the kick, 61 for slot b, etc. The CV bank
(`pitch_src speed_src gene_src slide_src level_src`) overlays every
slot, Morphagene-style. Publishes `sampler/N/env`.

**lfo** (Batumi-style quad bank): the dedicated slow-curve source —
`mode` ∈ free, quad (90° bank), phase, div (/2…/16, integer-locked);
per channel `freq` (0–1 log over 209 s–50 Hz), `shape` (sine, tri,
saw, sqr, s&h), `phase`; `rst_src` re-zeros the bank on a trigger
(bind a track and the LFOs snap to the form). Sources:
`lfo/N/s1`–`s4` (sines) and `a1`–`a4` (the shaped assigns). Stop
borrowing MATHs loop channels for vibrato — bind `lfo/0/s2` instead.

**elements** (the Mutable Instruments modal voice, full port): a
physical-modelling instrument — `contour` picks the envelope family
(below 0.4 percussive, 0.4–0.6 adsr, above sustained swell); exciters
`bow` (friction, sustains), `blow` (breath through a reed tube),
`strike` (`strike_meta` walks samples → mallet → plectrum →
particles); the modal resonator's `geometry` (plate → string → bar),
`brightness`, `damping`, `position`; `space` is dry → wide → reverb
wash. Monophonic; velocity = strike strength (the accent law is the
hardware's). Every parameter has a `*_src` twin (`geometry_src`,
`strike_meta_src`, `level_src`, …) — bind anything; `amp_src` is a
VCA after the voice, `notes_src` picks the note track. Gamelan/metallic
territory: strike-only patches with geometry 0.55–0.75, damping
0.3–0.5, several instances tuned apart.

**mixer**: per-track `level` (0–1), `pan` (−1–1), `drive` (0–1), 3-band
EQ (±15 dB, `eq_freq` 0–1), `mute`/`solo`; a master strip with the
same console plus `master_width` (0–2).

## Modulation — the cables

**Every parameter of every module is modulatable** — that's the core
contract of the instrument. Each value field has a `*_src` twin that
binds it to a source address (`module/instance/output`); a plugged
cable replaces the knob. Highlights beyond the obvious: the
sequencer's tempo (`bpm_src`, 0–1 → 20–300), the DLD's free-mode base
(`ping_src`, 0–1 → 0–2 s — tape warble), the sampler's whole
Morphagene bank (`start_src`/`len_src`/`gene_src`/… applied to
whichever slot is playing), and the mixer's strips including the
master console (`master_src`, `master_width_src`, `master_*_src`).

Sources:

| module | outputs |
|---|---|
| `sequencer/N/` | `t1`–`t8` — notes (note tracks) or CV (modulation tracks) |
| `envelope/N/` | `ch1`–`ch6`, `sum`, `or`, `and`, `inv`, `eor`, `eoc` |
| `delay/N/` | `in`, `t1`–`t8` (envelope followers on the taps) |
| `filterbank/N/` | `b1`–`b16` (per-band followers) |
| `swarm/N/` | `swl` (the ladder's swell — the bloom as a source) |
| `lfo/N/` | `s1`–`s4` (sines), `a1`–`a4` (assigns) |
| `wasp/N/` | `env` (envelope follower) |
| `dpo/N/` | `lfo` |
| `elements/N/` | `exc` (exciter level follower) |
| `template/N/` | `lfo` |

Audio `input` fields are 2-segment: a producing module (`voice`,
`swarm`, `tone`, `template`, `delay`, `filterbank`) or a mixer virtual
(`send/0`, `send/1`, `mix/0`). Addresses are names, not channel
numbers — they survive restarts, and a binding to a module not in the
file is legal-but-dead until `los add` brings it up (check warns).

Self-patching is encouraged: `eor`/`eoc` retrigger envelopes, a delay
tap's follower can duck the dry, a filterbank band can drive a pan.

## Dynamics — what the first record taught

These came out of audit loops on a real take; they generalize:

- **Valleys must be deterministic.** Probability thins a section but
  won't reliably hush it — the quiet bars that matter should be a
  *different pattern* (mutes + a ghost slot), not a dice roll. The
  outro that finally worked was one note at velocity 34.
- **A peak is three things at once**: a denser pattern AND hotter
  velocities (up to ~120) AND a tempo lift. Any one alone reads as a
  variation, not a peak. Give the bass its own peak riff.
- **Aim the arc in numbers**: a floor-to-ceiling spread of ~8–12 dB
  mean-RMS across sections reads as dynamic; under ~4 dB reads flat.
  Ask the audit, not your hopes.
- **Don't leave random modulation plugged into the mix.** A
  free-running LFO on a send or a wash fills exactly the silences you
  composed (this shipped once — the cable came out). The composition
  owns the dynamics; cables serve sections.
- **Ghosts and pushes are the humanity budget**: low-velocity
  maybe-notes (prob 45–70) and late ornaments (delay 40–60 ms,
  delay_prob 80) do more than any amount of extra notes.

## Prompting tips (for the human driving)

- **Specify form in bars and BPM**, not vibes: "128 bars; intro 12,
  build 16, …; tempo 74 home, 90 at the peak, 58 out" gives the model
  a lane to fill. Reference points ("à la Cortini") work *on top* of
  that, not instead of it.
- **Demand the loop**: "after every change, `los check`, then render
  and audit, and show me the section table." A model that must produce
  the table catches its own flat arcs.
- **Make targets measurable**: "the peak's mean RMS ≥ 6 dB over the
  thin section", "the outro floor at least 10 dB under the ceiling".
  Vibes in, vibes out; numbers in, revisions out.
- **One change per render**, and keep takes (`take-01.wav`,
  `take-02.wav`) so regressions are audible and diffable.
- **Tell the agent where the truth lives**: this file, the examples,
  `src/session/state.rs` for the schema, `los check`'s messages. An
  agent that reads first writes songs, not error reports.
- For style, constrain the *materials*: a scale, a register per track,
  a tempo arc, which fx bus breathes. Leaving materials open invites
  mush; constraining them invites taste.

## Limits, honestly

- Rendering is realtime and audible — a 7-minute song costs 7 minutes
  of speaker time per audition. Compose in short forms, lengthen late.
- The audit hears loudness, not harmony: a wrong-but-in-scale note
  sails through. The scale guardrail is doing that work; trust it or
  listen.
- `prob`, `humanize`, and `random`/`drunk` cycles make takes differ in
  detail (never in form). For a fully repeatable take, set prob = 100,
  humanize = 0, and deterministic cycle modes.
- Up to ~500 ms of pre-roll precedes bar 0 (the mixer's arm polling);
  `los audit --song` notes the length mismatch.

There is also the other path — the one this project was built around:
driving the live TUI through tmux send-keys, vi grammar and all. It
works (a record was made that way), but it is frontier-model territory:
modal state, blind keystrokes, compounding errors. The file path above
is the one that scales down. Drive the rig live with your hands; let
the file be the score.
