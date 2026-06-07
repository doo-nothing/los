# LOS TUI Migration - Complete

## Overview
All modules have been converted from raw stdout printing to proper ratatui terminal user interfaces. Each module now runs as an independent TUI application in its own tmux pane.

**Key design principle:** tmux manages panes. Modules do not close themselves on Esc.

## Modules

### 1. Conductor (Pane 1)
- **Status**: ✅ Working
- **UI**: Simple text-based command interface
- **Controls**:
  - `j/k`, `↑/↓`: Navigate state list
  - `s`: Save current session state
  - `l`: Load selected state (creates new session)
  - `d`: Delete selected state
  - `?`: Toggle help
  - `q`: Quit module

### 2. Sequencer (Pane 2)
- **Status**: ✅ Working
- **UI**: 16-step grid with visual step indicators
- **Features**:
  - Step grid showing active/inactive steps and note names
  - Transport controls (BPM, play/stop status, current step)
  - Single-mode keybindings — all keys work all the time
- **Controls**:
  - `h/l`, `←/→`: Navigate steps
  - `0`: First step
  - `$`: Last step
  - `w`: Next active step
  - `b`: Previous active step
  - `Enter`: Toggle step on/off
  - `x`: Cut step (copy + deactivate)
  - `y`: Yank step (copy)
  - `p`: Paste step
  - `k/K`: Raise note (semitone/octave)
  - `j/J`: Lower note (semitone/octave)
  - `N<NUM>`: Set note (e.g. `N60` for C4)
  - `<N>P`: Set pulses and fill (e.g. `5P`)
  - `<N>L`: Set pattern length (e.g. `16L`)
  - `<N>R`: Set rotation (e.g. `3R`)
  - `R`: Rotate by 1
  - `n`: New track
  - `d`: Delete current track
  - `[`/`]`: Previous/next track
  - `m`: Toggle mute track
  - `Y`: Yank track (copy)
  - `P`: Paste track after current
  - `space`: Play/pause
  - `s`: Stop
  - `t<NUM>`: Set BPM (e.g. `t140`)
  - `?`: Toggle help
  - `q`: Quit module

### 3. Voice (Pane 3)
- **Status**: ✅ Working
- **UI**: Parameter gauges and controls for STO wave shaping
- **Features**:
  - Shape control (sine → saw → square morphing)
  - Sub oscillator level
  - FM amount
  - Output routing (sine, sine+sub, shaped+sub)
  - ADSR envelope visualization
- **Controls**:
  - `j/k`, `↑/↓`: Select parameter
  - `h/l`, `←/→`: Adjust value
  - `1/2/3`: Set output mode directly
  - `?`: Toggle help
  - `q`: Quit module

### 4. Mixer (Pane 4)
- **Status**: ✅ Working
- **UI**: 4-track mixer with level/pan/mute/solo controls
- **Features**:
  - Per-track level, pan, mute, solo controls
  - Master level control
  - Real-time level meters
- **Controls**:
  - `h/l`, `←/→`: Select track/master
  - `j/k`, `↓/↑`: Decrease/increase level
  - `+`/`-`: Increase/decrease pan
  - `m`: Toggle mute
  - `s`: Toggle solo
  - `?`: Toggle help
  - `q`: Quit module

### 5. Scope (Pane 5)
- **Status**: ✅ Working
- **UI**: Waveform visualization with multiple render modes
- **Features**:
  - 4 render modes: Braille, HalfBlock, Bars, Dots
  - Channel selection: Left, Right, Stereo
  - Zoom control (0.5x - 10.0x)
  - Gain control (1.0x - 5.0x)
- **Controls**:
  - `m`: Cycle render mode
  - `c`: Cycle channel mode
  - `+`/`=`: Zoom in
  - `-`: Zoom out
  - `g`: Increase gain
  - `G`: Decrease gain
  - `t/T`: Trigger level
  - `?`: Toggle help
  - `q`: Quit module

## Architecture

### Shared State
Each module uses `Arc<Mutex<State>>` to share state between:
- Audio processing thread (reads/writes audio data)
- UI rendering thread (displays state, handles input)

### IPC via SHM
Modules communicate through POSIX shared memory:
- **Audio ringbuffer** (`/los_mix_in`): Voice writes audio, mixer reads it
- **Event ringbuffer** (`/los_events`): Sequencer writes note events, voice reads them
- **Transport clock** (`/los_transport`): Shared timing reference

### tmux Session
- Session name: `los`
- Layout: 5 panes in even-vertical arrangement
- Window size: Tracks largest attached client
- Pane borders: Show module names
- **Modules never close on Esc** — use tmux to manage panes

## Testing

Run the system:
```bash
./target/release/los
```

This will:
1. Create a tmux session named "los"
2. Spawn all 5 modules in separate panes
3. Attach to the session

To quit a module: Press `q`
To detach from tmux: `Ctrl-b d`

## Dependencies Added

- `ratatui = "0.28"`: Terminal UI framework
- `crossterm = "0.28"`: Terminal manipulation backend
