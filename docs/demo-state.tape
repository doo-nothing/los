# vhs tape — records docs/demo.gif from YOUR curated session.
# Used by `just demo` when ~/.config/los/states/demo.toml exists.
#
# Curate it: arrange a session that looks (and sounds) good, save it from
# the conductor under the name "demo", then run `just demo`. The saved
# tmux layout is portable — tmux rescales it to this terminal size. The
# transport is started with an idempotent `los ctl play` (not a toggle),
# so it doesn't matter whether the state was saved playing or stopped.

Output docs/demo.gif

Set FontSize 14
Set Width 1200
Set Height 1160
Set Padding 4
Set Framerate 10
Set Theme { "background": "#0d0b08", "foreground": "#e8dcc8" }

# Launch off-camera: the gif opens on the rig, not on a shell prompt.
Hide
Type `export COLORTERM=truecolor PATH=target/release:$PATH; (sleep 9 && los ctl play) & los load "$HOME/.config/los/states/demo.toml"`
Enter
Sleep 6s
Show

# `ctl play` fires ~3s in; the rig wakes on camera.
Sleep 14s

# Wander the rig while it grooves.
Ctrl+B
Sleep 200ms
Up
Sleep 4s
Ctrl+B
Sleep 200ms
Left
Sleep 5s
