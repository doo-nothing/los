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

## 4. Phases

| Phase | Deliverable | Status |
|-------|-------------|--------|
| **0** | Conductor: tmux session creation, pane layout, module spawning, global key bindings, conductor CLI | ✅ |
| **1** | SHM ringbuffer crate (`shm.rs`) + `los mixer` (cpal output, reads SHM) + `los tone` writes to SHM | ✅ |
| **2** | `los voice` — synth engine (oscillators, ADSR, filter), writes audio to SHM | 🔜 |
| **3** | `los sequencer` — step sequencer TUI, writes events to SHM | |
| **4** | `los scope` — reads mixer SHM, ASCII oscilloscope | |
| **5** | Effects (delay, reverb, filter), recorder, patch save/load, polish | |
