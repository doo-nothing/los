//! # Rings DSP — primitives, lookup laws, exciters
//!
//! A faithful port of Mutable Instruments Rings' DSP foundation:
//! the stmlib state-variable filter and delay line, the approximate
//! cosine oscillator the resonator's position comb uses, the note
//! filter (median + dual-lag smoothing of pitch), the band-split
//! envelope follower, the stereo limiter, and the noise-burst
//! plucker. The modal/string/FM voices live in `models.rs`, the part
//! dispatcher in `part.rs`.
//!
//! Ported from pichenettes/eurorack (rings/dsp/*, stmlib/dsp/*),
//! copyright 2015 Emilie Gillet, MIT license; attribution preserved.
//! Lookup tables are transcribed analytically from
//! rings/resources/lookup_tables.py rather than embedded.
//!
//! Sample-rate note: the firmware runs at 48 kHz. Every law here is
//! either rate-independent or written against `sample_rate`, so the
//! port stays honest on other host rates (the elements port's +7 st
//! sample-rate bug is the cautionary tale).

#![allow(clippy::excessive_precision)]

/// Firmware native rate — laws expressed in firmware samples scale
/// from here.
pub const NATIVE_SR: f32 = 48_000.0;

/// 2^(x/12) without `powf` in the hot path: split into octave shift
/// and fractional semitone (matches stmlib's SemitonesToRatio law).
#[inline]
pub fn semitones_to_ratio(semitones: f32) -> f32 {
    (semitones / 12.0).exp2()
}

/// stmlib SoftLimit: x(27+x²)/(27+9x²).
#[inline]
pub fn soft_limit(x: f32) -> f32 {
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

/// SLOPE macro: asymmetric one-pole (different up/down coefficients).
#[inline]
pub fn slope(y: &mut f32, x: f32, up: f32, down: f32) {
    let coef = if x > *y { up } else { down };
    *y += coef * (x - *y);
}

/// ONE_POLE macro.
#[inline]
pub fn one_pole(y: &mut f32, x: f32, coef: f32) {
    *y += coef * (x - *y);
}

// ── lookup-table laws (lookup_tables.py, transcribed) ──────────────────────

/// `lut_4_decades`: 10^(4x) for x in 0..1.
#[inline]
pub fn four_decades(x: f32) -> f32 {
    10.0_f32.powf(4.0 * x.clamp(0.0, 1.0))
}

/// `lut_stiffness` (rings flavor — different from elements'):
/// negative inharmonicity below 0.25, a harmonic plateau, an
/// exponential rise, then a cosine push to 2.0 ("bell" territory).
pub fn stiffness(structure: f32) -> f32 {
    let g = structure.clamp(0.0, 1.0);
    if g >= 0.996 {
        // table endpoints are pinned to exactly 2.0
        return 2.0;
    }
    if g < 0.25 {
        -(0.25 - g) * 0.25
    } else if g < 0.3 {
        0.0
    } else if g < 0.9 {
        let g = (g - 0.3) / 0.6;
        0.01 * 10.0_f32.powf(g * 2.005) - 0.01
    } else {
        let g = ((g - 0.9) / 0.1).powi(2);
        1.5 - (g * std::f32::consts::PI).cos() / 2.0
    }
}

/// `lut_svf_shift`: group-delay compensation for the string's IIR
/// damping filter. Indexed by semitone offset (0..=256 in the table);
/// value = 2·atan(2^(−i/12))/(2π).
#[inline]
pub fn svf_shift(index: f32) -> f32 {
    let i = index.clamp(0.0, 256.0);
    let ratio = (i / 12.0).exp2();
    2.0 * (1.0 / ratio).atan() / (2.0 * std::f32::consts::PI)
}

/// `lut_fm_frequency_quantizer`: 23 musically-useful FM ratios in
/// semitone space, each tripled for plateaus, gap-filled to 128
/// entries by repeatedly splitting the largest interval (the exact
/// generator from lookup_tables.py).
pub fn build_fm_quantizer() -> Vec<f32> {
    let ratios: [f32; 23] = [
        0.5,
        0.5 * (16.0 / 1200.0_f32).exp2(),
        std::f32::consts::SQRT_2 / 2.0,
        std::f32::consts::FRAC_PI_4,
        1.0,
        (16.0 / 1200.0_f32).exp2(),
        std::f32::consts::SQRT_2,
        std::f32::consts::FRAC_PI_2,
        7.0 / 4.0,
        2.0,
        2.0 * (16.0 / 1200.0_f32).exp2(),
        9.0 / 4.0,
        11.0 / 4.0,
        2.0 * std::f32::consts::SQRT_2,
        3.0,
        std::f32::consts::PI,
        3.0_f32.sqrt() * 2.0,
        4.0,
        std::f32::consts::SQRT_2 * 3.0,
        std::f32::consts::PI * 1.5,
        5.0,
        std::f32::consts::SQRT_2 * 4.0,
        8.0,
    ];
    let mut scale: Vec<f32> = Vec::with_capacity(128);
    for r in ratios {
        let st = 12.0 * r.log2();
        scale.extend_from_slice(&[st, st, st]);
    }
    let target = scale.len().next_power_of_two();
    while scale.len() < target {
        let gap = scale
            .windows(2)
            .enumerate()
            .max_by(|a, b| {
                let da = a.1[1] - a.1[0];
                let db = b.1[1] - b.1[0];
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        let mid = (scale[gap] + scale[gap + 1]) / 2.0;
        scale.insert(gap + 1, mid);
    }
    scale.push(*scale.last().unwrap_or(&0.0));
    scale
}

/// Linear interpolation into a generated table, stmlib `Interpolate`
/// semantics: `index = x * scale`, integral/fractional split.
#[inline]
pub fn interpolate(table: &[f32], x: f32, scale: f32) -> f32 {
    let index = (x * scale).max(0.0);
    let i = (index as usize).min(table.len().saturating_sub(2));
    let f = index - i as f32;
    table[i] + (table[i + 1] - table[i]) * f
}

// ── stmlib Svf ─────────────────────────────────────────────────────────────

/// stmlib state-variable filter (filter.h), exact-tan flavor. The
/// firmware picks FAST/ACCURATE/DIRTY tangent approximations per call
/// site to save cycles; on a desktop CPU the exact prewarp is both
/// cheaper to reason about and consistent with the analytically
/// transcribed `svf_shift` compensation, so all three map here.
#[derive(Debug, Clone, Default)]
pub struct Svf {
    g: f32,
    r: f32,
    h: f32,
    state1: f32,
    state2: f32,
}

impl Svf {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.set_f_q(0.01, 100.0);
        s
    }

    pub fn reset(&mut self) {
        self.state1 = 0.0;
        self.state2 = 0.0;
    }

    /// `set_f_q`: f is normalized frequency (cycles/sample), q the
    /// resonance quality.
    pub fn set_f_q(&mut self, f: f32, q: f32) {
        let f = f.clamp(1e-6, 0.499);
        self.g = (std::f32::consts::PI * f).tan();
        self.r = 1.0 / q.max(1e-3);
        self.h = 1.0 / (1.0 + self.r * self.g + self.g * self.g);
    }

    #[inline]
    pub fn process_bp(&mut self, x: f32) -> f32 {
        let hp = (x - self.r * self.state1 - self.g * self.state1 - self.state2) * self.h;
        let bp = self.g * hp + self.state1;
        self.state1 = self.g * hp + bp;
        let lp = self.g * bp + self.state2;
        self.state2 = self.g * bp + lp;
        bp
    }

    #[inline]
    pub fn process_lp(&mut self, x: f32) -> f32 {
        let hp = (x - self.r * self.state1 - self.g * self.state1 - self.state2) * self.h;
        let bp = self.g * hp + self.state1;
        self.state1 = self.g * hp + bp;
        let lp = self.g * bp + self.state2;
        self.state2 = self.g * bp + lp;
        lp
    }

    #[inline]
    pub fn process_hp(&mut self, x: f32) -> f32 {
        let hp = (x - self.r * self.state1 - self.g * self.state1 - self.state2) * self.h;
        let bp = self.g * hp + self.state1;
        self.state1 = self.g * hp + bp;
        let lp = self.g * bp + self.state2;
        self.state2 = self.g * bp + lp;
        hp
    }
}

/// stmlib NaiveSvf (Chamberlin form) — the follower's band-split uses
/// it; `lp()` exposes the low-pass state for cascading.
#[derive(Debug, Clone, Default)]
pub struct NaiveSvf {
    f: f32,
    damp: f32,
    lp: f32,
    bp: f32,
}

impl NaiveSvf {
    pub fn set_f_q(&mut self, f: f32, q: f32) {
        let f = f.clamp(1e-6, 0.497);
        self.f = 2.0 * (std::f32::consts::PI * f).sin();
        self.damp = 1.0 / q.max(1e-3);
    }

    #[inline]
    pub fn process_hp(&mut self, x: f32) -> f32 {
        let bp_normalized = self.bp * self.damp;
        let notch = x - bp_normalized;
        self.lp += self.f * self.bp;
        let hp = notch - self.lp;
        self.bp += self.f * hp;
        hp
    }

    #[inline]
    pub fn lp(&self) -> f32 {
        self.lp
    }
}

// ── delay line ─────────────────────────────────────────────────────────────

/// stmlib DelayLine: write head walks backwards in firmware; here a
/// forward head with read-at-delay semantics, including the Hermite
/// interpolated read and the all-pass read the string's stretch stage
/// uses.
#[derive(Debug, Clone)]
pub struct DelayLine {
    buf: Vec<f32>,
    write: usize,
}

impl DelayLine {
    pub fn new(len: usize) -> Self {
        DelayLine {
            buf: vec![0.0; len.max(8)],
            write: 0,
        }
    }

    pub fn reset(&mut self) {
        self.buf.fill(0.0);
        self.write = 0;
    }

    #[inline]
    pub fn write(&mut self, v: f32) {
        self.write = (self.write + self.buf.len() - 1) % self.buf.len();
        self.buf[self.write] = v;
    }

    /// Integer-delay read (delay counted from the most recent write).
    #[inline]
    pub fn read(&self, delay: usize) -> f32 {
        let n = self.buf.len();
        self.buf[(self.write + delay.min(n - 1)) % n]
    }

    /// Fractional read, linear interpolation.
    #[inline]
    pub fn read_frac(&self, delay: f32) -> f32 {
        let n = self.buf.len();
        let delay = delay.clamp(1.0, (n - 2) as f32);
        let d = delay as usize;
        let f = delay - d as f32;
        let a = self.buf[(self.write + d) % n];
        let b = self.buf[(self.write + d + 1) % n];
        a + (b - a) * f
    }

    /// 4-point Hermite read (stmlib ReadHermite).
    #[inline]
    pub fn read_hermite(&self, delay: f32) -> f32 {
        let n = self.buf.len();
        let delay = delay.clamp(2.0, (n - 3) as f32);
        let d = delay as usize;
        let f = delay - d as f32;
        let xm1 = self.buf[(self.write + d - 1) % n];
        let x0 = self.buf[(self.write + d) % n];
        let x1 = self.buf[(self.write + d + 1) % n];
        let x2 = self.buf[(self.write + d + 2) % n];
        let c = (x1 - xm1) * 0.5;
        let v = x0 - x1;
        let w = c + v;
        let a = w + v + (x2 - x0) * 0.5;
        let b_neg = w + a;
        ((((a * f) - b_neg) * f + c) * f) + x0
    }

    /// All-pass read/write at fractional delay with gain (stmlib
    /// Allpass): reads at `delay`, writes `x + read·gain`, returns
    /// `read − written·gain`.
    #[inline]
    pub fn allpass(&mut self, x: f32, delay: f32, gain: f32) -> f32 {
        let read = self.read_frac(delay);
        let written = x + read * gain;
        self.write(written);
        read - written * gain
    }
}

// ── DC blocker ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct DcBlocker {
    pole: f32,
    x: f32,
    y: f32,
}

impl DcBlocker {
    pub fn new(pole: f32) -> Self {
        DcBlocker {
            pole,
            x: 0.0,
            y: 0.0,
        }
    }

    #[inline]
    pub fn process(&mut self, s: f32) -> f32 {
        let y = self.pole * self.y + s - self.x;
        self.x = s;
        self.y = y;
        y
    }
}

// ── cosine oscillator ──────────────────────────────────────────────────────

/// stmlib CosineOscillator, both flavors. Rings' resonator uses the
/// APPROXIMATE init (a folded parabola standing in for 2cos(2πf)) —
/// the hardware's exact voicing of the mode-amplitude combs depends
/// on its small error, so it is reproduced bit-for-law here (the
/// elements audit flagged exact-vs-approximate as an audible nuance).
#[derive(Debug, Clone, Default)]
pub struct CosineOscillator {
    y1: f32,
    y0: f32,
    iir: f32,
    initial: f32,
}

impl CosineOscillator {
    pub fn init_approximate(&mut self, frequency: f32) {
        let mut sign = 16.0;
        let mut f = frequency - 0.25;
        if f < 0.0 {
            f = -f;
        } else if f > 0.5 {
            f -= 0.5;
        } else {
            sign = -16.0;
        }
        self.iir = sign * f * (1.0 - 2.0 * f);
        self.initial = self.iir * 0.25;
        self.start();
    }

    pub fn init_exact(&mut self, frequency: f32) {
        self.iir = 2.0 * (2.0 * std::f32::consts::PI * frequency).cos();
        self.initial = self.iir * 0.25;
        self.start();
    }

    #[inline]
    pub fn start(&mut self) {
        self.y1 = self.initial;
        self.y0 = 0.5;
    }

    #[inline]
    pub fn next(&mut self) -> f32 {
        let temp = self.y0;
        self.y0 = self.iir * self.y0 - self.y1;
        self.y1 = temp;
        temp + 0.5
    }
}

// ── note filter ────────────────────────────────────────────────────────────

/// rings NoteFilter: median-of-4 + dual-lag smoothing. A fresh edge
/// (note jump > 0.4 st or a strum) snaps; the steady part glides with
/// a recovering coefficient. `stable_note` feeds the voice that is
/// NOT currently being strummed.
#[derive(Debug, Clone)]
pub struct NoteFilter {
    fast_coefficient: f32,
    slow_coefficient: f32,
    lag_coefficient: f32,
    note: f32,
    stable_note: f32,
    coefficient: f32,
    stable_coefficient: f32,
    previous_values: [f32; 4],
}

impl NoteFilter {
    pub fn new(control_rate: f32) -> Self {
        let fast = 1.0 / (0.001 * control_rate);
        let slow = 1.0 / (0.010 * control_rate);
        let lag = 1.0 / (0.050 * control_rate);
        NoteFilter {
            fast_coefficient: fast,
            slow_coefficient: slow,
            lag_coefficient: lag,
            note: 69.0,
            stable_note: 69.0,
            coefficient: fast,
            stable_coefficient: slow,
            previous_values: [69.0; 4],
        }
    }

    pub fn process(&mut self, note: f32, strum: bool) -> f32 {
        if (note - self.note).abs() > 0.4 || strum {
            self.stable_note = note;
            self.note = note;
            self.coefficient = self.fast_coefficient;
            self.stable_coefficient = self.slow_coefficient;
            self.previous_values = [note; 4];
        } else {
            self.previous_values.rotate_left(1);
            self.previous_values[3] = note;
            let mut sorted = self.previous_values;
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = 0.5 * (sorted[1] + sorted[2]);

            self.note += self.coefficient * (median - self.note);
            self.stable_note += self.stable_coefficient * (self.note - self.stable_note);
            self.coefficient += self.lag_coefficient * (self.slow_coefficient - self.coefficient);
            // upstream quirk preserved: the stable coefficient decays
            // toward the LAG coefficient, not the slow one
            self.stable_coefficient +=
                self.lag_coefficient * (self.lag_coefficient - self.stable_coefficient);
        }
        self.note
    }

    #[inline]
    pub fn note(&self) -> f32 {
        self.note
    }

    #[inline]
    pub fn stable_note(&self) -> f32 {
        self.stable_note
    }
}

// ── follower ───────────────────────────────────────────────────────────────

/// rings Follower: 3-band split with per-band asymmetric envelope
/// detectors; outputs total energy + a slewed spectral centroid.
/// Drives the FM voice's external-excitation envelopes.
#[derive(Debug, Clone)]
pub struct Follower {
    low_mid_filter: NaiveSvf,
    mid_high_filter: NaiveSvf,
    attack: [f32; 3],
    decay: [f32; 3],
    detector: [f32; 3],
    centroid: f32,
}

impl Follower {
    pub fn new(low: f32, low_mid: f32, mid_high: f32) -> Self {
        let mut low_mid_filter = NaiveSvf::default();
        let mut mid_high_filter = NaiveSvf::default();
        low_mid_filter.set_f_q(low_mid, 0.5);
        mid_high_filter.set_f_q(mid_high, 0.5);
        Follower {
            low_mid_filter,
            mid_high_filter,
            attack: [
                low_mid,
                (low_mid * mid_high).sqrt(),
                (mid_high * 0.5).sqrt(),
            ],
            decay: [
                (low_mid * low).sqrt(),
                low_mid,
                (mid_high * low_mid).sqrt(),
            ],
            detector: [0.0; 3],
            centroid: 0.0,
        }
    }

    /// Returns (envelope, centroid).
    #[inline]
    pub fn process(&mut self, sample: f32) -> (f32, f32) {
        let band2 = self.mid_high_filter.process_hp(sample);
        let band1 = self.low_mid_filter.process_hp(self.mid_high_filter.lp());
        let band0 = self.low_mid_filter.lp();
        let bands = [band0, band1, band2];

        let mut weighted = 0.0;
        let mut total = 0.0;
        let mut frequency = 0.0;
        for (i, b) in bands.iter().enumerate() {
            slope(&mut self.detector[i], b.abs(), self.attack[i], self.decay[i]);
            weighted += self.detector[i] * frequency;
            total += self.detector[i];
            frequency += 0.5;
        }

        let error = weighted / (total + 0.001) - self.centroid;
        let coefficient = if error > 0.0 { 0.05 } else { 0.001 };
        self.centroid += error * coefficient;
        (total, self.centroid)
    }
}

// ── limiter ────────────────────────────────────────────────────────────────

/// rings Limiter: stereo peak follower with soft clip toward 10Vpp.
#[derive(Debug, Clone)]
pub struct Limiter {
    peak: f32,
}

impl Limiter {
    pub fn new() -> Self {
        Limiter { peak: 0.5 }
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], pre_gain: f32) {
        for (l, r) in l.iter_mut().zip(r.iter_mut()) {
            let l_pre = *l * pre_gain;
            let r_pre = *r * pre_gain;
            let peak = l_pre.abs().max(r_pre.abs()).max((r_pre - l_pre).abs());
            slope(&mut self.peak, peak, 0.05, 0.00002);
            let gain = if self.peak <= 1.0 { 1.0 } else { 1.0 / self.peak };
            *l = soft_limit(l_pre * gain * 0.8);
            *r = soft_limit(r_pre * gain * 0.8);
        }
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

// ── rng ────────────────────────────────────────────────────────────────────

/// xorshift32 standing in for stmlib's LCG — same distribution, used
/// where the firmware calls Random::GetFloat().
#[derive(Debug, Clone)]
pub struct Rng {
    state: u32,
}

impl Rng {
    pub fn new(seed: u32) -> Self {
        Rng {
            state: seed.max(1),
        }
    }

    #[inline]
    pub fn word(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn float(&mut self) -> f32 {
        (self.word() >> 8) as f32 / 16_777_216.0
    }
}

// ── plucker ────────────────────────────────────────────────────────────────

/// rings Plucker: noise burst through a tuned comb + LP filter — the
/// internal excitation for the string models.
#[derive(Debug, Clone)]
pub struct Plucker {
    svf: Svf,
    comb: DelayLine,
    rng: Rng,
    remaining_samples: usize,
    comb_period: f32,
    comb_gain: f32,
}

impl Plucker {
    pub fn new(seed: u32) -> Self {
        Plucker {
            svf: Svf::new(),
            comb: DelayLine::new(256),
            rng: Rng::new(seed),
            remaining_samples: 0,
            comb_period: 0.0,
            comb_gain: 0.0,
        }
    }

    pub fn trigger(&mut self, frequency: f32, cutoff: f32, position: f32) {
        let ratio = position * 0.9 + 0.05;
        let mut comb_period = 1.0 / frequency * ratio;
        self.remaining_samples = comb_period as usize;
        while comb_period >= 255.0 {
            comb_period *= 0.5;
        }
        self.comb_period = comb_period;
        self.comb_gain = (1.0 - position) * 0.8;
        self.svf.set_f_q(cutoff.min(0.499), 1.0);
    }

    pub fn process(&mut self, out: &mut [f32]) {
        for v in out.iter_mut() {
            let input = if self.remaining_samples > 0 {
                self.remaining_samples -= 1;
                2.0 * self.rng.float() - 1.0
            } else {
                0.0
            };
            let combed = input + self.comb_gain * self.comb.read_frac(self.comb_period.max(1.0));
            self.comb.write(combed);
            *v = self.svf.process_lp(combed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stiffness_law_matches_generator() {
        // anchors from lookup_tables.py
        assert!((stiffness(0.0) - (-0.0625)).abs() < 1e-3, "{}", stiffness(0.0));
        assert!(stiffness(0.27).abs() < 1e-6, "plateau is zero");
        assert!((stiffness(0.9) - (0.01 * 10.0_f32.powf(2.005) - 0.01)).abs() < 0.02);
        assert!((stiffness(1.0) - 2.0).abs() < 1e-6, "endpoint pinned to 2");
        // monotone through the exponential region
        assert!(stiffness(0.6) > stiffness(0.4));
    }

    #[test]
    fn svf_shift_matches_formula() {
        // index 0 → 2·atan(1)/2π = 0.25
        assert!((svf_shift(0.0) - 0.25).abs() < 1e-6);
        // large index → ratio huge → shift → 0
        assert!(svf_shift(256.0) < 0.001);
    }

    #[test]
    fn fm_quantizer_has_128_entries_and_plateaus() {
        let q = build_fm_quantizer();
        assert_eq!(q.len(), 129, "128 + guard entry");
        // first ratio 0.5 → −12 st; last 8.0 → +36 st
        assert!((q[0] - (-12.0)).abs() < 1e-4);
        assert!((q[127] - 36.0).abs() < 1e-4);
        // monotone non-decreasing
        assert!(q.windows(2).all(|w| w[1] >= w[0] - 1e-6));
    }

    #[test]
    fn cosine_approximate_tracks_cosine_loosely() {
        // the approximation is a folded parabola — verify it stays in
        // [0,1] and oscillates at roughly the right rate
        let mut c = CosineOscillator::default();
        c.init_approximate(0.05);
        let v: Vec<f32> = (0..40).map(|_| c.next()).collect();
        assert!(v.iter().all(|x| (-0.1..=1.1).contains(x)), "{v:?}");
        // ~2 cycles in 40 samples at f=0.05: at least one full swing
        let max = v.iter().cloned().fold(f32::MIN, f32::max);
        let min = v.iter().cloned().fold(f32::MAX, f32::min);
        assert!(max > 0.9 && min < 0.1, "swings the full range: {min}..{max}");
    }

    #[test]
    fn delay_line_hermite_recovers_a_ramp() {
        let mut d = DelayLine::new(64);
        for i in 0..64 {
            d.write(i as f32);
        }
        // most recent write = 63 at delay 0; Hermite at delay 10.5
        // on a linear ramp must be exact
        let v = d.read_hermite(10.5);
        assert!((v - (63.0 - 10.5)).abs() < 1e-3, "{v}");
    }

    #[test]
    fn note_filter_snaps_on_edges_and_glides_in_between() {
        let mut nf = NoteFilter::new(48_000.0 / 24.0);
        nf.process(60.0, true);
        assert!((nf.note() - 60.0).abs() < 1e-6, "strum snaps");
        // a sustained small offset: the median admits it after two
        // samples and the lag glides toward it without snapping
        let mut n = 60.0;
        for _ in 0..3 {
            n = nf.process(60.2, false);
        }
        assert!(n > 60.0 && n < 60.2, "median+lag glide: {n}");
    }

    #[test]
    fn limiter_keeps_hot_signals_bounded() {
        let mut lim = Limiter::new();
        let mut l = vec![4.0; 4800];
        let mut r = vec![-4.0; 4800];
        lim.process(&mut l, &mut r, 1.4);
        assert!(l.iter().all(|v| v.abs() <= 1.2), "soft-limited");
        assert!(l.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn plucker_bursts_then_decays() {
        let mut p = Plucker::new(0xbeef);
        p.trigger(220.0 / 48_000.0, 0.25, 0.5);
        let mut burst = vec![0.0; 512];
        p.process(&mut burst);
        let burst_energy: f32 = burst.iter().map(|v| v * v).sum();
        assert!(burst_energy > 0.0, "the pluck sounds");
        let mut tail = vec![0.0; 4096];
        p.process(&mut tail);
        let tail_energy: f32 = tail[2048..].iter().map(|v| v * v).sum();
        assert!(tail_energy < burst_energy * 0.1, "the burst dies out");
    }
}
