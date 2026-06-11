//! The tiny runtime for Faust-generated DSP cores.
//!
//! los modules can write their audio-rate core in the
//! [Faust](https://faust.grame.fr) DSP language instead of Rust: a `.dsp`
//! source file lives next to the module, `just dsp` compiles it with
//! `faust -lang rust` (brew install faust), and the **generated Rust is
//! committed** — building los never requires the Faust compiler, only
//! regenerating does. docs/writing-dsp.md is the guide; the delay's
//! `tap8fx` block is the worked example.
//!
//! Generated code is free-standing except for the handful of types and
//! traits in this file. Their shapes mirror the `faust-types` crate
//! (MIT/Apache-2.0, github.com/Frando/rust-faust) so the output of any
//! standard Faust Rust architecture drops in unchanged; we vendor ~100
//! lines instead of adding a dependency for what is, in the end, an
//! interface.
//!
//! What a generated core gives you (all inherent methods, no trait
//! needed at the call site):
//!
//! - `T::new()`, then `init(sample_rate)` once, on the audio thread is
//!   fine (table builds run at init, not per block).
//! - `compute(count, &inputs, &mut outputs)` — non-interleaved channel
//!   slices, any block size.
//! - `set_param(ParamIndex(i), v)` / `get_param(ParamIndex(i))` for
//!   every widget the `.dsp` declares (sliders are inputs, bargraphs are
//!   outputs you read back — Faust's envelope followers reach Rust that
//!   way). Indices are assigned in `build_user_interface` order; pin
//!   them with named constants and a test that walks the UI (see
//!   docs/writing-dsp.md §param indices).
//!
//! Note on licensing: the Faust *compiler* is GPL, but the Faust
//! standard libraries carry an explicit exception allowing generated
//! code to be distributed under your own license — committing codegen
//! into this AGPL repo is fine.

/// Faust's float type. We build everything `-single`; if a core ever
/// needs doubles, generate it with `-double` into its own module — this
/// alias is per-crate on purpose, not per-core.
pub type FaustFloat = f32;
/// Generated numeric code calls intrinsics through this alias
/// (`F32::sin(…)`, `F32::max(…)`).
#[allow(non_camel_case_types)]
pub type F32 = f32;
/// And occasionally integer helpers through this one.
#[allow(non_camel_case_types)]
pub type F64 = f64;

/// A widget handle: the index of a parameter in `build_user_interface`
/// declaration order. Stable for a given `.dsp` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamIndex(pub i32);

/// Receiver for the `declare`d metadata of a core (name, author,
/// library versions). los doesn't display these today; the trait exists
/// because generated `metadata()` needs it.
pub trait Meta {
    fn declare(&mut self, key: &str, value: &str);
}

/// Soundfile support is unimplemented (no `soundfile` primitive in our
/// cores); the type exists so the `UI` trait is complete.
#[derive(Debug, Default)]
pub struct Soundfile;

/// Receiver for a core's widget tree. Implement on a small struct and
/// pass to `build_user_interface` to *discover* params — the way to map
/// labels to `ParamIndex`es without hardcoding (and to assert the
/// mapping in tests).
#[allow(unused_variables)]
pub trait UI<T> {
    // layout boxes — safe to ignore for headless use
    fn open_tab_box(&mut self, label: &str) {}
    fn open_horizontal_box(&mut self, label: &str) {}
    fn open_vertical_box(&mut self, label: &str) {}
    fn close_box(&mut self) {}
    // active widgets: values you push into the core
    fn add_button(&mut self, label: &str, param: ParamIndex) {}
    fn add_check_button(&mut self, label: &str, param: ParamIndex) {}
    fn add_vertical_slider(
        &mut self,
        label: &str,
        param: ParamIndex,
        init: T,
        min: T,
        max: T,
        step: T,
    ) {
    }
    fn add_horizontal_slider(
        &mut self,
        label: &str,
        param: ParamIndex,
        init: T,
        min: T,
        max: T,
        step: T,
    ) {
    }
    fn add_num_entry(&mut self, label: &str, param: ParamIndex, init: T, min: T, max: T, step: T) {}
    // passive widgets: values the core computes for you to read back
    fn add_horizontal_bargraph(&mut self, label: &str, param: ParamIndex, min: T, max: T) {}
    fn add_vertical_bargraph(&mut self, label: &str, param: ParamIndex, min: T, max: T) {}
    fn add_sound_file(&mut self, label: &str, filename: &str, sf_zone: &mut Soundfile) {}
    fn declare(&mut self, param: Option<ParamIndex>, key: &str, value: &str) {}
}

/// The object-safe face of a generated core. Generated code also exposes
/// everything as inherent methods (with a nicer `compute` taking
/// `impl AsRef<[f32]>`), which is what module code should call; the
/// trait matters when you hold cores behind `dyn` (the 296e's band map
/// tests do).
pub trait FaustDsp {
    type T;
    fn new() -> Self
    where
        Self: Sized;
    fn metadata(&self, m: &mut dyn Meta);
    fn get_sample_rate(&self) -> i32;
    fn get_num_inputs(&self) -> i32;
    fn get_num_outputs(&self) -> i32;
    fn class_init(sample_rate: i32)
    where
        Self: Sized;
    fn instance_reset_params(&mut self);
    fn instance_clear(&mut self);
    fn instance_constants(&mut self, sample_rate: i32);
    fn instance_init(&mut self, sample_rate: i32);
    fn init(&mut self, sample_rate: i32);
    fn build_user_interface(&self, ui_interface: &mut dyn UI<Self::T>);
    fn build_user_interface_static(ui_interface: &mut dyn UI<Self::T>)
    where
        Self: Sized;
    fn get_param(&self, param: ParamIndex) -> Option<Self::T>;
    fn set_param(&mut self, param: ParamIndex, value: Self::T);
    fn compute(&mut self, count: i32, inputs: &[&[Self::T]], outputs: &mut [&mut [Self::T]]);
}

/// A `UI` that records `(label, ParamIndex)` pairs — the standard way to
/// assert a core's param layout in tests.
#[derive(Debug, Default)]
pub struct ParamMap {
    /// `(label, index, is_output)` in declaration order; `is_output` is
    /// true for bargraphs (values the core writes).
    pub params: Vec<(String, i32, bool)>,
}

impl UI<FaustFloat> for ParamMap {
    fn add_button(&mut self, label: &str, param: ParamIndex) {
        self.params.push((label.into(), param.0, false));
    }
    fn add_check_button(&mut self, label: &str, param: ParamIndex) {
        self.params.push((label.into(), param.0, false));
    }
    fn add_vertical_slider(
        &mut self,
        label: &str,
        param: ParamIndex,
        _: f32,
        _: f32,
        _: f32,
        _: f32,
    ) {
        self.params.push((label.into(), param.0, false));
    }
    fn add_horizontal_slider(
        &mut self,
        label: &str,
        param: ParamIndex,
        _: f32,
        _: f32,
        _: f32,
        _: f32,
    ) {
        self.params.push((label.into(), param.0, false));
    }
    fn add_num_entry(&mut self, label: &str, param: ParamIndex, _: f32, _: f32, _: f32, _: f32) {
        self.params.push((label.into(), param.0, false));
    }
    fn add_horizontal_bargraph(&mut self, label: &str, param: ParamIndex, _: f32, _: f32) {
        self.params.push((label.into(), param.0, true));
    }
    fn add_vertical_bargraph(&mut self, label: &str, param: ParamIndex, _: f32, _: f32) {
        self.params.push((label.into(), param.0, true));
    }
}
