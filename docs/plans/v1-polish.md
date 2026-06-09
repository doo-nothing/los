# v1 Polish — Routing, Keybindings, Transport, Lifecycle

**Status: ✅ Complete** (PRs #5–#12, 2026-06-09)

Everything between "all roadmap phases complete" and a coherent, hyper-polished
v1. After this: new voices and fx modules.

## Decision record (interview 2026-06-09)

| Decision | Choice |
|----------|--------|
| Note routing | Receiver-side: inputs own bindings, outputs are dumb |
| Modbus channels | Dynamic allocation via manifest (no fixed map) |
| Binding UX | `@` opens a source picker (manifest-driven menu) |
| Global transport | Space in every module + `los ctl` + real tmux prefix keys |
| Nav doctrine | Axis rule: navigate along layout axis, adjust perpendicular |
| Adjust keys | `#` counts everywhere + Shift = coarse (~10x) |
| Undo | All modules, u / Ctrl-r / counts; coalesce per sweep |
| Patches & quit | Shared vi ex-style `:` command line (`:w`, `:e`, `:q`, `:q!`, `:x`/`:wq`, `:set`) |
| Old saves | Clean break — state format version bump, old saves ignored |
| v1 scope | All of the above + multi-voice fixes + conductor add/remove |
| Sequencer grammar | Full operator+motion grammar: y/d/c × w/b/e/0/$/t#/f#, x/p in normal mode |
| Register | Unified vi register (step range or track; p pastes contextually; steps overwrite, tracks insert). `#p` = paste N times; `#P`/`#L`/`#R` stay Euclidean (documented quirk) |
| `t` key | BPM moves to `:set bpm` so t/f become true motions |
| Vi extras in v1 | Dot-repeat (`.`), visual mode (`v`/`V`), `o`/`O` new track, `>>`/`<<` rotate |
| Orca-inspired depth | Post-v1 (chance, ratchet, clock division, swing) — documented in keybindings.md |

Full key reference and grammar vocabulary: `docs/keybindings.md` (canonical;
help overlays and README derive from it).

## Problems being solved

### Routing (sloppy and inconsistent — confirmed)

- Voices ignore the event `source` field: every voice plays every note track.
  No way to give voice 0 track 1 and voice 1 track 2.
- Modbus map is implicit and collision-prone: ch 0–3 envelope outputs, 4–7
  envelope logic (sum/or/and/inv), 8–15 sequencer tracks. Undocumented,
  unbindable (voice amp hardwired to ch 0), and a second envelope instance
  would clobber ch 0–7.
- Three mechanisms (note events, modbus ints, `@N` bindings) with no shared
  vocabulary and no place to see the patch graph.

### Keybindings (inconsistency matrix)

- mixer inverts voice/envelope j/k–h/l roles (this is actually the axis rule —
  keep it, formalize it).
- scope follows nothing: g/G, t/T, n/N shifted pairs, no hjkl at all.
- Counts, chords, undo exist only in the sequencer.
- conductor: bare `d` deletes a state file with no chord or confirm; `s`
  save, `l` load instead of prompt/Enter conventions.
- voice burns 1/2/3 on output mode (conflicts with count prefixes).
- README documents Ctrl-b p/s/q "global commands" that were never implemented
  (they collide with stock tmux bindings).

### Multi-voice bugs

- `voice.rs` reconnect path opens EventRingbuf consumer 0 regardless of
  instance (initial open uses the right id) — a reconnecting voice 1 steals
  voice 0's events.
- Envelope reconnect uses magic `4 + instance`. Consumer-id assignment should
  be one documented scheme, not per-module arithmetic.

## Design

### 1. Dynamic modbus allocation + source addresses

Modules claim modbus channels at startup via the manifest (extend entries with
a claimed-channel range + per-channel output labels). Nothing hardcodes
channel numbers.

- **Source address** = `module/instance/output`, e.g. `seq/0/t3`,
  `env/0/ch2`, `env/0/sum`. Stored in bindings and state files.
- Resolution: address → manifest lookup → live channel index. Re-resolved on
  manifest changes, so a restarted module keeps its bindings.
- Sequencer claims one channel per track (labels `t1`–`t8`); envelope claims
  8 (`ch1`–`ch4`, `sum`, `or`, `and`, `inv`). Multiple instances just claim
  more channels.
- Voice gains bindable inputs: `notes` (note-event source track filter, with
  an "all" option = today's behavior), `amp` (default `env/0/ch1`, replacing
  the hardwired ch 0), plus existing shape/sub/fm/level.
- Scope's modbus browser shows labels (`env/0/sum`), not raw indices.

### 2. `@` source picker

`@` on a bindable param opens a menu of live sources read from the manifest:
j/k navigate, Enter binds, Esc cancels, `x` (or selecting "— none —") unbinds.
Same component in every module. The bound source's short label renders next to
the param value.

### 3. Global transport

- Play/stop flag added to the ShmTransport header; sequencer's `playing`
  honors it (its local space/s keys write the shared flag).
- Space = play/pause in every module's top level (sequencer insert mode keeps
  Space = toggle step).
- `los ctl play|stop|toggle` CLI writes the flag.
- Session creation installs tmux prefix bindings so `Ctrl-b p` (play/pause)
  and `Ctrl-b s` (stop) actually work, via `los ctl`. README table becomes
  true. (`Ctrl-b s` shadows tmux's session chooser inside the los session
  only — acceptable; `Ctrl-b q` stays stock tmux.)

### 4. Keybinding doctrine (all modules)

- **Axis rule**: navigate along the module's visual layout axis; adjust on
  the perpendicular. Mixer unchanged; scope refactored into a vertical param
  list (mode, channel, zoom, gain, trigger, source, modbus source) with j/k
  select + h/l adjust.
- **Counts**: `#` prefix works on every nav/adjust key in every module.
- **Coarse**: Shift-variant of the adjust keys = ~10x step. Scope's g/G, t/T,
  n/N pairs are removed.
- **Chords**: gg/G (first/last) wherever there's a list (envelope channels,
  mixer channels, conductor files); `gt#` where numbered jumps make sense.
- voice 1/2/3 output shortcuts removed (digits are count prefixes; output is
  an adjustable row).
- conductor: `dd` + y/n confirm to delete, Enter = load (l kept as alias).
- `?` help and Ctrl-s save stay universal. Help overlays get a standard
  layout listing count/chord forms.

### 5. Undo/redo everywhere

- Extract the sequencer's `History`/`Command` machinery into a shared module
  (`src/undo.rs`), genericized over a module's state.
- voice, envelope, mixer, scope record param edits and binding changes;
  u / Ctrl-r / `#u` / `#Ctrl-r` as in the sequencer.
- **Coalescing**: consecutive adjustments of the same param merge into one
  undo entry; entry closes on param/channel switch or ~1s idle. Sequencer
  note-transpose sweeps adopt the same rule.

### 6. Ex-style command line

`:` opens a one-line prompt at the bottom of every module (shared component):

- `:w <name>` — save module patch to `~/.config/los/patches/<name>.toml`
- `:w` — re-save current patch name
- `:e <name>` — load patch (tab-completion over patches dir)
- `:q` — clean module shutdown (unregister, restore terminal, exit; tmux
  pane closes)
- Unknown commands report in the status line. Room for `:set` later.

### 7. Conductor module lifecycle

- `a` — add-module picker (module type + instance auto-numbered): conductor
  splits the modules window via tmux and spawns `los <module> <n>`.
- `x` (on a module list view) — clean removal: signal the module to save,
  kill the pane.
- `los add <module>` CLI does the same from any shell.
- Conductor gains a modules view (manifest-driven) alongside the states list,
  showing each module's claimed outputs — the de-facto routing overview.

### 8. State format clean break

- `[meta] format = 2` in state files; loader ignores v1 files (logs a note).
- Bindings serialize as source addresses. Module param structs drop the
  legacy flat fields (sequencer `euclidean_*`/`steps`, voice `*_track` ints).

## Build order (one PR each)

1. **Transport** — SHM flag, Space everywhere, `los ctl`, tmux bindings,
   README. Small, independent, immediately satisfying.
2. **Keybinding doctrine** — scope refactor, counts + Shift coarse in
   voice/envelope/mixer/scope, conductor dd/confirm/Enter, voice digit keys
   removed, help overlays standardized against keybindings.md.
3. **Ex command line + patches** — shared `:` component, :w/:w name/:e/:q/
   :q!/:x/:set, dirty-flag tracking, patches dir. Moves sequencer BPM to
   `:set bpm` (frees `t`).
4. **Sequencer grammar** — operator-pending mode (y/d/c × motions), true
   word semantics (w/b/e), t#/f# motions, unified register, x/p/~ in normal
   mode, visual mode (v/V), dot-repeat, o/O, >>/<<. Builds on the Command
   undo machinery; every grammar action is undoable.
5. **Routing core** — manifest channel claiming + labels, source addresses,
   `@` picker, voice notes/amp inputs, note-event filtering, consumer-id
   scheme fix, scope label browser, state format v2.
6. **Undo everywhere** — shared History extraction, per-sweep coalescing,
   voice/envelope/mixer/scope.
7. **Conductor lifecycle** — add/remove UI, modules/routing view,
   `los add` CLI.
8. **Docs final pass** — DESIGN.md (dynamic allocation protocol, keybinding
   doctrine section), README keymap reference from keybindings.md, roadmap
   v1 close-out.

Each PR updates DESIGN.md/README for what it changes; tests required
(routing resolution, undo coalescing, ex-command parsing, count handling).

## Out of scope (post-v1)

New voice types, fx modules, mixer param modulation, MIDI I/O, routing
overview graph rendering, patch morphing.
