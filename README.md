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

**Phase 0** — Conductor infrastructure. Session creation, pane layout, module
spawning, global key bindings. Modules are placeholders.

See [DESIGN.md](DESIGN.md) for the full architecture and roadmap.

## License

TBD
