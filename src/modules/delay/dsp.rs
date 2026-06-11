//! The delay's audio-rate core: a mono line with 8 series read heads,
//! envelope followers, and the feedback sum (docs/plans/delay-288.md §5).
//!
//! Hand-written Rust here; the shimmer/reverb feedback characters are
//! Faust (tap8fx.dsp → tap8fx_gen.rs). The split is deliberate and
//! docs/writing-dsp.md uses it as the worked example: tight per-sample
//! integration (swept fractional heads, per-tap state) reads better in
//! Rust, while mature library algorithms (granular transposer, freeverb)
//! are one line each in Faust.

pub const MAX_TAPS: usize = 8;
/// Per-stage delay, seconds. Tap 8 reaches 8 × TIME_MAX = 2 s.
pub const TIME_MIN: f32 = 0.001;
pub const TIME_MAX: f32 = 0.250;

/// Map a 0..1 control onto per-stage seconds, exponentially — equal
/// steps feel equal from flange (1 ms) through slapback to long echo.
pub fn time_from_norm(v: f32) -> f32 {
    TIME_MIN * (TIME_MAX / TIME_MIN).powf(v.clamp(0.0, 1.0))
}

/// …and back, for displaying/persisting the knob position.
pub fn norm_from_time(t: f32) -> f32 {
    (t.clamp(TIME_MIN, TIME_MAX) / TIME_MIN).log2() / (TIME_MAX / TIME_MIN).log2()
}

/// One-pole exponential smoother with a configurable time constant.
/// The time smoother (~80 ms) IS the repitch character: read heads
/// glide to new positions like a swept analog clock, so time changes
/// bend pitch instead of clicking.
#[derive(Debug, Clone)]
pub struct Smoother {
    v: f32,
    coeff: f32,
}

impl Smoother {
    pub fn new(v: f32, tau_secs: f32, sample_rate: f32) -> Self {
        // standard one-pole: v += (target - v) * (1 - e^(-1/(tau·sr)))
        let coeff = 1.0 - (-1.0 / (tau_secs * sample_rate)).exp();
        Self { v, coeff }
    }

    pub fn tick(&mut self, target: f32) -> f32 {
        self.v += (target - self.v) * self.coeff;
        self.v
    }

    pub fn value(&self) -> f32 {
        self.v
    }

    /// Jump without gliding (preset loads — no 2-second pitch dive).
    pub fn snap(&mut self, v: f32) {
        self.v = v;
    }
}

/// Mono delay line with fractional (linear-interpolated) read taps.
#[derive(Debug)]
pub struct DelayLine {
    buf: Vec<f32>,
    write: usize,
}

impl DelayLine {
    pub fn new(sample_rate: f32) -> Self {
        // 8 stages of TIME_MAX plus interpolation slack.
        let len = (MAX_TAPS as f32 * TIME_MAX * sample_rate) as usize + 64;
        Self { buf: vec![0.0; len], write: 0 }
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        self.buf[self.write] = x;
        self.write = (self.write + 1) % self.buf.len();
    }

    /// Read `delay` samples behind the write head (fractional).
    #[inline]
    pub fn read(&self, delay: f32) -> f32 {
        let len = self.buf.len();
        let d = delay.clamp(1.0, (len - 2) as f32);
        let i = d as usize;
        let frac = d - i as f32;
        // write points at the NEXT slot; latest sample is write-1.
        let a = self.buf[(self.write + len - 1 - i) % len];
        let b = self.buf[(self.write + len - 2 - i) % len];
        a + (b - a) * frac
    }
}

/// Rectify → attack/release one-pole: the per-tap envelope followers
/// (2 ms / 150 ms, design doc §2 — the MDP's CV-cascade feature).
#[derive(Debug, Clone)]
pub struct Follower {
    y: f32,
    atk: f32,
    rel: f32,
}

impl Follower {
    pub fn new(sample_rate: f32) -> Self {
        let coeff = |tau: f32| 1.0 - (-1.0 / (tau * sample_rate)).exp();
        Self { y: 0.0, atk: coeff(0.002), rel: coeff(0.150) }
    }

    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let r = x.abs();
        let c = if r > self.y { self.atk } else { self.rel };
        self.y += (r - self.y) * c;
        self.y
    }

    pub fn value(&self) -> f32 {
        self.y
    }
}

/// Soft clip for the feedback sum: self-oscillation compresses into
/// saturation instead of exploding (the analog input mixer's mercy).
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    x.tanh()
}

/// Equal-power pan (same law as the console's strips).
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let p = (pan.clamp(-1.0, 1.0) + 1.0) * 0.25 * std::f32::consts::PI;
    (p.cos(), p.sin())
}

/// Per-block parameters for the core, already resolved by the module
/// (manual or mod-bound — the DSP doesn't know about cables).
#[derive(Debug, Clone, Copy)]
pub struct BlockParams {
    /// Per-stage time, seconds (pre-smoothing).
    pub time: f32,
    /// Plain tap-8 regeneration 0..1.
    pub regen: f32,
    /// Octave-up feedback amount 0..1 (Faust shim channel).
    pub shim: f32,
    /// Reverb-washed feedback amount 0..1 (Faust wash channel).
    pub wash: f32,
    /// Instantaneous input level into the mix 0..1.
    pub dry: f32,
    /// Active tap count 1..=8.
    pub taps: usize,
    /// Per-tap fader 0..1.
    pub level: [f32; MAX_TAPS],
    /// Per-tap pan -1..1.
    pub pan: [f32; MAX_TAPS],
    /// Per-tap phase: +1 normal, 0 off, -1 inverted (the 288's switch).
    pub phase: [f32; MAX_TAPS],
}

impl Default for BlockParams {
    fn default() -> Self {
        Self {
            time: 0.120,
            regen: 0.0,
            shim: 0.0,
            wash: 0.0,
            dry: 0.8,
            taps: MAX_TAPS,
            level: [0.6; MAX_TAPS],
            pan: [0.0; MAX_TAPS],
            phase: [1.0; MAX_TAPS],
        }
    }
}

/// The stateful core. One instance on the audio thread; everything
/// per-sample lives here, everything per-block comes in via
/// [`BlockParams`], follower values go out via [`DelayCore::followers`].
pub struct DelayCore {
    sample_rate: f32,
    line: DelayLine,
    time_smooth: Smoother,
    lvl_smooth: [Smoother; MAX_TAPS],
    pan_smooth: [Smoother; MAX_TAPS],
    phase_smooth: [Smoother; MAX_TAPS],
    dry_smooth: Smoother,
    /// input + 8 taps, in modbus-output order (in, t1…t8).
    followers: [Follower; MAX_TAPS + 1],
    /// Tap 8's audio from the previous block, fed to the Faust fx.
    tap8_buf: Vec<f32>,
    /// The Faust fx's outputs from the previous block — shimmer/wash
    /// feedback runs one block (~1.3 ms) late, inaudible in a diffuse
    /// path and it keeps the per-sample loop branch-free.
    shim_buf: Vec<f32>,
    wash_buf: Vec<f32>,
}

impl DelayCore {
    pub fn new(sample_rate: f32, block: usize) -> Self {
        let p = BlockParams::default();
        Self {
            sample_rate,
            line: DelayLine::new(sample_rate),
            time_smooth: Smoother::new(p.time, 0.080, sample_rate),
            lvl_smooth: std::array::from_fn(|i| Smoother::new(p.level[i], 0.001, sample_rate)),
            pan_smooth: std::array::from_fn(|i| Smoother::new(p.pan[i], 0.001, sample_rate)),
            phase_smooth: std::array::from_fn(|i| Smoother::new(p.phase[i], 0.001, sample_rate)),
            dry_smooth: Smoother::new(p.dry, 0.001, sample_rate),
            followers: std::array::from_fn(|_| Follower::new(sample_rate)),
            tap8_buf: vec![0.0; block],
            shim_buf: vec![0.0; block],
            wash_buf: vec![0.0; block],
        }
    }

    /// Process one interleaved-stereo block in place; `fx` is the
    /// generated Faust core (tap8fx). `inout` holds the input on entry
    /// and the module's stereo output on return.
    pub fn process_block(
        &mut self,
        inout: &mut [f32],
        p: &BlockParams,
        fx: &mut super::tap8fx::Tap8Fx,
    ) {
        let frames = inout.len() / 2;
        debug_assert!(frames <= self.tap8_buf.len());
        let taps = p.taps.clamp(1, MAX_TAPS);

        for f in 0..frames {
            let in_l = inout[2 * f];
            let in_r = inout[2 * f + 1];
            // The 288 is mono in; the line carries the mono sum and
            // stereo happens at the tap pans.
            let x = 0.5 * (in_l + in_r);
            self.followers[0].tick(x);

            let stage = self.time_smooth.tick(p.time) * self.sample_rate;
            let dry = self.dry_smooth.tick(p.dry);
            let mut out_l = in_l * dry;
            let mut out_r = in_r * dry;
            let mut tap8 = 0.0;

            for i in 0..MAX_TAPS {
                let active = i < taps;
                let lvl = self.lvl_smooth[i].tick(if active { p.level[i] } else { 0.0 });
                let ph = self.phase_smooth[i].tick(if active { p.phase[i] } else { 0.0 });
                // Inactive taps fully faded out: skip the read, decay
                // the follower toward silence.
                if !active && lvl < 1e-4 && ph.abs() < 1e-4 {
                    self.followers[i + 1].tick(0.0);
                    continue;
                }
                // reads precede this frame's push: delay D ≡ read(D-1)
                let s = self.line.read(stage * (i + 1) as f32 - 1.0) * ph;
                if i == MAX_TAPS - 1 {
                    tap8 = s;
                }
                self.followers[i + 1].tick(s);
                let (gl, gr) = pan_gains(self.pan_smooth[i].tick(p.pan[i]));
                out_l += s * lvl * gl;
                out_r += s * lvl * gr;
            }

            // The feedback sum: plain regen per-sample, shimmer/wash
            // from the previous block's Faust pass, soft-clipped so
            // runaway settles into saturation.
            let fb = soft_clip(
                p.regen * tap8 + p.shim * self.shim_buf[f] + p.wash * self.wash_buf[f],
            );
            self.line.push(x + fb);
            self.tap8_buf[f] = tap8;

            inout[2 * f] = out_l;
            inout[2 * f + 1] = out_r;
        }

        // Run the Faust characters on this block's tap 8 for the next one.
        let ins = [&self.tap8_buf[..frames]];
        let (shim, wash) = (&mut self.shim_buf, &mut self.wash_buf);
        let mut outs = [&mut shim[..frames], &mut wash[..frames]];
        fx.compute(frames, &ins, &mut outs);
    }

    /// Follower values in modbus order (in, t1…t8).
    pub fn followers(&self) -> [f32; MAX_TAPS + 1] {
        std::array::from_fn(|i| self.followers[i].value())
    }

    /// Jump the time smoother (preset/patch loads).
    pub fn snap_time(&mut self, time: f32) {
        self.time_smooth.snap(time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reads_back_with_delay() {
        let mut l = DelayLine::new(48_000.0);
        l.push(1.0);
        for _ in 0..99 {
            l.push(0.0);
        }
        // delay 0 = the latest sample; the impulse is 99 behind it
        assert!((l.read(99.0) - 1.0).abs() < 1e-6);
        assert!(l.read(50.0).abs() < 1e-6);
        // fractional read interpolates between neighbors
        assert!((l.read(98.5) - 0.5).abs() < 1e-6);
        assert!((l.read(99.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn time_norm_round_trips_and_spans_range() {
        assert!((time_from_norm(0.0) - TIME_MIN).abs() < 1e-7);
        assert!((time_from_norm(1.0) - TIME_MAX).abs() < 1e-6);
        for v in [0.0, 0.25, 0.5, 0.9, 1.0] {
            assert!((norm_from_time(time_from_norm(v)) - v).abs() < 1e-3);
        }
    }

    #[test]
    fn follower_attacks_fast_releases_slow() {
        let mut f = Follower::new(48_000.0);
        for _ in 0..480 {
            f.tick(1.0); // 10 ms of full scale
        }
        assert!(f.value() > 0.95, "attack reaches in 10 ms, got {}", f.value());
        for _ in 0..480 {
            f.tick(0.0); // 10 ms of silence
        }
        assert!(f.value() > 0.5, "release holds past 10 ms, got {}", f.value());
        for _ in 0..48_000 {
            f.tick(0.0);
        }
        assert!(f.value() < 0.01, "1 s of silence decays, got {}", f.value());
    }

    #[test]
    fn smoother_glides_and_snaps() {
        let mut s = Smoother::new(0.0, 0.080, 48_000.0);
        for _ in 0..48_000 / 2 {
            s.tick(1.0);
        }
        let half_sec = s.value();
        assert!(half_sec > 0.9, "500 ms ≈ 6τ, nearly there: {}", half_sec);
        assert!(half_sec < 1.0);
        s.snap(0.3);
        assert_eq!(s.value(), 0.3);
    }

    #[test]
    fn core_delays_taps_inverts_phase_and_follows() {
        let mut core = DelayCore::new(48_000.0, 64);
        let mut fx = super::super::tap8fx::Tap8Fx::new();
        fx.init(48_000);
        let mut p = BlockParams {
            time: 64.0 / 48_000.0, // one block per stage — tap1 lands in block 2
            dry: 0.0,
            taps: 2,
            level: [1.0; MAX_TAPS],
            pan: [0.0; MAX_TAPS],
            phase: [1.0; MAX_TAPS],
            ..Default::default()
        };
        core.snap_time(p.time);
        // settle the 1 ms param smoothers (dry glides from its 0.8
        // default toward this test's 0.0) before measuring silence
        for _ in 0..5 {
            let mut warm = vec![0.0_f32; 128];
            core.process_block(&mut warm, &p, &mut fx);
        }

        // Block 1: an impulse in, nothing out yet (tap1 = 1 block away).
        let mut block = vec![0.0_f32; 128];
        block[0] = 1.0;
        block[1] = 1.0;
        core.process_block(&mut block, &p, &mut fx);
        assert!(block.iter().map(|s| s.abs()).fold(0.0, f32::max) < 1e-3);

        // Block 2: tap 1 speaks, centered pan = equal L/R.
        let mut block2 = vec![0.0_f32; 128];
        core.process_block(&mut block2, &p, &mut fx);
        let peak = block2.iter().map(|s| s.abs()).fold(0.0, f32::max);
        assert!(peak > 0.3, "tap 1 should sound in block 2, peak {}", peak);
        let (l, r) = (block2[0], block2[1]);
        assert!((l - r).abs() < 1e-4, "center pan is symmetric");
        assert!(core.followers()[1] > 0.01, "tap 1 follower moved");
        assert!(core.followers()[0] > 0.0, "input follower moved");

        // Phase inversion flips the tap's sign (fresh core, same drive).
        let mut core2 = DelayCore::new(48_000.0, 64);
        core2.snap_time(p.time);
        p.phase[0] = -1.0;
        for _ in 0..5 {
            let mut warm = vec![0.0_f32; 128];
            core2.process_block(&mut warm, &p, &mut fx);
        }
        let mut b1 = vec![0.0_f32; 128];
        b1[0] = 1.0;
        b1[1] = 1.0;
        core2.process_block(&mut b1, &p, &mut fx);
        let mut b2 = vec![0.0_f32; 128];
        core2.process_block(&mut b2, &p, &mut fx);
        let (i2, _) = block2
            .iter()
            .enumerate()
            .fold((0, 0.0_f32), |acc, (i, &s)| if s.abs() > acc.1 { (i, s.abs()) } else { acc });
        assert!(
            b2[i2] * block2[i2] < 0.0,
            "inverted phase flips polarity: {} vs {}",
            b2[i2],
            block2[i2]
        );
    }

    #[test]
    fn feedback_regenerates_and_soft_clip_bounds_it() {
        let mut core = DelayCore::new(48_000.0, 64);
        let mut fx = super::super::tap8fx::Tap8Fx::new();
        fx.init(48_000);
        let p = BlockParams {
            time: 8.0 / 48_000.0, // tap 8 = 64 samples = 1 block
            dry: 0.0,
            regen: 1.0, // unity regen: the loop must not explode
            taps: 8,
            level: [0.0; MAX_TAPS], // listen later; just drive the loop
            ..Default::default()
        };
        core.snap_time(p.time);
        let mut block = vec![0.0_f32; 128];
        block[0] = 1.0;
        block[1] = 1.0;
        core.process_block(&mut block, &p, &mut fx);
        // run the loop for a second of blocks; the line must stay finite
        for _ in 0..750 {
            let mut b = vec![0.0_f32; 128];
            core.process_block(&mut b, &p, &mut fx);
        }
        let probe = core.line.read(32.0);
        assert!(probe.is_finite(), "soft clip keeps the loop finite");
        assert!(probe.abs() <= 1.0, "tanh bounds the line at ±1: {}", probe);
    }

    #[test]
    fn soft_clip_and_pan_laws() {
        assert!(soft_clip(0.1) - 0.0997 < 1e-3, "≈linear when small");
        assert!(soft_clip(10.0) <= 1.0);
        assert!(soft_clip(-10.0) >= -1.0);
        let (l, c) = (pan_gains(-1.0), pan_gains(0.0));
        assert!((l.0 - 1.0).abs() < 1e-6 && l.1.abs() < 1e-6, "hard left");
        assert!((c.0 - c.1).abs() < 1e-6, "center equal power");
        let p = pan_gains(0.5);
        assert!((p.0 * p.0 + p.1 * p.1 - 1.0).abs() < 1e-5, "unity power");
    }
}
