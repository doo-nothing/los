//! Console DSP for the mixer: RBJ biquad EQ, soft-clip drive, equal-power
//! pan, stereo width, and the 1 ms parameter smoothers that keep every
//! gain-class move click-free (docs/plans/mixer-v2.md).

/// One biquad section (RBJ cookbook), direct form I, mono.
#[derive(Debug, Clone, Copy, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Identity (flat) coefficients.
    pub fn unity() -> Self {
        Self {
            b0: 1.0,
            ..Default::default()
        }
    }

    fn set(&mut self, b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) {
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    /// RBJ low shelf at `f0` Hz, `db` gain (S = 1).
    pub fn low_shelf(&mut self, fs: f32, f0: f32, db: f32) {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sw, cw) = (w0.sin(), w0.cos());
        let alpha = sw / 2.0 * 2f32.sqrt();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        self.set(
            a * ((a + 1.0) - (a - 1.0) * cw + two_sqrt_a_alpha),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
            a * ((a + 1.0) - (a - 1.0) * cw - two_sqrt_a_alpha),
            (a + 1.0) + (a - 1.0) * cw + two_sqrt_a_alpha,
            -2.0 * ((a - 1.0) + (a + 1.0) * cw),
            (a + 1.0) + (a - 1.0) * cw - two_sqrt_a_alpha,
        );
    }

    /// RBJ high shelf at `f0` Hz, `db` gain (S = 1).
    pub fn high_shelf(&mut self, fs: f32, f0: f32, db: f32) {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sw, cw) = (w0.sin(), w0.cos());
        let alpha = sw / 2.0 * 2f32.sqrt();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        self.set(
            a * ((a + 1.0) + (a - 1.0) * cw + two_sqrt_a_alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
            a * ((a + 1.0) + (a - 1.0) * cw - two_sqrt_a_alpha),
            (a + 1.0) - (a - 1.0) * cw + two_sqrt_a_alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cw),
            (a + 1.0) - (a - 1.0) * cw - two_sqrt_a_alpha,
        );
    }

    /// RBJ peaking bell at `f0` Hz, `db` gain, Q ≈ 0.9.
    pub fn peaking(&mut self, fs: f32, f0: f32, db: f32) {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let (sw, cw) = (w0.sin(), w0.cos());
        let alpha = sw / (2.0 * 0.9);
        self.set(
            1.0 + alpha * a,
            -2.0 * cw,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cw,
            1.0 - alpha / a,
        );
    }
}

/// Soft-clip drive: 0 = bit-transparent bypass; otherwise tanh with
/// output compensation so peaks stay near unity.
pub fn drive(x: f32, amount: f32) -> f32 {
    if amount <= 0.0 {
        return x;
    }
    let g = 1.0 + 9.0 * amount.clamp(0.0, 1.0);
    (x * g).tanh() / g.tanh()
}

/// Equal-power pan: −1 hard left, 0 center (−3 dB each), +1 hard right.
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let t = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (t.cos(), t.sin())
}

/// Mid-band sweep: param 0..1 → 200 Hz – 5 kHz, log.
pub fn mid_freq_hz(param: f32) -> f32 {
    200.0 * 25f32.powf(param.clamp(0.0, 1.0))
}

/// One-pole parameter smoother (~1 ms at 48 kHz with the default coeff):
/// every gain-class param goes through one — the anti-click law.
#[derive(Debug, Clone, Copy)]
pub struct Smoother {
    pub current: f32,
    pub target: f32,
}

impl Smoother {
    pub fn new(v: f32) -> Self {
        Self {
            current: v,
            target: v,
        }
    }

    #[inline]
    pub fn tick(&mut self) -> f32 {
        self.current += (self.target - self.current) * 0.02;
        self.current
    }
}

/// Per-strip DSP state: drive → 3-band EQ → pan → level, stereo.
pub struct ChannelDsp {
    pub lo: [Biquad; 2],
    pub mid: [Biquad; 2],
    pub hi: [Biquad; 2],
    pub level: Smoother,
    pub pan: Smoother,
    pub drive_amt: Smoother,
    /// Post-fader fx send taps (mixer sends A/B).
    pub send_a: Smoother,
    pub send_b: Smoother,
    /// Last EQ params the coefficients were computed for.
    coeffs_for: (f32, f32, f32, f32),
}

impl ChannelDsp {
    pub fn new() -> Self {
        Self {
            lo: [Biquad::unity(); 2],
            mid: [Biquad::unity(); 2],
            hi: [Biquad::unity(); 2],
            level: Smoother::new(0.0),
            pan: Smoother::new(0.0),
            drive_amt: Smoother::new(0.0),
            send_a: Smoother::new(0.0),
            send_b: Smoother::new(0.0),
            coeffs_for: (f32::NAN, f32::NAN, f32::NAN, f32::NAN),
        }
    }

    /// Recompute EQ coefficients when the (slot-rate smoothed) params
    /// moved. lo/hi gains in dB, freq as the 0..1 sweep param.
    pub fn tune(&mut self, fs: f32, lo_db: f32, mid_db: f32, freq: f32, hi_db: f32) {
        let want = (lo_db, mid_db, freq, hi_db);
        if want == self.coeffs_for {
            return;
        }
        self.coeffs_for = want;
        for side in 0..2 {
            self.lo[side].low_shelf(fs, 120.0, lo_db);
            self.mid[side].peaking(fs, mid_freq_hz(freq), mid_db);
            self.hi[side].high_shelf(fs, 8000.0, hi_db);
        }
    }

    /// Process one stereo frame through drive → EQ (pan/level are applied
    /// by the caller with this struct's smoothers).
    #[inline]
    pub fn process(&mut self, l: f32, r: f32, drive_amt: f32) -> (f32, f32) {
        let l = drive(l, drive_amt);
        let r = drive(r, drive_amt);
        let l = self.hi[0].process(self.mid[0].process(self.lo[0].process(l)));
        let r = self.hi[1].process(self.mid[1].process(self.lo[1].process(r)));
        (l, r)
    }
}

impl Default for ChannelDsp {
    fn default() -> Self {
        Self::new()
    }
}

/// Stereo width via mid/side: 0 = mono, 1 = unchanged, 2 = extra wide.
pub fn width(l: f32, r: f32, w: f32) -> (f32, f32) {
    let m = (l + r) * 0.5;
    let s = (l - r) * 0.5 * w.clamp(0.0, 2.0);
    (m + s, m - s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RMS gain of a biquad at frequency `f` (steady state).
    fn gain_at(make: impl Fn(&mut Biquad), f: f32, fs: f32) -> f32 {
        let mut bq = Biquad::unity();
        make(&mut bq);
        let n = (fs as usize) / 2;
        let (mut in_sq, mut out_sq) = (0.0f64, 0.0f64);
        for i in 0..n {
            let x = (2.0 * std::f32::consts::PI * f * i as f32 / fs).sin();
            let y = bq.process(x);
            if i > n / 4 {
                in_sq += (x * x) as f64;
                out_sq += (y * y) as f64;
            }
        }
        ((out_sq / in_sq) as f32).sqrt()
    }

    fn db(g: f32) -> f32 {
        20.0 * g.log10()
    }

    #[test]
    fn shelves_and_bell_hit_their_gains() {
        let fs = 48000.0;
        // low shelf +12 dB: strong at 40 Hz, flat at 5 kHz
        let lo = |bq: &mut Biquad| bq.low_shelf(fs, 120.0, 12.0);
        assert!((db(gain_at(lo, 40.0, fs)) - 12.0).abs() < 1.5);
        assert!(db(gain_at(lo, 5000.0, fs)).abs() < 0.5);
        // high shelf −9 dB: cut at 14 kHz, flat at 200 Hz
        let hi = |bq: &mut Biquad| bq.high_shelf(fs, 8000.0, -9.0);
        assert!((db(gain_at(hi, 14000.0, fs)) + 9.0).abs() < 1.5);
        assert!(db(gain_at(hi, 200.0, fs)).abs() < 0.5);
        // bell +9 dB at 1 kHz: peak at center, near-flat 3 octaves out
        let bell = |bq: &mut Biquad| bq.peaking(fs, 1000.0, 9.0);
        assert!((db(gain_at(bell, 1000.0, fs)) - 9.0).abs() < 1.0);
        assert!(db(gain_at(bell, 125.0, fs)).abs() < 1.0);
        // zero gain = flat everywhere
        let flat = |bq: &mut Biquad| bq.peaking(fs, 1000.0, 0.0);
        for f in [100.0, 1000.0, 10000.0] {
            assert!(db(gain_at(flat, f, fs)).abs() < 0.2, "flat at {f}");
        }
    }

    #[test]
    fn drive_is_transparent_at_zero_and_bounded() {
        for x in [-1.0f32, -0.3, 0.0, 0.5, 1.0] {
            assert_eq!(drive(x, 0.0), x, "bit-transparent bypass");
        }
        let mut prev = -2.0;
        for i in 0..=20 {
            let x = -1.0 + i as f32 / 10.0;
            let y = drive(x, 0.8);
            assert!(y.abs() <= 1.001, "bounded");
            assert!(y >= prev, "monotonic");
            prev = y;
        }
        // saturation actually saturates: mid levels come up
        assert!(drive(0.25, 1.0) > 0.5);
    }

    #[test]
    fn pan_law_and_width() {
        let (l, r) = pan_gains(0.0);
        assert!((db(l) + 3.01).abs() < 0.1, "center is −3 dB");
        assert!((l - r).abs() < 1e-6);
        let (l, r) = pan_gains(-1.0);
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6, "hard left");
        // width: 0 folds to mono, 1 passes through
        let (l, r) = width(0.8, 0.2, 0.0);
        assert!((l - r).abs() < 1e-6, "mono");
        let (l, r) = width(0.8, 0.2, 1.0);
        assert!((l - 0.8).abs() < 1e-6 && (r - 0.2).abs() < 1e-6);
    }

    #[test]
    fn mid_sweep_is_log_200_to_5k() {
        assert!((mid_freq_hz(0.0) - 200.0).abs() < 1e-3);
        assert!((mid_freq_hz(1.0) - 5000.0).abs() < 0.5);
        assert!(
            (mid_freq_hz(0.5) - 1000.0).abs() < 1.0,
            "log midpoint = 1 kHz"
        );
    }

    #[test]
    fn smoother_converges_without_jumping() {
        let mut s = Smoother::new(0.0);
        s.target = 1.0;
        let first = s.tick();
        assert!(first < 0.05, "no single-step cliff");
        for _ in 0..600 {
            s.tick();
        }
        assert!((s.current - 1.0).abs() < 0.01, "converges in ~ms scale");
    }
}
