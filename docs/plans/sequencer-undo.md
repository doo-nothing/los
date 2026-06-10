# Sequencer Undo/Redo

**Status: ✅ Complete** (Phase 7; later generalized — `undo.rs` ParamHistory
gives every module the same `u`/`Ctrl-r` with sweep coalescing)

## Goal
Add Vi-style undo/redo to the sequencer: `u` = undo, `Ctrl-r` = redo.

## Design: Command Pattern

Each undoable action creates a `Command` value that stores enough state to reverse
it. A `History` struct manages the command list and undo/redo index.

```
Command list:  [cmd0, cmd1, cmd2, ...]
                    ^
                    |
                 index (next to undo)
```

- `undo()` decrements index and reverses `commands[index]`
- `redo()` applies `commands[index]` and increments index
- `push()` truncates any redo commands after index, then appends and advances

### Command variants

| Variant | Stores | Undoable actions |
|---------|--------|------------------|
| `ToggleStep { track, step, was_active }` | old active state | Insert mode Enter/Space |
| `EditStep { track, step, old_step, new_step }` | full step twice | cut, paste, transpose, set-note |
| `SetTrackParams { track, old, new }` | `EuclidState` snapshots (pulses/length/rotation + steps) | P/L/R (count-prefixed and insert-mode) |
| `ToggleMute { track, was_muted }` | old mute | normal mode m |
| `ToggleMode { track, was_mode }` | old mode | normal mode @ |
| `NewTrack { at }` | insertion index | normal mode n |
| `DeleteTrack { at, track }` | deleted track data | normal mode dd |
| `PasteTrack { at, track }` | inserted track data | normal mode P |
| `SetBpm { old_bpm, new_bpm }` | old/new BPM | submode bpm Enter |

### No-op filtering

Actions that change nothing are NOT recorded: paste with an empty clipboard,
transpose at the note range limit, re-applying an unchanged Euclidean pattern,
setting BPM to its current value. Otherwise `u` would silently "undo" invisible
commands and feel broken.

### Cursor focus

Like vi, undo/redo moves the cursor (`current_track`/`selected`) to the change
site, clamped so it stays valid when track count or length changes. Track
insert/remove also keeps the `current_steps`/`last_notes` bookkeeping vectors
in sync (shared `insert_track`/`remove_track` helpers).

### Undo/redo logic

Each variant implements `undo(state)` and `redo(state)` directly on `SequencerState`:
- **undo**: restore old values from the command
- **redo**: apply new values from the command (same as forward action)

### History size cap

Capped at 100 commands. When full, the oldest command is dropped on push.

## UI Changes

- **`u`** in normal mode: undo last action; show "Undo: <description>" in status bar
- **`Ctrl-r`** (both modes): redo last undone action; show "Redo: <description>"
- **Counts**: `3u` / `3<Ctrl-r>` undo/redo 3 times, stopping early if history
  runs out; status shows "Undo ×3: <description>" with the actual count done
- With empty history, show "Nothing to undo" / "Nothing to redo" so the keypress is acknowledged
- Status message auto-clears after 2 seconds or on next keypress

## Save/Restore

Deferred — not implemented in this phase. The undo history is in-memory only.
A future phase can serialize commands alongside `SequencerParams`.

## Test Plan

Implemented in `#[cfg(test)] mod tests` at the bottom of `src/modules/sequencer.rs`
(24 tests), plus extras: history cap, no-op filtering, redo clamps the
selected step after a length change, undo focuses the changed location,
count-prefixed undo/redo (runs N times, stops at history end).

1. Toggle step, undo, verify step is restored
2. Cut step, undo, verify step is restored
3. Paste step, undo, verify step is restored
4. Transpose up, undo, verify note is restored
5. Delete track, undo, verify track is restored
6. New track, undo, verify track count is restored
7. Multi-step: toggle, toggle, undo, undo, verify both restored
8. Redo after undo, verify action is re-applied
9. New action after undo clears redo history
10. Mute toggle + undo
11. Mode toggle + undo
