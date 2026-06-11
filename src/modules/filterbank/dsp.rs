//! The filterbank's Rust-side control core (docs/plans/filterbank-296e.md).
//!
//! The sixteen filters themselves are Faust (bank16.dsp → bank16_gen.rs,
//! raw band signals out, zero widgets); everything the 296e does *around*
//! its filters lives here, where it touches the modbus and the UI:
//! per-band VCAs, envelope followers with programmable decay, the
//! odd↔even spectral transfer (the vocoder), freeze, the swept
//! center/width window from the original 296, the per-band time spread,
//! and the odd/even stereo split.

use crate::delay::dsp::{pan_gains, DelayLine, Smoother};

pub const BANDS: usize = 16;

/// Band centers, Hz — the 296e's published curve. Band 1 is the <100
/// lowpass, band 16 the >10k highpass; shown in the UI and used by the
/// gain-verification test.
pub const CENTERS: [f32; BANDS] = [
    100.0, 150.0, 250.0, 350.0, 500.0, 630.0, 800.0, 1000.0, 1300.0, 1600.0, 2000.0, 2600.0,
    3500.0, 5000.0, 8000.0, 10000.0,
];

/// Spread ceiling, seconds: band 16 arrives this much after band 1 at
/// full spread.
pub const SPREAD_MAX: f32 = 0.250;

/// Map the decay knob 0..1 onto follower release seconds, exponentially
/// 50 ms → 40 s (the 296e's programmable per-band decay reaches ~40 s;
/// ours is one knob for all bands, CV-able).
pub fn decay_secs(v: f32) -> f32 {
    0.05 * 800.0_f32.powf(v.clamp(0.0, 1.0))
}

/// The 296's "programmed spectrum" window: a soft band-index mask swept
/// by center (0..1 across the bank) and width (0..1; 1 = everything
/// passes). One band of soft skirt on each edge.
pub fn window_mask(band: usize, center: f32, width: f32) -> f32 {
    let half = width.clamp(0.0, 1.0) * (BANDS as f32 / 2.0);
    let dist = (band as f32 - center.clamp(0.0, 1.0) * (BANDS - 1) as f32).abs();
    (1.0 - (dist - half)).clamp(0.0, 1.0)
}

/// Spectral transfer modes (the vocoder switch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Xfer {
    Off,
    /// Odd bands' followers drive even bands' VCAs (1-based, like the
    /// panel: band 1 drives band 2, …).
    OddToEven,
    EvenToOdd,
    Both,
}

/// Follower with a fixed fast attack and a runtime-settable release.
#[derive(Debug, Clone)]
struct BandFollower {
    y: f32,
    atk: f32,
    rel: f32,
}

impl BandFollower {
    fn new(sample_rate: f32) -> Self {
        let mut f = Self {
            y: 0.0,
            atk: 0.0,
            rel: 0.0,
        };
        f.atk = Self::coeff(0.002, sample_rate);
        f.set_release(0.4, sample_rate);
        f
    }

    fn coeff(tau: f32, sample_rate: f32) -> f32 {
        1.0 - (-1.0 / (tau * sample_rate)).exp()
    }

    fn set_release(&mut self, tau: f32, sample_rate: f32) {
        self.rel = Self::coeff(tau.max(0.001), sample_rate);
    }

    #[inline]
    fn tick(&mut self, x: f32) {
        let r = x.abs();
        let c = if r > self.y { self.atk } else { self.rel };
        self.y += (r - self.y) * c;
    }
}

/// Per-block parameters, already resolved by the module (manual or
/// mod-bound; per-band `gain` is post-morph/CV/window).
#[derive(Debug, Clone, Copy)]
pub struct BlockParams {
    pub gain: [f32; BANDS],
    pub xfer: Xfer,
    /// Latch the followers (spectral hold).
    pub freeze: bool,
    /// Follower decay knob 0..1 (see [`decay_secs`]).
    pub decay: f32,
    /// Per-band time stagger 0..1 of [`SPREAD_MAX`].
    pub spread: f32,
    /// Odd/even stereo split 0..1 (odd bands left, even right).
    pub split: f32,
    /// Instantaneous input in the mix.
    pub dry: f32,
}

impl Default for BlockParams {
    fn default() -> Self {
        Self {
            gain: [0.8; BANDS],
            xfer: Xfer::Off,
            freeze: false,
            decay: 0.3,
            spread: 0.0,
            split: 0.3,
            dry: 0.0,
        }
    }
}

/// How hard a partner band's follower drives a transferred VCA — tuned
/// so ordinary program material articulates without gating to silence.
const XFER_DRIVE: f32 = 3.0;

pub struct FilterCore {
    sample_rate: f32,
    gain_smooth: [Smoother; BANDS],
    split_smooth: Smoother,
    dry_smooth: Smoother,
    spread_smooth: Smoother,
    followers: [BandFollower; BANDS],
    /// Per-band stagger lines (≤ SPREAD_MAX each).
    lines: [DelayLine; BANDS],
    /// Scratch: the Faust core's 16 band outputs for one block.
    band_buf: Vec<Vec<f32>>,
    mono_buf: Vec<f32>,
    decay_now: f32,
}

impl FilterCore {
    pub fn new(sample_rate: f32, block: usize) -> Self {
        let p = BlockParams::default();
        let cap = (SPREAD_MAX * sample_rate) as usize + 8;
        Self {
            sample_rate,
            gain_smooth: std::array::from_fn(|i| Smoother::new(p.gain[i], 0.001, sample_rate)),
            split_smooth: Smoother::new(p.split, 0.001, sample_rate),
            dry_smooth: Smoother::new(p.dry, 0.001, sample_rate),
            // spread glides slowly: sweeping it smears pitch like the
            // delay's time knob, which is the fun
            spread_smooth: Smoother::new(0.0, 0.050, sample_rate),
            followers: std::array::from_fn(|_| BandFollower::new(sample_rate)),
            lines: std::array::from_fn(|_| DelayLine::with_capacity(cap)),
            band_buf: vec![vec![0.0; block]; BANDS],
            mono_buf: vec![0.0; block],
            decay_now: -1.0,
        }
    }

    /// Live follower values (modbus order b1…b16; also the UI meters).
    pub fn followers(&self) -> [f32; BANDS] {
        std::array::from_fn(|i| self.followers[i].y)
    }

    /// The transfer multiplier for band `i` (0-based) given the current
    /// follower state. 1-based odd bands are even indices.
    fn xfer_mult(&self, i: usize, xfer: Xfer) -> f32 {
        let drive = |partner: usize| (self.followers[partner].y * XFER_DRIVE).clamp(0.0, 1.0);
        // 1-based even band ← its odd neighbor below
        let odd_drives_even = !i.is_multiple_of(2);
        let even_drives_odd = i.is_multiple_of(2) && i + 1 < BANDS;
        match xfer {
            Xfer::Off => 1.0,
            Xfer::OddToEven => {
                if odd_drives_even {
                    drive(i - 1)
                } else {
                    1.0
                }
            }
            Xfer::EvenToOdd => {
                if even_drives_odd {
                    drive(i + 1)
                } else {
                    1.0
                }
            }
            Xfer::Both => {
                if odd_drives_even {
                    drive(i - 1)
                } else if even_drives_odd {
                    drive(i + 1)
                } else {
                    1.0
                }
            }
        }
    }

    /// Process one interleaved-stereo block in place through the Faust
    /// bank (`fx`).
    pub fn process_block(
        &mut self,
        inout: &mut [f32],
        p: &BlockParams,
        fx: &mut super::bank16::Bank16,
    ) {
        let frames = inout.len() / 2;
        debug_assert!(frames <= self.mono_buf.len());

        // follower decay tracks the knob (cheap; only on change)
        if (p.decay - self.decay_now).abs() > 1e-4 {
            self.decay_now = p.decay;
            let tau = decay_secs(p.decay);
            for f in self.followers.iter_mut() {
                f.set_release(tau, self.sample_rate);
            }
        }

        // mono sum in, sixteen raw bands out
        for f in 0..frames {
            self.mono_buf[f] = 0.5 * (inout[2 * f] + inout[2 * f + 1]);
        }
        {
            let ins = [&self.mono_buf[..frames]];
            fx.compute(frames, &ins, &mut self.band_buf[..]);
        }

        for f in 0..frames {
            let (in_l, in_r) = (inout[2 * f], inout[2 * f + 1]);
            let dry = self.dry_smooth.tick(p.dry);
            let split = self.split_smooth.tick(p.split);
            let spread = self.spread_smooth.tick(p.spread.clamp(0.0, 1.0));
            let mut out_l = in_l * dry;
            let mut out_r = in_r * dry;

            for i in 0..BANDS {
                let raw = self.band_buf[i][f];
                if !p.freeze {
                    self.followers[i].tick(raw);
                }
                // stagger: band 1 immediate, band 16 at full spread
                self.lines[i].push(raw);
                let d = spread * (i as f32 / (BANDS - 1) as f32) * SPREAD_MAX * self.sample_rate;
                let sig = if d < 1.0 { raw } else { self.lines[i].read(d) };

                let g = self.gain_smooth[i].tick(p.gain[i] * self.xfer_mult(i, p.xfer));
                // 1-based odd bands (even indices) lean left
                let pan = if i % 2 == 0 { -split } else { split };
                let (gl, gr) = pan_gains(pan);
                out_l += sig * g * gl;
                out_r += sig * g * gr;
            }
            inout[2 * f] = out_l;
            inout[2 * f + 1] = out_r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the Faust bank with a sine at each band's center and check
    /// the band's own output comes back near unity while a far band
    /// stays quiet — pins the stagger-tuned cascade's makeup gain.
    #[test]
    fn band_gains_near_unity_at_center() {
        let sr = 48_000.0;
        let mut fx = super::super::bank16::Bank16::new();
        fx.init(sr as i32);
        for (band, hz) in CENTERS.iter().enumerate() {
            let mut peak = 0.0_f32;
            let mut far_peak = 0.0_f32;
            let far = (band + 8) % BANDS;
            let mut phase = 0.0_f32;
            // 0.5 s warmup + measure
            for blk in 0..375 {
                let mono: Vec<f32> = (0..64)
                    .map(|_| {
                        phase = (phase + hz / sr).fract();
                        (phase * std::f32::consts::TAU).sin()
                    })
                    .collect();
                let ins = [&mono[..]];
                let mut outs: Vec<Vec<f32>> = vec![vec![0.0; 64]; BANDS];
                fx.compute(64, &ins, &mut outs);
                if blk > 200 {
                    peak = outs[band].iter().fold(peak, |a, s| a.max(s.abs()));
                    far_peak = outs[far].iter().fold(far_peak, |a, s| a.max(s.abs()));
                }
            }
            assert!(
                (0.4..=2.5).contains(&peak),
                "band {} ({} Hz): center gain {} out of range",
                band + 1,
                hz,
                peak
            );
            assert!(
                far_peak < peak * 0.5,
                "band {} ({} Hz): far band {} not attenuated ({} vs {})",
                band + 1,
                hz,
                far + 1,
                far_peak,
                peak
            );
        }
    }

    #[test]
    fn window_mask_sweeps_and_opens() {
        // full width = everything passes
        for i in 0..BANDS {
            assert_eq!(window_mask(i, 0.5, 1.0), 1.0, "band {}", i);
        }
        // narrow window at the left edge: band 1 in, band 16 out
        assert!(window_mask(0, 0.0, 0.15) > 0.9);
        assert!(window_mask(15, 0.0, 0.15) < 0.05);
        // sweeping center moves the pass region
        assert!(window_mask(15, 1.0, 0.15) > 0.9);
        assert!(window_mask(0, 1.0, 0.15) < 0.05);
    }

    #[test]
    fn decay_maps_exponentially() {
        assert!((decay_secs(0.0) - 0.05).abs() < 1e-3);
        assert!((decay_secs(1.0) - 40.0).abs() < 0.5);
        assert!(decay_secs(0.5) > 1.0 && decay_secs(0.5) < 2.0);
    }

    #[test]
    fn transfer_gates_even_bands_from_odd_followers() {
        let sr = 48_000.0;
        let mut core = FilterCore::new(sr, 64);
        let mut fx = super::super::bank16::Bank16::new();
        fx.init(sr as i32);
        // silence in: all followers at zero → transferred bands gate shut
        let p = BlockParams {
            xfer: Xfer::OddToEven,
            ..Default::default()
        };
        let mut block = vec![0.0_f32; 128];
        core.process_block(&mut block, &p, &mut fx);
        assert_eq!(
            core.xfer_mult(1, Xfer::OddToEven),
            0.0,
            "no odd energy → band 2 shut"
        );
        assert_eq!(
            core.xfer_mult(0, Xfer::OddToEven),
            1.0,
            "odd bands unaffected"
        );
        // fake some follower energy on band 1 (index 0) → band 2 opens
        core.followers[0].y = 0.5;
        assert_eq!(
            core.xfer_mult(1, Xfer::OddToEven),
            1.0,
            "driven hard enough to clamp"
        );
        assert_eq!(core.xfer_mult(1, Xfer::Off), 1.0);
        assert_eq!(
            core.xfer_mult(0, Xfer::EvenToOdd),
            (0.0_f32).max(0.0),
            "band 2 silent → band 1 shut"
        );
    }

    #[test]
    fn freeze_latches_followers() {
        let sr = 48_000.0;
        let mut core = FilterCore::new(sr, 64);
        let mut fx = super::super::bank16::Bank16::new();
        fx.init(sr as i32);
        // noise burst with followers running
        let p = BlockParams::default();
        let mut seed = 1u32;
        for _ in 0..40 {
            let mut block: Vec<f32> = (0..128)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 16) as f32 / 32768.0 - 1.0
                })
                .collect();
            core.process_block(&mut block, &p, &mut fx);
        }
        let live = core.followers();
        assert!(live.iter().any(|v| *v > 0.01), "noise moved the followers");
        // freeze + silence: values hold exactly
        let frozen = BlockParams {
            freeze: true,
            ..Default::default()
        };
        for _ in 0..40 {
            let mut block = vec![0.0_f32; 128];
            core.process_block(&mut block, &frozen, &mut fx);
        }
        assert_eq!(core.followers(), live, "frozen followers do not move");
        // unfreeze: they decay again
        for _ in 0..200 {
            let mut block = vec![0.0_f32; 128];
            core.process_block(&mut block, &p, &mut fx);
        }
        assert!(core.followers()[4] < live[4], "unfrozen followers decay");
    }

    #[test]
    fn spread_delays_high_bands() {
        let sr = 48_000.0;
        let mut core = FilterCore::new(sr, 64);
        let mut fx = super::super::bank16::Bank16::new();
        fx.init(sr as i32);
        // full spread, only band 16 (the highpass) open: a click should
        // arrive ~SPREAD_MAX late
        let mut gain = [0.0; BANDS];
        gain[15] = 1.0;
        let p = BlockParams {
            gain,
            split: 0.0,
            spread: 1.0,
            ..Default::default()
        };
        // let the spread smoother land first
        for _ in 0..150 {
            let mut warm = vec![0.0_f32; 128];
            core.process_block(&mut warm, &p, &mut fx);
        }
        let mut block = vec![0.0_f32; 128];
        block[0] = 1.0;
        block[1] = 1.0;
        let mut energy_early = 0.0_f32;
        core.process_block(&mut block, &p, &mut fx);
        energy_early += block.iter().map(|s| s * s).sum::<f32>();
        // next ~200 ms should stay quiet (the click is in the stagger line)
        for _ in 0..130 {
            let mut b = vec![0.0_f32; 128];
            core.process_block(&mut b, &p, &mut fx);
            energy_early += b.iter().map(|s| s * s).sum::<f32>();
        }
        // …and it lands around 250 ms
        let mut energy_late = 0.0_f32;
        for _ in 0..80 {
            let mut b = vec![0.0_f32; 128];
            core.process_block(&mut b, &p, &mut fx);
            energy_late += b.iter().map(|s| s * s).sum::<f32>();
        }
        assert!(
            energy_late > energy_early * 2.0,
            "spread click arrives late: early {} late {}",
            energy_early,
            energy_late
        );
    }
}
