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

**Loading a MIDI file:** `:set midi path/to/file.mid` in the sequencer imports a Standard MIDI File — note-ons quantized to sixteenth-note steps, one sequencer track per MIDI channel, the file's tempo applied as the BPM. A quick way to get a riff or a drum pattern in from a DAW.
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

**rings** (the Mutable Instruments resonator, full port): `mode`
resonator | synth (the "Disastrous Peace" string synth);
resonator `model` ∈ modal, sympathetic, string, fm, chords,
string+verb; `poly` 1–4 (odd counts ping-pong the voices);
`structure` (inharmonicity / string tuning / fm ratio), `brightness`,
`damping`, `position`, `chord` (the 11-chord table, used by the
quantized modes and the synth) — every value param has a `*_src`
twin. `exciter` = internal pluck/pulse on each note-on; patch
`input` (any audio source, `module/N`) to excite the resonator
externally instead — Rings as an fx is half the instrument. Synth
mode `fx` ∈ formant, chorus, reverb, formant2, ensemble, reverb2.
Notes strum; there is no gate — everything decays by `damping`.

**tides** (the Mutable Instruments tidal modulator, 2018, full
port): `mode` ∈ ad (one-shot envelope), loop (LFO/osc), ar (sustain);
`output` ∈ gates (slope + raw + EOA/EOR), amplitude (level scan
across four outs), phase (four phase-shifted copies), frequency
(four ratio'd ramps — chords); `range` control | audio (audio
tracks notes V/oct, freq becomes ±2 oct); `freq`, `shape`, `slope`,
`smooth` (< 50% filters, > 50% wavefolds), `shift` — all with `*_src`
twins. `sync = true` locks the loop to the transport beat. Sources:
`tides/N/o1`–`o4`; o1/o2 are also the audio pair.

**peaks** (the Mutable Instruments dual function generator, full
fixed-point port): two channels, `fn1`/`fn2` ∈ envelope, lfo,
tap_lfo, bass_drum, snare_drum, high_hat, fm_drum, pulse_shaper,
pulse_random, bouncing_ball, mini_seq, number_station; four knobs
per channel (`p1 = [a, b, c, d]`, relabelled per function — kick:
freq/punch/tone/decay; envelope: ADSR) with `p1a_src`…`p2d_src`
twins. Separate note tracks per channel (`notes1_src` = kick on t1,
`notes2_src` = snare on t2). Channel 1 is the left audio out,
channel 2 the right; both publish `peaks/N/ch1`/`ch2` on the bus, so
a channel running an envelope or tap LFO is a patchable modulator.
The 808s keep the firmware's integer arithmetic — that crunch is
the instrument.

**branches** (the Mutable Instruments Bernoulli gate, full port):
two channels; each consumes a note track (`notes1_src`/`notes2_src`)
and re-emits every note onto one of two outputs by a coin flip —
`p1`/`p2` set the bias (0% = all out a, 100% = all out b, the
firmware's dead zones at the ends), `toggle*` makes heads flip the
previous outcome, `latch*` holds gates until the next flip. Bind a
voice's notes to `branches/N/1a` (or 1b/2a/2b) to route
probabilistically; the same four outputs sit on the bus as gate
levels for trigger inputs (dld hold, lfo reset). Both p knobs take
cables.

**grids** (the Mutable Instruments topographic drum sequencer,
full port): transport-clocked (8 steps per beat, 32-step patterns).
`mode` drums | euclid; `x`/`y` walk the 5×5 map of beats (the 25
node tables are the firmware's, byte-exact), `chaos` perturbs per
pattern, `fill1`–`fill3` set kick/snare/hat density, `len1`–`len3`
the euclid lengths — every knob has a `*_src` twin (an LFO on `x`
is the classic move). Emits notes on `grids/N/bd·sd·hh` (bind a
peaks channel or any voice); triggers + accent publish on the bus.

**edges** (the Mutable Instruments quad chiptune generator, full
port): four channels, each with `shapes` ∈ square, triangle,
nes_tri, noise, nes_noise, nes_short, sine; `pw` (square: the
50/66/75/87/95% detents in the lower half, free sweep above; sine:
bitcrush hold), `xpose` (±24 st), `lvl` — all with per-channel
`*_src` twins (`pw1_src` … `lvl4_src`). One note track per channel
(`notes1_src`–`notes4_src`) = four-voice chip polyphony; channels
1+3 left, 2+4 right. The NES LFSRs and 8-bit quantization are the
firmware's, bit for bit.

**frames** (the Mutable Instruments keyframer, full port): the
`frame` knob scans stored keyframes across four bus outputs
(`frames/N/ch1`–`ch4`) — bind `frame_src` to an LFO or the
sequencer and a whole mixer scene morphs by itself. `keyframes` live
in the patch (`[[…keyframes]] t = 0.3, values = [a,b,c,d]`); in the
TUI `a` records the current `ch` values at the frame position, `d`
deletes the nearest. Per-channel `easing` ∈ step, linear, in4, out4,
sine, bounce and `response` (linear ↔ exponential, the 2164 law).
`mode = "polylfo"` repurposes the four outputs as coupled wavetable
LFOs (frame = rate over 13 octaves; `shape`, `spread`,
`shape_spread`, `coupling`). Every knob has a `*_src` twin.

**streams** (the Mutable Instruments dual dynamics gate, full
port): claims one audio input (ch1 = left, ch2 = right) and runs
each side through one of six processors — `fn` ∈ envelope, vactrol,
follower, compressor, filter, lorenz — applying the resulting VCA
gain in-line at the firmware's 31.25 kHz. The vactrol is the face:
smooth mode is the hysteresis low-pass-gate, `alt` (plucked) snaps
it open and rings it down through the Gompertz law. Excitation per
channel: an `excite*_src` cable beats a `notes*_src` gate beats the
`excite` knob (a kick track on `notes1_src` ducks-and-blooms the
left side like an LPG). Two knobs per channel relabel per function
(`p1`/`p2`: shape & →freq, thresh & ratio, rate & balance …), all
with `*_src` twins. Both sides publish gain and frequency CVs as
`streams/N/{g1,f1,g2,f2}` (g 0.5 = unity) — patch `f1` into a
wasp's `freq_src` to close the vactrol-filter loop the hardware
implies.

**stages** (the Mutable Instruments segment generator, full port):
six segments, each `type` ∈ ramp, step, hold, alt with a `loop`
flag and two knobs (`p`/`s`). The hardware's grouping is the whole
trick — a segment with a `gate{n}_src` binding (a note track) starts
a new group, and the group's configuration decides what it becomes:
ramps in a gated group are a **multi-stage envelope**, one looping
ramp alone is an **LFO** (shape morphs triangle→sine→saw via `s`),
hold-then-steps is the **sequencer** (seven directions via `s` of
the first step), a single step is **sample-and-hold**, a single hold
is a **pulse/delay**, a single alt is an **audio oscillator**. Every
knob has a `p{n}_src`/`s{n}_src` twin. CV-only — publishes
`stages/N/o1`–`o6` (the leader's envelope plus each slave segment's
gate); patch `o1` into a voice's `amp_src` for a hand-drawn
envelope, or into a `level_src` to shape a whole strip.

**marbles** (the Mutable Instruments random sampler, full port of
its random core): a stochastic generator clocked by the transport.
The **t-section** makes random rhythms — `t_model` ∈ bernoulli,
clusters, drums, independent, divider, three_states, markov; `t_bias`
biases which of t1/t3 fires (t2 is the steady master); `t_range`
multiplies the clock 0.25/1/4×. The **x-section** makes random
voltages — `x_spread` morphs from constant (all the same) through a
bell to bernoulli (extremes); `x_bias` centres them; `x_steps`
crossfades a smooth glide into hard quantization to `x_scale`;
`x_deja_vu` is the signature knob — 0 is fresh every clock, 1 locks
a loop of `x_length` (1–16). The real scipy Beta-distribution tables
shape the spread. t1/t2/t3 emit note events (bind them to a voice's
`notes_src` — three stochastic voices); marbles/N/{t1,t2,t3,x1,x2,x3,y}
publish on the bus (x's are pitched CV, y a slower companion). Every
continuous knob has a `*_src` twin.

**warps** (the Mutable Instruments meta-modulator, full port of the
cross-mod core): two audio inputs — `carrier` and `modulator` — and
one `algorithm` knob that sweeps a whole world of cross-modulation:
cross-fade → wavefold → analog ring-mod → digital ring-mod → XOR →
comparator → vocoder. `timbre` is each algorithm's parameter (fold
depth, ring index, comparator window). `drive1`/`drive2` overdrive
each input into the noise gate. `carrier = "sine"` (or triangle/saw/
pulse/noise) swaps the external carrier for an internal oscillator at
`note`, so warps self-oscillates as a synth voice. Patch a bass to
`modulator` and a pad to `carrier`, then ride the algorithm knob —
that's the classic warps move. The modulated signal comes out its
ring; `warps/N/aux` publishes the secondary output. Every knob has a
`*_src` twin. (Runs at session rate — no 6× oversampling — and the
vocoder is a 16-band simplification; the cross-mod algorithms are
exact.)

**braids** (the Mutable Instruments macro-oscillator, analog models):
a monophonic synth voice driven by a note track. `model` ∈ csaw,
morph, saw_square, sine_triangle, buzz, square/saw_sub,
square/saw_sync, triple saw/square/triangle/sine — each a different
oscillator topology (morph is the famous tri→saw→square→sine sweep
through a fold/fuzz stage; the sync and sub models are classic
analog tricks; the triples are three detuned voices). `timbre` and
`color` are the model's two parameters (they mean different things
per model — fold depth, pulse width, detune, sub level). `notes_src`
sets the pitch, `amp_src` (an envelope channel) shapes the level —
like the voice and elements modules. Every knob has a `*_src` twin.
Renders at 96 kHz, resampled to the session rate. (The digital
models — FM, physical models, wavetables, drums — arrive in later
releases.)

**clouds** (the Mutable Instruments granular processor, full port of
the granular core): a stereo FX that records its `input` into a
3-second buffer and granulates it into a cloud. `position` is where
in the buffer grains are drawn, `size` their length, `pitch`
transposes them (±2 octaves), `density` is the grain rate (centre is
silent; both directions thicken — probabilistic one way, regular the
other), `texture` morphs the grain window and adds diffusion past
75%, `dry_wet` blends, `spread` scatters grains across the stereo
field, `feedback` re-injects the cloud, and `reverb` is the space.
`freeze` holds the buffer (stops recording) so you can play a frozen
texture. Every knob has a `*_src` twin — an LFO on `position`, a
sequence on `pitch`, an envelope on `density`… Patch a pad, a drum
break, or another voice to the input and granulate it; publishes
`clouds/N/level`. (v1 is the granular playback mode; the stretch,
looping-delay and spectral modes are follow-ups.)

**plaits** (the Mutable Instruments macro-oscillator, the engine
bank): a monophonic synth voice driven by a note track. `engine`
selects the synthesis model — `noise` (two clocked-noise
sources through a multimode filter and two band-passes — wind,
percussion, texture) and `fm` (2-operator FM with feedback and a
sub-oscillator), and `virtual_analog` (two band-limited
variable-shape oscillators, the second detuned by harmonics, with a
hard-synced voice on the aux — the classic two-oscillator synth). `harmonics`, `timbre` and `morph` are the three
macro knobs, meaning something different per engine (the FM
ratio/index/feedback, the noise formants/clock/resonance).
`notes_src` sets the pitch and retriggers the engine on each note;
`amp_src` (an envelope channel) shapes the level — the voice/braids
wiring. Every knob has a `*_src` twin; the secondary output publishes
on `plaits/N/aux`. Engines render at 48 kHz, resampled to the
session. (The remaining ~18 engines — virtual-analog, waveshaping,
chord, wavetable, physical models, drums, speech — arrive in later
releases.)

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
| `tides/N/` | `o1`–`o4` (the four slopes) |
| `peaks/N/` | `ch1`, `ch2` (the two channel outputs) |
| `branches/N/` | `1a`, `1b`, `2a`, `2b` (gate levels; also note re-emit sources) |
| `grids/N/` | `bd`, `sd`, `hh` (triggers; also note sources), `acc` |
| `frames/N/` | `ch1`–`ch4` (keyframed levels / poly LFOs) |
| `streams/N/` | `g1`, `f1`, `g2`, `f2` (gain + frequency CV per side) |
| `stages/N/` | `o1`–`o6` (segment outputs: envelopes, LFOs, sequencer steps) |
| `marbles/N/` | `t1`–`t3` (random gates; also note sources), `x1`–`x3`, `y` (random CV) |
| `warps/N/` | `aux` (the secondary cross-mod output) |
| `braids/N/` | `level` (the output follower) |
| `clouds/N/` | `level` (the output follower) |
| `plaits/N/` | `aux` (the engine's secondary output) |
| `template/N/` | `lfo` |

Audio `input` fields are 2-segment: a producing module (`voice`,
`swarm`, `tone`, `template`, `delay`, `filterbank`, `streams`, `warps`, `clouds`) or a mixer virtual
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
