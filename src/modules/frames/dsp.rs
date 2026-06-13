//! # Frames engines — the keyframer and the poly LFO
//!
//! Ported from pichenettes/eurorack (frames/{keyframer,poly_lfo}.cc,
//! MIT, copyright 2013 Emilie Gillet, attribution preserved): the
//! keyframe store with binary-search evaluation, the easing curves
//! (step, linear, quartic in/out, sine, bounce — generated from the
//! lookup_tables.py formulas), the 2164 VCA response blending, and
//! the four-phasor wavetable poly LFO (wt_lfo_waveforms extracted
//! byte-exact, 18 × 257 entries) with spread, shape spread, and
//! cross-channel phase coupling.

#![allow(clippy::excessive_precision)]

pub const NUM_CHANNELS: usize = 4;
pub const MAX_KEYFRAMES: usize = 64;

/// 18 wavetable rows × 257 samples, extracted from frames/resources.cc.
static WAVES: &[u8] = include_bytes!("../frames_waves.bin");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EasingCurve {
    Step,
    #[default]
    Linear,
    InQuartic,
    OutQuartic,
    Sine,
    Bounce,
}

pub const EASING_NAMES: [&str; 6] = ["step", "linear", "in4", "out4", "sine", "bounce"];
pub const EASING_CURVES: [EasingCurve; 6] = [
    EasingCurve::Step,
    EasingCurve::Linear,
    EasingCurve::InQuartic,
    EasingCurve::OutQuartic,
    EasingCurve::Sine,
    EasingCurve::Bounce,
];

/// BounceEaseOut from the table generator.
fn bounce_ease_out(mut t: f32) -> f32 {
    if t < 1.0 / 2.75 {
        7.5625 * t * t
    } else if t < 2.0 / 2.75 {
        t -= 1.5 / 2.75;
        7.5625 * t * t + 0.75
    } else if t < 2.5 / 2.75 {
        t -= 2.25 / 2.75;
        7.5625 * t * t + 0.9375
    } else {
        t -= 2.625 / 2.75;
        7.5625 * t * t + 0.984375
    }
}

/// The easing laws (0..1 → 0..1), straight from lookup_tables.py.
pub fn ease(curve: EasingCurve, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match curve {
        EasingCurve::Step => {
            if t < 0.5 {
                0.0
            } else {
                1.0
            }
        }
        EasingCurve::Linear => t,
        EasingCurve::InQuartic => t * t * t * t,
        EasingCurve::OutQuartic => 1.0 - (1.0 - t).powi(4),
        EasingCurve::Sine => (1.0 - (t * std::f32::consts::PI).cos()) / 2.0,
        EasingCurve::Bounce => bounce_ease_out(t),
    }
}

/// The 2164 response blend: response 0 = linear gain (via the
/// log-linearization table law), 1 = exponential; the balance curve
/// is x^1.5 (lut_response_balance).
pub fn response_blend(gain: f32, response: f32) -> f32 {
    let gain = gain.clamp(0.0, 1.0);
    // exponential side: straight through (the 2164 is exponential)
    let exponential = gain;
    // linear side: the lut_vca_linear law inverted into 0..1 —
    // voltage = -2/3·log10(g)/2.5 normalized; clamp the log at the
    // table's 1/4096 floor
    let g = gain.max(1.0 / 4096.0);
    let linear = (1.0 - (-2.0 / 3.0 * g.log10() / 2.5)).clamp(0.0, 1.0);
    let balance = response.clamp(0.0, 1.0).powf(1.5);
    linear + (exponential - linear) * balance
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct Keyframe {
    /// 0..1 position on the frame axis.
    pub timestamp: f32,
    pub values: [f32; NUM_CHANNELS],
}

/// The keyframer: sorted keyframes, eased interpolation per channel.
#[derive(Debug, Clone, Default)]
pub struct Keyframer {
    pub keyframes: Vec<Keyframe>,
    pub easing: [EasingCurve; NUM_CHANNELS],
    pub response: [f32; NUM_CHANNELS],
    /// Fallback levels when no keyframes exist.
    pub immediate: [f32; NUM_CHANNELS],
}

impl Keyframer {
    pub fn add(&mut self, timestamp: f32, values: [f32; NUM_CHANNELS]) -> bool {
        if self.keyframes.len() >= MAX_KEYFRAMES {
            return false;
        }
        let timestamp = timestamp.clamp(0.0, 1.0);
        match self
            .keyframes
            .iter_mut()
            .find(|k| (k.timestamp - timestamp).abs() < 1e-4)
        {
            Some(k) => k.values = values,
            None => {
                self.keyframes.push(Keyframe { timestamp, values });
                self.keyframes
                    .sort_by(|a, b| a.timestamp.total_cmp(&b.timestamp));
            }
        }
        true
    }

    pub fn remove_nearest(&mut self, timestamp: f32, tolerance: f32) -> bool {
        let Some((i, _)) = self
            .keyframes
            .iter()
            .enumerate()
            .map(|(i, k)| (i, (k.timestamp - timestamp).abs()))
            .min_by(|a, b| a.1.total_cmp(&b.1))
        else {
            return false;
        };
        if (self.keyframes[i].timestamp - timestamp).abs() <= tolerance {
            self.keyframes.remove(i);
            true
        } else {
            false
        }
    }

    /// keyframer.cc Evaluate: levels per channel at the frame position,
    /// post response blending.
    pub fn evaluate(&self, timestamp: f32) -> [f32; NUM_CHANNELS] {
        let mut levels = [0.0_f32; NUM_CHANNELS];
        if self.keyframes.is_empty() {
            levels = self.immediate;
        } else {
            let t = timestamp.clamp(0.0, 1.0);
            let position = self
                .keyframes
                .partition_point(|k| k.timestamp < t);
            if position == 0 {
                levels = self.keyframes[0].values;
            } else if position == self.keyframes.len() {
                levels = self.keyframes[self.keyframes.len() - 1].values;
            } else {
                let a = &self.keyframes[position - 1];
                let b = &self.keyframes[position];
                let span = (b.timestamp - a.timestamp).max(1e-6);
                let scale = (t - a.timestamp) / span;
                for (i, l) in levels.iter_mut().enumerate() {
                    let shaped = ease(self.easing[i], scale);
                    *l = a.values[i] + (b.values[i] - a.values[i]) * shaped;
                }
            }
        }
        for (i, l) in levels.iter_mut().enumerate() {
            *l = response_blend(*l, self.response[i]);
        }
        levels
    }
}

// ── poly LFO ───────────────────────────────────────────────────────────────

#[inline]
fn interp_u8(table: &[u8], phase: u32) -> f32 {
    let i = (phase >> 24) as usize;
    let a = table[i.min(table.len() - 1)] as f32;
    let b = table[(i + 1).min(table.len() - 1)] as f32;
    let f = ((phase >> 8) & 0xffff) as f32 / 65536.0;
    (a + (b - a) * f - 128.0) / 128.0
}

/// frames PolyLfo: four phasors over the 18-row wavetable, with
/// frequency spread, shape spread, and cross-channel coupling.
#[derive(Debug, Clone)]
pub struct PolyLfo {
    pub spread: f32,
    pub shape: f32,
    pub shape_spread: f32,
    pub coupling: f32,
    phase: [u32; NUM_CHANNELS],
    value: [f32; NUM_CHANNELS],
}

impl Default for PolyLfo {
    fn default() -> Self {
        PolyLfo {
            spread: 0.0,
            shape: 0.0,
            shape_spread: 0.0,
            coupling: 0.0,
            phase: [0; NUM_CHANNELS],
            value: [0.0; NUM_CHANNELS],
        }
    }
}

impl PolyLfo {
    /// One control tick at `dt` seconds; `hz` is channel 1's rate.
    /// Returns the four unipolar levels.
    pub fn render(&mut self, hz: f32, dt: f32) -> [f32; NUM_CHANNELS] {
        let base_inc = (hz.max(0.0) * dt * 4_294_967_296.0) as u64;
        if self.spread >= 0.0 {
            self.phase[0] = self.phase[0].wrapping_add(base_inc as u32);
            // positive spread: fixed phase offsets
            let diff = ((self.spread * 32767.0) as u32) << 15;
            self.phase[1] = self.phase[0].wrapping_add(diff);
            self.phase[2] = self.phase[1].wrapping_add(diff);
            self.phase[3] = self.phase[2].wrapping_add(diff);
        } else {
            // negative spread: frequency detune per channel
            let mut f = hz;
            for p in self.phase.iter_mut() {
                *p = p.wrapping_add((f.max(0.0) * dt * 4_294_967_296.0) as u32);
                f *= 1.0 - self.spread * 0.5;
            }
        }
        let sine_row = &WAVES[17 * 257..18 * 257];
        let mut out = [0.0_f32; NUM_CHANNELS];
        let mut wavetable_index = self.shape.clamp(0.0, 1.0) * 16.0;
        #[allow(clippy::needless_range_loop)] // i strides phase,
        // value and out in parallel with neighbor access
        for i in 0..NUM_CHANNELS {
            let mut phase = self.phase[i];
            // coupling: neighbor's sine value bends this phase
            let coupling = self.coupling;
            if coupling > 0.0 {
                let v = self.value[(i + 1) % NUM_CHANNELS];
                phase = phase.wrapping_add((v * coupling * 268_435_456.0) as i64 as u32);
            } else if coupling < 0.0 {
                let v = self.value[(i + NUM_CHANNELS - 1) % NUM_CHANNELS];
                phase = phase.wrapping_add((v * -coupling * 268_435_456.0) as i64 as u32);
            }
            let row = (wavetable_index as usize).min(16);
            let frac = wavetable_index - row as f32;
            let a = &WAVES[row * 257..(row + 1) * 257];
            let b = &WAVES[(row + 1).min(17) * 257..(row + 2).min(18) * 257];
            let va = interp_u8(a, phase);
            let vb = interp_u8(b, phase);
            let v = va + (vb - va) * frac;
            self.value[i] = interp_u8(sine_row, phase);
            out[i] = (v + 1.0) * 0.5;
            wavetable_index = (wavetable_index + self.shape_spread * 16.0).rem_euclid(17.0);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveforms_extracted_in_full() {
        assert_eq!(WAVES.len(), 18 * 257, "18 rows x 257 samples");
    }

    #[test]
    fn easing_curves_hit_their_anchors() {
        for c in EASING_CURVES {
            assert!(ease(c, 0.0).abs() < 1e-3, "{c:?} starts at 0");
            assert!((ease(c, 1.0) - 1.0).abs() < 1e-2, "{c:?} ends at 1");
        }
        assert!(ease(EasingCurve::InQuartic, 0.5) < 0.1);
        assert!(ease(EasingCurve::OutQuartic, 0.5) > 0.9);
        assert_eq!(ease(EasingCurve::Step, 0.49), 0.0);
        assert_eq!(ease(EasingCurve::Step, 0.51), 1.0);
        // bounce overshoots its way up but stays in range
        assert!((0.0..=1.0).contains(&ease(EasingCurve::Bounce, 0.3)));
    }

    #[test]
    fn keyframer_interpolates_between_frames() {
        let mut k = Keyframer::default();
        k.add(0.0, [0.0, 1.0, 0.5, 0.0]);
        k.add(1.0, [1.0, 0.0, 0.5, 0.0]);
        let mid = k.evaluate(0.5);
        assert!((mid[0] - response_blend(0.5, 0.0)).abs() < 1e-4);
        assert!((mid[1] - response_blend(0.5, 0.0)).abs() < 1e-4);
        // outside the range: clamps to the edge keyframes
        let lo = k.evaluate(0.0);
        assert!(lo[0] < 0.1);
        let hi = k.evaluate(1.0);
        assert!(hi[0] > 0.9);
    }

    #[test]
    fn keyframer_add_remove_round_trip() {
        let mut k = Keyframer::default();
        assert!(k.add(0.3, [0.1; 4]));
        assert!(k.add(0.7, [0.9; 4]));
        assert_eq!(k.keyframes.len(), 2);
        // re-adding at the same spot replaces
        assert!(k.add(0.3, [0.2; 4]));
        assert_eq!(k.keyframes.len(), 2);
        assert!(k.remove_nearest(0.31, 0.05));
        assert_eq!(k.keyframes.len(), 1);
        assert!(!k.remove_nearest(0.0, 0.05), "tolerance respected");
    }

    #[test]
    fn poly_lfo_runs_four_living_channels() {
        let mut lfo = PolyLfo {
            spread: 0.2,
            shape: 0.3,
            shape_spread: 0.1,
            coupling: 0.2,
            ..Default::default()
        };
        let mut mins = [f32::MAX; 4];
        let mut maxs = [f32::MIN; 4];
        for _ in 0..10_000 {
            let v = lfo.render(2.0, 1.0 / 1000.0);
            for i in 0..4 {
                mins[i] = mins[i].min(v[i]);
                maxs[i] = maxs[i].max(v[i]);
                assert!((0.0..=1.0).contains(&v[i]), "bounded");
            }
        }
        for i in 0..4 {
            assert!(maxs[i] - mins[i] > 0.4, "channel {i} swings");
        }
    }
}
