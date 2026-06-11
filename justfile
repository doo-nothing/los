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
    cargo fmt --version >/dev/null 2>&1 && cargo fmt || true

# install to ~/.cargo/bin
install:
    cargo install --path .

# shared post-processing: cut the seamless one-loop gif and mux the
# tape-out audio (expects docs/demo-raw.mp4 + /tmp/los_tape.wav, and the
# tapes' timing contract: play at video t=3.0, loop boundary at t=5.0)
_demo-post:
    ffmpeg -y -loglevel error -ss 5.0 -t 2.0 -i docs/demo-raw.mp4 \
        -vf "fps=20,split[a][b];[a]palettegen[p];[b][p]paletteuse=dither=bayer:bayer_scale=4" \
        -loop 0 docs/demo.gif
    @if command -v gifsicle >/dev/null; then \
        gifsicle -O3 docs/demo.gif -o docs/demo.gif.opt && mv docs/demo.gif.opt docs/demo.gif; \
    fi
    onset=$(ffmpeg -i /tmp/los_tape.wav -af silencedetect=noise=-45dB:d=0.2 -f null - 2>&1 \
        | awk '/silence_end/ && $8 >= 2.0 {print $5; exit}'); \
    onset=${onset:-6.5}; \
    start=$(echo "$onset - 3.0" | bc); \
    echo "audio onset ${onset}s -> trim ${start}s"; \
    ffmpeg -y -loglevel error -ss 2.4 -i docs/demo-raw.mp4 \
        -ss "$(echo "$start + 2.4" | bc)" -i /tmp/los_tape.wav \
        -map 0:v -map 1:a -c:v libx264 -crf 24 -pix_fmt yuv420p \
        -c:a aac -b:a 160k -movflags +faststart -shortest docs/demo.mp4
    rm -f docs/demo-raw.mp4
    @ls -lh docs/demo.gif docs/demo.mp4

# record the demo media from the out-of-the-box fresh session:
# docs/demo.gif (seamless one-loop, the autoplaying README hero) and
# docs/demo.mp4 (same take with tape-out audio). needs vhs + ffmpeg.
demo: build && _demo-post
    @if tmux has-session -t los 2>/dev/null; then \
        echo "error: a 'los' tmux session is already running — save and close it first"; \
        exit 1; \
    fi
    rm -f /tmp/los_tape.wav /tmp/los_tape.wav.done
    target/release/los record 16 /tmp/los_tape.wav &
    vhs docs/demo.tape
    -tmux kill-session -t los 2>/dev/null
    wait

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
