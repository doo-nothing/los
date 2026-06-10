# vhs tape — records docs/demo-raw.mp4 from a CURATED saved state.
# Run via `just demo-state state=NAME` (default "demo"); the recipe sets
# $LOS_DEMO_STATE and handles the gif cut + audio mux afterwards.
#
# Timing contract (same as demo.tape — the recipe depends on it):
#   video t=0   : Show (5s after launch)
#   video t=3.0 : `los ctl play` fires -> first loop starts
# A saved state may have been saved PLAYING, so the launch line stops
# the transport at +4s (off-camera) and replays at +8s — the rig is
# paused when the camera opens and the audio gets its >=2s silence gap
# for onset alignment either way.

Output docs/demo-raw.mp4

Set FontSize 14
Set Width 1200
Set Height 1080
Set Padding 4
Set Framerate 20
Set TypingSpeed 1ms
Set Theme { "background": "#0d0b08", "foreground": "#e8dcc8" }

Hide
Type `export COLORTERM=truecolor PATH=target/release:$PATH; (sleep 4 && los ctl stop) & (sleep 8 && los ctl play) & los load "$HOME/.config/los/states/${LOS_DEMO_STATE:-demo}.toml"`
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
