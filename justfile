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

# re-record docs/demo.gif with vhs (brew install vhs)
# Uses your curated session if you've saved one as "demo" (arrange a
# session, save it from the conductor under that name, leave it stopped);
# falls back to the fresh-session choreography otherwise.
# Refuses to run while a live `los` session exists — recording spawns
# (and afterwards kills) a session of its own.
demo: build
    @if tmux has-session -t los 2>/dev/null; then \
        echo "error: a 'los' tmux session is already running — save and close it first"; \
        exit 1; \
    fi
    @if [ -f "$HOME/.config/los/states/demo.toml" ]; then \
        echo "recording from your saved 'demo' state"; \
        vhs docs/demo-state.tape; \
    else \
        echo "no saved 'demo' state — recording the fresh-session choreography"; \
        vhs docs/demo.tape; \
    fi
    -tmux kill-session -t los 2>/dev/null
    @ls -lh docs/demo.gif
