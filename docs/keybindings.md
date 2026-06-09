# los — Keybindings & Editing Grammar

The canonical reference for every binding, the vi grammar behind them, and
the vocabulary used across docs and help overlays.

Status markers: **✅ today** · **🔜 v1** (see `docs/plans/v1-polish.md`) ·
**🔮 post-v1**

## Doctrine (applies to every module)

| Rule | Meaning |
|------|---------|
| Axis rule | Navigate along the module's visual layout axis; adjust on the perpendicular. Vertical param list → j/k select, h/l adjust. Horizontal strip (mixer channels, sequencer steps) → h/l select, j/k adjust. |
| Counts | A number prefix repeats any nav/adjust key: `5l`, `3j`, `10u`. ✅ |
| Coarse | Shift-variant of an adjust key = ~10× step: `L` vs `l` (mixer: `J`/`K`). ✅ |
| `?` | Help overlay. ✅ |
| `Ctrl-s` | Save module state. ✅ |
| `Space` | Global transport play/pause (except sequencer insert mode); also `Ctrl-b p`/`Ctrl-b s` and `los ctl`. ✅ |
| `u` / `Ctrl-r` | Undo / redo, count-prefixed; value sweeps coalesce into one entry. ✅ |
| `@` | Open the source picker on a bindable param (live sources from the manifest; Enter binds, x unbinds). ✅ |
| `:` | Ex command line (`:w :e :q :q! :x :set`); not in conductor (session save/load lives there). ✅ |
| `gg` / `G` | First / last of the module's primary collection (sequencer tracks, envelope/mixer channels, voice/scope params, conductor states). ✅ |

## Vocabulary

- **step** — one cell of a track's 16-slot grid. The "character" of the grammar.
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
  it holds. 🔜 (replaces today's separate step/track clipboards)

## Sequencer

Modes: **normal** (operators, track ops, transport), **insert** (direct step
entry/tuning), **visual** / **visual-line** (`v`/`V`; visual-line covers the
current track only in v1), **operator-pending**, **ex**. All ✅.

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
| `j` / `k` | next / previous track (normal mode; counts) | ✅ |
| `gg` / `G` | first / last track (normal); first step (insert) | ✅ |
| `gt#` | go to track # (both modes) | ✅ |

### Operators (normal & visual modes) ✅

| Form | Action |
|------|--------|
| `y{motion}` | yank step range into register — `yw`, `ye`, `y$`, `yt8`, `yf8`, `y3l` |
| `d{motion}` | clear step range (deactivate), range into register |
| `c{motion}` | clear range, move to range start, enter insert mode |
| `yy` / `dd` / `cc` | whole track: yank / delete / clear+insert |

### Register & paste

| Key | Action | Status |
|-----|--------|--------|
| `x` | cut current step into register (normal + insert) | ✅ |
| `y` | yank current step (insert mode; in normal mode `y` is the operator) | ✅ |
| `p` | paste register at cursor — steps **overwrite** from cursor (fixed 16-slot grid, no shifting); a track **inserts after** current | ✅ |
| `P` | paste before — track inserts before current; step range overwrites ending at cursor | ✅ |
| `#p` | paste # times (vi idiom) | ✅ |

> **The `#P` quirk:** counted `#P` / `#L` / `#R` set Euclidean
> pulses/length/rotation (los idiom) and do **not** mean "paste # times
> before". Bare `P` pastes. `:set pulses 5` is the canonical form once ex
> lands; `#P` is the fast idiom. Documented loudly on purpose.

### Steps (insert mode)

| Key | Action | Status |
|-----|--------|--------|
| `Enter` / `Space` | toggle step | ✅ |
| `~` | toggle step (normal mode; flips each step of a visual selection) | ✅ |
| `k` / `j` | note +1 / −1 semitone (or mod value ±0.01) | ✅ |
| `K` / `J` | note +1 / −1 octave (or mod value ±0.1) | ✅ |
| `N<num>` | set MIDI note directly | ✅ |

### Tracks (normal mode)

| Key | Action | Status |
|-----|--------|--------|
| `o` / `O` | new track after / before current (`n` = alias of `o`) | ✅ |
| `dd` / `yy` / `P`/`p` | delete / yank / paste track | ✅ (register-unified 🔜) |
| `m` | toggle mute | ✅ |
| `@` | track mode / routing (becomes source picker) | ✅ → 🔜 |
| `>>` / `<<` | rotate the actual step pattern right / left (counts: `3>>`); preserves hand-edits, unlike Euclid `R` | ✅ |
| `#P` / `#L` / `#R` | Euclidean pulses / length / rotation | ✅ |
| `P`/`L`/`R` (insert, bare) | re-apply / clamp / rotate+1 | ✅ |

### Editing power ✅

| Key | Action |
|-----|--------|
| `.` | repeat last change at cursor (toggle, transpose, paste, euclidean tweak) |
| `v` | visual mode: motions extend a step selection; `y`/`d`/`c`/`~` act on it; `Esc` cancels |
| `V` | visual line: select whole track(s) for `y`/`d` |

### Transport & misc

| Key | Action | Status |
|-----|--------|--------|
| `Space` | play/pause (normal mode; global flag 🔜) | ✅ |
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
| `:set <key> <value>` | module settings: sequencer `bpm`, `pulses`, `length`, `rotation`; others as they grow |

Requires a per-module dirty flag (changed since last save) for `:q` vs `:q!`.

## Other modules (current → v1)

### Voice
`j/k` select param · `h/l` adjust · `H/L` coarse · counts · `gg/G` (✅) ·
`1/2/3` output shortcuts removed — digits are counts ·
`@` source picker on bindable rows · new rows: `notes` (which seq track's
notes to play; unbound = all) and `amp` (amplitude source, default
`envelope/0/ch1`; unbound = 1.0) · `:` · undo (all ✅)

### Envelope
`j/k` select · `h/l` adjust · `H/L` coarse · `[`/`]` channel (counts) ·
`gg/G` first/last channel · `t` trigger · `c` cycle · `o` gate (moved off `g`
to free the gg chord) · `@` source picker per param + trigger · `:` · undo (all ✅)

### Mixer
`h/l` select channel (counts) · `j/k` level down/up · `J/K` coarse ·
`<`/`>` pan (replaces `+/-`) · `gg` first track / `G` master · `m` mute ·
`s` solo · `:` · undo (all ✅)

### Scope
Rebuilt as a vertical param list (✅): `j/k` select (mode, source, channel,
modbus ch, zoom, gain, trigger) · `h/l` adjust · `H/L` coarse · counts ·
`gg/G` · the old `g/G t/T n/N m c b +/-` keys are gone. `@` on the source /
modbus rows opens the picker; the channel row shows the live source label
(`envelope/0/sum`). `:` ✅.

### Conductor
`j/k` list nav (counts) · `gg/G` · `Enter` load (`l` alias) · `s` save
session · `dd` + y/n confirm to delete (✅) · `a` add module, `x` remove
module 🔜

## Future (🔮 post-v1, documented so the grammar reserves space)

- **Sequencer depth (orca-inspired):** per-step chance/probability,
  ratcheting (substep repeats), per-track clock division, swing. Likely
  surface: extra insert-mode rows per step + `:set div 2` style commands.
- `;` / `,` — repeat last `f`/`t` motion forward / back.
- `J` — join: merge next track's pattern into current (OR steps).
- `m{a-z}` / `` `{a-z} `` — marks: save/jump cursor (track, step).
- `/` — search (next step with note N / velocity above X).
- `q{a-z}` — macros. Far future, deeply vi.
- `r` — replace-one (set note without leaving normal mode); evaluate against
  `N` overlap.
