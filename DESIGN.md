# los — Live Operating System

A console-based groovebox/synth workstation where every module runs in its own
`tmux` pane.

**Core principle:** The entire instrument is a `tmux` session. Process isolation,
low-level IPC, and Unix philosophy.

---

## 0. Name & Philosophy

`los` = "Live Operating System" (also "L-O-S" as in lossless).

- **Keyboard-first** — everything from the home row, zero mouse.
- **Console-native** — runs over SSH, in a TTY, inside `tmux`/`screen`.
- **`tmux`-native architecture** — each component is an independent process in
  its own pane. Crash one → others keep playing.

---

## 1. Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    tmux session "los"                        │
├─────────────┬──────────────┬──────────────┬─────────────────┤
│  sequencer  │   voice 1    │   voice 2    │     mixer       │
│  (pane)     │   (pane)     │   (pane)     │     (pane)      │
├─────────────┼──────────────┼──────────────┼─────────────────┤
│   voice 3   │   voice 4    │    scope     │   conductor     │
│   (pane)    │   (pane)     │    (pane)    │   (pane/CLI)    │
└─────────────┴──────────────┴──────────────┴─────────────────┘
```

### Components

| Module | Role |
|--------|------|
| **Conductor** | Session orchestrator — creates tmux session, spawns modules, monitors health, provides CLI for global commands |
| **Voice** | Synth voice (osc → filter → envelope). One per instance. Free-running, writes audio to SHM ringbuffer |
| **Mixer** | Runs cpal output. Pulls samples from each voice's SHM ringbuffer, sums, applies master effects, writes to device |
| **Sequencer** | Step sequencer TUI. Writes note events to per-voice event ringbuffers |
| **Scope** | Reads shared audio buffer, renders ASCII oscilloscope at low fps |

### IPC

| Data | Mechanism |
|------|-----------|
| Audio (voice→mixer) | SHM multi-slot ringbuffer (`shm_open` + `mmap`) |
| Events (sequencer→voice) | SHM event ringbuffer (fixed-size messages) |
| Transport state | SHM region (bpm, playhead, playing) |
| Module control | UDS handshake at startup |
| Global commands | tmux prefix keys → conductor stdin |

### Audio clock

The mixer runs the sole cpal output stream. Voices are **free-running**,
continuously generating samples into their ringbuffer. The mixer pulls the
latest block in each audio callback.

Voice pacing is regulated by a shared sample clock counter in transport SHM.
Each voice checks how far ahead it is of the mixer and sleeps adaptively to
stay ~5ms ahead — no polling, no syscalls in the audio path.

---

## 2. Binary structure

Single binary with subcommand dispatch:

```
los              → conductor (creates session, attaches)
los conductor    → conductor monitor (inside conductor pane)
los voice N      → voice module instance N (Phase 2)
los mixer        → mixer module (cpal output, reads SHM)
los tone [freq]  → test tone generator, writes to SHM (Phase 1 testing)
los sequencer    → sequencer module (Phase 3)
los scope        → scope module (Phase 4)
```

### SHM Audio Ringbuffer (`shm.rs`)

Lock-free SPSC ringbuffer backed by POSIX shared memory (`shm_open` + `mmap`).

**Header layout (64 bytes):**
| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | `write_index` — producer advances |
| 8 | 8 | `read_index` — consumer advances |
| 16 | 4 | `channels` |
| 20 | 4 | `slot_frames` — frames per slot |
| 24 | 4 | `num_slots` — total slots in buffer |

**Data:** `slot_frames × channels × sizeof(f32)` bytes per slot, `num_slots` slots.

**Default params:** 2 ch, 64 frames/slot, 128 slots = 8192 frames ≈ 170ms buffer.

Access is via `write_unaligned`/`read_unaligned` with `compiler_fence(Acquire/Release)` for memory ordering on the indices. Aligned u64 loads/stores are atomic on x86_64 & aarch64, so the index operations are safe without explicit atomic instructions.

**Key operations:**
```rust
fn write(&mut self, data: &[f32]) — write one slot, spin if full
fn read(&mut self, data: &mut [f32]) -> bool — read one slot, returns false if empty
```

---

## 3. Layout config (TOML)

```toml
session_name = "los"

[[module]]
type = "sequencer"

[[module]]
type = "voice"
count = 4

[[module]]
type = "mixer"

[[module]]
type = "scope"
```

Lookup order: `./los.toml` → `~/.config/los/layout.toml` → built-in defaults.

---

## 5. Sequencer Architecture

The sequencer is a ratatui TUI in its own tmux pane. It runs as an independent
process (`los sequencer [instance]`) communicating with voices via the event
ringbuffer.

### Data Model

```
SequencerState
├── tracks: Vec<Track>         — 1-N tracks, each with its own step pattern
│   ├── steps: Vec<Step>       — 16 steps per track
│   │   ├── active: bool
│   │   ├── note: u8
│   │   ├── velocity: u8
│   │   └── mod_value: f32
│   ├── length: usize          — Euclidean length (1-16)
│   ├── pulses: usize          — Euclidean pulses (0-16)
│   ├── rotation: usize
│   ├── muted: bool
│   └── mode: TrackMode        — Note | Modulation
├── bpm: f64
├── playing: bool
├── current_steps: Vec<usize>  — playhead per track
├── last_notes: Vec<Option<u8>>— note-off tracking
├── selected: usize            — selected step index
├── clipboard: Option<Step>    — per-step clipboard
└── track_clipboard: Option<Track> — per-track clipboard
```

### Undo/Redo System

Every state-modifying user action in the sequencer is undoable via a command
pattern. The system is defined entirely within `src/sequencer.rs`:

**`Command` enum** — one variant per undoable action type:
| Variant | Action |
|---------|--------|
| `ToggleStep` | Step on/off (Enter/Space in insert mode) |
| `EditStep` | Cut, paste, transpose, set-note |
| `SetTrackParams` | Euclidean params (P/L/R), rotation |
| `ToggleMute` | Mute toggle (m) |
| `ToggleMode` | Note/Mod toggle (@) |
| `NewTrack` | Create track (n) |
| `DeleteTrack` | Delete track (dd) |
| `PasteTrack` | Paste track (P) |
| `SetBpm` | BPM change (t<num>) |

**`History` struct** — manages command list with 100-command cap:
- `push(cmd)` — truncates redo stack, appends, advances index
- `undo(state)` — decrements index, reverses command at that index
- `redo(state)` — applies command at current index, increments

Key bindings: `u`=undo, `Ctrl-r`=redo. Undo/redo messages appear in the
status bar for 2 seconds.

**Any new state-modifying user action in the sequencer MUST be undoable.**

To add a new undoable action:

1. Add a variant to the `Command` enum (in `src/sequencer.rs`)
2. Implement `undo()` and `redo()` for it (matching the existing pattern)
3. Add its description to `Command::description()`
4. Call `history.push(Command::YourVariant { ... })` at the action site
5. If the action uses a submode, push the command when Enter is pressed in the
   submode handler, not when the submode is entered.
6. Add the key binding to the help text in `draw_ui()`

Non-undoable actions: navigation (step selection, track switching, scrolling),
clipboard operations (yank), help toggling, and save operations.

History is **in-memory only** — not saved with session state. Future work could
serialize commands alongside `SequencerParams`.

### Sequencer Thread

A background thread (`sequencer_thread`) advances the playhead every ~10ms,
reading the SHM transport clock. It sends `note_on`/`note_off` events to the
event ringbuffer and writes modulation values to the modulation bus. The thread
holds the state lock briefly (~100μs) to snapshot bpm/playing/tracks each cycle.

### Euclidean Rhythm

Each track has independent Euclidean parameters (pulses, length, rotation).
The `euclidean_apply()` function distributes pulses evenly across the step
grid using the Bjorklund algorithm. Applying Euclidean overrides existing step
active states. Tracks default to enabling every 4th step.

---

## 6. Phases

| Phase | Deliverable | Status |
|-------|-------------|--------|
| **0** | Conductor: tmux session creation, pane layout, module spawning, global key bindings, conductor CLI | ✅ |
| **1** | SHM ringbuffer crate (`shm.rs`) + `los mixer` (cpal output, reads SHM) + `los tone` writes to SHM | ✅ |
| **2** | `los voice` — synth engine (oscillators, ADSR, filter), writes audio to SHM | ✅ |
| **3** | `los sequencer` — step sequencer TUI, writes events to SHM | ✅ |
| **4** | `los scope` — reads mixer SHM, ASCII oscilloscope | ✅ |
| **5** | Save/load session state, envelope module, track routing, multi-track | ✅ |
| **6** | Module lifecycle: add/remove at runtime, `/los_manifest` SHM registry, per-module audio ringbuffers, dynamic mixer channels, CLI aliases | ✅ |
| **7** | Sequencer undo/redo: command pattern with `u`/`Ctrl-r`, 18 undoable actions, status-bar feedback | ✅ |

---

## 7. SHM Protocol Specification

This section describes the wire format of every shared memory object. These byte
layouts are the **language-agnostic contract** — any process in any language that
can call `shm_open` + `mmap` can participate as a producer or consumer.

### 7.1 AudioRingbuf (`/los_audio_<module>_<instance>`)

Lock-free SPSC ringbuffer. Exactly one producer writes, exactly one consumer reads.

```
Offset  Size   Field
------  ----   -----
0       8      write_index: u64 (LE)    — producer advances after writing a slot
8       8      read_index:  u64 (LE)    — consumer advances after reading a slot
16      4      channels:    u32 (LE)    — default 2
20      4      slot_frames: u32 (LE)    — frames per slot, default 64
24      4      num_slots:   u32 (LE)    — total slots, default 128
28      36     (reserved / padding)
64      N      data: f32[slot_frames × channels × num_slots], interleaved

Total size: 64 + (slot_frames × channels × num_slots × 4) bytes
Default:    64 + (64 × 2 × 128 × 4) = 64 + 65536 = 65600 bytes
```

**Protocol rules:**
- Producer writes interleaved f32 samples to `slot_ptr(write_index % num_slots)`,
  then increments `write_index` (release fence).
- Consumer reads from `slot_ptr(read_index % num_slots)`, then increments
  `read_index` (acquire fence on indices).
- Available slots: `write_index - read_index`. If this reaches `num_slots`,
  producer is blocked (buffer full).
- Both indices are monotonic u64 counters, wrapping handled by `% num_slots`.
- Shm name convention: `/los_audio_<module>_<instance>`. The mixer sums all
  audio-producing modules to `/los_mix_in` for scope consumption.

### 7.2 EventRingbuf (`/los_events_v2`)

Lock-free MPMC ringbuffer for fixed-size 32-byte events. 16 independent consumers.

```
Offset  Size   Field
------  ----   -----
0       8      write_index: u64 (LE)
8       8      (reserved)
16      128    16 × read_index: u64 (LE)   — consumer 0 at +16, consumer 1 at +24, ...
144     112    (reserved)
256     N      event data: AudioEvent[256]

Total size: 256 + (256 × 32) = 8448 bytes
```

**Protocol rules:**
- Producer writes to `slot_ptr(write_index % 256)`, then increments `write_index`
  (release fence).
- Each consumer reads from `slot_ptr(read_index_c % 256)`, increments its own
  read index (release fence).
- Producer checks `min(all 16 read indices)` to determine if buffer is full.
  If `write_index - min(read) >= 256`, blocked.
- Consumer initializes its read index to the current `write_index` on first open
  — it only sees events produced after joining.

### 7.3 ShmTransport (`/los_transport`)

Simple shared state, no ringbuffer. One writer (mixer), many readers.

```
Offset  Size   Field
------  ----   -----
0       8      clock: u64       — total output frames consumed by mixer
8       4      sample_rate: u32 — default 48000
12      4      flags: u32        — bit 0 = playing
16      48     (reserved)

Total size: 64 bytes
```

**Protocol rules:**
- Mixer advances `clock` by `frames per callback` each audio cycle.
- `playing` flag: bit 0 set = transport is running. Other bits reserved (must be 0).
- The play flag is global state: any module (Space key), the sequencer
  (Space/s), or `los ctl` may write it. The sequencer treats it as the source
  of truth and mirrors it into its UI state; it gates step events, not the
  audio clock.
- Readers poll `clock` and `playing` with acquire fence on the `clock` read.
- Voices compare their generated-frame count against `clock` to pace adaptive sleep.

### 7.4 ModulationBus (`/los_mod`)

32 atomic f32 channels. Multiple writers (sequencer, envelope), many readers
(voices, scope).

```
Offset  Size   Field
------  ----   -----
0       4      version: u32 = 1
4       4      num_channels: u32 = 32
8       56     (reserved)
64      4      channel[0]: f32
68      4      channel[1]: f32
...    ...     ...
188     4      channel[31]: f32

Total size: 64 + (32 × 4) = 192 bytes
```

**Channel allocation:**

| Ch    | Writer(s)    | Meaning |
|-------|--------------|---------|
| 0-3   | envelope 0   | Envelope channel 1-4 output (0.0-1.0, amplitude) |
| 4     | envelope 0   | SUM (all 4 envelope channels summed, clamped 0-1) |
| 5     | envelope 0   | OR (max of envelope channels) |
| 6     | envelope 0   | AND (min of envelope channels) |
| 7     | envelope 0   | INV (inverted ch0) |
| 8+N   | sequencer 0  | Track N mod value: velocity (note tracks) or modulation (mod tracks) |

**Protocol rules:**
- `set(ch, val)` uses `ptr::write_volatile` + `compiler_fence(Release)`. Aligned
  f32 writes are atomic on supported architectures.
- `get(ch)` uses `ptr::read_volatile`. No fence needed for single-channel reads.
- Writers should only write to their allocated channels. No central allocation
  protocol yet — channels are documented here as the source of truth.
- Channels 0-7 are hardcoded to envelope 0. Channels 8-15 are sequencer tracks
  0-7. Channels 16-31 are unused/unallocated.

### 7.5 Manifest (`/los_manifest`)

Lock-free fixed-size module registry. 16 entries, two-phase atomic claim protocol.

```
Offset  Size   Field
------  ----   -----
0       4      version: u32 = 1
4       4      max_entries: u32 = 16
8       4      entry_size: u32 = 64
12      52     (reserved)

64      4      entry[0].valid: AtomicU32   — 0=empty, 2=claiming, 1=active
68      16     entry[0].module_name: u8[16] — null-terminated
84      4      entry[0].instance: u32
88      4      entry[0].pid: u32
92      32     entry[0].audio_shm: u8[32]  — null-terminated, empty if no audio
124     4      entry[0].(reserved)

... repeat for entries 1-15 ...

Total size: 64 + (16 × 64) = 1088 bytes
```

**Two-phase claim protocol (register):**

1. Scan entries for `valid == 0` (empty)
2. CAS `valid` from 0 → 2 (claiming). If CAS fails, try next slot.
3. Write module_name, instance, pid, audio_shm to entry data
4. Store `valid = 1` (active) with release ordering — now visible to readers

**Unregister:** Store `valid = 0` with release ordering. Called on `Drop`.

**Reader protocol (entries):**
- Scan all 16 slots
- For each slot where `valid == 1` (acquire load): read data
- Values in `valid == 0` or `valid == 2` slots are undefined — skip them
- `valid == 2` is transient (module in the middle of writing); readers must not
  read these entries

**Constraints:**
- Module name: max 15 chars (15 + null = 16 bytes)
- Audio SHM name: max 31 chars (31 + null = 32 bytes)
- Max 16 registered modules total
- Instance numbers are user-chosen, not validated (convention: 0, 1, 2...)

---

## 8. AudioEvent Message Format

A fixed-size 32-byte message sent via EventRingbuf. `#[repr(C)]` layout, portable
across processes.

```
Offset  Size   Field       Type    Description
------  ----   -----       ----    -----------
0       1      event_type  u8      0=note_on, 1=note_off, 2=param, 3=mod, 4=trigger
1       1      source      u8      Encoded source module + instance
2       1      target      u8      Encoded target module + instance
3       1      param       u8      Target parameter ID, or velocity (0-127) for note events
4       4      value       f32     Note frequency (Hz), modulation amount, trigger level
8       4      step        u32     Step index / timestamp
12      20     _reserved   u8[20]  Padding (undefined, do not read)
```

### 8.1 Event Types

| Constant     | Value | Produced by             | Consumed by          | Semantics |
|--------------|-------|-------------------------|----------------------|-----------|
| `NOTE_ON`    | 0     | sequencer               | voice(s), envelope   | `value` = frequency (Hz) from MIDI note, `param` = velocity (0-127) |
| `NOTE_OFF`   | 1     | sequencer               | voice(s), envelope   | `param` = note number that should stop |
| `PARAM`      | 2     | any                     | any                  | General parameter set. `param` = parameter ID, `value` = new value |
| `MOD`        | 3     | envelope, sequencer     | voice, mixer, scope  | Modulation signal. `target`/`param` encode destination, `value` = 0.0-1.0 |
| `TRIGGER`    | 4     | sequencer, conductor    | envelope             | Manual trigger. `source`/`target` identify envelope instance, `value` = trigger level |

### 8.2 Parameter IDs

Used in `param` field for `PARAM` and `MOD` events:

| Constant       | ID | Target module | Parameter |
|----------------|----|---------------|-----------|
| `PARAM_SHAPE`  | 0  | voice         | Oscillator shape (0.0-1.0) |
| `PARAM_SUB`    | 1  | voice         | Sub oscillator level (0.0-1.0) |
| `PARAM_FM`     | 2  | voice         | FM amount (0.0-1.0) |
| `PARAM_OUTPUT` | 3  | voice         | Output mode (0-2) |
| `PARAM_LEVEL`  | 4  | voice, mixer  | Level/velocity (0.0-1.0) |
| `PARAM_RISE`   | 5  | envelope      | Attack/rise time |
| `PARAM_FALL`   | 6  | envelope      | Decay/fall time |

**Source/target encoding:** Currently uses track index or module+instance packed
into a u8. This field is under-specified and should be treated as module-defined
for now. A future protocol version should standardize source/target encoding.

### 8.3 Module-Specific Event Handling

**Voice:**
- Reads `NOTE_ON`: sets `freq = value`, `velocity = param/127`, `gate = true`
- Reads `NOTE_OFF`: sets `gate = false`
- Reads `MOD`: applies to named param (shape, sub, fm, level) based on
  `target`/`param`
- Does NOT read `PARAM` or `TRIGGER` events

**Envelope:**
- Reads `NOTE_ON` / `NOTE_OFF`: triggers attack/decay cycle
- Reads `TRIGGER`: manual fire with `value` as intensity
- Does NOT read `PARAM` or `MOD` events
- **Produces** `MOD` events writing current envelope level to ModulationBus
  channels 0-7

**Sequencer:**
- **Produces** `NOTE_ON` and `NOTE_OFF` events per step, one per active step
- **Produces** `MOD` events writing track modulation to ModulationBus channels 8+N

**Mixer, Scope, Conductor:**
- These modules do NOT consume or produce AudioEvents. Mixer reads AudioRingbufs;
  scope reads AudioRingbuf and ModulationBus; conductor reads Manifest and tmux
  state.

---

## 9. Module Lifecycle

Every module process follows this lifecycle. It is a **convention**, not enforced
by a trait or framework. The sections below describe what each phase must do and why.

### 9.1 Startup Sequence (in order)

```
1. setup_save_signal()         — install SIGUSR1 handler for save-on-signal
2. setup_reload_signal()       — install SIGUSR2 handler for reload-on-signal
3. write_pid_file(name, inst)  — write PID to ~/.config/los/tmp/<name>_<inst>.pid
4. enable_raw_mode()           — with 20-retry loop, 200ms sleep, for tmux PTY race
5. EnterAlternateScreen        — ratatui alternate terminal buffer
6. manifest.register()         — claim a slot in /los_manifest, register module+instance
7. Open SHM objects            — create (if first) or open AudioRingbuf, EventRingbuf,
                                  ModulationBus, ShmTransport as needed for role
8. load_module_state()         — check ~/.config/los/tmp/<module>_<inst>.state, apply
9. Spawn background thread     — audio/event processing thread, shares state via
                                  Arc<Mutex<...>>
10. Enter main event loop      — ratatui TUI with event::poll() for keyboard input
```

**Why each step matters:**

- **Steps 1-2:** Enable signal-based save/reload independent of keyboard. The
  conductor can trigger a global save by sending SIGUSR1 to all module PIDs.
- **Step 3:** PID file lets the conductor find and signal all running modules.
- **Step 4:** tmux delivers the PTY a fraction of a second after the pane is
  created. Standard I/O may report "not a terminal" for 100-200ms. The retry
  loop handles this transparently.
- **Steps 5-6:** Terminal init and manifest registration establish the module's
  presence in the session.
- **Steps 7-8:** SHM connections are the IPC plumbing. Module state is loaded
  from disk if a previous save exists. The create/open pattern
  (`open().or_else(|| create())`) ensures the first module creates the SHM
  object and subsequent modules open it.
- **Steps 9-10:** The background thread does the real-time work; the main thread
  handles keyboard and TUI.

### 9.2 SHM Role Matrix

| Module    | AudioRingbuf                | EventRingbuf        | ShmTransport       | ModulationBus         | Manifest  |
|-----------|-----------------------------|---------------------|--------------------|-----------------------|-----------|
| voice     | Create `/los_audio_voice_N` | Open consumer N     | Open (read clock)  | Open (read channels)  | Register  |
| tone      | Create `/los_audio_tone_N`  | —                   | —                  | —                     | Register  |
| mixer     | Open all from manifest      | —                   | Create (write)     | —                     | Open      |
| sequencer | —                           | Open producer       | Open (read clock)  | Open (write ch 8+)    | Register  |
| envelope  | —                           | Open prod + consumer| —                  | Create (write ch 0-7) | Register  |
| scope     | Open `/los_mix_in` (peek)   | —                   | —                  | Open (read all)       | Register  |
| conductor | —                           | —                   | —                  | —                     | Create+Open |

**Consumer ID assignment:** A module that opens EventRingbuf as a consumer needs
a unique consumer ID (0-15). The current convention: `consumer_id = instance
number` (capped at 15). Consumer 0 is shared by voice 0 and envelope 0 (they
read independent streams — the MPMC design supports this). The sequencer opens
as producer only (no consumer ID needed).

### 9.3 Runtime Loop

Every module's main loop does:

```
loop {
    check_save_signal()   → if saved via SIGUSR1, write state file
    check_reload_signal() → if reloaded via SIGUSR2, read state file and apply
    draw_ui()             → ratatui Terminal::draw()
    poll(Duration)        → crossterm event::poll()
    handle_key()          → match KeyCode, mutate state
    if Ctrl-s             → write state file directly
}
```

**Background thread pattern** (voice, sequencer, envelope):

```
loop {
    if shutdown.try_recv().is_ok() { break; }
    reconnect_shm_if_lost()   // handle transient SHM object unavailability
    read_events()             // drain event ringbuffer
    generate_audio()           // compute next block
    write_to_ringbuf()         // push to audio SHM (or write modulation)
    read_transport_clock()     // pace against mixer clock
    sleep_adaptively()         // stay ~5ms ahead of mixer
}
```

### 9.4 Shutdown Sequence

1. tmux pane closed (`Ctrl-b x`) or process exits (`q` key in some modules)
2. `Manifest::drop()` calls `unregister()` — stores `valid = 0` in manifest entry
3. `AudioRingbuf::drop()` calls `shm_unlink()` if module was the creator
   (`owned = true`)
4. Other SHM objects unmap and close their fd
5. Mixer detects missing manifest entries on next 500ms scan cycle, removes channel
6. Other modules are unaffected — process isolation means one crash doesn't cascade

**Note on SHM cleanup:** The first process to create an SHM object owns it
(`owned = true`) and unlinks it on drop. If the owning process crashes without
running Drop, the SHM object persists. This is harmless — a new process can
recreate it, and the OS cleans up SHM objects on full system restart. Future
work: a `/los_session` namespace or session-lifetime SHM management.

---

## 10. Save/Load Contract

### 10.1 State File Format

Each module serializes its parameters to
`~/.config/los/tmp/<module>_<instance>.state` as TOML.

**Contract for each module:**
- Define a `Params` struct with `#[derive(Serialize, Deserialize)]` and optional
  fields
- Implement `save_module_state(name, instance, &params)` to write TOML to the
  temp file
- Implement `load_module_state::<Params>(name, instance) -> Result<Params>` to
  read it back
- Atomic write: write to `.tmp` file, fsync, rename to `.state`

**When saves happen:**
- User presses `Ctrl-s` in any module → module writes its state file
- Conductor sends SIGUSR1 to all module PIDs → each module writes its state file
- Conductor assembles full `~/.config/los/states/<name>.toml` from all temp files
  + tmux layout

**When loads happen:**
- `los <state.toml>` → conductor creates tmux layout, writes state files to tmp,
  spawns modules
- Module startup reads its state file from tmp directory, applies parameters
- Module continues with saved state

### 10.2 Conductor Expectations

The conductor (session orchestrator) expects:
- Each module writes its state file **atomically** to the tmp directory
- All currently running modules produce a state file within a reasonable time
  after SIGUSR1
- The conductor polls the tmp directory to check when all expected modules have
  written
- Module names in state files match the manifest module_name exactly
- Instance numbers in state files match the manifest instance exactly

### 10.3 Per-Module Params Structs

Each module defines its own Params struct in `src/state.rs`. All fields are
`Option<T>` to support partial updates — an incoming state file may not include
every field, and only present fields should be applied. The module merges loaded
params into its runtime state, keeping current values for any field not present
in the TOML.

```rust
// Example: VoiceParams in src/state.rs
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoiceParams {
    pub shape: Option<f32>,
    pub sub: Option<f32>,
    pub fm: Option<f32>,
    pub output: Option<u8>,
    pub freq: Option<f32>,
    pub gate: Option<bool>,
    pub level: Option<f32>,
    pub velocity: Option<f32>,
    pub shape_track: Option<i32>,
    pub sub_track: Option<i32>,
    pub fm_track: Option<i32>,
    pub level_track: Option<i32>,
}
```

---

## 11. Adding a Module (Current Process)

To add a new module to los today (e.g., a delay or reverb module), follow these
steps:

**1. Create the module file:** `src/delay.rs`

**2. Add the entry point** — a `pub fn run(instance: usize) -> Result<()>`
function following the startup sequence from Section 9.

**3. Define state and params:**
```rust
#[derive(Clone, Copy, Default)]
struct DelayState {
    time: f32,
    feedback: f32,
    mix: f32,
}

// Add to src/state.rs:
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelayParams {
    pub time: Option<f32>,
    pub feedback: Option<f32>,
    pub mix: Option<f32>,
}
```

**4. Register in the module registry:**
- Add `pub mod delay;` to `src/lib.rs`
- Add `"delay" => delay::run(instance)` to the match in
  `src/main.rs::dispatch_module()` (line 49)

**5. Determine SHM role:**
- If the module produces audio: create an AudioRingbuf named
  `/los_audio_delay_<instance>`, register in manifest with audio SHM name
- If the module consumes events: open EventRingbuf with a unique consumer ID
- If the module reads/writes modulation: open ModulationBus, claim unused
  channels (document them in Section 7.4)

**6. Add to conductor's layout defaults** (in `src/layout.rs`) if it should
auto-spawn in new sessions.

**7. Add to `los.toml`** as a default module entry.

**8. Handle save/load:** implement `save_module_state` / `load_module_state`
calls for the new Params struct.

**9. Test:** `cargo build`, then `los delay 0` in a tmux pane or add to
`los.toml`.

**10. Update docs:** Add the module's role to the SHM role matrix (Section 9.2)
and modulation bus channel allocation (Section 7.4) if applicable.

**Current limitations:**
- Module names are limited to 15 characters (manifest constraint)
- Max 16 simultaneous registered modules (manifest size)
- No way to add modules without rebuilding the binary
- Consumer IDs must be manually coordinated; collision causes missed events
- The startup boilerplate (~60 lines) must be copy-pasted from an existing module

---

## 12. Future Direction

### 12.1 LosModule Trait

The module lifecycle should be formalized as a Rust trait:

```rust
pub trait LosModule: Sized {
    type Params: Serialize + DeserializeOwned + Default;

    fn module_name() -> &'static str;
    fn produces_audio() -> bool { false }
    fn consumes_events() -> Option<usize> { None }

    fn init(instance: usize) -> Result<Self>;
    fn thread_main(
        state: Arc<Mutex<Self>>,
        shutdown: Receiver<()>,
        instance: usize,
    ) -> Result<()>;
    fn handle_key(&mut self, key: KeyEvent) -> bool;
    fn draw(&self, f: &mut Frame, area: Rect);
}
```

A derive macro could generate the `run()` function boilerplate (signals, PID
file, terminal init, manifest, state load, thread spawn, event loop). This
eliminates the 60-line copy-paste and makes the contract enforceable at compile
time rather than by convention.

A `LosModule` registry could then replace the hardcoded `dispatch_module()`
match, enabling module discovery via procedural macros or build-time registration.

### 12.2 Dynamic Consumer ID Allocation

The current convention (`consumer_id = instance % 16`) works but is fragile.
A negotiation protocol would:

1. Reserve one consumer ID (e.g., 15) for "negotiation requests"
2. New consumer writes a request event with its desired module name to consumer
   15's read index
3. The conductor (or first module to start) assigns a free consumer ID and
   writes a response
4. Consumer reads the response and opens with the assigned ID

The stale consumer pointer problem (crashed module's read index blocking the
buffer) could be mitigated by the conductor periodically checking
`Process::id()` against manifest PIDs and resetting read indices for dead
consumers.

### 12.3 Plugin System

Longer-term, modules could be dynamically loaded shared libraries (.so/.dylib)
implementing the `LosModule` trait. The conductor would:

1. Scan a `plugins/` directory for `.so` files
2. `dlopen` each one, look up a `fn create_los_module() -> Box<dyn LosModule>`
   symbol
3. Dispatch to the module for initialization and lifecycle

This would enable externally-contributed modules without rebuilding los itself.

### 12.4 Protocol Versioning

As the protocol stabilizes, version fields should be added to each SHM object
header:

- `AudioRingbuf` header currently has 36 bytes reserved at offset 28 — a
  `version: u32` at offset 28
- `EventRingbuf` header has space at offset 8-15 for a version field
- `ModulationBus` already has `version = 1` at offset 0
- `ShmTransport` has 48 bytes reserved at offset 16 — room for version +
  extensions
- `Manifest` already has `version = 1` at offset 0

A negotiated version protocol: opener reads the version, and if it's higher
than supported, falls back gracefully (read-only for unknown fields, or refuse
to open).
