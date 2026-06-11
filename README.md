<p align="center">
  <img src="docs/logo.svg" width="190" alt="Los">
</p>

**A modular groovebox that lives in your terminal.** Every module is its
own process in its own tmux pane, wired together over shared memory. Edit
patterns with vi grammar. Patch modulation like a Eurorack. No DAW, no
plugins, no mouse required (but the mouse works too).

![the console playing one bar of the house drone, seamlessly looping](docs/demo.gif)

**[🔊 the house drone with sound — and a tour of the fx rack →](docs/demo.mp4)**

- **vi grammar, on sequences** — `3x`, `yw`, `ct4`, visual mode, registers,
  `.` repeat, `u` undo. Everywhere. Macros too: `qa…q` records a
  performance gesture, `@a` fires it on the next bar.
- **Weird on purpose** — per-step probability, eight playhead cycle modes
  (pingpong, drunk, prime-jump…), generative fills, and a 139-scale
  microtonal library with Scala `.scl` import.
- **Cable-colored patching** — bind any parameter to any output with `@`;
  the connection wears one color at both ends.
- **MATHs** — a [Make Noise Maths](https://www.makenoisemusic.com/synthesizers/maths)
  homage: 0.5 ms–25 min (or zero) times, vari-response curves, cycling,
  slew, EOR/EOC self-patching, vactrol plucks into a low-pass gate.
- **Processes, not threads** — modules talk over POSIX shared memory; tmux
  is the window manager; kill a pane and the session heals.

## Quick start

```sh
cargo install --path .

los new    # fresh session, pre-patched melody + bass — Space is the noise button
los        # resume your most recent save
```

`Ctrl-b` + arrows moves between panes. One dialect everywhere: `hjkl`
moves and adjusts, counts and `Shift` go coarse, `@` patches, `u`/`Ctrl-r`
undoes, `:` for ex commands, `?` for help. Wheel, click, drag work too.

First five minutes: [docs/tour.md](docs/tour.md) · every key:
[docs/keybindings.md](docs/keybindings.md) · the sequencer, in depth:
[docs/sequencer.md](docs/sequencer.md)

## The rig

| Module | What it does |
|--------|--------------|
| **sequencer** | 8 tracks × up to 128 steps × 8 pattern slots, Euclidean rhythms, probability, cycle modes, scales (microtonal, `.scl` import), per-step mod cables, macros + a macro lane — [the full tour](docs/sequencer.md) |
| **voice** (`sto`) | STO-style osc: waveshaping, sub, FM, vactrol-ish low-pass gate |
| **envelope** (`maths`) | 6 function generators: trig/gate/cycle, slew, pluck, SUM/OR/INV, EOR/EOC, audio-rate out |
| **mixer** | Auto-discovers sources, per-track meters, clip warning, tape out (`los record`) |
| **scope** | Braille/half-block/bars/dots renderers, level trigger, taps any signal |
| **conductor** | Save/load, add/remove modules at runtime, routing overview |
| **badge** | The faceplate. Breathes with the beat, sleeps when you stop |

## Under the hood

```
sequencer ──events──▶ ┌──────────────┐ ◀──events── (any consumer)
voice ──audio ring──▶ │  POSIX SHM    │ ◀──audio ring── envelope
envelope ──modbus──▶  │  manifest     │ ──modbus──▶ voice, scope, …
                      │  transport    │
                      └──────────────┘
                             ▲
            mixer scans the manifest, mixes every ring → speakers
```

Bindings are stable names (`envelope/0/eoc`), not channel numbers — patches
survive restarts, dead modules get reaped, the session never wedges. Saves
are TOML. Protocol + module contract: [DESIGN.md](DESIGN.md) · visual
language: [docs/plans/design-language.md](docs/plans/design-language.md)

```
src/
  main.rs     CLI entry — dispatch, ctl, ps, record
  modules/    the runnable panes: sequencer, voice, mixer, envelope,
              scope, tone, badge, conductor — and template, the
              commented worked example for writing your own
  ui/         shared TUI kit: theme, vi keys, : command line, @ picker, undo
  ipc/        POSIX shared memory (manifest, rings, modbus, transport)
              and modulation routing
  session/    save/load state, house layout, tmux wrapper
```

Fresh sessions open already playing **the house drone** — a slow,
evolving A-minor piece (74 BPM) that runs itself: the macro lane walks
pattern slots a–d through a 16-bar form, probability and ratchets keep
it alive, a drunk modulation track strolls the filterbank's window,
and the ping-pong bass runs 12 steps against 16. It's a worked example
of the sequencer's depth as much as a patch.

![the fx rack: the delay's tap ladders answering the pattern, the filterbank strolling](docs/demo-fx.gif)

The fourth window is a **tape deck** (Tascam 4-track × OP-1): six
tracks recording the mix or any single source, varispeed, loop
overdubs, reverse, fader automation lanes, bounce, and export to
`~/Music/los/` — fresh sessions arrive with track 1 armed over the
drone's 16-bar form, so `r` is a song. An optional `tools/los-rave`
helper reprocesses takes through RAVE neural models (the `vintage`
model is a credible cassette).

Fx modules (the **delay**, after the Buchla 288, and the
**filterbank**, after the 296e) consume any audio source you point them
at — a voice directly (insert: that strip leaves the console), or one
of the mixer's two **send buses** (`sa`/`sb` rows on every strip; the
fx module's own strip is the return). Fresh sessions open with the fx
rack pre-cabled on a second tmux window: sends feeding the delay and
the filterbank, a MATHs LFO breathing the bank's spectrum, and a second
sequencer whose tracks can step fx params without touching the voices.
Their DSP is part Rust, part Faust ([docs/writing-dsp.md](docs/writing-dsp.md)).

Adding a module touches `src/modules/` plus a few registration points —
[docs/writing-a-module.md](docs/writing-a-module.md) is the guide and
`src/modules/template.rs` the worked example ([CONTRIBUTING.md](CONTRIBUTING.md)
has the house rules).

## Status

v1: vi grammar, dynamic routing, the Maths build-out, undo everywhere,
module lifecycle, mouse, the design pass. Next: more voices, FX,
orca-ish sequencer tricks — [roadmap](docs/plans/roadmap.md).

Rust, [ratatui](https://ratatui.rs) + crossterm + cpal. macOS today
(POSIX SHM + tmux; Linux should be close).

## Hacking

`just check` — clippy (`-D warnings`) + tests. `just demo` — re-records
the GIF/mp4 above ([vhs](https://github.com/charmbracelet/vhs) + ffmpeg);
`just demo-state NAME` records from your own saved session instead.
`los record 16 take.wav` bounces the master mix of any running session.

