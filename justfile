# los — task runner (https://github.com/casey/just)

# list recipes
default:
    @just --list

# clippy with warnings as errors + full test suite
check:
    cargo clippy --all-targets -- -D warnings
    cargo test

# release build
build:
    cargo build --release

# regenerate the committed Faust DSP cores (requires `brew install faust`;
# building los itself never does — see docs/writing-dsp.md)
dsp:
    faust -lang rust -cn Tap8Fx src/modules/delay/tap8fx.dsp -o src/modules/delay/tap8fx_gen.rs
    faust -lang rust -cn Bank16 src/modules/filterbank/bank16.dsp -o src/modules/filterbank/bank16_gen.rs
    faust -lang rust -cn Swarm src/modules/swarm/swarm.dsp -o src/modules/swarm/swarm_gen.rs
    cargo fmt --version >/dev/null 2>&1 && cargo fmt || true

# install to ~/.cargo/bin
install:
    cargo install --path .

# shared post-processing: cut the seamless one-bar gifs (hero = the
# console, fx = the rack) and mux the tape-out audio into docs/demo.mp4.
# Timing contract (see docs/demo.tape): play fires at video t=3.0 and a
# bar at 74 BPM is 3.2432s — both gifs are bar 2, cut on the boundary.
_demo-post:
    ffmpeg -y -loglevel error -ss 6.243 -t 3.243 -i docs/demo-raw.mp4 \
        -vf "fps=20,split[a][b];[a]palettegen[p];[b][p]paletteuse=dither=bayer:bayer_scale=4" \
        -loop 0 docs/demo.gif
    ffmpeg -y -loglevel error -ss 6.243 -t 3.243 -i docs/demo-fx-raw.mp4 \
        -vf "fps=20,split[a][b];[a]palettegen[p];[b][p]paletteuse=dither=bayer:bayer_scale=4" \
        -loop 0 docs/demo-fx.gif
    @if command -v gifsicle >/dev/null; then \
        gifsicle -O3 docs/demo.gif -o docs/demo.gif.opt && mv docs/demo.gif.opt docs/demo.gif; \
        gifsicle -O3 docs/demo-fx.gif -o docs/demo-fx.gif.opt && mv docs/demo-fx.gif.opt docs/demo-fx.gif; \
    fi
    onset=$(ffmpeg -i /tmp/los_tape.wav -af silencedetect=noise=-45dB:d=0.2 -f null - 2>&1 \
        | awk '/silence_end/ && $8 >= 2.0 {print $5; exit}'); \
    onset=${onset:-6.5}; \
    start=$(echo "$onset - 3.0" | bc); \
    echo "audio onset ${onset}s -> trim ${start}s"; \
    ffmpeg -y -loglevel error -i docs/demo-raw.mp4 \
        -ss "$start" -i /tmp/los_tape.wav \
        -map 0:v -map 1:a -c:v libx264 -crf 24 -pix_fmt yuv420p \
        -c:a aac -b:a 160k -movflags +faststart -shortest docs/demo.mp4
    rm -f docs/demo-raw.mp4 docs/demo-fx-raw.mp4
    @ls -lh docs/demo.gif docs/demo-fx.gif docs/demo.mp4

# record the demo media from the out-of-the-box fresh session:
# docs/demo.gif (seamless one-loop, the autoplaying README hero) and
# docs/demo.mp4 (same take with tape-out audio). needs vhs + ffmpeg.
demo: build && _demo-post
    @if tmux has-session -t los 2>/dev/null; then \
        echo "error: a 'los' tmux session is already running — save and close it first"; \
        exit 1; \
    fi
    rm -f /tmp/los_tape.wav /tmp/los_tape.wav.done
    target/release/los record 19 /tmp/los_tape.wav &
    vhs docs/demo.tape
    -tmux kill-session -t los 2>/dev/null
    wait
    vhs docs/demo-fx.tape
    -tmux kill-session -t los 2>/dev/null

# same, but from a curated saved state: arrange + save a session from
# the conductor, then `just demo-state state=NAME`
demo-state state="demo": build && _demo-post
    @if tmux has-session -t los 2>/dev/null; then \
        echo "error: a 'los' tmux session is already running — save and close it first"; \
        exit 1; \
    fi
    @if [ ! -f "$HOME/.config/los/states/{{state}}.toml" ]; then \
        echo "error: no saved state {{state}}"; exit 1; \
    fi
    rm -f /tmp/los_tape.wav /tmp/los_tape.wav.done
    target/release/los record 16 /tmp/los_tape.wav &
    LOS_DEMO_STATE={{state}} vhs docs/demo-state.tape
    -tmux kill-session -t los 2>/dev/null
    wait
