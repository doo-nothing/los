# Writing DSP — Rust or Faust

los modules can write their audio-rate core two ways: plain Rust (the
mixer's `dsp.rs`, the delay's line and taps) or the
[Faust](https://faust.grame.fr) DSP language compiled to Rust at
development time. This is the guide to both, and to choosing.

The worked example is the delay (docs/plans/delay-288.md): its delay
line, taps, and envelope followers are hand-written Rust in
`src/modules/delay/dsp.rs`, while its shimmer/reverb feedback block is
Faust in `src/modules/delay/tap8fx.dsp`. Read those two files side by
side and you have the whole story.

## Choosing

**Write Rust when** the DSP is entangled with module state — per-tap
params, swept read heads, conditional routing, anything the UI pokes
per-sample. You get ordinary testing, ordinary debugging, no toolchain.

**Write Faust when** you want algorithms that are *hard to get right* —
reverbs, pitch shifters, filter banks, physical models. Faust's stdlib
is decades of audited DSP: `re.mono_freeverb`, `ef.transpose`,
`fi.svf`, `an.amp_follower` are one line each, and the compiler does
whole-graph optimization. A 16-band filter bank that would be 400 lines
of careful biquad code is 20 lines of Faust (that's the plan for the
296e — docs/plans/filterbank-296e.md).

A third option for in-between cases: the `fundsp` crate is a pure-Rust
DSP DSL (graph expressions, no external compiler). We don't use it yet;
if a module wants it, it's just a Cargo dependency — but prefer Faust
for anything its stdlib already does well.

## The Faust pipeline

The rule that makes this painless: **generated code is committed**.
Building los never requires Faust; only *regenerating* does.

```
src/modules/delay/tap8fx.dsp       ← the source you edit
src/modules/delay/tap8fx_gen.rs    ← committed codegen (never edit)
src/faust.rs                       ← the ~100-line runtime prelude
justfile: `just dsp`               ← regenerates every _gen.rs
```

1. **Install the compiler** (once, for regeneration only):
   `brew install faust` (pulls LLVM; the generated .rs has zero
   dependencies).
2. **Write the `.dsp`** next to your module. Keep `declare name`,
   author, license at the top.
3. **Add a line to the `dsp` recipe** in the justfile:
   `faust -lang rust -cn YourCore src/modules/you/yourcore.dsp -o src/modules/you/yourcore_gen.rs`
   (`-cn` names the generated struct.)
4. **Include it** behind the prelude, with lints off for generated
   code:

```rust
#[allow(clippy::all, non_snake_case, non_camel_case_types,
        non_upper_case_globals, unused_parens, unused_variables,
        unused_mut, dead_code)]
pub mod yourcore {
    use crate::faust::*;
    include!("you/yourcore_gen.rs");
}
```

5. **Call it** from your audio thread:

```rust
let mut core = yourcore::YourCore::new();
core.init(48_000);                       // once; builds wavetables etc.
// per block — non-interleaved channel slices, any block size:
let ins = [&mono_in[..]];
let mut outs = [&mut out_a[..], &mut out_b[..]];
core.compute(frames, &ins, &mut outs);
```

`FAUST_INPUTS` / `FAUST_OUTPUTS` consts in the generated module tell
you the channel counts; assert them in a test so a `.dsp` edit that
changes the graph's arity fails loudly (see `tap8fx_core_shape_and_tail`
in delay.rs).

## Params: the ParamIndex contract

Every UI widget in the `.dsp` (`hslider`, `button`, `nentry`, …)
becomes a settable param; every `bargraph` becomes a *readable output*
— that's how Faust-side envelope followers and meters reach Rust:

```rust
core.set_param(ParamIndex(0), 0.7);          // push a control in
let env = core.get_param(ParamIndex(3));     // read a bargraph out
```

Indices are assigned in `build_user_interface` declaration order, which
is stable for a given `.dsp` but **shifts when you add/remove widgets**.
So: never scatter raw `ParamIndex(n)` literals. Define named constants
next to the include, and pin them with a test using
`crate::faust::ParamMap` (a `UI` implementation that records
`(label, index, is_output)`):

```rust
let mut map = faust::ParamMap::default();
yourcore::YourCore::build_user_interface_static(&mut map);
assert_eq!(map.params[GAIN.0 as usize].0, "gain");
```

The delay's tap8fx sidesteps all of this by declaring **zero widgets**
— amounts are Rust-side faders multiplying the core's outputs. For a
fixed-character block, that's the recommended shape: the smaller the
generated interface, the less there is to drift.

## House rules

- `.dsp` and `_gen.rs` live next to the module that owns them; the
  shared prelude is `src/faust.rs`.
- Commit the `.dsp` and its `_gen.rs` **in the same change**; `just
  dsp` before `just check` when you've touched a `.dsp`.
- Generated code runs on the audio thread like anything else: `new()` +
  `init()` at thread start, `compute()` per block, params per block.
  Nothing in it allocates after init.
- Mod-bindable params follow the same convention as everywhere: the
  module resolves bindings and pushes *effective* values into the core
  per block. The core never sees the modbus.
- Block-boundary feedback (Rust loop ↔ Faust core) costs one block
  (~1.3 ms at 64 frames) of latency in that path — fine for diffuse
  effects (the delay's shimmer), wrong for tight loops; put those
  inside the `.dsp`.

## Licensing

The Faust compiler is GPL, but the Faust standard libraries carry an
explicit exception: code generated from them may be distributed under
your own license. Our generated cores are AGPL like the rest of los
(stamp `declare license "AGPL-3.0-or-later";` in each `.dsp`).
