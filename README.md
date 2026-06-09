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

### Global transport

| Key | Action |
|-----|--------|
| `Space` | Play/pause (in any module pane; sequencer insert mode keeps Space = toggle step) |
| `Ctrl-b p` | Play/pause (tmux prefix, works from any pane) |
| `Ctrl-b s` | Stop |
| `los ctl play\|stop\|toggle\|status` | Control transport from any shell |

The prefix bindings only take effect inside the `los` session — in other tmux
sessions `Ctrl-b p`/`Ctrl-b s` keep their stock behavior.

### Module commands

Module-specific keybindings work when that pane is focused.

## Project status

**Phase 7 complete** — Full modular synth workstation with save/load and sequencer undo/redo.

- ✅ Conductor with session management TUI
- ✅ Sequencer with 8 tracks, Euclidean rhythms, step editing, vi-style undo/redo (`u` / `Ctrl-r`)
- ✅ Voice (STO-style waveshaping, sub osc, FM)
- ✅ Mixer (dynamic channels, manifest-driven discovery)
- ✅ Scope (Braille/HalfBlock/Bars/Dots render modes)
- ✅ Envelope (per-channel rise/fall with modulation)
- ✅ Save/load: full session state (pane order, layout, active pane, params)
- ✅ Module lifecycle: add/remove modules at runtime, `/los_manifest` SHM registry, per-module audio ringbuffers
- ✅ Mixer auto-detects new audio sources, dynamically creates channels
- ✅ CLI: `los --help`, module aliases (`sto` → voice, `maths` → envelope)

See [DESIGN.md](DESIGN.md) for architecture details and [docs/plans/roadmap.md](docs/plans/roadmap.md) for future phases.

## License

TBD
