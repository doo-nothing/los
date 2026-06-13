//! # Clouds engine — the granular processor
//!
//! Ported from pichenettes/eurorack (clouds/dsp/*, MIT, copyright
//! 2014 Emilie Gillet, attribution preserved): the granular core —
//! a circular audio buffer, the windowed overlap-add grain with
//! pitch-shifted playback, and the grain scheduler (density, position,
//! size, pitch, stereo spread, window shape) with its gain
//! normalization. A float circular buffer stands in for the
//! firmware's 16-bit store (los has the RAM); linear interpolation is
//! used for grain playback (the medium-quality path).
//!
//! The reverb/diffusion stage and the alternate playback modes
//! (stretch/looping) build on this core in later passes.

#![allow(clippy::excessive_precision)]

use std::sync::OnceLock;

pub const MAX_NUM_GRAINS: usize = 64;
pub const MAX_BLOCK: usize = 32;

pub struct Tables {
    pub grain_size: Vec<f32>, // 257
    pub sin: Vec<f32>,        // 512 + guard (pan)
    pub window: Vec<f32>,     // 4097 (grain window)
}

static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn tables() -> &'static Tables {
    TABLES.get_or_init(|| {
        // lut_grain_size = floor(1024 * 2^(i/256 * 4)), 1024..16384 samples
        let grain_size: Vec<f32> = (0..257)
            .map(|i| {
                let s = i as f32 / 256.0 * 4.0;
                (1024.0 * 2.0_f32.powf(s)).floor()
            })
            .collect();
        // sin table for equal-power panning (one period over 512 + a
        // quarter guard so sin+256 reads cosine)
        let sin: Vec<f32> = (0..769)
            .map(|i| (i as f32 / 512.0 * std::f32::consts::TAU).sin())
            .collect();
        // the grain window: a raised cosine (Hann), the smooth end of
        // the window-shape morph
        let window: Vec<f32> = (0..4097)
            .map(|i| {
                let x = i as f32 / 4096.0;
                0.5 - 0.5 * (x * std::f32::consts::PI).cos()
            })
            .collect();
        Tables {
            grain_size,
            sin,
            window,
        }
    })
}

#[inline]
fn interpolate(table: &[f32], t: f32, size: f32) -> f32 {
    let p = (t.clamp(0.0, 1.0) * size).min(size);
    let i = p as usize;
    let frac = p - i as f32;
    let a = table[i.min(table.len() - 1)];
    let b = table[(i + 1).min(table.len() - 1)];
    a + (b - a) * frac
}

#[inline]
fn semitones_to_ratio(x: f32) -> f32 {
    2.0_f32.powf(x / 12.0)
}

#[inline]
fn crossfade(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// A small xorshift RNG so the granular cloud is deterministic per
/// instance (the firmware uses a global Random).
#[derive(Debug, Clone)]
pub struct Rng {
    state: u32,
}

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self {
            state: seed | 1,
        }
    }
    #[inline]
    pub fn next_float(&mut self) -> f32 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        (self.state >> 8) as f32 / 16_777_216.0
    }
}

// ── circular audio buffer ────────────────────────────────────────────────────

/// A circular recording buffer. One per channel; reads are linearly
/// interpolated at a 16.16 fractional position.
pub struct AudioBuffer {
    data: Vec<f32>,
    size: usize,
    write_head: usize,
}

impl AudioBuffer {
    pub fn new(size: usize) -> Self {
        Self {
            data: vec![0.0; size],
            size,
            write_head: 0,
        }
    }

    #[inline]
    pub fn head(&self) -> i32 {
        self.write_head as i32
    }

    #[inline]
    pub fn size(&self) -> i32 {
        self.size as i32
    }

    pub fn write_block(&mut self, input: &[f32]) {
        for &v in input {
            self.data[self.write_head] = v;
            self.write_head += 1;
            if self.write_head >= self.size {
                self.write_head = 0;
            }
        }
    }

    /// Linear read at integral sample + 16-bit fractional.
    #[inline]
    pub fn read(&self, mut integral: i32, fractional: u16) -> f32 {
        // wrap into [0, size)
        integral = integral.rem_euclid(self.size as i32);
        let i = integral as usize;
        let x0 = self.data[i];
        let x1 = self.data[if i + 1 >= self.size { 0 } else { i + 1 }];
        x0 + (x1 - x0) * (fractional as f32 / 65536.0)
    }
}

// ── grain ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Grain {
    first_sample: i32,
    phase: i64, // 16.16, widened to avoid overflow over long grains
    phase_increment: i64,
    pre_delay: i32,
    envelope_smoothness: f32,
    envelope_slope: f32,
    envelope_phase: f32,
    envelope_phase_increment: f32,
    gain_l: f32,
    gain_r: f32,
    active: bool,
}

impl Default for Grain {
    fn default() -> Self {
        Self {
            first_sample: 0,
            phase: 0,
            phase_increment: 0,
            pre_delay: 0,
            envelope_smoothness: 0.0,
            envelope_slope: 0.0,
            envelope_phase: 2.0,
            envelope_phase_increment: 0.0,
            gain_l: 0.0,
            gain_r: 0.0,
            active: false,
        }
    }
}

impl Grain {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        pre_delay: i32,
        buffer_size: i32,
        start: i32,
        width: i32,
        phase_increment: u32,
        window_shape: f32,
        gain_l: f32,
        gain_r: f32,
    ) {
        self.pre_delay = pre_delay;
        self.first_sample = (start + buffer_size).rem_euclid(buffer_size);
        self.phase_increment = phase_increment as i64;
        self.phase = 0;
        self.envelope_phase = 0.0;
        self.envelope_phase_increment = 2.0 / width.max(1) as f32;
        if window_shape >= 0.5 {
            self.envelope_smoothness = (window_shape - 0.5) * 2.0;
            self.envelope_slope = 0.0;
        } else {
            self.envelope_smoothness = 0.0;
            self.envelope_slope = 0.5 / (window_shape + 0.01);
        }
        self.gain_l = gain_l;
        self.gain_r = gain_r;
        self.active = true;
    }

    pub fn active(&self) -> bool {
        self.active
    }

    fn render_envelope(&mut self, envelope: &mut [f32], size: usize) {
        let t = tables();
        let increment = self.envelope_phase_increment;
        let smoothness = self.envelope_smoothness;
        let slope = self.envelope_slope;
        let mut phase = self.envelope_phase;
        for e in envelope.iter_mut().take(size) {
            let mut gain = if phase >= 1.0 { 2.0 - phase } else { phase };
            if smoothness > 0.0 {
                let window = interpolate(&t.window, gain, 4096.0);
                gain += smoothness * (window - gain);
            } else if slope > 0.0 {
                gain *= slope;
                if gain >= 1.0 {
                    gain = 1.0;
                }
            }
            phase += increment;
            if phase >= 2.0 {
                *e = -1.0;
                self.envelope_phase = phase;
                return;
            }
            *e = gain;
        }
        self.envelope_phase = phase;
    }

    /// Overlap-add this grain (stereo) into `out` (interleaved L/R).
    pub fn overlap_add(
        &mut self,
        buf_l: &AudioBuffer,
        buf_r: &AudioBuffer,
        out: &mut [f32],
        envelope: &mut [f32],
        mut size: usize,
    ) {
        if !self.active {
            return;
        }
        let mut off = 0usize;
        while self.pre_delay > 0 && size > 0 {
            off += 2;
            size -= 1;
            self.pre_delay -= 1;
        }
        self.render_envelope(envelope, size);
        let first_sample = self.first_sample;
        let gain_l = self.gain_l;
        let gain_r = self.gain_r;
        let mut phase = self.phase;
        let mut oi = off;
        for &gain in envelope.iter().take(size) {
            let sample_index = first_sample + (phase >> 16) as i32;
            if gain == -1.0 {
                self.active = false;
                break;
            }
            let frac = (phase & 0xffff) as u16;
            let l = buf_l.read(sample_index, frac) * gain;
            let r = buf_r.read(sample_index, frac) * gain;
            out[oi] += l * gain_l + r * (1.0 - gain_r);
            out[oi + 1] += r * gain_r + l * (1.0 - gain_l);
            oi += 2;
            phase += self.phase_increment;
        }
        self.phase = phase;
    }
}

// ── granular sample player (the scheduler) ───────────────────────────────────

pub struct GranularSamplePlayer {
    grains: Vec<Grain>,
    available: Vec<usize>,
    envelope: Vec<f32>,
    max_num_grains: usize,
    num_grains: f32,
    gain_normalization: f32,
    grain_size_hint: f32,
    grain_rate_phasor: f32,
}

/// Panel parameters for the granular engine (0..1 unless noted).
#[derive(Debug, Clone, Copy)]
pub struct GranularParams {
    pub position: f32,
    pub size: f32,
    pub pitch: f32, // semitones
    pub overlap: f32,
    pub window_shape: f32,
    pub stereo_spread: f32,
    pub trigger: bool,
}

impl Default for GranularParams {
    fn default() -> Self {
        Self {
            position: 0.5,
            size: 0.5,
            pitch: 0.0,
            overlap: 0.5,
            window_shape: 0.5,
            stereo_spread: 0.0,
            trigger: false,
        }
    }
}

impl GranularSamplePlayer {
    pub fn new(max_num_grains: usize) -> Self {
        let max_num_grains = max_num_grains.min(MAX_NUM_GRAINS);
        Self {
            grains: vec![Grain::default(); MAX_NUM_GRAINS],
            available: vec![0; MAX_NUM_GRAINS],
            envelope: vec![0.0; MAX_BLOCK],
            max_num_grains,
            num_grains: 0.0,
            gain_normalization: 1.0,
            grain_size_hint: 1024.0,
            grain_rate_phasor: 0.0,
        }
    }

    fn fill_available(&mut self) -> usize {
        let mut n = 0;
        for i in 0..self.max_num_grains {
            if !self.grains[i].active() {
                self.available[n] = i;
                n += 1;
            }
        }
        n
    }

    #[allow(clippy::too_many_arguments)]
    fn schedule_grain(
        &mut self,
        index: usize,
        params: &GranularParams,
        pre_delay: i32,
        buffer_size: i32,
        buffer_head: i32,
        rng: &mut Rng,
    ) {
        let t = tables();
        let mut grain_size = interpolate(&t.grain_size, params.size, 256.0);
        let pitch_ratio = semitones_to_ratio(params.pitch);
        let inv_pitch_ratio = semitones_to_ratio(-params.pitch);
        let pan = 0.5 + params.stereo_spread * (rng.next_float() - 0.5);
        let (gain_l, gain_r) = if pan < 0.5 {
            (1.0, 2.0 * pan)
        } else {
            (2.0 * (1.0 - pan), 1.0)
        };
        if pitch_ratio > 1.0 {
            grain_size = grain_size.min(buffer_size as f32 * 0.25 * inv_pitch_ratio);
        }
        let eaten_by_play = grain_size * pitch_ratio;
        let eaten_by_rec = grain_size;
        let available = buffer_size as f32 - eaten_by_play - eaten_by_rec;
        let size = (grain_size as i32) & !1;
        let start = buffer_head - (params.position * available + eaten_by_play) as i32;
        self.grains[index].start(
            pre_delay,
            buffer_size,
            start,
            size,
            (pitch_ratio * 65536.0) as u32,
            params.window_shape,
            gain_l,
            gain_r,
        );
        // ONE_POLE the grain-size hint toward the new size
        self.grain_size_hint += (grain_size - self.grain_size_hint) * 0.1;
    }

    /// Render `size` frames into interleaved stereo `out`.
    pub fn play(
        &mut self,
        buf_l: &AudioBuffer,
        buf_r: &AudioBuffer,
        params: &GranularParams,
        out: &mut [f32],
        size: usize,
        rng: &mut Rng,
    ) {
        let overlap = params.overlap * params.overlap * params.overlap;
        let target_num_grains = self.max_num_grains as f32 * overlap;
        let p = target_num_grains / self.grain_size_hint;
        let space_between_grains = self.grain_size_hint / target_num_grains.max(0.001);

        let mut num_available = self.fill_available();
        let mut seed_trigger = params.trigger;
        let buffer_size = buf_l.size();
        let head = buf_l.head();
        for tpos in 0..size {
            self.grain_rate_phasor += 1.0;
            let seed_prob = rng.next_float() < p && target_num_grains > self.num_grains;
            let seed_det = self.grain_rate_phasor >= space_between_grains;
            let seed = seed_prob || seed_det || seed_trigger;
            if num_available > 0 && seed {
                num_available -= 1;
                let index = self.available[num_available];
                let buffer_head = head - size as i32 + tpos as i32;
                self.schedule_grain(index, params, tpos as i32, buffer_size, buffer_head, rng);
                self.grain_rate_phasor = 0.0;
                seed_trigger = false;
            }
        }

        out[..size * 2].fill(0.0);
        let mut envelope = std::mem::take(&mut self.envelope);
        for i in 0..self.max_num_grains {
            self.grains[i].overlap_add(buf_l, buf_r, out, &mut envelope, size);
        }
        self.envelope = envelope;

        let active_grains = (self.max_num_grains - num_available) as f32;
        // SLOPE: rise fast (0.9), fall slow (0.2)
        let coeff = if active_grains > self.num_grains { 0.9 } else { 0.2 };
        self.num_grains += (active_grains - self.num_grains) * coeff;
        let mut gain_norm = if self.num_grains > 2.0 {
            1.0 / (self.num_grains - 1.0).sqrt()
        } else {
            1.0
        };
        let window_gain = (1.0 + 2.0 * params.window_shape).clamp(1.0, 2.0);
        gain_norm *= crossfade(1.0, window_gain, params.overlap);
        for i in 0..size {
            self.gain_normalization += (gain_norm - self.gain_normalization) * 0.01;
            out[i * 2] *= self.gain_normalization;
            out[i * 2 + 1] *= self.gain_normalization;
        }
    }
}

// ── the reverb (Dattorro topology, clouds tuning) ────────────────────────────

pub const NATIVE_SR: f32 = 32_000.0;

/// An allpass / delay line with interpolated reads (the fx_engine
/// idiom, matching the elements port).
struct Ap {
    buf: Vec<f32>,
    pos: usize,
}

impl Ap {
    fn new(len: usize) -> Self {
        Ap {
            buf: vec![0.0; len.max(4)],
            pos: 0,
        }
    }
    #[inline]
    fn process(&mut self, x: f32, k: f32) -> f32 {
        let tail = self.buf[self.pos];
        let w = x + tail * k;
        self.buf[self.pos] = w;
        self.pos = (self.pos + 1) % self.buf.len();
        tail - w * k
    }
    #[inline]
    fn read_at(&self, offset: f32) -> f32 {
        let n = self.buf.len();
        let offset = offset.clamp(1.0, (n - 2) as f32);
        let int = offset as usize;
        let frac = offset - int as f32;
        let a = self.buf[(self.pos + n - 1 - int) % n];
        let b = self.buf[(self.pos + n - 2 - int) % n];
        a + (b - a) * frac
    }
    #[inline]
    fn write_at(&mut self, offset: usize, v: f32) {
        let n = self.buf.len();
        let off = offset.min(n - 1);
        self.buf[(self.pos + n - off) % n] = v;
    }
}

struct Delay {
    buf: Vec<f32>,
    pos: usize,
}

impl Delay {
    fn new(len: usize) -> Self {
        Delay {
            buf: vec![0.0; len.max(4)],
            pos: 0,
        }
    }
    #[inline]
    fn read_tail(&self) -> f32 {
        self.buf[self.pos]
    }
    #[inline]
    fn read_mod(&self, offset: f32) -> f32 {
        let n = self.buf.len();
        let offset = offset.clamp(1.0, (n - 2) as f32);
        let int = offset as usize;
        let frac = offset - int as f32;
        let a = self.buf[(self.pos + n - 1 - int) % n];
        let b = self.buf[(self.pos + n - 2 - int) % n];
        a + (b - a) * frac
    }
    #[inline]
    fn write(&mut self, v: f32) {
        self.buf[self.pos] = v;
        self.pos = (self.pos + 1) % self.buf.len();
    }
}

/// The clouds reverb (fx/reverb.h): the Dattorro/Griesinger topology
/// — 4 input allpass diffusers + two modulated decay branches.
pub struct Reverb {
    ap: [Ap; 4],
    dap1a: Ap,
    dap1b: Ap,
    del1: Delay,
    dap2a: Ap,
    dap2b: Ap,
    del2: Delay,
    lp1: f32,
    lp2: f32,
    lfo_phase: [f32; 2],
    lfo_inc: [f32; 2],
    rate_scale: f32,
    pub amount: f32,
    pub diffusion: f32,
    pub time: f32,
    pub input_gain: f32,
    pub lp: f32,
}

impl Reverb {
    pub fn new(sample_rate: f32) -> Self {
        let s = sample_rate / NATIVE_SR;
        let sz = |n: f32| ((n * s) as usize).max(16);
        Reverb {
            ap: [
                Ap::new(sz(113.0)),
                Ap::new(sz(162.0)),
                Ap::new(sz(241.0)),
                Ap::new(sz(399.0)),
            ],
            dap1a: Ap::new(sz(1653.0)),
            dap1b: Ap::new(sz(2038.0)),
            del1: Delay::new(sz(3411.0)),
            dap2a: Ap::new(sz(1913.0)),
            dap2b: Ap::new(sz(1663.0)),
            del2: Delay::new(sz(4782.0)),
            lp1: 0.0,
            lp2: 0.0,
            lfo_phase: [0.0, 0.0],
            lfo_inc: [0.5 / sample_rate, 0.3 / sample_rate],
            rate_scale: s,
            amount: 0.0,
            diffusion: 0.625,
            time: 0.35,
            input_gain: 0.2,
            lp: 0.7,
        }
    }

    /// Process a stereo block in place (interleaved L/R).
    pub fn process(&mut self, io: &mut [f32], size: usize) {
        let kap = self.diffusion;
        let klp = self.lp;
        let krt = self.time;
        let amount = self.amount;
        let gain = self.input_gain;
        let s = self.rate_scale;
        for i in 0..size {
            for (phase, inc) in self.lfo_phase.iter_mut().zip(self.lfo_inc.iter()) {
                *phase += inc;
                if *phase >= 1.0 {
                    *phase -= 1.0;
                }
            }
            let lfo1 = 0.5 - 0.5 * (self.lfo_phase[0] * std::f32::consts::TAU).cos();
            let lfo2 = 0.5 - 0.5 * (self.lfo_phase[1] * std::f32::consts::TAU).cos();

            // smear ap1 (clouds: base 10, depth 60)
            let smear = self.ap[0].read_at((10.0 + 60.0 * lfo1) * s);
            self.ap[0].write_at((100.0 * s).round() as usize, smear);

            let mut acc = (io[i * 2] + io[i * 2 + 1]) * gain;
            for ap in self.ap.iter_mut() {
                acc = ap.process(acc, kap);
            }
            let apout = acc;

            // branch 1 → left (del2-modulated, clouds base 4680)
            let mod_offset = (4680.0 + 100.0 * lfo2) * s;
            acc = apout + self.del2.read_mod(mod_offset) * krt;
            self.lp1 += klp * (acc - self.lp1);
            let mut b = self.lp1;
            b = self.dap1a.process(b, -kap);
            b = self.dap1b.process(b, kap);
            self.del1.write(b);
            let wet = b * 2.0;
            io[i * 2] += (wet - io[i * 2]) * amount;

            // branch 2 → right (del1 tail)
            acc = apout + self.del1.read_tail() * krt;
            self.lp2 += klp * (acc - self.lp2);
            let mut b = self.lp2;
            b = self.dap2a.process(b, kap);
            b = self.dap2b.process(b, -kap);
            self.del2.write(b);
            let wet = b * 2.0;
            io[i * 2 + 1] += (wet - io[i * 2 + 1]) * amount;
        }
    }
}

// ── the granular diffuser (fx/diffuser.h) ────────────────────────────────────

/// Stereo 4-allpass diffuser (k = 0.625), one chain per channel.
pub struct Diffuser {
    apl: [Ap; 4],
    apr: [Ap; 4],
    pub amount: f32,
}

impl Diffuser {
    pub fn new(sample_rate: f32) -> Self {
        let s = sample_rate / NATIVE_SR;
        let sz = |n: f32| ((n * s) as usize).max(8);
        Diffuser {
            apl: [
                Ap::new(sz(126.0)),
                Ap::new(sz(180.0)),
                Ap::new(sz(269.0)),
                Ap::new(sz(444.0)),
            ],
            apr: [
                Ap::new(sz(151.0)),
                Ap::new(sz(205.0)),
                Ap::new(sz(245.0)),
                Ap::new(sz(405.0)),
            ],
            amount: 0.0,
        }
    }

    pub fn process(&mut self, io: &mut [f32], size: usize) {
        const K: f32 = 0.625;
        let amount = self.amount;
        for i in 0..size {
            let mut l = io[i * 2];
            let mut r = io[i * 2 + 1];
            let dry_l = l;
            let dry_r = r;
            for ap in self.apl.iter_mut() {
                l = ap.process(l, -K);
            }
            for ap in self.apr.iter_mut() {
                r = ap.process(r, -K);
            }
            io[i * 2] = dry_l + (l - dry_l) * amount;
            io[i * 2 + 1] = dry_r + (r - dry_r) * amount;
        }
    }
}

// ── the granular processor (top level) ───────────────────────────────────────

/// Full clouds parameters (the panel).
#[derive(Debug, Clone, Copy)]
pub struct CloudsParams {
    pub position: f32,
    pub size: f32,
    pub pitch: f32, // semitones
    pub density: f32,
    pub texture: f32,
    pub dry_wet: f32,
    pub stereo_spread: f32,
    pub feedback: f32,
    pub reverb: f32,
    pub freeze: bool,
    pub trigger: bool,
}

impl Default for CloudsParams {
    fn default() -> Self {
        Self {
            position: 0.5,
            size: 0.5,
            pitch: 0.0,
            density: 0.5,
            texture: 0.5,
            dry_wet: 0.5,
            stereo_spread: 0.0,
            feedback: 0.0,
            reverb: 0.0,
            freeze: false,
            trigger: false,
        }
    }
}

#[inline]
fn soft_convert(x: f32) -> f32 {
    // stmlib SoftLimit-style soft clip
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

/// Records the input into a circular buffer and granulates it, then
/// diffuses and reverberates, with a feedback path and a dry/wet mix.
pub struct GranularProcessor {
    buf_l: AudioBuffer,
    buf_r: AudioBuffer,
    player: GranularSamplePlayer,
    diffuser: Diffuser,
    reverb: Reverb,
    rng: Rng,
    sample_rate: f32,
    fb: Vec<f32>,
    fb_hp_l: f32,
    fb_hp_r: f32,
    dry_wet: f32,
    scratch: Vec<f32>,
}

impl GranularProcessor {
    pub fn new(sample_rate: f32, buffer_seconds: f32, seed: u32) -> Self {
        let n = (sample_rate * buffer_seconds) as usize;
        Self {
            buf_l: AudioBuffer::new(n),
            buf_r: AudioBuffer::new(n),
            player: GranularSamplePlayer::new(48),
            diffuser: Diffuser::new(sample_rate),
            reverb: Reverb::new(sample_rate),
            rng: Rng::new(seed),
            sample_rate,
            fb: vec![0.0; MAX_BLOCK * 2],
            fb_hp_l: 0.0,
            fb_hp_r: 0.0,
            dry_wet: 0.5,
            scratch: vec![0.0; MAX_BLOCK * 2],
        }
    }

    /// Process a block of interleaved stereo audio in place.
    pub fn process(&mut self, io: &mut [f32], params: &CloudsParams, size: usize) {
        // 1. feedback into the input (HP-filtered), then record
        let cutoff = (20.0 + 100.0 * params.feedback * params.feedback) / self.sample_rate;
        let hp_a = (cutoff * std::f32::consts::TAU).min(0.9);
        let fb_gain = params.feedback;
        let mut rec_l = vec![0.0_f32; size];
        let mut rec_r = vec![0.0_f32; size];
        for i in 0..size {
            // high-pass the fed-back signal (one-pole HP)
            self.fb_hp_l += hp_a * (self.fb[i * 2] - self.fb_hp_l);
            self.fb_hp_r += hp_a * (self.fb[i * 2 + 1] - self.fb_hp_r);
            let hp_l = self.fb[i * 2] - self.fb_hp_l;
            let hp_r = self.fb[i * 2 + 1] - self.fb_hp_r;
            rec_l[i] = io[i * 2] + fb_gain * hp_l;
            rec_r[i] = io[i * 2 + 1] + fb_gain * hp_r;
        }
        if !params.freeze {
            self.buf_l.write_block(&rec_l);
            self.buf_r.write_block(&rec_r);
        }

        // 2. granular: density → overlap, texture → window shape
        let density = params.density;
        let overlap = if density >= 0.53 {
            (density - 0.53) * 2.12
        } else if density <= 0.47 {
            (0.47 - density) * 2.12
        } else {
            0.0
        };
        let window_shape = if params.texture < 0.75 {
            params.texture * 1.333
        } else {
            1.0
        };
        let gp = GranularParams {
            position: params.position,
            size: params.size,
            pitch: params.pitch,
            overlap: overlap.clamp(0.0, 1.0),
            window_shape,
            stereo_spread: params.stereo_spread,
            trigger: params.trigger,
        };
        if self.scratch.len() < size * 2 {
            self.scratch.resize(size * 2, 0.0);
        }
        self.player
            .play(&self.buf_l, &self.buf_r, &gp, &mut self.scratch, size, &mut self.rng);

        // 3. diffuser (granular mode: texture > 0.75 → diffusion)
        let diffusion = if params.texture > 0.75 {
            (params.texture - 0.75) * 4.0
        } else {
            0.0
        };
        self.diffuser.amount = diffusion.clamp(0.0, 1.0);
        self.diffuser.process(&mut self.scratch, size);

        // 4. the feedback tap is the granular output BEFORE reverb
        self.fb[..size * 2].copy_from_slice(&self.scratch[..size * 2]);

        // 5. reverb
        let reverb_amount = (params.reverb * 0.95).clamp(0.0, 1.0);
        self.reverb.amount = reverb_amount * 0.54;
        self.reverb.diffusion = 0.7;
        self.reverb.time = 0.35 + 0.63 * reverb_amount;
        self.reverb.input_gain = 0.2;
        self.reverb.lp = 0.6 + 0.37 * params.feedback;
        self.reverb.process(&mut self.scratch, size);

        // 6. dry/wet (equal power) + soft clip
        let post_gain = 1.2;
        for i in 0..size {
            self.dry_wet += (params.dry_wet - self.dry_wet) * 0.05;
            let dw = self.dry_wet.clamp(0.0, 1.0);
            let fade_in = (dw * std::f32::consts::FRAC_PI_2).sin();
            let fade_out = (dw * std::f32::consts::FRAC_PI_2).cos();
            let l = io[i * 2] * fade_out + self.scratch[i * 2] * post_gain * fade_in;
            let r = io[i * 2 + 1] * fade_out + self.scratch[i * 2 + 1] * post_gain * fade_in;
            io[i * 2] = soft_convert(l);
            io[i * 2 + 1] = soft_convert(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_buffer(buf: &mut AudioBuffer, gen: impl Fn(usize) -> f32) {
        let n = buf.size() as usize;
        let block: Vec<f32> = (0..n).map(gen).collect();
        buf.write_block(&block);
    }

    #[test]
    fn tables_load() {
        let t = tables();
        assert_eq!(t.grain_size.len(), 257);
        assert_eq!(t.window.len(), 4097);
        // grain size spans 1024..16384
        assert!((t.grain_size[0] - 1024.0).abs() < 1.0);
        assert!(t.grain_size[256] >= 16000.0);
        // window is an S-curve transfer (smooths the grain ramp): 0→1
        assert!(t.window[0] < 0.01, "starts at 0");
        assert!((t.window[4096] - 1.0).abs() < 0.01, "ends at 1");
        assert!((t.window[2048] - 0.5).abs() < 0.05, "0.5 at the midpoint");
    }

    #[test]
    fn buffer_writes_and_reads_back() {
        let mut buf = AudioBuffer::new(1024);
        fill_buffer(&mut buf, |i| (i as f32 * 0.1).sin());
        // read at integral 100, no fraction → exact sample
        let v = buf.read(100, 0);
        assert!((v - (100.0_f32 * 0.1).sin()).abs() < 1e-5);
        // midpoint fraction interpolates
        let mid = buf.read(100, 32768);
        let lo = (100.0_f32 * 0.1).sin();
        let hi = (101.0_f32 * 0.1).sin();
        assert!((mid - (lo + hi) * 0.5).abs() < 1e-5);
    }

    #[test]
    fn buffer_read_wraps() {
        let mut buf = AudioBuffer::new(256);
        fill_buffer(&mut buf, |i| i as f32);
        // reading past the end wraps around
        assert!((buf.read(300, 0) - (300 % 256) as f32).abs() < 1e-3);
        assert!((buf.read(-1, 0) - 255.0).abs() < 1e-3);
    }

    #[test]
    fn a_single_grain_plays_a_windowed_slice() {
        let mut g = Grain::default();
        let mut buf_l = AudioBuffer::new(4096);
        let mut buf_r = AudioBuffer::new(4096);
        fill_buffer(&mut buf_l, |i| (i as f32 * 0.05).sin());
        fill_buffer(&mut buf_r, |i| (i as f32 * 0.05).sin());
        // a grain of width 512 at unity pitch
        g.start(0, 4096, 1000, 512, 65536, 0.5, 1.0, 1.0);
        let mut out = vec![0.0_f32; MAX_BLOCK * 2];
        let mut env = vec![0.0_f32; MAX_BLOCK];
        let mut peak = 0.0_f32;
        // render several blocks until the grain dies
        for _ in 0..40 {
            out.iter_mut().for_each(|v| *v = 0.0);
            g.overlap_add(&buf_l, &buf_r, &mut out, &mut env, MAX_BLOCK);
            peak = out.iter().fold(peak, |m, v| m.max(v.abs()));
            if !g.active() {
                break;
            }
        }
        assert!(peak > 0.1, "the grain produced sound: {peak}");
        assert!(!g.active(), "and the grain finished");
    }

    #[test]
    fn the_cloud_produces_sound_at_any_density() {
        // the gain normalization deliberately keeps loudness roughly even
        // across density — so at both sparse and dense overlap the cloud
        // makes bounded, non-silent sound
        let mut buf_l = AudioBuffer::new(16384);
        let mut buf_r = AudioBuffer::new(16384);
        fill_buffer(&mut buf_l, |i| (i as f32 * 0.03).sin() * 0.5);
        fill_buffer(&mut buf_r, |i| (i as f32 * 0.03).sin() * 0.5);

        let energy_at = |overlap: f32| -> f32 {
            let mut player = GranularSamplePlayer::new(32);
            let mut rng = Rng::new(0x1234);
            let mut out = vec![0.0_f32; MAX_BLOCK * 2];
            let params = GranularParams {
                overlap,
                size: 0.3,
                ..Default::default()
            };
            let mut e = 0.0;
            for _ in 0..200 {
                player.play(&buf_l, &buf_r, &params, &mut out, MAX_BLOCK, &mut rng);
                assert!(out.iter().all(|v| v.is_finite() && v.abs() < 8.0));
                e += out.iter().map(|v| v * v).sum::<f32>();
            }
            e
        };
        assert!(energy_at(0.2) > 1.0, "sparse cloud sings");
        assert!(energy_at(0.9) > 1.0, "dense cloud sings");
    }

    #[test]
    fn pitch_shift_changes_playback_rate() {
        // a grain an octave up advances its phase twice as fast
        let mut g = Grain::default();
        let buf = AudioBuffer::new(4096);
        let buf2 = AudioBuffer::new(4096);
        g.start(0, 4096, 0, 1024, 131072, 0.5, 1.0, 1.0); // 2x pitch
        let mut out = vec![0.0_f32; MAX_BLOCK * 2];
        let mut env = vec![0.0_f32; MAX_BLOCK];
        g.overlap_add(&buf, &buf2, &mut out, &mut env, MAX_BLOCK);
        // phase increment 131072 = 2.0 in 16.16 → 64 samples consumed in 32
        assert_eq!(g.phase_increment, 131072);
    }

    #[test]
    fn processor_passes_dry_and_adds_wet() {
        // dry_wet 0 = clean passthrough; dry_wet up brings in the cloud
        let mut proc = GranularProcessor::new(48_000.0, 1.0, 0x77);
        let dry: Vec<f32> = (0..MAX_BLOCK)
            .flat_map(|i| {
                let v = (i as f32 * 0.2).sin() * 0.5;
                [v, v]
            })
            .collect();
        // prime the buffer with several blocks of input
        let mut params = CloudsParams {
            dry_wet: 1.0,
            density: 0.9,
            size: 0.3,
            reverb: 0.4,
            ..Default::default()
        };
        let mut io = dry.clone();
        let mut energy = 0.0;
        for _ in 0..120 {
            io.copy_from_slice(&dry);
            proc.process(&mut io, &params, MAX_BLOCK);
            assert!(io.iter().all(|v| v.is_finite()), "stays finite");
            energy += io.iter().map(|v| v * v).sum::<f32>();
        }
        assert!(energy > 0.1, "the wet cloud produces sound: {energy}");

        // dry_wet 0 → output ≈ input
        params.dry_wet = 0.0;
        for _ in 0..40 {
            io.copy_from_slice(&dry);
            proc.process(&mut io, &params, MAX_BLOCK);
        }
        let diff: f32 = io
            .iter()
            .zip(dry.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(diff < 1.0, "dry_wet 0 passes the input through: {diff}");
    }

    #[test]
    fn reverb_rings_out() {
        // an impulse through the reverb produces a decaying tail
        let mut rev = Reverb::new(48_000.0);
        rev.amount = 1.0;
        rev.time = 0.7;
        let mut io = vec![0.0_f32; MAX_BLOCK * 2];
        io[0] = 1.0;
        io[1] = 1.0;
        rev.process(&mut io, MAX_BLOCK);
        let mut tail_energy = 0.0;
        for _ in 0..50 {
            io.iter_mut().for_each(|v| *v = 0.0);
            rev.process(&mut io, MAX_BLOCK);
            tail_energy += io.iter().map(|v| v * v).sum::<f32>();
            assert!(io.iter().all(|v| v.is_finite()));
        }
        assert!(tail_energy > 0.0, "the reverb rings: {tail_energy}");
    }

    #[test]
    fn play_is_bounded() {
        let mut buf_l = AudioBuffer::new(8192);
        let mut buf_r = AudioBuffer::new(8192);
        fill_buffer(&mut buf_l, |i| (i as f32 * 0.07).sin());
        fill_buffer(&mut buf_r, |i| (i as f32 * 0.07).cos());
        let mut player = GranularSamplePlayer::new(48);
        let mut rng = Rng::new(0xbeef);
        let mut out = vec![0.0_f32; MAX_BLOCK * 2];
        let params = GranularParams {
            overlap: 1.0,
            size: 0.5,
            pitch: 7.0,
            stereo_spread: 0.8,
            ..Default::default()
        };
        for _ in 0..500 {
            player.play(&buf_l, &buf_r, &params, &mut out, MAX_BLOCK, &mut rng);
            assert!(out.iter().all(|v| v.is_finite()), "stays finite");
        }
    }
}
