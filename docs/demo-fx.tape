# vhs tape — records docs/demo-fx-raw.mp4 (run via `just demo`)
#
# The fx rack window: the delay's tap ladders answering the pattern and
# the filterbank's wall strolling under the drunk window + morph LFO.
# Same timing contract as demo.tape (play at video t=3.0, 3.2432s bars);
# the GIF cut is bar 2, like the hero.
Output docs/demo-fx-raw.mp4

Set FontSize 14
Set Width 1200
Set Height 1080
Set Padding 4
Set Framerate 20
Set TypingSpeed 1ms
Set Theme { "background": "#0d0b08", "foreground": "#e8dcc8" }

Hide
Type `export COLORTERM=truecolor PATH=target/release:$PATH; (sleep 6 && los ctl stop) & (sleep 7 && tmux select-window -t los:fx) & (sleep 11 && los ctl play) & los new`
Enter
Sleep 8s
Show

Sleep 12s
