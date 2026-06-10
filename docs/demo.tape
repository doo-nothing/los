# vhs tape — records docs/demo.gif (run via `just demo`)
# https://github.com/charmbracelet/vhs

Output docs/demo.gif

Set FontSize 14
Set Width 1300
Set Height 840
Set Padding 4
Set Framerate 10
Set Theme { "background": "#0d0b08", "foreground": "#e8dcc8" }

# Launch off-camera: the gif opens on the rig, not on a shell prompt.
Hide
Type `export COLORTERM=truecolor; PATH=target/release:$PATH los new`
Enter
Sleep 6s
Show

# Fresh session focuses the sequencer. Sketch a pattern, vi-style.
Sleep 1.5s
Type "i"
Sleep 500ms
Space
Sleep 300ms
Type "lll"
Sleep 200ms
Space
Sleep 300ms
Type "kkk"
Sleep 200ms
Type "lll"
Sleep 200ms
Space
Sleep 300ms
Type "kkkk"
Sleep 200ms
Type "llll"
Sleep 200ms
Space
Sleep 300ms
Type "jjjjjjj"
Sleep 300ms
Escape
Sleep 500ms

# Roll it. Let the meters, scope, playhead, and badge do their thing.
Space
Sleep 9s

# Wander the rig while it grooves.
Ctrl+B
Sleep 200ms
Up
Sleep 3s
Ctrl+B
Sleep 200ms
Left
Sleep 4s
