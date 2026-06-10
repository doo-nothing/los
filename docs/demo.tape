# vhs tape — records docs/demo-raw.mp4 (run via `just demo`)
# https://github.com/charmbracelet/vhs
#
# Records the out-of-the-box instrument: `los new` comes pre-patched
# (t1 melody / t3 bass through pluck envelopes into two voices), so the
# tape just opens the rig and presses play. The justfile recipe then
# cuts the seamless one-loop GIF and muxes the mixer's tape-out audio
# into docs/demo.mp4 — timing below is load-bearing:
#   video t=0   : Show (5s after launch)
#   video t=3.0 : `los ctl play` fires -> first loop starts
#   loop length : exactly 2.0s (16 steps @ 120 BPM, 16th-note steps)
#   gif window  : [5.0, 7.0) = loop 2, cut on the boundary

Output docs/demo-raw.mp4

Set FontSize 14
Set Width 1200
Set Height 1160
Set Padding 4
Set Framerate 20
Set TypingSpeed 1ms
Set Theme { "background": "#0d0b08", "foreground": "#e8dcc8" }

# Launch off-camera: the recording opens on the rig, not a shell prompt.
Hide
Type `export COLORTERM=truecolor PATH=target/release:$PATH; (sleep 8 && los ctl play) & los new`
Enter
Sleep 5s
Show

# paused rig for a beat, then play fires at t=3.0 and it grooves
Sleep 12s

# wander the rig while it plays (after the gif window, t > 7)
Ctrl+B
Sleep 200ms
Up
Sleep 3s
