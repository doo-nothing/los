# Writing a Module

The hands-on guide to adding a new module to los. The matching worked
example is [`src/modules/template.rs`](../src/modules/template.rs) — a
small, fully wired module written to be read top to bottom and copied.
This document is the map; the template is the territory.

For the formal protocol (SHM layouts, lifecycle, save contract) see
[DESIGN.md](../DESIGN.md) §7–§11. For writing your audio-rate core in a
DSP language instead of Rust, see [writing-dsp.md](writing-dsp.md).

## The big picture

A los module is **one OS process in one tmux pane**. There is no plugin
API, no trait to implement, no callback registry. A module is a
`pub fn run(instance: usize) -> Result<()>` that:

- registers itself in the shared-memory **manifest** so other modules can
  discover it,
- produces and/or consumes signals over shared memory (audio
  ringbuffers, the 64-channel **modbus**, the event ring, the transport
  clock),
- draws a ratatui UI that speaks the house vi grammar
  ([keybindings.md](keybindings.md)),
- saves and reloads its state when the conductor signals it.

Because modules are processes, your module can crash, be killed, be
restarted, or lag without taking the session down. The mixer reaps dead
channels; the manifest reaps dead entries; bindings re-resolve when a
module comes back. Lean on that: fail loudly in development, the rig
survives it.

## Start here

1. Copy `src/modules/template.rs` to `src/modules/yourmodule.rs`.
2. Rename "template" throughout (module name strings, state struct,
   labels). Names must be ≤15 chars (manifest field limit).
3. Wire the registration points (next section).
4. `cargo build && los new`, then `los add yourmodule` in the session.
5. Replace the template's LFO/drone with your actual idea.

## The registration checklist

Grep for `template` to see every one of these in place. A new module
touches:

| Where | What |
|---|---|
| `src/modules/yourmodule.rs` | the module itself |
| `src/modules.rs` | `pub mod yourmodule;` |
| `src/lib.rs` | add to the `pub use modules::{…}` re-export |
| `src/main.rs` | `dispatch_module` match arm + a usage line |
| `src/modules/conductor.rs` | `canonical_module()` (name + aliases) and `ADDABLE_MODULES` (enables `los add`) |
| `src/ipc/routing.rs` | `output_labels()` — labels for your claimed modbus channels, in claim order |
| `src/session/state.rs` | your `YourmoduleParams` serde struct |
| `docs/keybindings.md` | your module's key table (if it adds any beyond the doctrine) |
| `DESIGN.md` §9.2 | the SHM role matrix row |

Optional, only if the module should appear in **fresh** sessions: the
house layout in `conductor.rs` (`HOUSE_TITLES`, `build_house_layout()`,
`house_dims()`) and `los.toml`.

## Choosing your I/O

Decide what the module *is* in signal terms; everything else follows.

**Produces audio** → create an `AudioRingbuf` named
`/los_audio_<module>_<instance>` and pass the name to
`Manifest::register`. The mixer discovers it within ~500 ms and gives you
a strip. You write 64-frame stereo blocks at your own pace (see the
template's `audio_thread` for the pacing idiom). You never touch the
audio device — only the mixer talks to cpal.

**Produces modulation** → claim channels at registration
(`manifest.register(…, n)`), publish with `ModulationBus::set` from your
signal thread, and list your output labels in `routing::output_labels()`.
Your outputs then appear in every module's `@` picker as
`yourmodule/<instance>/<label>`. Claims are contiguous and fixed at
registration — claim your maximum up front.

**Consumes modulation** (params with mod inputs) → store bindings as
`SourceAddr`esses ("envelope/0/ch1"), resolve them to live channels
through the manifest periodically (~170 ms — the template shows the
cadence), and read with `ModulationBus::get` in the hot loop. The
convention everywhere in los: **a bound source replaces the manual
value**, the knob becomes a display, and the UI shows the live value in
the cable's color.

**Consumes notes** → open the `EventRingbuf` with
`EventRingbuf::open_dynamic()` and drain it in your signal thread (see
voice.rs). Slots are claimed atomically at boot (32 available) and
freed on drop or unclean death — never hardcode a consumer ID; two
processes sharing a cursor steal each other's notes at random.

**Consumes audio** → today only the mixer consumes audio ringbuffers
(they're single-consumer SPSC). FX-style modules that sit between a
source and the mixer are the next architectural step — see
`docs/plans/` for the design work.

## The editing grammar

Non-negotiables, all demonstrated in the template (doctrine table at the
top of [keybindings.md](keybindings.md)):

- **Axis rule** — navigate along your layout's visual axis, adjust on the
  perpendicular. Vertical param list: j/k select, h/l adjust, H/L coarse.
- **Counts** (`5l`), **`gg`/`G`**, **`0`** reset, **`?`** help.
- **`@`** opens the shared source picker on bindable params; **`x`**
  unbinds. Use `crate::picker::Picker` — don't roll your own.
- **`u`/`Ctrl-r`** undo/redo via `crate::undo::ParamHistory`: map your
  fields onto slots, record old/new around every mutation, and sweeps
  coalesce correctly for free.
- **`:`** ex line via `crate::excmd::ExLine` — `:w`/`:e` patches,
  `:q`/`:q!` with a real dirty check, `:set <param> <value>`.
- **`Space`** toggles the global transport, **`Ctrl-s`** saves.
- **Mouse** mirrors the keyboard, never extends it: wheel = adjust,
  click = select, identical undo behavior.

## State & persistence

One pair of functions — `snapshot_params` / `apply_params` — bridges your
live state to a serde struct in `src/session/state.rs`. That same pair
serves all three persistence paths: SIGUSR1/SIGUSR2 session saves driven
by the conductor, manual `Ctrl-s`, and `:w`/`:e` patches.

Rules that keep old saves loading forever:

- Every field `Option<T>` or `#[serde(default)]`.
- Apply only what's present; absent fields keep current values.
- Serialize enums by name, not index.
- Bindings serialize as address strings, never channel numbers.

## UI

Use the shared `theme::` components for everything — header, rule,
status line, bars, meters, colors. The color law (cable color wins,
signal types keep fixed hues) lives in
`docs/plans/design-language.md`. Branch on pane height and degrade
gracefully; never panic on a small pane (DESIGN.md §11.5 is the
geometry contract). If you draw value bars, compute their width with
`theme::bar_width` and use the same call in your mouse hit-tests.

## Threads & real time

The house pattern is two threads sharing one `Arc<Mutex<State>>`:

- **Signal thread**: owns every RT resource (ringbuffer, manifest
  registration, modbus), produces one 64-frame block per iteration,
  locks the mutex briefly per block. No allocation, no string parsing,
  no manifest walking in the per-block path — resolve addresses on a
  slow cadence, cache channel indices.
- **UI thread**: ratatui event loop, 50 ms poll, draws from a short
  lock.

A 64-frame block is ~1.3 ms at 48 kHz; the 16-slot ringbuffer gives you
~21 ms of slack. You do not need a real-time-safe allocator — you need
to not do silly things per block.

## Tests

Test the pure parts as plain unit tests in your module file: param
stepping/clamping, mod-value mapping, undo slot round-trips, params
TOML round-trips, `:set` parsing. The template's test module is the
checklist. Don't touch live SHM in `cargo test` — the ipc tests cover
the plumbing with `/los_test_*` objects.

The bar before a PR: `just check` (clippy with `-D warnings` + full
test suite) — see [CONTRIBUTING.md](../CONTRIBUTING.md).

## Things that bite

- **Dropping your `Manifest` handle unregisters you.** The handle's
  lifetime is your registration's lifetime — keep it in the signal
  thread's stack frame.
- **`Ctrl` keys arrive as `Char(c) + CONTROL`** — match them before your
  bare `Char` arms or `Ctrl-r` becomes "r".
- **A leading `0` is a reset, not a count** — `keys::Count::push`
  already refuses it; route the refused `'0'` to your reset binding.
- **Claim sizing**: `routing::output_labels()` length must match your
  claimed channel count, or the picker shows outputs that don't resolve
  (or hides ones that should).
- **tmux respawns panes fast** — keep the raw-mode retry loop from the
  template or you'll flake on restart.
