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
# Refuses to run while a live `los` session exists — recording spawns
# (and afterwards kills) a session of its own.
demo: build
    @if tmux has-session -t los 2>/dev/null; then \
        echo "error: a 'los' tmux session is already running — save and close it first"; \
        exit 1; \
    fi
    vhs docs/demo.tape
    -tmux kill-session -t los 2>/dev/null
    @ls -lh docs/demo.gif
