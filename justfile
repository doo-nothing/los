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

# install to ~/.cargo/bin
install:
    cargo install --path .

# Record the demo media: docs/demo.gif (seamless ONE-LOOP gif — the
# autoplaying README hero) and docs/demo.mp4 (same video with the
# mixer's tape-out audio muxed in — the click-to-hear clip).
# Records the out-of-the-box fresh session; needs vhs + ffmpeg
# (gifsicle optional). Refuses to run while a live `los` session
# exists — recording spawns (and afterwards kills) its own.
demo: build
    @if tmux has-session -t los 2>/dev/null; then \
        echo "error: a 'los' tmux session is already running — save and close it first"; \
        exit 1; \
    fi
    rm -f /tmp/los_tape.wav /tmp/los_tape.wav.done
    target/release/los record 16 /tmp/los_tape.wav &
    vhs docs/demo.tape
    -tmux kill-session -t los 2>/dev/null
    wait
    # one exact sequencer loop (2.0s @ 120 BPM), cut on the loop boundary
    ffmpeg -y -loglevel error -ss 5.0 -t 2.0 -i docs/demo-raw.mp4 \
        -vf "fps=20,split[a][b];[a]palettegen[p];[b][p]paletteuse=dither=bayer:bayer_scale=4" \
        -loop 0 docs/demo.gif
    @if command -v gifsicle >/dev/null; then \
        gifsicle -O3 docs/demo.gif -o docs/demo.gif.opt && mv docs/demo.gif.opt docs/demo.gif; \
    fi
    # mux the tape: first pluck in the wav aligns to video t=3.0
    onset=$(ffmpeg -i /tmp/los_tape.wav -af silencedetect=noise=-45dB:d=0.2 -f null - 2>&1 \
        | grep -m1 silence_end | sed 's/.*silence_end: \([0-9.]*\).*/\1/'); \
    onset=$${onset:-6.5}; \
    start=$(echo "$$onset - 3.0" | bc); \
    echo "audio onset $${onset}s -> trim $${start}s"; \
    ffmpeg -y -loglevel error -ss 2.4 -i docs/demo-raw.mp4 \
        -ss "$$(echo "$$start + 2.4" | bc)" -i /tmp/los_tape.wav \
        -map 0:v -map 1:a -c:v libx264 -crf 24 -pix_fmt yuv420p \
        -c:a aac -b:a 160k -movflags +faststart -shortest docs/demo.mp4
    rm -f docs/demo-raw.mp4
    @ls -lh docs/demo.gif docs/demo.mp4

# Record from your curated saved state instead (save a session as
# "demo" from the conductor first). Gif only.
demo-state: build
    @if tmux has-session -t los 2>/dev/null; then \
        echo "error: a 'los' tmux session is already running — save and close it first"; \
        exit 1; \
    fi
    @if [ ! -f "$HOME/.config/los/states/demo.toml" ]; then \
        echo "error: no saved 'demo' state"; exit 1; \
    fi
    vhs docs/demo-state.tape
    -tmux kill-session -t los 2>/dev/null
    @if command -v gifsicle >/dev/null; then \
        gifsicle -O3 --lossy=60 docs/demo.gif -o docs/demo.gif.opt && mv docs/demo.gif.opt docs/demo.gif; \
    fi
    @ls -lh docs/demo.gif
