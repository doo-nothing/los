//! The faithful 20-band channel vocoder from warps' `vocoder.cc` /
//! `filter_bank.cc`, ported to run at the los session rate.
//!
//! The hardware runs the whole modulator at 96 kHz and the filter bank's
//! `CrossoverSvf` coefficients (`filter_bank_table`) are baked for that
//! rate — every band sits at `fn <= 0.35` of its decimated sample rate,
//! which keeps the modified-Chamberlin SVF inside its accurate region.
//! Re-deriving the table at 48 kHz would push the top bands to `fn ~ 0.7`
//! where that SVF mistunes, so instead we run the vocoder path at 96 kHz:
//! the incoming 48 kHz carrier/modulator are 2× upsampled, the filter bank
//! runs with the verbatim 96 kHz table, and the result is 2× downsampled.
//! Audio is buffered into fixed 48-sample frames (96 oversampled samples =
//! `kMaxFilterBankBlockSize`, a multiple of the 12× low-group decimation),
//! introducing one frame (~1 ms) of latency.

#![allow(clippy::excessive_precision)]

use super::dsp::soft_limit;

const NUM_BANDS: usize = 20;
const NATIVE_SR: f32 = 96000.0;
const FRAME: usize = 48; // session-rate samples per vocoder frame
const OVS_FRAME: usize = FRAME * 2; // 96 oversampled samples (== kMaxFilterBankBlockSize)
const LOW_FACTOR: usize = 4;
const MID_FACTOR: usize = 3;

#[inline]
fn semitones_to_ratio(s: f32) -> f32 {
    2.0_f32.powf(s / 12.0)
}

// ── filter_bank_table (warps/resources.cc, verbatim, baked for 96 kHz) ───────
// Per band: [decimation_factor, delay, post_gain, f0, fq0, f1, fq1].
#[rustfmt::skip]
const FILTER_BANK_TABLE: [[f32; 7]; NUM_BANDS] = [
    [12.0, 110.0, 1.000000000e+00, -7.027765790e-02, 2.374233182e-02, -4.036276502e-02, 5.641820638e-02],
    [12.0, 306.0, 2.500000000e-01, -9.339148150e-02, 1.518494012e-02, -7.931045024e-02, 1.290681251e-02],
    [12.0, 243.0, 2.500000000e-01, -1.175253889e-01, 1.909187455e-02, -9.982608285e-02, 1.623625964e-02],
    [12.0, 192.0, 2.500000000e-01, -1.478403116e-01, 2.399019825e-02, -1.256102632e-01, 2.041717073e-02],
    [12.0, 153.0, 2.500000000e-01, -1.858796047e-01, 3.012281471e-02, -1.579890264e-01, 2.566359509e-02],
    [12.0, 121.0, 2.500000000e-01, -2.335415850e-01, 3.778648442e-02, -1.986017761e-01, 3.224184764e-02],
    [12.0,  96.0, 2.500000000e-01, -2.931359041e-01, 4.733980069e-02, -2.494585699e-01, 4.048298668e-02],
    [12.0,  76.0, 2.500000000e-01, -3.674252077e-01, 5.920914546e-02, -3.129923847e-01, 5.079873862e-02],
    [12.0,  60.0, 2.500000000e-01, -4.596234805e-01, 7.388912125e-02, -3.920877866e-01, 6.370275117e-02],
    [12.0,  48.0, 2.500000000e-01, -5.732942351e-01, 9.193058505e-02, -4.900471292e-01, 7.984109811e-02],
    [12.0,  38.0, 2.500000000e-01, -7.120398134e-01, 1.139025399e-01, -6.104158833e-01, 1.000408086e-01],
    [12.0,  30.0, 2.500000000e-01, -8.787817044e-01, 1.402986139e-01, -7.565125704e-01, 1.253975556e-01],
    [12.0,  24.0, 2.500000000e-01, -9.303628940e-01, 1.574589048e-01, -1.074283292e+00, 1.713175349e-01],
    [ 3.0,  76.0, 2.500000000e-01, -3.674252077e-01, 5.920914546e-02, -3.129923847e-01, 5.079873862e-02],
    [ 3.0,  60.0, 2.500000000e-01, -4.596234805e-01, 7.388912125e-02, -3.920877866e-01, 6.370275117e-02],
    [ 3.0,  48.0, 2.500000000e-01, -5.732942351e-01, 9.193058505e-02, -4.900471292e-01, 7.984109811e-02],
    [ 3.0,  38.0, 2.500000000e-01, -7.120398134e-01, 1.139025399e-01, -6.104158833e-01, 1.000408086e-01],
    [ 3.0,  30.0, 2.500000000e-01, -8.787817044e-01, 1.402986139e-01, -7.565125704e-01, 1.253975556e-01],
    [ 3.0,  24.0, 2.500000000e-01, -9.303628940e-01, 1.574589048e-01, -1.074283292e+00, 1.713175349e-01],
    [ 1.0,   6.0, 3.080000000e+00, -4.088135601e-01, 1.514311173e-01, -5.414722362e-01, 6.413463290e-01],
];

// ── sample-rate-conversion FIR coefficients (half-length, mirrored) ──────────
// warps/dsp/sample_rate_conversion_filters.h, plus a 2× pair (remez, same style).
#[rustfmt::skip]
const SRC_UP_4_HALF: [f32; 24] = [
    -6.014371929e-04, -1.116027480e-03, -1.547569918e-03, -1.288608084e-03,
     2.786886230e-04,  3.529342828e-03,  8.203156385e-03,  1.308970614e-02,
     1.600199910e-02,  1.419074690e-02,  5.231038872e-03, -1.177915684e-02,
    -3.506738553e-02, -5.953252182e-02, -7.699933415e-02, -7.757902368e-02,
    -5.198496872e-02,  5.703716839e-03,  9.559598586e-02,  2.106660616e-01,
     3.371310483e-01,  4.566603688e-01,  5.500087786e-01,  6.012053946e-01,
];
#[rustfmt::skip]
const SRC_DOWN_4_HALF: [f32; 24] = [
    -1.503592982e-04, -2.790068700e-04, -3.868924795e-04, -3.221520211e-04,
     6.967215575e-05,  8.823357070e-04,  2.050789096e-03,  3.272426536e-03,
     4.000499774e-03,  3.547686724e-03,  1.307759718e-03, -2.944789209e-03,
    -8.766846381e-03, -1.488313045e-02, -1.924983354e-02, -1.939475592e-02,
    -1.299624218e-02,  1.425929210e-03,  2.389899647e-02,  5.266651541e-02,
     8.428276207e-02,  1.141650922e-01,  1.375021946e-01,  1.503013486e-01,
];
#[rustfmt::skip]
const SRC_UP_3_HALF: [f32; 18] = [
     2.111177486e-04,  9.399136027e-04,  2.516356933e-03,  4.847507152e-03,
     6.912087023e-03,  6.524576194e-03,  8.579855461e-04, -1.203466052e-02,
    -3.103696515e-02, -5.013495031e-02, -5.827142630e-02, -4.183809689e-02,
     1.038391226e-02,  1.014554664e-01,  2.222529437e-01,  3.515426263e-01,
     4.610075226e-01,  5.238640837e-01,
];
#[rustfmt::skip]
const SRC_DOWN_3_HALF: [f32; 18] = [
     7.037258286e-05,  3.133045342e-04,  8.387856444e-04,  1.615835717e-03,
     2.304029008e-03,  2.174858731e-03,  2.859951820e-04, -4.011553507e-03,
    -1.034565505e-02, -1.671165010e-02, -1.942380877e-02, -1.394603230e-02,
     3.461304086e-03,  3.381848881e-02,  7.408431457e-02,  1.171808754e-01,
     1.536691742e-01,  1.746213612e-01,
];
#[rustfmt::skip]
const SRC_UP_2_HALF: [f32; 24] = [
    -3.062140377e-04, -5.996681663e-04,  9.958173886e-04,  1.354119329e-03,
    -2.149501674e-03, -2.873342487e-03,  4.166512612e-03,  5.343159695e-03,
    -7.337116504e-03, -9.170096158e-03,  1.211159599e-02,  1.487163169e-02,
    -1.911109751e-02, -2.322948429e-02,  2.934450809e-02,  3.562014150e-02,
    -4.476728825e-02, -5.499806753e-02,  7.010873291e-02,  8.954681976e-02,
    -1.209897645e-01, -1.738710097e-01,  2.969947527e-01,  8.987374072e-01,
];
#[rustfmt::skip]
const SRC_DOWN_2_HALF: [f32; 24] = [
    -1.531070188e-04, -2.998340832e-04,  4.979086943e-04,  6.770596645e-04,
    -1.074750837e-03, -1.436671244e-03,  2.083256306e-03,  2.671579848e-03,
    -3.668558252e-03, -4.585048079e-03,  6.055797995e-03,  7.435815844e-03,
    -9.555548754e-03, -1.161474215e-02,  1.467225405e-02,  1.781007075e-02,
    -2.238364412e-02, -2.749903376e-02,  3.505436645e-02,  4.477340988e-02,
    -6.049488226e-02, -8.693550485e-02,  1.484973763e-01,  4.493687036e-01,
];

/// Reconstruct a symmetric FIR of length `2 * half.len()` from its
/// stored first half (the firmware stores half and mirrors at use).
fn mirror(half: &[f32]) -> Vec<f32> {
    let n = half.len() * 2;
    (0..n)
        .map(|i| {
            if i < half.len() {
                half[i]
            } else {
                half[n - 1 - i]
            }
        })
        .collect()
}

// ── polyphase sample-rate converters (runtime port of the templated SRC) ─────

/// Interpolating polyphase up-sampler (1 input → `ratio` outputs).
#[derive(Debug, Clone)]
struct SrcUp {
    ratio: usize,
    h: Vec<f32>, // full mirrored FIR, length filter_size
    hist: Vec<f32>,
}

impl SrcUp {
    fn new(ratio: usize, half: &[f32]) -> Self {
        let h = mirror(half);
        let taps = h.len() / ratio;
        Self {
            ratio,
            h,
            hist: vec![0.0; taps],
        }
    }

    fn delay(&self) -> usize {
        self.h.len() / self.ratio / 2
    }

    /// Push one input sample, emit `ratio` output samples.
    fn process(&mut self, x: f32, out: &mut [f32]) {
        // hist[0] is newest.
        self.hist.rotate_right(1);
        self.hist[0] = x;
        for (p, o) in out.iter_mut().enumerate().take(self.ratio) {
            let mut acc = 0.0;
            for (i, &hx) in self.hist.iter().enumerate() {
                acc += hx * self.h[p + i * self.ratio];
            }
            *o = acc;
        }
    }
}

/// Decimating polyphase down-sampler (`ratio` inputs → 1 output).
#[derive(Debug, Clone)]
struct SrcDown {
    ratio: usize,
    h: Vec<f32>, // full mirrored FIR, length filter_size == taps
    hist: Vec<f32>,
}

impl SrcDown {
    fn new(ratio: usize, half: &[f32]) -> Self {
        let h = mirror(half);
        let n = h.len();
        Self {
            ratio,
            h,
            hist: vec![0.0; n],
        }
    }

    fn delay(&self) -> usize {
        self.h.len() / 2
    }

    /// Consume `in_buf` (length a multiple of `ratio`), emit
    /// `in_buf.len() / ratio` samples into `out`.
    fn process(&mut self, in_buf: &[f32], out: &mut [f32]) {
        let n = self.h.len();
        let mut o = 0;
        let mut i = 0;
        while i < in_buf.len() {
            for _ in 0..self.ratio {
                self.hist.rotate_right(1);
                self.hist[0] = in_buf[i];
                i += 1;
            }
            // hist[0] newest; symmetric FIR so orientation is immaterial.
            let mut acc = 0.0;
            for k in 0..n {
                acc += self.hist[k] * self.h[k];
            }
            out[o] = acc;
            o += 1;
        }
    }
}

// ── CrossoverSvf (stmlib/dsp/filter.h, two cascaded chamberlin stages) ───────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SvfMode {
    LowPass,
    BandPassNormalized,
    HighPass,
}

#[derive(Debug, Clone, Default)]
struct CrossoverSvf {
    f: f32,
    fq: f32,
    lp: [f32; 2],
    bp: [f32; 2],
    x: [f32; 2],
}

impl CrossoverSvf {
    fn set_f_fq(&mut self, f: f32, fq: f32) {
        self.f = f;
        self.fq = fq;
    }

    fn process(&mut self, mode: SvfMode, src: &[f32], dst: &mut [f32]) {
        let (f, fq) = (self.f, self.fq);
        let (mut lp1, mut bp1) = (self.lp[0], self.bp[0]);
        let (mut lp2, mut bp2) = (self.lp[1], self.bp[1]);
        let (mut x1, mut x2) = (self.x[0], self.x[1]);
        let bp_mode = mode == SvfMode::BandPassNormalized;
        for (d, &input) in dst.iter_mut().zip(src.iter()) {
            lp1 += f * bp1;
            bp1 += -fq * bp1 - f * lp1 + input;
            if bp_mode {
                bp1 += x1;
            }
            x1 = input;
            let y = match mode {
                SvfMode::LowPass => lp1 * f,
                SvfMode::BandPassNormalized => bp1 * fq,
                SvfMode::HighPass => x1 - lp1 * f - bp1 * fq,
            };
            lp2 += f * bp2;
            bp2 += -fq * bp2 - f * lp2 + y;
            if bp_mode {
                bp2 += x2;
            }
            x2 = y;
            *d = match mode {
                SvfMode::LowPass => lp2 * f,
                SvfMode::BandPassNormalized => bp2 * fq,
                SvfMode::HighPass => x2 - lp2 * f - bp2 * fq,
            };
        }
        self.lp = [lp1, lp2];
        self.bp = [bp1, bp2];
        self.x = [x1, x2];
    }
}

// ── pooled delay line (group-delay compensation) ─────────────────────────────

#[derive(Debug, Clone)]
struct PooledDelayLine {
    buf: Vec<f32>,
    head: usize,
}

impl PooledDelayLine {
    fn new(delay: usize) -> Self {
        Self {
            buf: vec![0.0; delay + 1],
            head: 0,
        }
    }
    fn read_write(&mut self, value: f32) -> f32 {
        self.buf[self.head] = value;
        self.head = (self.head + 1) % self.buf.len();
        self.buf[self.head]
    }
}

// ── filter bank ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Band {
    group: usize,
    sample_rate: f32,
    post_gain: f32,
    svf: [CrossoverSvf; 2],
    decimation_factor: usize,
    samples: Vec<f32>,
    delay_line: PooledDelayLine,
}

#[derive(Debug, Clone)]
struct FilterBank {
    mid_down: SrcDown,
    mid_up: SrcUp,
    low_down: SrcDown,
    low_up: SrcUp,
    bands: Vec<Band>,
    // group scratch for analyze/synthesize: low (/12), mid (/3), high (/1)
    tmp_mid: Vec<f32>, // size / MID_FACTOR
    tmp_low: Vec<f32>, // size / (MID*LOW)
    last_group: usize,
}

impl FilterBank {
    fn new(sample_rate: f32) -> Self {
        let mid_down = SrcDown::new(MID_FACTOR, &SRC_DOWN_3_HALF);
        let mid_up = SrcUp::new(MID_FACTOR, &SRC_UP_3_HALF);
        let low_down = SrcDown::new(LOW_FACTOR, &SRC_DOWN_4_HALF);
        let low_up = SrcUp::new(LOW_FACTOR, &SRC_UP_4_HALF);

        let mut bands: Vec<Band> = Vec::with_capacity(NUM_BANDS);
        let mut group: isize = -1;
        let mut decimation_factor = 0usize;
        let mut delays = [0usize; NUM_BANDS];
        let mut max_delay = 0usize;
        for (i, c) in FILTER_BANK_TABLE.iter().enumerate() {
            let dec = c[0] as usize;
            if dec != decimation_factor {
                decimation_factor = dec;
                group += 1;
            }
            let delay = (c[1] as usize) * dec;
            delays[i] = delay;
            max_delay = max_delay.max(delay);
            let mut svf = [CrossoverSvf::default(), CrossoverSvf::default()];
            for (pass, s) in svf.iter_mut().enumerate() {
                s.set_f_fq(c[pass * 2 + 3], c[pass * 2 + 4]);
            }
            bands.push(Band {
                group: group as usize,
                sample_rate: sample_rate / dec as f32,
                post_gain: c[2],
                svf,
                decimation_factor: dec,
                samples: vec![0.0; OVS_FRAME / dec],
                delay_line: PooledDelayLine::new(0),
            });
        }
        max_delay = max_delay.min(256);
        // Group-delay compensation (warps/dsp/filter_bank.cc Init).
        let low_comp =
            LOW_FACTOR * (low_down.delay() + low_up.delay()) + mid_down.delay() + mid_up.delay();
        let mid_comp = mid_down.delay() + mid_up.delay();
        for (i, b) in bands.iter_mut().enumerate() {
            let mut compensation = (max_delay as isize) - (delays[i] as isize);
            if b.group == 0 {
                compensation -= low_comp as isize;
            } else if b.group == 1 {
                compensation -= mid_comp as isize;
            }
            compensation = (compensation - (b.decimation_factor as isize) / 2).max(0);
            b.delay_line = PooledDelayLine::new(compensation as usize / b.decimation_factor);
        }

        Self {
            mid_down,
            mid_up,
            low_down,
            low_up,
            bands,
            tmp_mid: vec![0.0; OVS_FRAME / MID_FACTOR],
            tmp_low: vec![0.0; OVS_FRAME / (MID_FACTOR * LOW_FACTOR)],
            last_group: bands_last_group(),
        }
    }

    fn band_sample_rate(&self, i: usize) -> f32 {
        self.bands[i].sample_rate
    }
    fn band_decimation(&self, i: usize) -> usize {
        self.bands[i].decimation_factor
    }

    /// Split `input` (length OVS_FRAME) into the per-band `samples`.
    fn analyze(&mut self, input: &[f32]) {
        let size = input.len();
        // Pre-decimate into the mid (/3) and low (/12) group rates.
        let mut mid = std::mem::take(&mut self.tmp_mid);
        let mut low = std::mem::take(&mut self.tmp_low);
        self.mid_down.process(input, &mut mid[..size / MID_FACTOR]);
        self.low_down.process(
            &mid[..size / MID_FACTOR],
            &mut low[..size / (MID_FACTOR * LOW_FACTOR)],
        );

        for (i, b) in self.bands.iter_mut().enumerate() {
            let band_size = size / b.decimation_factor;
            // group 0 -> low, 1 -> mid, 2 -> high (input)
            let group_in: &[f32] = match b.group {
                0 => &low[..band_size],
                1 => &mid[..band_size],
                _ => &input[..band_size],
            };
            let mode = if i == 0 {
                SvfMode::LowPass
            } else if i == NUM_BANDS - 1 {
                SvfMode::HighPass
            } else {
                SvfMode::BandPassNormalized
            };
            // pass 0: group_in -> samples ; pass 1: samples -> samples
            let mut scratch = vec![0.0; band_size];
            b.svf[0].process(mode, group_in, &mut scratch);
            b.svf[1].process(mode, &scratch, &mut b.samples[..band_size]);
            let gain = b.post_gain;
            for s in b.samples[..band_size].iter_mut() {
                *s *= gain;
            }
        }
        self.tmp_mid = mid;
        self.tmp_low = low;
    }

    /// Recombine the per-band `samples` into `out` (length OVS_FRAME).
    fn synthesize(&mut self, out: &mut [f32]) {
        let size = out.len();
        let mut mid = std::mem::take(&mut self.tmp_mid);
        let mut low = std::mem::take(&mut self.tmp_low);
        for v in low[..size / (MID_FACTOR * LOW_FACTOR)].iter_mut() {
            *v = 0.0;
        }
        for v in mid[..size / MID_FACTOR].iter_mut() {
            *v = 0.0;
        }
        for v in out.iter_mut() {
            *v = 0.0;
        }
        for i in 0..self.bands.len() {
            let (group, dec) = {
                let b = &self.bands[i];
                (b.group, b.decimation_factor)
            };
            let band_size = size / dec;
            for j in 0..band_size {
                let v = self.bands[i].samples[j];
                let d = self.bands[i].delay_line.read_write(v);
                match group {
                    0 => low[j] += d,
                    1 => mid[j] += d,
                    _ => out[j] += d,
                }
            }
            // When the next band changes group, up-sample this group up a level.
            let next_group = if i + 1 < self.bands.len() {
                self.bands[i + 1].group
            } else {
                self.last_group
            };
            if next_group != group {
                if group == 0 {
                    // low (/12) -> mid (/3): ×4
                    self.up_into(&mut low, &mut mid, band_size, LOW_FACTOR, true);
                } else if group == 1 {
                    // mid (/3) -> out (/1): ×3
                    self.up_into(&mut mid, out, band_size, MID_FACTOR, false);
                }
            }
        }
        self.tmp_mid = mid;
        self.tmp_low = low;
    }

    /// Up-sample `band_size` samples of `src` by `ratio` into `dst`.
    fn up_into(
        &mut self,
        src: &mut [f32],
        dst: &mut [f32],
        band_size: usize,
        ratio: usize,
        low: bool,
    ) {
        let mut chunk = vec![0.0; ratio];
        let conv = if low {
            &mut self.low_up
        } else {
            &mut self.mid_up
        };
        for j in 0..band_size {
            conv.process(src[j], &mut chunk);
            for (k, &c) in chunk.iter().enumerate() {
                dst[j * ratio + k] += c;
            }
        }
    }
}

fn bands_last_group() -> usize {
    // group of the sentinel band_[kNumBands] = last real group + 1
    let mut group = 0usize;
    let mut dec = FILTER_BANK_TABLE[0][0] as usize;
    for c in FILTER_BANK_TABLE.iter() {
        if c[0] as usize != dec {
            dec = c[0] as usize;
            group += 1;
        }
    }
    group + 1
}

// ── envelope follower ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct EnvelopeFollower {
    attack: f32,
    decay: f32,
    envelope: f32,
    peak: f32,
    freeze: bool,
}

impl EnvelopeFollower {
    fn new() -> Self {
        Self {
            attack: 0.1,
            decay: 0.1,
            envelope: 0.0,
            peak: 0.0,
            freeze: false,
        }
    }
    fn process(&mut self, input: &[f32], out: &mut [f32], gain: f32) {
        let mut envelope = self.envelope;
        let attack = if self.freeze { 0.0 } else { self.attack };
        let decay = if self.freeze { 0.0 } else { self.decay };
        let mut peak = 0.0_f32;
        for (o, &x) in out.iter_mut().zip(input.iter()) {
            let error = (x * gain).abs() - envelope;
            envelope += (if error > 0.0 { attack } else { decay }) * error;
            if envelope > peak {
                peak = envelope;
            }
            *o = envelope;
        }
        self.envelope = envelope;
        let error = peak - self.peak;
        self.peak += (if error > 0.0 { 0.5 } else { 0.1 }) * error;
    }
}

// ── limiter (warps/dsp/limiter.h) ────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Limiter {
    peak: f32,
}
impl Limiter {
    fn new() -> Self {
        Self { peak: 0.5 }
    }
    fn process(&mut self, in_out: &mut [f32], pre_gain: f32) {
        for s in in_out.iter_mut() {
            let v = *s * pre_gain;
            let target = v.abs();
            let slope = if target > self.peak { 0.05 } else { 0.00002 };
            self.peak += slope * (target - self.peak);
            let gain = if self.peak <= 1.0 {
                1.0
            } else {
                1.0 / self.peak
            };
            *s = soft_limit(v * gain * 0.8);
        }
    }
}

// ── the vocoder ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
struct BandGain {
    carrier: f32,
    vocoder: f32,
}

/// The 20-band channel vocoder, faithful to warps' `vocoder.cc`, wrapped
/// in a 2× oversampling + framing layer so it can run at the 48 kHz
/// session rate. One `FRAME` (~1 ms) of latency.
#[derive(Debug, Clone)]
pub struct Vocoder {
    release_time: f32,
    formant_shift: f32,
    follower_gain: f32,
    previous_gain: [BandGain; NUM_BANDS],
    gain: [BandGain; NUM_BANDS],
    modulator_fb: FilterBank,
    carrier_fb: FilterBank,
    follower: Vec<EnvelopeFollower>,
    limiter: Limiter,
    // 2× oversampling
    up_mod: SrcUp,
    up_car: SrcUp,
    down_out: SrcDown,
    // framing
    in_mod: Vec<f32>,
    in_car: Vec<f32>,
    fill: usize,
    out_ring: std::collections::VecDeque<f32>,
}

impl Vocoder {
    pub fn new(_sample_rate: f32) -> Self {
        let follower = (0..NUM_BANDS).map(|_| EnvelopeFollower::new()).collect();
        Self {
            release_time: 0.5,
            formant_shift: 0.5,
            follower_gain: (NUM_BANDS as f32).sqrt(),
            previous_gain: [BandGain::default(); NUM_BANDS],
            gain: [BandGain::default(); NUM_BANDS],
            modulator_fb: FilterBank::new(NATIVE_SR),
            carrier_fb: FilterBank::new(NATIVE_SR),
            follower,
            limiter: Limiter::new(),
            up_mod: SrcUp::new(2, &SRC_UP_2_HALF),
            up_car: SrcUp::new(2, &SRC_UP_2_HALF),
            down_out: SrcDown::new(2, &SRC_DOWN_2_HALF),
            in_mod: vec![0.0; FRAME],
            in_car: vec![0.0; FRAME],
            fill: 0,
            out_ring: std::iter::repeat_n(0.0, FRAME).collect(), // prime latency
        }
    }

    pub fn set_release_time(&mut self, t: f32) {
        self.release_time = t.clamp(0.0, 1.0);
    }
    pub fn set_formant_shift(&mut self, f: f32) {
        self.formant_shift = f.clamp(0.0, 1.0);
    }

    pub fn process(&mut self, modulator: &[f32], carrier: &[f32], out: &mut [f32]) {
        for n in 0..out.len() {
            self.in_mod[self.fill] = modulator[n];
            self.in_car[self.fill] = carrier[n];
            self.fill += 1;
            if self.fill == FRAME {
                self.process_frame();
                self.fill = 0;
            }
            out[n] = self.out_ring.pop_front().unwrap_or(0.0);
        }
    }

    fn process_frame(&mut self) {
        // 2× upsample the 48-sample frame to 96 oversampled samples.
        let mut mod_ovs = vec![0.0; OVS_FRAME];
        let mut car_ovs = vec![0.0; OVS_FRAME];
        let mut pair = [0.0; 2];
        for i in 0..FRAME {
            self.up_mod.process(self.in_mod[i], &mut pair);
            mod_ovs[i * 2] = pair[0];
            mod_ovs[i * 2 + 1] = pair[1];
            self.up_car.process(self.in_car[i], &mut pair);
            car_ovs[i * 2] = pair[0];
            car_ovs[i * 2 + 1] = pair[1];
        }

        // Run through filter banks.
        self.modulator_fb.analyze(&mod_ovs);
        self.carrier_fb.analyze(&car_ovs);

        // Attack/release of envelope followers (third-octave spaced).
        let mut f = 80.0 * semitones_to_ratio(-72.0 * self.release_time);
        for i in 0..NUM_BANDS {
            let decay = f / self.modulator_fb.band_sample_rate(i);
            self.follower[i].attack = decay * 2.0;
            self.follower[i].decay = decay * 0.5;
            self.follower[i].freeze = self.release_time > 0.995;
            f *= 1.2599; // 2 ** (4/12)
        }

        // Per-band amplitude / modulation amount (formant shift).
        let mut formant_shift_amount = 2.0 * (self.formant_shift - 0.5).abs();
        formant_shift_amount *= 2.0 - formant_shift_amount;
        formant_shift_amount *= 2.0 - formant_shift_amount;
        let envelope_increment = 4.0 * semitones_to_ratio(-48.0 * self.formant_shift);
        let mut envelope = 0.0_f32;
        let last_band = NUM_BANDS as f32 - 1.0001;
        for i in 0..NUM_BANDS {
            let source_band = envelope.clamp(0.0, last_band);
            let integral = source_band as usize;
            let fractional = source_band - integral as f32;
            let a = self.follower[integral].peak;
            let b = self.follower[integral + 1].peak;
            let mut band_gain = a + (b - a) * fractional;
            let attenuation = envelope - last_band;
            if attenuation >= 0.0 {
                band_gain *= 1.0 / (1.0 + attenuation);
            }
            envelope += envelope_increment;
            self.gain[i].carrier = band_gain * formant_shift_amount;
            self.gain[i].vocoder = 1.0 - formant_shift_amount;
        }

        // Apply the modulation to the carrier bands.
        let mut env_buf = vec![0.0; OVS_FRAME];
        for i in 0..NUM_BANDS {
            let band_size = OVS_FRAME / self.modulator_fb.band_decimation(i);
            let step = 1.0 / band_size as f32;
            self.follower[i].process(
                &self.modulator_fb.bands[i].samples[..band_size],
                &mut env_buf[..band_size],
                self.follower_gain,
            );
            let mut vocoder_gain = self.previous_gain[i].vocoder;
            let vocoder_inc = (self.gain[i].vocoder - vocoder_gain) * step;
            let mut carrier_gain = self.previous_gain[i].carrier;
            let carrier_inc = (self.gain[i].carrier - carrier_gain) * step;
            let car = &mut self.carrier_fb.bands[i].samples;
            for j in 0..band_size {
                car[j] *= carrier_gain + vocoder_gain * env_buf[j];
                vocoder_gain += vocoder_inc;
                carrier_gain += carrier_inc;
            }
            self.previous_gain[i] = self.gain[i];
        }

        // Resynthesize and limit, then 2× downsample back to 48 samples.
        let mut out_ovs = vec![0.0; OVS_FRAME];
        self.carrier_fb.synthesize(&mut out_ovs);
        self.limiter.process(&mut out_ovs, 1.4);
        let mut out_frame = vec![0.0; FRAME];
        self.down_out.process(&out_ovs, &mut out_frame);
        for v in out_frame {
            self.out_ring.push_back(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With formant_shift centred the vocoder imposes the modulator's
    /// per-band envelope onto the carrier. A noise carrier driven by an
    /// energetic modulator must be bounded and far louder than the same
    /// carrier driven by silence.
    #[test]
    fn vocoder_is_bounded_and_envelope_gated() {
        let n = 8192;
        let mut seed: u32 = 0x1234_5678;
        let mut rng = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0
        };
        let carrier: Vec<f32> = (0..n).map(|_| rng() * 0.4).collect();
        // broadband modulator (noise) so many bands energize
        let modulator: Vec<f32> = (0..n).map(|_| rng() * 0.5).collect();
        let silence = vec![0.0_f32; n];

        let run = |modu: &[f32]| {
            let mut voc = Vocoder::new(48_000.0);
            voc.set_release_time(0.3);
            voc.set_formant_shift(0.5);
            let mut out = vec![0.0; n];
            voc.process(modu, &carrier, &mut out);
            out
        };
        let voiced = run(&modulator);
        let quiet = run(&silence);
        assert!(
            voiced
                .iter()
                .chain(quiet.iter())
                .all(|v| v.is_finite() && v.abs() <= 2.0),
            "vocoder output bounded"
        );
        // measure the settled tail (skip latency + attack ramp)
        let e_voiced: f32 = voiced[2000..].iter().map(|v| v * v).sum();
        let e_quiet: f32 = quiet[2000..].iter().map(|v| v * v).sum();
        assert!(e_voiced > 1e-3, "voiced produces sound: {e_voiced}");
        assert!(
            e_voiced > e_quiet * 8.0,
            "gated: voiced {e_voiced} >> quiet {e_quiet}"
        );
    }

    /// The filter bank alone should reconstruct a broadband signal with
    /// roughly flat gain (analyze → synthesize is near-unity).
    #[test]
    fn filter_bank_reconstruction_is_stable() {
        let mut fb = FilterBank::new(NATIVE_SR);
        let mut seed: u32 = 0x9e37_79b9;
        for _ in 0..64 {
            let block: Vec<f32> = (0..OVS_FRAME)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 9) as f32 / (1u32 << 23) as f32 * 2.0 - 1.0
                })
                .collect();
            fb.analyze(&block);
            let mut out = vec![0.0; OVS_FRAME];
            fb.synthesize(&mut out);
            assert!(
                out.iter().all(|v| v.is_finite() && v.abs() < 8.0),
                "filter bank reconstruction bounded"
            );
        }
    }
}
