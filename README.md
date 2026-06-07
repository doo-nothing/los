# los

A console-based groovebox/synth workstation. Every module runs in its own
`tmux` pane. Process isolation. Unix pipes. Keyboard-driven.

```
                     ___
                    / _ \
 _ __    ___   ___ | | | |
| '_ \  / _ \ / _ \| | | |
| | | || (_) | (_) \ \_/ /
|_| |_| \___/ \___/ \___/
```

## Quick start

```sh
los
```

Creates a `tmux` session with all modules in tiled panes, attaches you to it.
Use `Ctrl-b` arrows to navigate panes.

### Global commands (via `Ctrl-b` prefix)

| Key | Action |
|-----|--------|
| `Ctrl-b p` | Play |
| `Ctrl-b s` | Stop |
| `Ctrl-b q` | Quit |

### Module commands

Module-specific keybindings work when that pane is focused.

## Project status

**Phase 1 complete** — Full modular synth workstation with save/load.

- ✅ Conductor with session management TUI
- ✅ Sequencer with 8 tracks, Euclidean rhythms, step editing
- ✅ Voice (STO-style waveshaping, sub osc, FM)
- ✅ Mixer (4-track + master)
- ✅ Scope (Braille/HalfBlock/Bars/Dots render modes)
- ✅ Envelope (per-channel rise/fall with modulation)
- ✅ Save/load: full session state (pane order, layout, active pane, params)

See [DESIGN.md](DESIGN.md) for architecture details and [docs/plans/roadmap.md](docs/plans/roadmap.md) for future phases.

## License

TBD
