# vhs tape — records docs/demo-raw.mp4 (run via `just demo`)
# https://github.com/charmbracelet/vhs
#
# Records the out-of-the-box instrument. `los new` opens already playing
# the house drone, so the tape stops the transport off-camera and fires
# play at a KNOWN video time — the GIF cut lands on bar boundaries.
# Timing contract (the justfile's ffmpeg cuts depend on it):
#   video t=0    : Show (8s after launch; stop fired at 6s)
#   video t=3.0  : `los ctl play` fires — bar 1 begins
#   bar length   : 60/74*4 = 3.2432s (16 steps @ 74 BPM)
#   hero gif     : [6.243, 9.486) = bar 2, cut on the boundary
#   window tour  : t≈11 switches to the fx rack for the video
Output docs/demo-raw.mp4

Set FontSize 14
Set Width 1200
Set Height 1080
Set Padding 4
Set Framerate 20
Set TypingSpeed 1ms
Set Theme { "background": "#0d0b08", "foreground": "#e8dcc8" }

# Launch off-camera: the recording opens on the rig, not a shell prompt.
Hide
Type `export COLORTERM=truecolor PATH=target/release:$PATH; (sleep 6 && los ctl stop) & (sleep 11 && los ctl play) & (sleep 19 && tmux select-window -t los:fx) & los new`
Enter
Sleep 8s
Show

# a paused beat, then play fires at t=3.0 and the drone breathes;
# the scheduled select-window tours the fx rack at t=11 (after the
# gif window)
Sleep 17s
