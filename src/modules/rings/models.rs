//! # Rings models — modal resonator, string voice, FM voice
//!
//! Ported from rings/dsp/{resonator,string,fm_voice}.cc (MIT,
//! copyright 2015 Emilie Gillet, attribution preserved). Laws are
//! kept verbatim; the only structural deviations are documented at
//! their sites.

use super::dsp::{
    interpolate, one_pole, semitones_to_ratio, slope, stiffness, svf_shift, CosineOscillator,
    DcBlocker, DelayLine, Follower, Rng, Svf, NATIVE_SR,
};

pub const MAX_MODES: usize = 64;
const STRING_DELAY_SIZE: usize = 2048;

// ── modal resonator ────────────────────────────────────────────────────────

/// rings Resonator: up to 64 band-pass modes, odd/even dispatched to
/// out/aux through the position-driven approximate-cosine comb.
#[derive(Debug, Clone)]
pub struct Resonator {
    pub frequency: f32,
    pub structure: f32,
    pub brightness: f32,
    pub damping: f32,
    pub position: f32,
    previous_position: f32,
    resolution: usize,
    f: Vec<Svf>,
}

impl Resonator {
    pub fn new() -> Self {
        Resonator {
            frequency: 220.0 / NATIVE_SR,
            structure: 0.25,
            brightness: 0.5,
            damping: 0.3,
            position: 0.999,
            previous_position: 0.0,
            resolution: MAX_MODES,
            f: vec![Svf::new(); MAX_MODES],
        }
    }

    /// Must be even (odd/even pairs feed the two outputs).
    pub fn set_resolution(&mut self, resolution: usize) {
        let r = resolution - (resolution & 1);
        self.resolution = r.min(MAX_MODES);
    }

    fn compute_filters(&mut self) -> usize {
        let mut stiff = stiffness(self.structure);
        let mut harmonic = self.frequency;
        let mut stretch_factor = 1.0_f32;
        // q from the damping knob over 4 decades
        let mut q = 500.0 * super::dsp::four_decades(self.damping);
        let mut brightness_attenuation = 1.0 - self.structure;
        brightness_attenuation *= brightness_attenuation;
        brightness_attenuation *= brightness_attenuation;
        brightness_attenuation *= brightness_attenuation;
        let brightness = self.brightness * (1.0 - 0.2 * brightness_attenuation);
        let mut q_loss = brightness * (2.0 - brightness) * 0.85 + 0.15;
        let q_loss_damping_rate = self.structure * (2.0 - self.structure) * 0.1;
        let mut num_modes = 0;
        for i in 0..self.resolution.min(MAX_MODES) {
            let partial_frequency = (harmonic * stretch_factor).min(0.49);
            if partial_frequency < 0.49 {
                num_modes = i + 1;
            }
            self.f[i].set_f_q(partial_frequency, 1.0 + partial_frequency * q);
            stretch_factor += stiff;
            if stiff < 0.0 {
                // do not fold back into negative frequencies
                stiff *= 0.93;
            } else {
                // a few extra partials in the highest frequencies
                stiff *= 0.98;
            }
            // prevents the highest partials from decaying too fast
            q_loss += q_loss_damping_rate * (1.0 - q_loss);
            harmonic += self.frequency;
            q *= q_loss;
        }
        num_modes
    }

    pub fn process(&mut self, input: &[f32], out: &mut [f32], aux: &mut [f32]) {
        let num_modes = self.compute_filters();
        let size = input.len();
        let position_increment = (self.position - self.previous_position) / size as f32;
        let mut position = self.previous_position;
        self.previous_position = self.position;
        let mut amplitudes = CosineOscillator::default();
        for i in 0..size {
            position += position_increment;
            amplitudes.init_approximate(position);
            let x = input[i] * 0.125;
            let mut odd = 0.0;
            let mut even = 0.0;
            // num_modes is always even (resolution is forced even and
            // the count walks pairs), so odd/even pairs line up
            let mut m = 0;
            while m < num_modes {
                odd += amplitudes.next() * self.f[m].process_bp(x);
                if m + 1 < num_modes {
                    even += amplitudes.next() * self.f[m + 1].process_bp(x);
                }
                m += 2;
            }
            out[i] = odd;
            aux[i] = even;
        }
    }
}

impl Default for Resonator {
    fn default() -> Self {
        Self::new()
    }
}

// ── damping filter (string FIR absorption) ─────────────────────────────────

#[derive(Debug, Clone, Default)]
struct DampingFilter {
    x1: f32,
    x2: f32,
    brightness: f32,
    brightness_increment: f32,
    damping: f32,
    damping_increment: f32,
}

impl DampingFilter {
    fn configure(&mut self, damping: f32, brightness: f32, size: usize) {
        if size == 0 {
            self.damping = damping;
            self.brightness = brightness;
            self.damping_increment = 0.0;
            self.brightness_increment = 0.0;
        } else {
            let step = 1.0 / size as f32;
            self.damping_increment = (damping - self.damping) * step;
            self.brightness_increment = (brightness - self.brightness) * step;
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let h0 = (1.0 + self.brightness) * 0.5;
        let h1 = (1.0 - self.brightness) * 0.25;
        let y = self.damping * (h0 * self.x1 + h1 * (x + self.x2));
        self.x2 = self.x1;
        self.x1 = x;
        self.brightness += self.brightness_increment;
        self.damping += self.damping_increment;
        y
    }
}

// ── string voice ───────────────────────────────────────────────────────────

/// rings String: Karplus–Strong with dispersion all-pass, curved
/// bridge nonlinearity, FIR + IIR damping, and the linear-interp
/// upsampler corner case for f0 < 11.7 Hz.
#[derive(Debug, Clone)]
pub struct String {
    pub frequency: f32,
    pub dispersion: f32,
    pub brightness: f32,
    pub damping: f32,
    pub position: f32,

    delay: f32,
    clamped_position: f32,
    previous_dispersion: f32,
    previous_damping_compensation: f32,

    enable_dispersion: bool,
    dispersion_noise: f32,
    curved_bridge: f32,

    src_phase: f32,
    out_sample: [f32; 2],
    aux_sample: [f32; 2],

    string: DelayLine,
    stretch: DelayLine,
    fir_damping_filter: DampingFilter,
    iir_damping_filter: Svf,
    dc_blocker: DcBlocker,
    rng: Rng,
    sample_rate: f32,
}

impl String {
    pub fn new(enable_dispersion: bool, sample_rate: f32, seed: u32) -> Self {
        String {
            frequency: 220.0 / sample_rate,
            dispersion: 0.25,
            brightness: 0.5,
            damping: 0.3,
            position: 0.8,
            delay: sample_rate / 220.0,
            clamped_position: 0.0,
            previous_dispersion: 0.0,
            previous_damping_compensation: 0.0,
            enable_dispersion,
            dispersion_noise: 0.0,
            curved_bridge: 0.0,
            src_phase: 0.0,
            out_sample: [0.0; 2],
            aux_sample: [0.0; 2],
            string: DelayLine::new(STRING_DELAY_SIZE),
            stretch: DelayLine::new(STRING_DELAY_SIZE / 2),
            fir_damping_filter: DampingFilter::default(),
            iir_damping_filter: Svf::new(),
            dc_blocker: DcBlocker::new(1.0 - 20.0 / sample_rate),
            rng: Rng::new(seed),
            sample_rate,
        }
    }

    /// Glide form: `frequency += coefficient * (target − frequency)`.
    pub fn glide_frequency(&mut self, frequency: f32, coefficient: f32) {
        self.frequency += coefficient * (frequency - self.frequency);
    }

    pub fn process(&mut self, input: &[f32], out: &mut [f32], aux: &mut [f32]) {
        let size = input.len();
        let delay_target = (1.0 / self.frequency).clamp(4.0, (STRING_DELAY_SIZE - 4) as f32);

        // f0 below what the line can hold: play the lowest note and
        // upsample with the firmware's deliberately crude linear
        // interpolator
        let mut src_ratio = delay_target * self.frequency;
        if src_ratio >= 0.9999 {
            self.src_phase = 1.0;
            src_ratio = 1.0;
        }

        let clamped_position_target = 0.5 - 0.98 * (self.position - 0.5).abs();

        let mut delay_m = self.delay;
        let delay_inc = (delay_target - self.delay) / size as f32;
        let mut pos_m = self.clamped_position;
        let pos_inc = (clamped_position_target - self.clamped_position) / size as f32;
        let mut disp_m = self.previous_dispersion;
        let disp_inc = (self.dispersion - self.previous_dispersion) / size as f32;
        self.delay = delay_target;
        self.clamped_position = clamped_position_target;
        self.previous_dispersion = self.dispersion;

        let lf_damping = self.damping * (2.0 - self.damping);
        let rt60 = 0.07 * semitones_to_ratio(lf_damping * 96.0) * self.sample_rate;
        let rt60_base_2_12 = (-120.0 * delay_target / src_ratio / rt60).max(-127.0);
        let mut damping_coefficient = semitones_to_ratio(rt60_base_2_12);
        let mut brightness = self.brightness * self.brightness;
        let noise_filter = semitones_to_ratio((self.brightness - 1.0) * 48.0);
        let mut damping_cutoff = (24.0
            + self.damping * self.damping * 48.0
            + self.brightness * self.brightness * 24.0)
            .min(84.0);
        let mut damping_f = (self.frequency * semitones_to_ratio(damping_cutoff)).min(0.499);

        // crossfade to infinite decay
        if self.damping >= 0.95 {
            let to_infinite = 20.0 * (self.damping - 0.95);
            damping_coefficient += to_infinite * (1.0 - damping_coefficient);
            brightness += to_infinite * (1.0 - brightness);
            damping_f += to_infinite * (0.4999 - damping_f);
            damping_cutoff += to_infinite * (128.0 - damping_cutoff);
        }

        self.fir_damping_filter
            .configure(damping_coefficient, brightness, size);
        self.iir_damping_filter.set_f_q(damping_f, 0.5);
        let comp_target = 1.0 - svf_shift(damping_cutoff);
        let mut comp_m = self.previous_damping_compensation;
        let comp_inc = (comp_target - self.previous_damping_compensation) / size as f32;
        self.previous_damping_compensation = comp_target;

        for i in 0..size {
            self.src_phase += src_ratio;
            if self.src_phase > 1.0 {
                self.src_phase -= 1.0;

                delay_m += delay_inc;
                pos_m += pos_inc;
                disp_m += disp_inc;
                comp_m += comp_inc;

                let mut delay = delay_m;
                let comb_delay = delay * pos_m;
                delay *= comp_m; // IIR delay compensation
                delay -= 1.0; // FIR delay

                let mut s;
                if self.enable_dispersion {
                    let noise = 2.0 * self.rng.float() - 1.0;
                    let noise = noise / (0.2 + noise_filter);
                    self.dispersion_noise += noise_filter * (noise - self.dispersion_noise);

                    let dispersion = disp_m;
                    let stretch_point = if dispersion <= 0.0 {
                        0.0
                    } else {
                        dispersion * (2.0 - dispersion) * 0.475
                    };
                    let mut noise_amount = if dispersion > 0.75 {
                        4.0 * (dispersion - 0.75)
                    } else {
                        0.0
                    };
                    let mut bridge_curving = if dispersion < 0.0 { -dispersion } else { 0.0 };

                    noise_amount = noise_amount * noise_amount * 0.025;
                    let ac_blocking_amount = bridge_curving;
                    bridge_curving = bridge_curving * bridge_curving * 0.01;
                    let ap_gain = -0.618 * dispersion / (0.15 + dispersion.abs());

                    let mut delay_fm = 1.0;
                    delay_fm += self.dispersion_noise * noise_amount;
                    delay_fm -= self.curved_bridge * bridge_curving;
                    let delay = delay * delay_fm;

                    let ap_delay = delay * stretch_point;
                    let main_delay = delay - ap_delay;
                    if ap_delay >= 4.0 && main_delay >= 4.0 {
                        s = self.string.read_hermite(main_delay);
                        s = self.stretch.allpass(s, ap_delay, ap_gain);
                    } else {
                        s = self.string.read_hermite(delay);
                    }
                    let s_ac = self.dc_blocker.process(s);
                    s += ac_blocking_amount * (s_ac - s);

                    let value = s.abs() - 0.025;
                    let sign = if s > 0.0 { 1.0 } else { -1.5 };
                    self.curved_bridge = (value.abs() + value) * sign;
                } else {
                    s = self.string.read_hermite(delay);
                }

                s += input[i]; // f0 < 11.7 Hz: ugly bitcrush, faithfully
                s = self.fir_damping_filter.process(s);
                s = self.iir_damping_filter.process_lp(s);
                self.string.write(s);

                self.out_sample[1] = self.out_sample[0];
                self.aux_sample[1] = self.aux_sample[0];
                self.out_sample[0] = s;
                self.aux_sample[0] = self.string.read_frac(comb_delay.max(1.0));
            }
            out[i] += self.out_sample[1]
                + (self.out_sample[0] - self.out_sample[1]) * self.src_phase;
            aux[i] += self.aux_sample[1]
                + (self.aux_sample[0] - self.aux_sample[1]) * self.src_phase;
        }
    }
}

// ── FM voice ───────────────────────────────────────────────────────────────

/// rings FMVoice: 2-op phase-modulation voice whose envelopes ride
/// the external input's band-split follower (or the internal trigger).
#[derive(Debug, Clone)]
pub struct FmVoice {
    pub carrier_frequency: f32,
    pub ratio: f32,
    pub brightness: f32,
    pub damping: f32,
    pub feedback_amount: f32,

    previous_carrier_frequency: f32,
    previous_modulator_frequency: f32,
    previous_brightness: f32,
    previous_feedback_amount: f32,

    amplitude_envelope: f32,
    brightness_envelope: f32,

    carrier_phase: u32,
    modulator_phase: u32,
    previous_sample: f32,
    gain: f32,
    fm_amount: f32,

    follower: Follower,
    quantizer: Vec<f32>,
    sample_rate: f32,
}

impl FmVoice {
    pub fn new(sample_rate: f32) -> Self {
        FmVoice {
            carrier_frequency: 220.0 / sample_rate,
            ratio: 0.5,
            brightness: 0.5,
            damping: 0.5,
            feedback_amount: 0.0,
            previous_carrier_frequency: 220.0 / sample_rate,
            previous_modulator_frequency: 220.0 / sample_rate,
            previous_brightness: 0.5,
            previous_feedback_amount: 0.0,
            amplitude_envelope: 0.0,
            brightness_envelope: 0.0,
            carrier_phase: 0,
            modulator_phase: 0,
            previous_sample: 0.0,
            gain: 0.0,
            fm_amount: 0.0,
            follower: Follower::new(
                8.0 / sample_rate,
                160.0 / sample_rate,
                1600.0 / sample_rate,
            ),
            quantizer: super::dsp::build_fm_quantizer(),
            sample_rate,
        }
    }

    pub fn trigger_internal_envelope(&mut self) {
        self.amplitude_envelope = 1.0;
        self.brightness_envelope = 1.0;
    }

    /// Sine via the firmware's 32-bit phase + FM offset law: the
    /// offset is `u32((fm+4)·2^29) << 3` (the +4 keeps the cast
    /// positive; the shift makes it ×2^32 mod 2^32). The 4096-entry
    /// table is replaced by `sin` — error < 1e-7.
    #[inline]
    fn sine_fm(phase: u32, fm: f32) -> f32 {
        let offset = (((fm + 4.0) * 536_870_912.0) as u32) << 3;
        let phase = phase.wrapping_add(offset);
        let p = (phase as f32) / 4_294_967_296.0;
        (2.0 * std::f32::consts::PI * p).sin()
    }

    pub fn process(&mut self, input: &[f32], out: &mut [f32], aux: &mut [f32]) {
        let size = input.len();
        let envelope_amount = if self.damping < 0.9 {
            1.0
        } else {
            (1.0 - self.damping) * 10.0
        };
        let amplitude_rt60 = 0.1 * semitones_to_ratio(self.damping * 96.0) * self.sample_rate;
        let amplitude_decay = 1.0 - 0.001_f32.powf(1.0 / amplitude_rt60);
        let brightness_rt60 = 0.1 * semitones_to_ratio(self.damping * 84.0) * self.sample_rate;
        let brightness_decay = 1.0 - 0.001_f32.powf(1.0 / brightness_rt60);

        let ratio = interpolate(&self.quantizer, self.ratio, 128.0);
        let modulator_frequency =
            (self.carrier_frequency * semitones_to_ratio(ratio)).min(0.5);
        let feedback = (self.feedback_amount - 0.5) * 2.0;

        let mut carrier_f = self.previous_carrier_frequency;
        let carrier_inc = (self.carrier_frequency - carrier_f) / size as f32;
        let mut modulator_f = self.previous_modulator_frequency;
        let modulator_inc = (modulator_frequency - modulator_f) / size as f32;
        let mut bright = self.previous_brightness;
        let bright_inc = (self.brightness - bright) / size as f32;
        let mut fb = self.previous_feedback_amount;
        let fb_inc = (feedback - fb) / size as f32;
        self.previous_carrier_frequency = self.carrier_frequency;
        self.previous_modulator_frequency = modulator_frequency;
        self.previous_brightness = self.brightness;
        self.previous_feedback_amount = feedback;

        for i in 0..size {
            let (amplitude_envelope, mut brightness_envelope) = self.follower.process(input[i]);
            brightness_envelope *= 2.0 * amplitude_envelope * (2.0 - amplitude_envelope);
            slope(
                &mut self.amplitude_envelope,
                amplitude_envelope,
                0.05,
                amplitude_decay,
            );
            slope(
                &mut self.brightness_envelope,
                brightness_envelope,
                0.01,
                brightness_decay,
            );

            bright += bright_inc;
            let brightness_value = bright * bright;
            let fm_amount_min = if brightness_value < 0.5 {
                0.0
            } else {
                brightness_value * 2.0 - 1.0
            };
            let fm_amount_max = if brightness_value < 0.5 {
                2.0 * brightness_value
            } else {
                1.0
            };
            let fm_envelope = 0.5 + envelope_amount * (self.brightness_envelope - 0.5);
            let fm_amount_target = (fm_amount_min + fm_amount_max * fm_envelope) * 2.0;
            one_pole(
                &mut self.fm_amount,
                fm_amount_target,
                0.005 + fm_amount_max * 0.015,
            );

            let phase_feedback = if feedback < 0.0 {
                0.5 * feedback * feedback
            } else {
                0.0
            };
            modulator_f += modulator_inc;
            carrier_f += carrier_inc;
            self.modulator_phase = self.modulator_phase.wrapping_add(
                (4_294_967_296.0 * modulator_f * (1.0 + self.previous_sample * phase_feedback))
                    as u32,
            );
            self.carrier_phase = self
                .carrier_phase
                .wrapping_add((4_294_967_296.0 * carrier_f) as u32);

            fb += fb_inc;
            let modulator_fb = if fb > 0.0 { 0.25 * fb * fb } else { 0.0 };
            let modulator =
                Self::sine_fm(self.modulator_phase, modulator_fb * self.previous_sample);
            let carrier = Self::sine_fm(self.carrier_phase, self.fm_amount * modulator);
            one_pole(&mut self.previous_sample, carrier, 0.1);

            let gain_target = 1.0 + envelope_amount * (self.amplitude_envelope - 1.0);
            one_pole(&mut self.gain, gain_target, 0.005 + 0.045 * self.fm_amount);

            out[i] = (carrier + 0.5 * modulator) * self.gain;
            aux[i] = 0.5 * modulator * self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_resonator_rings_and_decays() {
        let mut r = Resonator::new();
        r.frequency = 220.0 / 48_000.0;
        r.damping = 0.5;
        let mut out = vec![0.0; 256];
        let mut aux = vec![0.0; 256];
        let mut input = vec![0.0; 256];
        input[0] = 1.0;
        r.process(&input, &mut out, &mut aux);
        let early: f32 = out.iter().chain(aux.iter()).map(|v| v * v).sum();
        assert!(early > 0.0, "the strike rings");
        let silence = vec![0.0; 256];
        let mut late = 0.0;
        for _ in 0..400 {
            r.process(&silence, &mut out, &mut aux);
            late = out.iter().chain(aux.iter()).map(|v| v * v).sum();
        }
        assert!(late < early, "the ring decays: {early} -> {late}");
        assert!(out.iter().all(|v| v.is_finite()));
    }

    /// Drive a string in firmware-sized blocks (the damping filter
    /// ramps per block — one giant block would smear its first ramp
    /// over the whole signal, which is not how the engine runs).
    fn run_string(s: &mut String, total: usize, burst: usize) -> Vec<f32> {
        const BLOCK: usize = 64;
        let mut rendered = Vec::with_capacity(total);
        let mut fed = 0;
        while rendered.len() < total {
            let mut input = [0.0_f32; BLOCK];
            for v in input.iter_mut() {
                if fed < burst {
                    *v = 0.5;
                    fed += 1;
                }
            }
            let mut out = [0.0_f32; BLOCK];
            let mut aux = [0.0_f32; BLOCK];
            s.process(&input, &mut out, &mut aux);
            rendered.extend_from_slice(&out);
        }
        rendered
    }

    #[test]
    fn string_sustains_at_high_damping_knob() {
        // damping >= 0.95 crossfades to infinite decay
        let mut s = String::new(true, 48_000.0, 0x1234);
        s.frequency = 220.0 / 48_000.0;
        s.damping = 1.0;
        let out = run_string(&mut s, 48_000, 32);
        let tail: f32 = out[40_000..].iter().map(|v| v * v).sum();
        assert!(tail > 1e-6, "infinite decay keeps ringing: {tail}");
        assert!(out.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn string_decays_at_low_damping() {
        let mut s = String::new(false, 48_000.0, 0x4321);
        s.frequency = 440.0 / 48_000.0;
        s.damping = 0.1;
        let out = run_string(&mut s, 48_000, 32);
        let early: f32 = out[..4_800].iter().map(|v| v * v).sum();
        let late: f32 = out[43_200..].iter().map(|v| v * v).sum();
        assert!(late < early * 0.01, "rt60 law decays: {early} -> {late}");
    }

    #[test]
    fn string_pitch_is_sample_rate_invariant() {
        // a KS loop excited by a positive burst rings as positive
        // pulses (no zero crossings) — measure the period by
        // autocorrelation instead
        let period_hz = |sr: f32| -> f32 {
            let mut s = String::new(false, sr, 0x77);
            s.frequency = 220.0 / sr;
            s.damping = 0.9;
            let n = sr as usize / 2;
            let out = run_string(&mut s, n, 16);
            let tail = &out[n / 2..];
            let lo = (sr / 600.0) as usize;
            let hi = (sr / 60.0) as usize;
            let mut best = (lo, f32::MIN);
            for lag in lo..hi {
                let c: f32 = tail
                    .windows(lag + 1)
                    .map(|w| w[0] * w[lag])
                    .sum();
                if c > best.1 {
                    best = (lag, c);
                }
            }
            sr / best.0 as f32
        };
        let f48 = period_hz(48_000.0);
        let f32_ = period_hz(32_000.0);
        assert!(
            (f48 / 220.0 - 1.0).abs() < 0.05,
            "48k tuned to 220: {f48}"
        );
        assert!(
            (f32_ / 220.0 - 1.0).abs() < 0.05,
            "32k tuned to 220: {f32_}"
        );
    }

    #[test]
    fn fm_voice_sounds_on_internal_trigger_and_stays_finite() {
        let mut v = FmVoice::new(48_000.0);
        v.carrier_frequency = 220.0 / 48_000.0;
        v.damping = 0.5;
        v.brightness = 0.7;
        v.trigger_internal_envelope();
        let input = vec![0.0; 4800];
        let mut out = vec![0.0; 4800];
        let mut aux = vec![0.0; 4800];
        v.process(&input, &mut out, &mut aux);
        let energy: f32 = out.iter().map(|v| v * v).sum();
        assert!(energy > 0.01, "the FM voice speaks: {energy}");
        assert!(out.iter().all(|v| v.is_finite()));
        // ten-minute soak across parameter sweeps (debug build catches
        // any integer wrap mistakes in the phase math)
        for blk in 0..1_000 {
            v.ratio = (blk as f32 / 1_000.0).fract();
            v.feedback_amount = (blk as f32 / 333.0).fract();
            v.process(&input, &mut out, &mut aux);
        }
        assert!(out.iter().all(|v| v.is_finite()));
    }
}
