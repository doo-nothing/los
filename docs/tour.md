# A short tour

Why this exists, and your first five minutes.

## Why

Hardware grooveboxes are joyful because they're *instruments* — dense,
tactile, opinionated. DAWs are powerful because they're software. Los tries
to keep both: the immediacy of a knob-per-function panel, rendered in a
terminal, driven by the text-editing muscle memory you already have.

- **vi is the editing grammar.** `3x` deletes three steps. `yw` yanks a word
  of steps, `p` pastes it. `ct4` changes everything up to step 4. `u` undoes,
  `.` repeats. Visual mode, registers, counts, dot-repeat — the whole kit,
  applied to sequences instead of text.
- **Patching is receiver-side and color-coded.** Any parameter can be bound
  to any output (`@` opens the picker). Every connection gets a cable color,
  and the *same* color shows at both ends — slider and source. Glance at the
  rig and see the patch.
- **The envelope is a [Make Noise Maths](https://www.makenoisemusic.com/synthesizers/maths)
  homage.** Function generators with 0.5 ms – 25 min times (or literally 0),
  analog-shaped vari-response curves, cycling to audio rate, slew limiting,
  EOR/EOC gates for self-patching, and a vactrol-style pluck mode. The voice
  carries a low-pass gate so plucks *thump* instead of click.
- **Processes, not threads.** Each module is a separate OS process talking
  through POSIX shared memory. tmux is the window manager. Kill a pane,
  respawn it, the session heals.

## First five minutes

A fresh session (`los new`) comes pre-patched: track 1 carries a melody and
track 3 a bass line, each triggering a spikey vactrol-pluck envelope (MATHs
ch1/ch3) that gates its own voice. Tracks 2 and 4 are modulation tracks and
MATHs ch2/ch4 are unwired — patch points waiting for you.

1. Press `Space` — the rig opens paused; this is the noise button.
2. In the **sequencer**, hit `i` for insert mode and tap steps in with
   `Space`; `k`/`j` nudge pitch, `x` deletes, `u` undoes.
3. Try the grammar: `v` select a few steps, `y` yank, move, `p` paste.
4. In **MATHs**, stretch a fall slider with the mouse wheel and hear the
   plucks bloom, or flip a channel to cycle (`c`) and you've got an LFO.
5. Patch it: on any slider press `@`, pick `envelope/0/ch2`, and watch the
   slider take the cable's color and start breathing.
6. `:w mypatch` saves. `:q` from the conductor tears it all down. `los`
   brings it back.

## Transport, anywhere

| Key | Action |
|-----|--------|
| `Space` | Play/pause from any module pane |
| `Ctrl-b p` / `Ctrl-b s` | Play-pause / stop via tmux prefix |
| `los ctl play\|stop\|toggle\|status` | From any shell or script |

The prefix bindings are scoped to the `los` session — your other tmux
sessions keep stock behavior.

## Saving & state

`:w name` in any module saves a patch; the conductor's `s` saves the whole
session (params, pane layout, active pane) as TOML under
`~/.config/los/states/`. `los` resumes the most recent save, `los new`
starts fresh, `los load <file>` loads a specific one. `los ps` dumps the
live session — manifest entries, ring lag per consumer, clock — when you
want to see the machinery. `los record 16 take.wav` bounces the master mix.
