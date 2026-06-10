# vhs tape — records docs/demo.gif from YOUR curated session.
# Used by `just demo` when ~/.config/los/states/demo.toml exists.
#
# Curate it: arrange a session that looks (and sounds) good, save it from
# the conductor under the name "demo", LEAVE IT STOPPED (the tape presses
# Space to start it), then run `just demo`. The saved tmux layout is
# portable — tmux rescales it to this terminal size.

Output docs/demo.gif

Set FontSize 14
Set Width 1300
Set Height 840
Set Padding 4
Set Framerate 15
Set Theme { "background": "#0d0b08", "foreground": "#e8dcc8" }

# Launch off-camera: the gif opens on the rig, not on a shell prompt.
Hide
Type `export COLORTERM=truecolor; PATH=target/release:$PATH los load "$HOME/.config/los/states/demo.toml"`
Enter
Sleep 6s
Show

Sleep 2s

# Roll it.
Space
Sleep 12s

# Wander the rig while it grooves.
Ctrl+B
Sleep 200ms
Up
Sleep 4s
Ctrl+B
Sleep 200ms
Left
Sleep 5s
