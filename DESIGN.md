# los — Design Document v1.0

A modal, terminal-based music instrument. Not a DAW. Not a plugin host. A dedicated
instrument that turns any Linux machine into a keyboard-driven, real-time audio
environment.

---

## 0. Name

The instrument is named **los**, after the prophetic blacksmith in William Blake's
*The Four Zoas* and *Jerusalem*. Los builds Golgonooza, the city of art, by
hammering visions into reality through relentless labour. He is the adversary of
cold reason (Urizen) and the patron of fiery, material creation.

**Why los**

- Short, stark, terminal-friendly. Three lowercase letters. Easy to type, easy to
  remember.
- Evokes weight and sacrifice. "Loss" with a different vowel — suggests
  transformation, burning away the unnecessary.
- Not vanilla. No other major audio tool, Rust crate, or Linux binary claims the
  name. Clean namespace, rich counter-cultural lineage.

**Namespacing**

- Binary: `los` (installed to `/usr/local/bin/`).
- Config paths:
  - `~/.config/los/config.toml`
  - `~/.config/los/presets/*.toml`
- OS image: when delivered as a bootable ISO, the system boots directly into los.
  The distribution is called **los** (not "Los OS").

**Aesthetic note**

This is not a polished commercial DAW. It is a forge. Every key press is a hammer
strike. The interface is modal, precise, and unforgiving — like the smith's anvil.

---

## 1. Philosophy & Guiding Goals

### Core principles

- **Modal interaction (Vim-inspired).** Modes — Normal, Insert, Command — allow
  efficient, mnemonic control without menus or mice.
- **Verb chaining & repeats.** Commands like `c2d` (change two delay parameters),
  `y3k` (yank three sequencer steps) mirror the composability of Vim's editing
  grammar. Exact semantics are discovered through use, not specified in advance.
- **Low CPU / minimal resource use.** Must run confidently on a ThinkPad T410
  (2012) with these targets:

  | State | CPU ceiling | Context |
  |---|---|---|
  | Idle | <3% | TUI rendered, audio thread sleeping |
  | Loaded | <15% | 1 oscillator + delay + filter, 128-sample buffer |
  | Memory | <50 MB RSS | Steady state |

- **Portable.** A single statically linked Rust binary (`x86_64-unknown-linux-musl`)
  plus config files. Runs on any Linux kernel. No glibc dependency, no package
  manager required.
- **Extensible.** Effects and TUI panels loadable as plugins starting in a future
  version. Plugin code runs in sandboxed threads (safety first, real-time second).
  Not in v1.0.
- **Zero configuration by default.** Sensible defaults, autodetection of
  audio/MIDI devices. Boots from a live USB and produces sound immediately.

### Non-goals for v1.0

- Multitrack recording / non-linear editing (use a DAW for that).
- VST / LV2 hosting — only built-in effects. Custom plugin ABI planned for a
  later version.
- Network sync / collaboration.
- GUI or web interface — terminal only.
- Full MIDI CC learning — basic hard-coded or config-file mappings initially.
- Headless / daemon mode — the TUI is the instrument.

### Ship order

- **v1.0**: The `los` Rust binary. Runs on any Linux with ALSA. Developable on
  macOS via CPAL's CoreAudio backend.
- **v2.0**: Bootable ISO. Minimal Linux + los binary + config files for device
  permissions and real-time scheduling.

---

## 2. High-Level Architecture

The instrument is a single Rust binary with two threads.

```
┌──────────────────────────────────────────────────────┐
│                   Main thread (TUI)                   │
│  ┌───────────┐  ┌──────────┐  ┌──────────┐          │
│  │  ratatui   │  │ crossterm│  │   midir  │          │
│  │  renderer  │  │ (input)  │  │ (control)│          │
│  └─────┬─────┘  └────┬─────┘  └────┬─────┘          │
│        │              │             │                │
│        └──────┬───────┴─────────────┘                │
│               │ writes to atomics                    │
│               ▼                                     │
│         Effect parameter atomics                     │
│         Sequencer step atomics                       │
│         Arc<DspGraph> (swapped on structural change) │
│               │                                     │
│               │ reads                                │
│               ▼                                     │
│    ┌────────────────────────────────────┐           │
│    │         Audio thread (cpal)         │           │
│    │  - callback is real-time            │           │
│    │  - runs DSP graph inline            │           │
│    │  - no allocations in callback       │           │
│    │  - reads atomics for parameters     │           │
│    │  - writes output buffer directly    │           │
│    └───────────────┬────────────────────┘           │
│                    │                                │
│                    ▼                                │
│               ALSA (via cpal)                       │
│                                                     │
│  ┌────────────────────────────────────────┐         │
│  │         MIDI thread (midir)             │         │
│  │  - dedicated thread for MIDI callback   │         │
│  │  - sends parsed messages via            │         │
│  │    crossbeam::Sender to TUI thread      │         │
│  └────────────────────────────────────────┘         │
└──────────────────────────────────────────────────────┘
```

### Threading model

**TUI thread (main).** Handles keyboard events, MIDI messages from the channel,
renders the terminal, and updates application state (effect parameters, sequencer
steps). Writes directly to atomics that the audio thread reads. Builds and swaps
`Arc<DspGraph>` when the user adds/removes/reorders effects.

**Audio thread (cpal callback).** Runs at a fixed buffer size (default 128
samples at 48kHz = 2.9ms latency). The callback *is* the DSP processor — it
reads the current `Arc<DspGraph>`, processes the effect chain in order, and
writes samples to the output buffer. No ring buffer. No intermediate queue. No
allocation. No mutex. No communication *to* the TUI thread.

Effects in the graph read their parameters directly from atomics. A parameter
change written by the TUI thread is visible in the next audio buffer at most.

**MIDI thread.** Dedicated thread running the `midir` input callback. Blocks
until the MIDI port closes. Sends parsed events (CC, note on/off, clock) over a
`crossbeam::channel::Sender` to the TUI thread. The MIDI thread never touches
audio data.

### Parameter changes vs. structural changes

Two mechanisms for two kinds of mutation:

| Kind | Mechanism | Example |
|---|---|---|
| Parameter tweak | Direct `AtomicF32` / `AtomicU64` write | Turn delay feedback from 0.3 to 0.4 |
| Structural change | Build new `DspGraph`, swap `Arc` | Add a filter after the delay |

Structural changes involve allocation (building the new graph) and happen on the
TUI thread. The atomic swap of the `Arc` is a single pointer write. The audio
callback sees the new graph on its next iteration — glitch-free.

### Audio backend

- v1.0 production: **ALSA only** (via CPAL's ALSA backend). Most portable,
  lowest-latency, no daemon required.
- Development: **macOS** via CPAL's CoreAudio backend. Same code compiles and
  runs for development convenience. ALSA-specific paths (`/dev/snd`, config
  paths) are guarded with `#[cfg(target_os = "linux")]`.
- JACK and PipeWire: deferred to a future version.

---

## 3. Modal Interaction Model

los borrows Vim's modal editing grammar and applies it to audio manipulation.

### Modes (v1.0)

| Mode | Trigger | Purpose |
|---|---|---|
| **Normal** | Default after startup and after `<Esc>` | Navigation, verb+motion commands, count prefixes (e.g., `5j`, `c2d`) |
| **Insert** | `i`, `a`, etc. | Unfiltered key input for naming, text entry |
| **Command** | `:` | Command-line at bottom of screen. Execute commands like `:save`, `:quit`, `:bpm 120` |

### Verb grammar (Normal mode)

Commands follow the pattern: `[count] verb [motion]`.

- **Count prefix:** A number typed before a verb or motion, repeating it. `5j`
  moves down 5 times. `3df` deletes three effects.
- **Verbs:** Actions on audio entities. `c` (change), `d` (delete/cut), `y`
  (yank/copy), `p` (paste). Additional verbs discovered in use.
- **Motions:** `h/j/k/l` (directional navigation), `w/b` (next/previous entity).
- **Repeat:** `.` repeats the last verb+motion command.

The exact mapping of Vim's text-editing grammar to audio operations (what does
"change two delay parameters" mean concretely?) is intentionally left open. The
right abstraction will emerge from building and playing. The architecture commits
only to the modal framework and the verb count motion pattern.

### Command-line (`:`)

A single-line editor appears at the bottom of the screen. Confirmed commands for
v1.0:

- `:save [name]` — save current state as preset
- `:load [name]` — load preset
- `:quit` / `:q` — exit los
- `:bpm [value]` — set tempo
- `:buffer [size]` — set audio buffer size
- `:device [name]` — select audio device
- `:midi refresh` — rescan MIDI ports

Additional commands added as needed. Vim muscle-memory aliases (`:w` → `:save`,
`:q` → `:quit`) are included.

### Keyboard map summary

Normal mode keys (conceptual, not exhaustive):

```
h/j/k/l    — Navigate (left/down/up/right)
w/b        — Next/previous entity
i          — Enter Insert mode
:          — Enter Command mode
c/d/y      — Change / delete / yank
p          — Paste
.          — Repeat last verb+motion
+/−        — BPM up/down
Space      — Play/pause
q          — Quit (or :q)
```

---

## 4. TUI Layout

Built with **ratatui** for rendering and **crossterm** for input.

### Widgets

```
┌──────────────────────────────────────────────────────────┐
│  effect rack (list of effects with parameter bars)       │
│                                                          │
├──────────────────────────────────────────────────────────┤
│  sequencer grid (rows for tracks, columns for steps)     │
│                                                          │
├──────────────────────────────────────────────────────────┤
│  waveform scope (ascii/braille, optional)                │
│                                                          │
├──────────────────────────────────────────────────────────┤
│  tape transport (4-track status, playhead, record arm)   │
│                                                          │
├──────────────────────────────────────────────────────────┤
│  status line  │ NORMAL │ 120 BPM │ ALSA:hw:0 │ 3% CPU   │
├──────────────────────────────────────────────────────────┤
│  command line / message area                             │
└──────────────────────────────────────────────────────────┘
```

- **Effect rack.** Each effect shows its name and a horizontal bar (ASCII fill
  characters) for each parameter. Selected effect highlighted.
- **Sequencer grid.** Rows = tracks (up to 4), columns = steps (default 16,
  adjustable). Active step highlighted. Step toggled on/off. Character blocks
  for filled/empty, color for velocity/accent.
- **Waveform scope.** Optional. Real-time scrolling waveform of the stereo
  output. Uses Braille patterns or block characters. Can be toggled off to
  reduce CPU.
- **Tape transport.** 4-track recorder status. Per track: arm, mute, level
  meter. Transport: play, stop, record, loop toggle.
- **Status line.** Current mode, BPM, audio device, CPU load percentage.
- **Command line.** Bottom line. Active when in Command mode. Otherwise shows
  last message.

### Rendering

- Re-render on change, not on a fixed timer. ratatui's double-buffering prevents
  flicker.
- Ratatui degrades gracefully on the Linux console (8 colors) and shines on
  modern terminals (true color).
- No per-frame allocations of large strings. Use `Frame::render` and `Buffer`.

### Input handling

- crossterm raw mode captures all keys including modifiers.
- Event loop uses `crossterm::event::poll()` with a ~16ms timeout to also check
  the MIDI channel and audio thread health.
- Terminal resize (SIGWINCH) handled via crossterm events.

---

## 5. DSP Engine & Effects

### Design

- The DSP graph is a **linear effect chain** for v1.0. Input → Effect 1 → Effect
  2 → … → Output.
- Branching (DAG) and feedback routing deferred to v2.0.
- All DSP runs inline in the audio callback. No allocation, no locking, no
  syscalls.
- Trait-based: each effect implements a common trait.

### Effect trait

```rust
pub trait Effect: Send {
    fn process(&mut self, input: &[f32], output: &mut [f32]);
    fn set_param(&mut self, index: usize, value: f32);
    fn param_count(&self) -> usize;
    fn param_name(&self, index: usize) -> Option<&str>;
}
```

### Built-in effects (v1.0)

| Effect | Parameters |
|---|---|
| **Delay** | time, feedback, mix, highpass filter on repeats |
| **Ping-pong delay** | time, feedback, mix, stereo width |
| **Low-pass / high-pass filter** | cutoff frequency, resonance (biquad) |
| **Saturation / wavefolder** | drive, asymmetry, mix |
| **Reverb** | room size, decay, mix (simple Schroeder or Freeverb-style) |
| **Gain / panner** | level, pan position |

### Deferred to future versions

- Granular freezer
- Pitch shifter
- Convolution reverb
- LFO and envelope modulator nodes

### Parameter control

Each parameter is backed by an `AtomicF32` or `AtomicU64` (for enum-style
params). The TUI thread writes to these atomics. The audio callback reads them
during processing.

Parameters can be controlled by:

1. **Normal mode verbs.** E.g., `=3` increases parameter 3 of the focused effect
   by 0.1. `-3` decreases. Count prefix multiplies delta.
2. **MIDI CC.** Mapping table in `config.toml`: `(device, channel, cc) → (effect_index, param_index)`.
3. **Sequencer per-step modulation.** Each step can have parameter offset
   values (deferred to v1.1).

---

## 6. Voice Engine (Internal Sound Generator)

los includes an internal synthesis voice so the instrument produces sound without
external hardware.

### Architecture

The voice is the first node in the effect chain. It generates samples that flow
through the effects just like an external audio input would. Up to 4 voices
(monophonic per track — one voice per sequencer track).

### Per-voice signal path (v1.0)

```
┌───────────┐   ┌───────────┐   ┌───────────┐   ┌────────┐
│ Osc 1     │   │ Osc 2     │   │           │   │        │
│ (sine,    │   │ (sine,    │   │  Mixer    │   │ Filter │
│  tri,     │───│  tri,     │───│  (levels, │───│ (LP/HP │───► output
│  saw, sq) │   │  saw, sq) │   │   balance)│   │  biquad)│
└───────────┘   └───────────┘   └───────────┘   └────────┘
                                                  │
┌──────────────┐   ┌──────────────┐               │
│  Envelope 1  │   │  Envelope 2  │               │
│  (ADSR)      │   │  (ADSR)      │               │
│  → amp mod   │   │  → filter    │               │
│              │   │     or pitch  │               │
└──────────────┘   └──────────────┘               │
                                                  │
              Feedback bus ◄──────────────────────┘
```

- **2 oscillators** with waveform select (sine, triangle, saw, square).
- **Amplitude envelope** (ADSR) per voice.
- **Modulation envelope** (ADSR) routable to filter cutoff or oscillator pitch.
- **1 biquad filter** per voice (low-pass or high-pass).
- **Feedback bus.** Audio from any point in the effect chain can be routed back
  into earlier stages. This is the "west coast" / modular synth feedback
  philosophy. The TUI provides a feedback routing matrix.
- **Wavefolder / wave shaper** can be inserted into the oscillator mixer path
  (reuses the Saturation effect).

### Deferred to future versions

- Arbitrary oscillator count per voice
- FM, AM, ring modulation between oscillators
- Wavetable / additive partial editing
- Per-voice insert effects (compressor, EQ before main chain)

---

## 7. Sequencer

Step sequencer. Classic X0X-style grid.

### Features

- 16 steps per pattern (default, adjustable up to 64).
- Up to 4 tracks. Each track drives a voice (internal) or sends MIDI note out
  (external) — one source per track.
- Per step: on/off, velocity (0–127), gate length (tie).
- Play/pause, BPM control via Normal mode `+`/`-` and `:bpm`.
- Swing (global percentage, deferred to v1.1).
- Probability and ratcheting (deferred to v1.1).

### TUI representation

```
Track 1: [■][ ][■][ ][■][ ][■][ ][=][ ][■][ ][■][ ][■][ ]
Track 2: [ ][■][ ][■][ ][ ][■][ ][■][=][ ][■][ ][ ][ ][■]
Track 3: [ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ]
Track 4: [ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ][ ]
          1  2  3  4  5  6  7  8  9  10 11 12 13 14 15 16
          ^
          playhead
```

`[=]` marks the current active step. `[■]` is an active (triggered) step. `[ ]`
is inactive. Velocity shown by character brightness or color on supported
terminals.

### Sequencing from MIDI

An external MIDI controller can trigger steps and set per-step velocity via MIDI
note input in "record" mode. Configurable note-to-step mapping.

---

## 8. Tape Mode (4-Track Recorder)

Four-track linear recording and playback. Captures the stereo output of the
instrument.

### Features

- **4 tracks**, each mono or stereo, saved as WAV files in
  `~/.config/los/tapes/`.
- **Record** onto an armed track while the sequencer plays. Audio is captured
  post-effect chain (the full mixed output).
- **Playback.** Each track can be played back independently with volume and pan
  controls.
- **Overdub.** Record new material while hearing existing tracks.
- **Looping.** Set loop region (start/end step). Track loops within that region.
  Overdub into loop creates layered takes.
- **Bounce.** Mix down all playing tracks to a new WAV file.

### TUI controls

Per-track: arm (R), mute (M), solo (S), volume, pan, track name (max 8 chars).
Transport buttons: play, stop, record, loop toggle. Accessed via Normal mode
keys or command-line.

---

## 9. MIDI & Control Input

### MIDI input

- Dedicated thread using `midir`.
- Parser handles: Note On/Off (with velocity), Control Change, Program Change,
  MIDI Clock (for external sync).
- Events sent via `crossbeam::channel::Sender` to TUI thread. TUI translates to
  parameter updates, sequencer triggers, or transport control.
- MIDI clock sync: if present, los slaves its BPM to incoming clock. Fallback to
  internal BPM.

### MIDI output

- Sequencer tracks can send MIDI Note On/Off + velocity to external hardware.
- Output port selectable via `:midi out [device]`.

### Mapping

Configured in `config.toml`:

```toml
[midi_mappings]
# (device_substring, channel, cc) -> (effect_index, param_index)
[[midi_mappings.cc_to_param]]
device = "Launchkey"
channel = 0
cc = 14
target_effect = 0
target_param = 1
```

Hard-coded defaults: CC 1 = mod wheel → filter cutoff on voice 1. CC 7 =
volume. No general-purpose learn mode in v1.0.

---

## 10. Configuration & Presets

### Format

TOML, serialized with `serde`. Human-editable.

### Files

```
~/.config/los/
├── config.toml          # Default audio device, sample rate, MIDI mappings,
│                        # key bindings, startup preset
├── presets/
│   ├── default.toml     # Loaded on startup
│   ├── my-patch.toml    # User preset
│   └── ...
├── tapes/
│   ├── track1.wav
│   └── ...
└── history              # Last 100 command-line commands
```

### Save/load

- `:save [name]` writes current state (effect chain, all parameters, sequencer
  patterns, tape state) to `~/.config/los/presets/<name>.toml`.
- `:load [name]` restores from a preset.
- Save to a temporary file first (`<name>.tmp`), then atomically rename. I/O on
  the TUI thread (which is not the real-time thread — safe).
- Startup: load `default.toml` if it exists, otherwise use hard-coded defaults
  (empty chain, 120 BPM, sine wave voice).

---

## 11. OS Layer Considerations

These are notes for the v2.0 bootable ISO. They do not affect the v1.0 binary
but the code should be written with these realities in mind.

### Real-time safety

1. **CPU governor.** The OS must set the scaling governor to `performance` to
   prevent frequency drops during audio. The TUI reads
   `/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor` and displays a
   warning if it's not performance.
2. **Thread priority.** Audio callback needs `SCHED_FIFO`. The v1.0 binary
   attempts to set this via `libc::pthread_setschedparam`. If it fails (no
   `RLIMIT_RTPRIO`), it logs a warning but continues.
3. **Memory locking.** Real-time threads should `mlockall` to prevent page
   faults. The binary calls `mlockall(MCL_CURRENT | MCL_FUTURE)` on startup if
   permissions allow.
4. **Timer resolution.** `CONFIG_HIGH_RES_TIMERS` = yes in the kernel config.
5. **Disk I/O.** Save/load I/O happens on the TUI thread (not the audio thread).
   No special mitigation needed.

### Device permissions

- User must be in `audio` group for `/dev/snd/*` and `/dev/snd/seq`.
- OS image: `/etc/security/limits.d/audio.conf` with:
  ```
  @audio - rtprio 95
  @audio - memlock unlimited
  ```

### Binary compilation

- Target: `x86_64-unknown-linux-musl`.
- Stripped symbols: `strip = true`.
- Opt level: `opt-level = "z"` for size.
- Release profile with LTO enabled.

### No X11 / Wayland

- crossterm works directly on the Linux console (`/dev/tty`). No display server
  required.
- On the Linux console, only 8 colors are available. Ratatui degrades
  gracefully.
- For a richer terminal experience (true color, better font rendering), users
  can run los inside a terminal emulator of their choice — but it is not
  required.

### Autologin

- The OS boots directly into los with no shell prompt.
- A dedicated `los` user with autologin via `agetty --autologin los`.
- `.profile` or `.bash_profile` runs `los` on login.

---

## 12. Development Roadmap

### Milestone 0: Skeleton

- Rust project with `ratatui`, `crossterm`, `cpal`, `midir`, `crossbeam`.
- Minimal TUI: renders "los" text, quits on `q`.
- Audio thread outputs a 440Hz sine wave.

### Milestone 1: Modal Input & Basic Effects

- Vim-like mode handling (Normal, Insert, Command).
- Command-line (`:`).
- Single delay effect controlled by parameter atomics.
- Effect rack display in TUI.

### Milestone 2: Voice Engine

- 2 oscillators with waveform select.
- 2 ADSR envelopes (amp + modulation).
- Biquad filter per voice.
- Feedback routing matrix.
- Voice as first node in effect chain.

### Milestone 3: Sequencer

- Step sequencer grid (4 tracks, 16 steps).
- Play/pause, BPM control (`+`/`-`, `:bpm`).
- Internal voice triggered by sequencer steps.
- MIDI output from sequencer tracks.

### Milestone 4: MIDI Input

- MIDI input thread with channel.
- CC-to-parameter mapping from config.
- Note-to-step triggering in record mode.
- MIDI clock sync (slave mode).

### Milestone 5: Tape Mode

- 4-track WAV recording.
- Playback with volume/pan per track.
- Overdub and looping.
- Bounce to file.

### Milestone 6: Presets & Configuration

- Save/load presets from TOML.
- Persist audio device, MIDI mappings, key bindings.
- Startup with `default.toml`.

### Milestone 7: Real-time Tuning

- Memory lock, thread priority attempts.
- CPU governor warning display.
- CPU load meter in status line.
- Stress-test under moderate CPU load, verify no xruns.

### Milestone 8: Polish & Testing

- Terminal resize handling.
- Graceful degradation on 8-color consoles.
- Error messages, boundary cases (no audio device, missing config files).
- `--help`, `--version` flags.

---

## 13. Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Audio xruns (buffer underruns) | Increase buffer size via `:buffer`. Set CPU governor to performance. Real-time thread priority. Use CPAL's blocking stream. |
| MIDI jitter | Dedicated high-priority MIDI thread. Use monotonic clock for timing. |
| Terminal flicker | Ratatui double-buffering. Render only on change. |
| Large preset save causes UI stall | I/O on TUI thread (acceptable — audio continues). Save to temp file then atomic rename. |
| No audio device found | Graceful fallback: render TUI, show error message, enter "headless" mode (UI functional, audio silent). |
| Terminal disconnect kills audio | v1.0: process exits. v2.0+: daemon mode considered. |
| Voice engine too CPU-heavy | Per-voice CPU budget enforced. User can reduce oscillator count or disable voices. |
| Feedback routing creates howl/swell | Hard limiter on master output. Visual feedback warning when feedback exceeds threshold. |

---

## Appendix A: Dependencies

| Crate | Purpose | Notes |
|---|---|---|
| `ratatui` | Terminal UI widgets, rendering | Lightweight, active |
| `crossterm` | Terminal raw mode, input, events | Cross-platform |
| `cpal` | Audio device enumeration, stream | ALSA backend |
| `midir` | MIDI input/output | ALSA seq and raw MIDI |
| `crossbeam` | Lock-free channels | MIDI thread → TUI thread |
| `serde` + `toml` | Config file parsing | Standard |
| `libc` | Thread priority, memory lock | Linux-specific, cfged |
| `libloading` | (Future) Dynamic plugin loading | Not in v1.0 |

---

## Appendix B: Verb Grammar — Open Questions

The exact semantics of Vim-like verbs applied to audio are intentionally left as
open questions to be resolved through building and playing:

- What does `c2d` ("change two delay parameters") do? Increment the second
  parameter of the focused effect? Open a parameter list? Enter a "knob mode"?
- What is a "word" in the audio context? An effect? A step? A track?
- What does visual selection mean? (Visual mode is not in v1.0 but is considered
  for future.)
- How does `.` (repeat) work when parameters are continuous values?

The architecture commits to the modal framework and the verb-count-motion
pattern. The exact mapping is a design exploration during development, not a
prerequisite.

---

*End of v1.0 design document.*
