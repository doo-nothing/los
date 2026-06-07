# los — Phase Roadmap & Architecture Design

## Overview

los is a modular synth workstation running inside tmux. Each module is an
independent process with its own ratatui TUI in a tmux pane. Modules communicate
over POSIX shared memory (SHM) for audio, events, and transport sync.

This document defines the architecture, save/load design, and future phases.

---

## Architecture Summary (current)

```
tmux session "los"
├── window: conductor    (session control, soon: save/load TUI)
└── window: modules      (2×2 grid: sequencer | voice
                                     mixer    | scope)
```

Each module is a standalone binary (`los <module>`) with its own ratatui TUI.
IPC happens over three SHM objects:
- `/los_mix_in`  — 64-bit audio ringbuffer (voice writes, mixer reads)
- `/los_events`  — 32-byte event ringbuffer (sequencer writes, voice reads)
- `/los_transport` — 64-bit clock (mixer advances, sequencer reads)

---

## File Format: TOML

TOML is the project format. Rationale:
- Already a dependency (`toml` crate + `serde`)
- Human-readable, unambiguous, no tab/space edge cases
- Easy to version-control, diff, and edit by hand
- Well-structured for hierarchical data (windows → panes → params)

A separate `.patch` file format uses the same TOML structure but only contains
a single module's parameters (for loading patches on individual modules).

---

## Phase 1: Save/Load State + Conductor TUI

### Directory Structure

```
~/.config/los/
├── states/               # Full session snapshots (created by save)
│   ├── default.toml
│   └── sketch-idea.toml
├── patches/              # Individual module patches (standalone)
│   ├── bass-lead.toml
│   └── arp-303.toml
└── tmp/                  # Temp files used during save/live-capture
    └── voice_0.state
```

### State File Format (`~/.config/los/states/<name>.toml`)

```toml
[meta]
name = "sketch-idea"
created = "2026-06-06T21:00:00Z"

[tmux]
session_name = "los"
active_window = "modules"
window_size = "largest"

[[windows]]
name = "conductor"
layout = "even-vertical"

[[windows.panes]]
module = "conductor"
instance = 0
size = 1

[[windows.overrides.conductor]]
# no params — conductor is a control panel

[[windows]]
name = "modules"
layout = "tiled"

[[windows.panes]]
module = "sequencer"
instance = 0
size = 3
patch = "../patches/bass-lead.toml"        # external patch reference (optional)

[[windows.panes.patch]]
# Or inline patch data (one or the other)
[windows.panes.patch.sequencer]
bpm = 120
playing = true
euclidean_pulses = 5
euclidean_length = 16
euclidean_rotation = 0

[[windows.panes.patch.sequencer.tracks]]
note = 60
active = true
velocity = 100

[[windows.panes.patch.sequencer.tracks]]
note = 67
active = false
velocity = 100

# ... 16 step entries per track

[windows.panes.patch.sequencer.track_params]
# Future: track-level settings (length override, routing)

[[windows.panes]]
module = "voice"
instance = 0
size = 3

[windows.panes.patch.voice]
shape = 0.5
sub = 0.0
fm = 0.0
output = 2
freq = 261.6
gate = true
level = 0.7

[[windows.panes]]
module = "mixer"
instance = 0
size = 3

[windows.panes.patch.mixer]
master = 0.8

[[windows.panes.patch.mixer.tracks]]
level = 0.8
pan = 0.0
mute = false
solo = false

[[windows.panes.patch.mixer.tracks]]
level = 0.8
pan = 0.0
mute = false
solo = false

# ... 4 tracks

[[windows.panes]]
module = "scope"
instance = 0
size = 3

[windows.panes.patch.scope]
mode = 0
channel = 2
zoom = 1.0
gain = 1.0
```

### Patch File Format (`~/.config/los/patches/<name>.toml`)

Same structure as the `[windows.panes.patch]` section above, but standalone.
Can be referenced by a state file (`patch = "../patches/bass-lead.toml"`) or
loaded directly onto a running module:

```
los voice 1 --patch patches/bass-lead.toml
```

### Save Mechanism

**Ctrl-s as global save (handled by every module):**

1. User presses Ctrl-s in ANY module
2. Module writes its current state to `~/.config/los/tmp/<module>_<instance>.state`
   - Uses atomic write: write to `.tmp`, rename to `.state`
   - File is complete TOML for that module's section
3. Conductor periodically (or on request) polls the tmp directory
4. When all expected modules have written their state, conductor assembles the
   full state file and writes to `~/.config/los/states/<name>.toml`
5. If the conductor is in a separate window, it can show a "saving..." status

**Ctrl-s inside conductor:**
- Conductor sends a SIGUSR1 to all module processes
- Each module catches SIGUSR1 and writes its state file
- Conductor watches for files to appear, then assembles

Chosen approach: **Ctrl-s handled by each module directly**.
- Most reliable: each module knows its own state, no IPC needed
- Most performant: file write is fast and atomic
- Most flexible: works for any module type and instance, scales to many modules
- Fallback: conductor can also request saves via SIGUSR1

### Load Mechanism

**Loading a saved state:**

1. `los my-sketch.toml` or `los load my-sketch`
2. Conductor reads the state file
3. Creates tmux session with windows/panes per the saved layout
4. For each pane, writes the module's patch data to
   `~/.config/los/tmp/<module>_<instance>.state`
5. Respawns the module in each pane
6. Module startup flow:
   a. Initialize SHM connections
   b. Check for a `.state` file in `~/.config/los/tmp/`
   c. If found, parse and apply the saved parameters
   d. Enter main loop

**Loading a single patch on a running module:**

`los voice 1 --patch bass-lead.toml`
or from inside the voice TUI: some keybinding to load a patch file

The module re-reads its params from the file and applies them.

### Conductor TUI

The conductor window gets a basic ratatui TUI with:
- File list showing saved states in `~/.config/los/states/`
- Status line showing current session state (unsaved changes indicator)
- Keybindings:
  - `s` — save current state (prompt for name)
  - `l` — load a selected state
  - `d` — delete a state
  - `q` — quit
  - `?` — help overlay

This keeps the conductor useful without adding complexity. No mouse support
needed — everything keyboard-driven.

### Implementation Steps (detailed)

1. **Create `~/.config/los/` directories** in conductor startup
2. **Add Ctrl-s handler to each module** (voice, sequencer, mixer, scope):
   - Write module state to `~/.config/los/tmp/<module>_<instance>.state`
   - Use atomic write pattern (write `.tmp`, fsync, rename)
3. **Add `--patch` / `--state` CLI flag** to each module for loading params
   - Module startup checks for state file, applies if present
4. **Implement state file parser** (shared module in `src/state.rs`)
   - Serde deserialize from TOML
   - Each module type has its own param struct
5. **Implement state file writer** (shared module in `src/state.rs`)
   - Serde serialize to TOML string
6. **Build conductor TUI** (ratatui, simple file list)
7. **Implement assembly logic** in conductor:
   - Read module state files from tmp
   - Combine with tmux layout info
   - Write to `~/.config/los/states/<name>.toml`
8. **Implement load logic**:
   - Read state file
   - Create tmux session/windows/panes
   - Write module state files
   - Respawn modules
9. **Test end-to-end**: save → kill session → load → verify everything restored
10. **Update `los.toml`** to support patch file references

### `src/state.rs` Module Structure

```rust
// Shared state representation (serialized to/from TOML)

pub struct SessionState {
    pub meta: Meta,
    pub tmux: TmuxState,
    pub windows: Vec<WindowState>,
}

pub struct Meta {
    pub name: String,
    pub created: String,
}

pub struct TmuxState {
    pub session_name: String,
    pub active_window: String,
    pub window_size: String,
}

pub struct WindowState {
    pub name: String,
    pub layout: String,
    pub panes: Vec<PaneState>,
}

pub struct PaneState {
    pub module: String,
    pub instance: usize,
    pub size: Option<usize>,
    pub patch: Option<String>,       // external patch reference
    pub patch_inline: Option<toml::Value>,  // or inline data
}

// Per-module param structs, all optional

pub struct VoiceParams {
    pub shape: Option<f32>,
    pub sub: Option<f32>,
    pub fm: Option<f32>,
    pub output: Option<u8>,
    pub freq: Option<f32>,
    pub gate: Option<bool>,
    pub level: Option<f32>,
}

pub struct SequencerParams {
    pub bpm: Option<f64>,
    pub playing: Option<bool>,
    pub euclidean_pulses: Option<usize>,
    pub euclidean_length: Option<usize>,
    pub euclidean_rotation: Option<usize>,
    pub steps: Vec<StepParam>,
}

pub struct StepParam {
    pub active: bool,
    pub note: u8,
    pub velocity: u8,
}

pub struct MixerParams {
    pub master: Option<f32>,
    pub tracks: Vec<MixerTrackParam>,
}

pub struct MixerTrackParam {
    pub level: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
}

pub struct ScopeParams {
    pub mode: Option<usize>,
    pub channel: Option<usize>,
    pub zoom: Option<f32>,
    pub gain: Option<f32>,
}
```

### Testing Plan

1. Start los, set some params (sequencer steps, voice shape, mixer levels)
2. Ctrl-s to save (name it "test-1")
3. Verify state file exists at `~/.config/los/states/test-1.toml`
4. Kill session
5. `los test-1.toml` — loads the state
6. Verify all modules have correct params
7. Repeat with modified params, verify save/load cycle
8. Test edge cases: empty state, missing modules, invalid files

---

## Phase 2: Merge tmux-arch → master

Once Phase 1 is stable and reviewed, merge the `tmux-arch` branch to `master`.

- Resolve any conflicts
- Update README with architecture docs
- Update DESIGN.md to reflect multi-process TUI architecture
- Tag release (v0.3.0)

---

## Phase 3: Multi-Track Sequencer

The sequencer grows from 1 track to N tracks. Each track is a self-contained
step pattern with its own length, pulses, rotation, and step data.

### Track Operations (vi-inspired)

The existing step clipboard (`x` cut, `p` paste) extends to tracks:

| Key | Action |
|-----|--------|
| `n` | New track (append empty track, select it) |
| `dd` | Delete current track (save to track clipboard) |
| `yy` | Yank (copy) current track to track clipboard |
| `P` | Put/paste track clipboard after current track |
| `J` | Join next track into current (append steps) |
| `[` / `]` | Switch to previous/next track |
| `1`..`9` | Jump to track N (1-indexed) |

### Track Data Model

```rust
struct Track {
    steps: Vec<Step>,
    length: usize,        // euclidean_length per track
    pulses: usize,        // euclidean_pulses per track
    rotation: usize,      // euclidean_rotation per track
    mod_dest: ModDest,    // where this track's output goes
    muted: bool,
    level: f32,           // per-track velocity multiplier
}

struct ModDest {
    target_module: String,   // "voice", "mixer", "scope", "envelope"
    target_instance: usize,  // 0, 1, 2...
    target_param: String,    // "shape", "fm", "level", etc.
}
```

### Track Routing / Modulation

Each sequencer track can target a parameter on any module:
- Default: track 0 → voice 0 pitch
- Track 1 → voice 0 shape
- Track 2 → envelope 0 trigger
- Etc.

The routing is stored as a `ModDest` in the track data. The sequencer sends
events to the target module's event ringbuffer with the appropriate param ID.
If the target module doesn't exist yet, the track is a no-op (or we could
auto-spawn the target module).

### Display

The sequencer TUI shows:
- Track tabs along the top: `│ 1*│ 2  │ 3  │ 4  │`
- Current track's step grid below
- Track number in status line
- Active track count in status line

---

## Phase 4: Envelope Module

A new `los envelope` module running in its own tmux pane. eurorack make noise Maths module-inspired
envelope generator with per-track rise/fall stages: https://www.makenoisemusic.com/wp-content/uploads/2024/03/MATHSmanual2013.pdf


### Params (per-track, up to 4 tracks)

| Param | Range | Description |
|-------|-------|-------------|
| Attack | 0-1000 | Rise time (exponential scale, 1ms to 10s) |
| Decay | 0-1000 | Fall time (same scale) |
| Shape | 0-1000 | Curve shape (concave → linear → convex) |
| Loop | 0/1 | Oneshot or cycle |
| Mod Target | 0-3 | Where output routes (amp/pitch/shape/fm) |

### TUI

Simple parameter display similar to the voice module:
- Per-track tab (like sequencer multi-track)
- Gauges for each parameter
- Keybindings: j/k select param, h/l adjust, [ / ] switch track

### IPC

The envelope module writes modulation events to the event ringbuffer
(`/los_events`) with a new event type `EVENT_MOD` that carries the current
modulation level (0-1 range). Target modules read these events and apply
the modulation to the specified parameter.

---

## Phase 5: Track Routing / Modulation UI

Once the envelope module exists and the sequencer has multiple tracks, users
need a way to assign routing.

### UI Proposal

In the sequencer, pressing a key (e.g. `@` or `m`) on a track enters "mod
routing" mode. A menu appears showing available targets:

```
Modulation target for Track 2:
  ┌────────────────────────────────────┐
  │ voice/0/shape                      │
  │ voice/0/fm                         │
  │ envelope/0/attack                  │
  │ mixer/0/track1/pan                 │
  │ mixer/0/master                     │
  └────────────────────────────────────┘
```

Navigate with j/k, select with Enter, cancel with Esc.

### Implementation

The conductor maintains a registry of running modules (via SHM or reading
tmux pane titles). When a module asks for available targets, the conductor
provides the list. Or simpler: hardcode known modules and let the user type
the target string.

---

## Phase 6: Module Lifecycle

### Adding a new module at runtime

User wants: "I need a second voice" or "I want another envelope."

Approach: tmux keybinding (e.g. `Ctrl-b n` or a conductor key) that:
1. Splits a pane or creates a new window
2. Prompts for module type
3. Spawns the module
4. Assigns it to the mixer

For now, this can be done manually in tmux:
1. `Ctrl-b "` or `Ctrl-b %` to create a new pane
2. `los voice 2` to spawn a second voice
3. The new voice picks up an available mixer channel
4. Or use a dedicated conductor keybinding: `a` for "add module" opens a prompt

### Mixer auto-assignment

When a new voice starts, it registers itself by writing a "module manifest"
to a new SHM object (`/los_manifest`). The mixer sees the new manifest entry
and creates a mixer channel for it. This avoids manual routing.

### Module removal

`dd` on a module in the conductor TUI kills the process and closes the pane.
The mixer detects the manifest removal and cleans up the channel.

---

## File & Doc Updates

After all phases are complete:

- `README.md` — rewrite with full architecture, keybindings, module reference
- `DESIGN.md` — update architecture diagram, add IPC details
- `docs/plans/roadmap.md` — update progress, remove completed phases
- New docs as needed per phase

---

## Summary of Priorities

```
Phase 1: Save/Load + Conductor TUI  ← IMPLEMENT NOW
Phase 2: Merge to master             ← After Phase 1 review
Phase 3: Multi-track sequencer       ← Next
Phase 4: Envelope module             ← Next
Phase 5: Track routing / modulation  ← After Phase 3+4
Phase 6: Module lifecycle            ← After Phase 5
```

