# Contributing to Los

Los is a modular groovebox in the terminal: every module is its own process
in its own tmux pane, wired together over POSIX shared memory. The best
preparation for hacking on it is to play with it for ten minutes —
`cargo install --path .`, then `los`.

## Prerequisites

- **macOS** (POSIX SHM + tmux; Linux should be close but is untested)
- **Rust** (stable), **tmux**, and [`just`](https://github.com/casey/just)
- [`vhs`](https://github.com/charmbracelet/vhs) only if you re-record the demo

## The bar

```sh
just check   # cargo clippy --all-targets -- -D warnings, then cargo test
```

All clippy warnings are errors. New features and bug fixes come with tests.
A few more style points the codebase holds to:

- No `unwrap()`/`expect()` outside tests — propagate with `?` and add
  context with anyhow's `.context()`.
- Exhaustive `match` over enums — avoid wildcard `_` arms so new variants
  cause compile errors at every site that must handle them.
- Doc comments (`///`, `//!`) on public items; each file opens with a `//!`
  header saying what it is and which design doc it implements.

**Heads-up on tests:** the `ipc::shm` tests operate on real shared memory
under global `/los_*` names. Running them while a live `los` session is
playing (or while another test binary runs concurrently) can cause spurious
failures — quit the session, re-run, and they pass.

## Source layout

```
src/
  main.rs     CLI entry — dispatch, ctl, ps, record
  lib.rs      module groups + crate-root re-exports
  modules/    the runnable panes: sequencer, voice, mixer, envelope,
              scope, tone, badge, conductor
  ui/         shared TUI kit: theme, vi keys, : command line, @ picker, undo
  ipc/        POSIX shared memory (manifest, rings, modbus, transport)
              and modulation routing
  session/    save/load state, house layout, tmux wrapper
```

Every module is re-exported at the crate root, so paths are short
(`los::sequencer`, `crate::shm`) and moving a file between groups never
breaks imports.

## Adding a module

This is the most fun kind of contribution. Start by reading
`src/modules/template.rs` — a small, fully wired worked example with the
why in the comments — alongside the guide in
[docs/writing-a-module.md](docs/writing-a-module.md). (DESIGN.md §11 is
the condensed protocol view; the `//!` header of `src/modules.rs` is the
shortest version.) The shape of it:

1. Create `src/modules/yourmodule.rs` with a
   `pub fn run(instance: usize) -> Result<()>` entry point — copying
   `template.rs` is the intended path. (`tone` is the smallest example,
   `scope` a mid-size TUI one.)
2. Register it: declare in `src/modules.rs`, re-export from `src/lib.rs`,
   add a dispatch arm in `src/main.rs`, and teach the conductor about it
   (`canonical_module`, plus `ADDABLE_MODULES` if `los add` should spawn it).
3. Talk to the rig only through `crate::ipc` — audio out is an
   `AudioRingbuf`, modulation in/out goes through the modbus with stable
   `module/instance/output` addresses, and the manifest is how the mixer
   and picker discover you.
4. Honor the contracts: the editing grammar in
   [docs/keybindings.md](docs/keybindings.md) (vi counts, axis rule,
   `Space`/`?`/`:` everywhere) and the visual language in
   [docs/plans/design-language.md](docs/plans/design-language.md) (use
   `ui::theme` tokens, never raw colors).
5. Add params to `src/session/state.rs` (all fields `Option<T>`) so `:w`
   and `:e` round-trip your state.

## Workflow

- Branch from `master`, open a PR — no direct pushes to `master`.
- Update README / DESIGN.md / `docs/` in the same PR as the code change.
- Commit messages: short imperative headline, then a body explaining what
  and why.

## Where things are written up

- [DESIGN.md](DESIGN.md) — architecture, the SHM protocol, module contract
- [docs/keybindings.md](docs/keybindings.md) — the keybinding doctrine
- [docs/composing.md](docs/composing.md) — the song-file format and the
  check/render/audit loop (agents start at [AGENTS.md](AGENTS.md));
  `just render-smoke` exercises the loop end to end
- [docs/plans/](docs/plans/) — design docs, including the
  [roadmap](docs/plans/roadmap.md) of ideas looking for an author
