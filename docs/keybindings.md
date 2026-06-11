# los — Keybindings & Editing Grammar

The canonical reference for every binding, the vi grammar behind them, and
the vocabulary used across docs and help overlays.

Status markers: **✅ today** · **🔜 v1** (see `docs/plans/v1-polish.md`) ·
**🔮 post-v1**

## Doctrine (applies to every module)

| Rule | Meaning |
|------|---------|
| Axis rule | Navigate along the module's visual layout axis; adjust on the perpendicular. Vertical param list → j/k select, h/l adjust. Horizontal strip (sequencer steps) → h/l select, j/k adjust. 2D grids (mixer console) → h/l and j/k both navigate; a dedicated adjust key (`-`/`=`) turns the knob. |
| Counts | A number prefix repeats any nav/adjust key: `5l`, `3j`, `10u`. ✅ |
| Coarse | Shift-variant of an adjust key = ~10× step: `L` vs `l` (mixer: `_`/`+`). ✅ |
| `?` | Help overlay. ✅ |
| `Ctrl-s` | Save module state. ✅ |
| `Space` | Global transport play/pause (except sequencer insert mode); also `Ctrl-b p`/`Ctrl-b s` and `los ctl`. ✅ |
| `u` / `Ctrl-r` | Undo / redo, count-prefixed; value sweeps coalesce into one entry. ✅ |
| `@` | Open the source picker on a bindable param (live sources from the manifest; Enter binds, x unbinds). **Sequencer exception:** `@` fires macros there; its picker key is `B` (per-step bindings). ✅ |
| `:` | Ex command line (`:w :e :q :q! :x :set`); not in conductor (session save/load lives there). ✅ |
| `gg` / `G` | First / last of the module's primary collection (sequencer tracks, envelope/mixer channels, voice/scope params, conductor states). ✅ |

## Vocabulary

- **step** — one cell of a track's step grid (default 16 slots, up to 128;
  long tracks scroll, `‹ ›` mark hidden steps). The "character" of the grammar.
- **track** — one row of steps with its own length/pulses/rotation/mode. The "line".
- **word** — a maximal run of consecutive **active** steps. Gaps (runs of
  inactive steps) separate words.
- **operator** — a verb awaiting a motion: `y` yank, `d` delete (clear),
  `c` change (clear + enter insert mode at the range start). Doubling the
  operator applies it to the whole track: `yy`, `dd`, `cc`.
- **motion** — a cursor movement that can also give an operator its range:
  `h l w b e 0 $ t# f#` (steps), `j k gg G` (tracks).
- **register** — the single clipboard. Holds either a step range
  ("charwise") or a whole track ("linewise"); paste does whatever fits what
  it holds. ✅

## Sequencer

Modes: **normal** (operators, track ops, transport), **insert** (direct step
entry/tuning), **visual** (`v`, steps) / **visual-line** (`V`, tracks —
`j`/`k` extend the span), **operator-pending**, **ex**. All ✅.
The full feature tour lives in [sequencer.md](sequencer.md).

### Motions (normal, insert, visual, after operators)

| Key | Motion | Status |
|-----|--------|--------|
| `h` / `l` | step left / right (counts) | ✅ |
| `w` | start of next word | ✅ |
| `b` | start of current/previous word | ✅ |
| `e` | end of current/next word | ✅ |
| `0` / `$` | first / last step | ✅ |
| `f#` | to step # (inclusive under an operator) | ✅ |
| `t#` | till step # (exclusive under an operator) | ✅ |
| `j` / `k` | next / previous track (normal mode; counts); `k` from track 1 reaches the **macro lane**, `j` leaves it | ✅ |
| `gg` / `G` | first / last track (normal); first step (insert) | ✅ |
| `gt#` | go to track # (normal mode) | ✅ |

### Operators (normal & visual modes) ✅

| Form | Action |
|------|--------|
| `y{motion}` | yank step range into register — `yw`, `ye`, `y$`, `yt8`, `yf8`, `y3l` |
| `d{motion}` | clear step range (deactivate), range into register |
| `c{motion}` | clear range, move to range start, enter insert mode |
| `yy` / `dd` / `cc` | whole track: yank / delete / clear+insert |
| `Y` / `D` / `C` | shorthand to end of pattern: `y$` / `d$` / `c$` |

### Register & paste

| Key | Action | Status |
|-----|--------|--------|
| `x` | cut current step into register (normal + insert) | ✅ |
| `y` | yank current step (insert mode; in normal mode `y` is the operator) | ✅ |
| `p` | paste register **into** the row(s) at the cursor — steps overwrite on the fixed grid; a multi-track yank **block-pastes** down successive rows | ✅ |
| `P` | paste before — the overwrite ends at the cursor | ✅ |
| `gp` / `gP` | materialize the register as a **new track** after / before (a 3-step yank becomes a 3-step polymeter track) | ✅ |
| `#p` / `#gp` | paste # times / insert # tracks (vi idiom) | ✅ |

> **The `#P` quirk:** counted `#P` / `#L` / `#R` set Euclidean
> pulses/length/rotation (los idiom) and do **not** mean "paste # times
> before". Bare `P` pastes. `:set pulses 5` is the canonical form once ex
> lands; `#P` is the fast idiom. Documented loudly on purpose.

### Steps (insert mode)

| Key | Action | Status |
|-----|--------|--------|
| `Enter` / `Space` | toggle step | ✅ |
| `~` | toggle step (normal mode; flips each step of a visual selection) | ✅ |
| `k` / `j` | active layer value ± fine — note ±1 semitone/degree, velocity ±4, prob ±5, mod ±0.01 | ✅ |
| `K` / `J` | active layer value ± coarse — note ±octave/period, velocity ±16, prob ±25, mod ±0.1 | ✅ |
| `N<num>` | set the active layer's value directly (note 0–127, vel 1–127, prob 0–100, mod −1…1) | ✅ |
| `1`–`9`, `0` | **prob layer only:** set 10–90% / 100% directly (Orca-style) | ✅ |

### Tracks (normal mode)

| Key | Action | Status |
|-----|--------|--------|
| `o` / `O` | new track after / before current (`n` = alias of `o`) | ✅ |
| `dd` / `yy` / `P`/`p` | delete / yank / paste track (unified register) | ✅ |
| `m` | toggle mute (kills gates AND the track's mod output) | ✅ |
| `M` | toggle track mode (note ↔ modulation) — `@` moved to macros | ✅ |
| `X` / `gX` | mark track / clear all marks — marked tracks (`t3*`) receive every fanned-out edit | ✅ |
| `"a`–`"h` | switch to pattern slot a–h (per track; swap-based, undoable) | ✅ |
| `"A`–`"H` | save the current pattern into a slot without switching | ✅ |
| `gc` / `gC` | next / previous cycle mode (forward reverse pingpong random drunk everyother spiral primejump) | ✅ |
| `>>` / `<<` | rotate the actual step pattern right / left (counts: `3>>`); preserves hand-edits, unlike Euclid `R` | ✅ |
| `#P` / `#L` / `#R` | Euclidean pulses / length / rotation | ✅ |
| `P`/`L`/`R` (insert, bare) | re-apply / clamp / rotate+1 | ✅ |

### Editing power ✅

| Key | Action |
|-----|--------|
| `.` | repeat last change at cursor (toggle, adjust, paste, fill, slot switch, …) |
| `v` | visual mode: motions extend a step selection; `y`/`d`/`c`/`~` act on it; `Esc` cancels |
| `V` | visual line over **tracks**: `j`/`k` extend; `d`/`x` delete the span, `c` clears it, `~` toggles every step, `m`/`M` mute/mode it, `y` yanks the current track. One undo entry per fan-out |
| `'n` `'v` `'p` `'m` | value layer: what the grid shows and what k/j/N edit (note / velocity / probability / mod) |
| `B` / `gB` | patch a mod source into the step's active-layer param (picker) / unplug it |
| `(` / `)` | dial the bound source's amount ±0.05 (counts; clamp ±2) |
| `F` | re-run the last `:fill` with a fresh seed (`.` repeats the same seed) |

### Macros & the lane ✅

| Key | Action |
|-----|--------|
| `q{a-z}` … `q` | record a macro: EVERYTHING undoable records as absolute state (step edits, sweeps merged, euclid, mute, slots, cycle, scale, fill, bpm); track lifecycle + lane edits don't |
| `@{a-z}` / `@@` | fire a macro (quantized per its `quant`: now/beat/bar/end; immediate when stopped) / refire the last |
| lane `@a` | assign macro a to the lane slot under the cursor |
| lane `x`/`d` `y` `p` `D` | cut / yank / paste (counts tile — `4p` = four bars) / wipe the lane |
| lane `#L` | lane length in bars (1–128) |

### Transport & misc

| Key | Action | Status |
|-----|--------|--------|
| `Space` | play/pause (normal mode; global transport flag) | ✅ |
| `s` | stop | ✅ |
| `:set bpm <n>` | set BPM (the old `t<num>` prompt is removed; `t` is reserved for the till motion) | ✅ |
| `u` / `Ctrl-r` / counts | undo / redo | ✅ |
| `?` | help | ✅ |

## Ex command line (all modules except conductor) ✅

| Command | Action |
|---------|--------|
| `:w` | save patch under current name (prompt if none) |
| `:w <name>` | save patch as `<name>` |
| `:e <name>` | load patch (tab-completes over `~/.config/los/patches/`) |
| `:q` | quit module (refuses if unsaved changes) |
| `:q!` | quit, discard changes |
| `:x` / `:wq` | save patch and quit |
| `:set <key> <value>` | module settings: sequencer `bpm`, `pulses`, `length`, `rotation`, `cycle <mode>`, `root <note>`; others as they grow |
| ex line extras | command/value completion menus (`Tab` cycles, `↓`/`↑` browse + AUDITION in the sequencer), `↑` history when no menu row is selected — all modules |
| `:scale <name>` | retune track(s): 139 built-ins, `off` = chromatic, `root <note>`, or a `.scl` file path (Scala import) — sequencer |
| `:fill <kind> [arg]` | auto-fill: `mutate density markov cantor thuemorse fibonacci sierpinski` — sequencer |
| `:macro [a] [= …]` | list / show / write macros (`pat 2 b \| mute 3 \| quant beat`) — sequencer |

Requires a per-module dirty flag (changed since last save) for `:q` vs `:q!`.

## Other modules (current → v1)

### Voice
Rows: Shape, Sub, FM, Output, Amp, Notes, LPG (0 = plain VCA, 1 = vactrol
low-pass gate: the amp envelope closes a tracking filter as it closes the
gate — amplitude and brightness fall together).
`j/k` select param · `h/l` adjust · `H/L` coarse · counts · `gg/G` (✅) ·
`1/2/3` output shortcuts removed — digits are counts ·
`@` source picker on bindable rows · new rows: `notes` (which seq track's
notes to play; unbound = all) and `amp` (amplitude source; unbound = 1.0 —
a drone by choice; bound but offline = silent, with a `✗ offline` marker
on the row) · voice `i` defaults to playing seq track `2i+1` through maths
channel `2i+1` (voice 0: t1/ch1, voice 1: t3/ch3 — even tracks/channels
stay free for patching) · `:` · undo (all ✅)

### Envelope / Maths
Maths-style panel: one column per channel + a logic column (SUM/OR/AND/INV,
EOR/EOC, live meters). Rows: Rise, Fall (0 = instant … 25min), Shape (log↔lin↔exp, τ±9),
Atten, Offset, Plck (vactrol snap+ring decay), Sig (slew input), Trig.
`j/k` select row · `h/l` adjust · `H/L` coarse · `[`/`]` channel (counts) ·
`gg/G` first/last channel · `a`/`x` add/remove channel (up to 6) ·
`t` manual trigger · `c` cycle · `m` trig/gate per channel (trig = full
rise→fall per note, note-off ignored — the default; gate = sustain until
note off; flipping a sustaining channel to trig releases it) · `o` manual
gate · `@` bind row (Trig row offers
— any note — / — off — / sources; a non-note source triggers on its rising
edge and, in gate mode, releases on its falling edge —
e.g. `envelope/0/eoc` for self-patching) · `:set rise 0|100ms|2s|1.5m|0.42`
(also fall/shape/atten/offset/pluck, `mode trig|gate`) · `:` · undo (all ✅)

### Mixer
The console: one vertical strip per audio source plus MASTER, signal
order top to bottom — drive · hi · mid · freq · lo · pan · fader
(master swaps pan for **width**). Per-channel chain is
drive → EQ → pan → fader; the meter lives inside the fader. All ✅:

| Key | Action |
|-----|--------|
| `h` / `l` | select strip (channels, then MASTER; counts, wraps) |
| `j` / `k` | select param within the strip (counts, wraps) |
| `-` / `=` | adjust the selected param (counts) |
| `_` / `+` (or `H`/`L`) | coarse adjust |
| `0` | reset the selected param to its default |
| `@` | bind a mod source to the selected param · `x` unbinds |
| `m` / `s` | mute / solo the selected strip |
| `gg` / `G` | first strip / MASTER |
| mouse | wheel turns the selected knob · click selects a strip |

Every strip param — drive, EQ gains, mid freq, pan, fader, master
width — is mod-bindable; bound values render in the source's cable
color with a `▸`, and a ghost tick on the fader shows the live
modulated level. Short panes collapse to dense rows plus a one-line
detail of the selected strip. `:` and undo as everywhere.

### Scope
Rebuilt as a vertical param list (✅): `j/k` select (mode, source, channel,
modbus ch, zoom, gain, trigger) · `h/l` adjust · `H/L` coarse · counts ·
`gg/G` · the old `g/G t/T n/N m c b +/-` keys are gone. `@` on the source /
modbus rows opens the picker; the channel row shows the live source label
(`envelope/0/sum`). The param strip auto-hides ~4s after the last
interaction — the scope is the picture. `:` ✅.

### Conductor
Two views, `Tab` switches. **States**: `j/k` nav (counts) · `gg/G` ·
`Enter` load (`l` alias) · `s` save session · `dd` + y/n confirm to delete.
**Modules** (manifest-driven, shows each module's claimed outputs — the
routing overview): `j/k`/`gg/G` nav · `a` add module (type picker; instance
auto-numbered) · `x` + y/n remove (saves state first; mixer/conductor are
protected). Also `los add <module> [instance]` from any shell. All ✅

## Mouse ✅

Mouse input is on session-wide (`tmux mouse on`) and follows one dialect in
every module — the pointer is a shortcut for the keyboard grammar, never a
separate feature:

| Gesture | Action |
|---------|--------|
| Wheel | Adjust the row/strip under use (same step as `h`/`l`; sweeps coalesce into one undo entry) |
| Click | Select the row, strip, channel, or step under the pointer |
| Drag | Slide a value bar continuously (voice/maths/mixer sliders; bipolar rows map around center) |

Per module: **sequencer** wheel = step nav, click = select step in the
visible window (identical geometry to the renderer, long tracks included) ·
**maths** click on the overview line selects a channel, click on detail rows
selects params, Trig/Sig rows open on click · **mixer** click/drag on a
strip's bar sets level · **scope** wheel adjusts and wakes the param strip ·
**conductor/badge** keyboard-first, no mouse surface.

Everything the mouse does is undoable exactly like its keyboard twin.

## Color (the law, briefly)

Defined in `docs/plans/design-language.md` §2.5. The short version:

- **Cable wins.** A bound param's slider takes the *connection's* color —
  the same hue shown at the source's output meter (channel-slot palette,
  12 muted hues). Unbound sliders wear the module's page hue.
- **Pitch** uses a 12-class muted wheel (terracotta → plum, brightness rises
  with octave) — on note steps, note names, and velocity in the sequencer.
- **Modulation steps** use a teal intensity ramp (CV hue).
- Signal types keep fixed hues everywhere: NOTE orange, CV teal, AUDIO
  green, CLOCK violet.

## Future (🔮 post-v1, documented so the grammar reserves space)

- **Sequencer depth:** ratcheting (substep repeats), per-track clock
  division, swing. (Probability, cycle modes, scales, macros: shipped ✅.)
- `;` / `,` — repeat last `f`/`t` motion forward / back.
- `J` — join: merge next track's pattern into current (OR steps).
- `m{a-z}` / `` `{a-z} `` — cursor marks (track marks `X` shipped; these
  would be position marks).
- `/` — search (next step with note N / velocity above X).
- `r` — replace-one (set note without leaving normal mode); evaluate against
  `N` overlap.
- A dedicated macro-sequencer pane (the lane's engine is UI-independent).
