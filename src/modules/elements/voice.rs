//! Elements voice wiring and the "space" reverb.
//!
//! Ported from Mutable Instruments Elements (pichenettes/eurorack) by
//! Emilie Gillet. Copyright 2014 Emilie Gillet — MIT license; the
//! copyright and permission notice from the upstream repository apply
//! to this port (see dsp.rs header for the notice).

use super::dsp::{
    accent_gain, Exciter, ExciterModel, MultistageEnvelope, Resonator, SampleData, Tube,
    FLAG_FALLING, FLAG_GATE, FLAG_RISING,
};

/// The patch — Elements' panel, one field per knob (patch.h).
#[derive(Debug, Clone, Copy)]
pub struct Patch {
    pub exciter_envelope_shape: f32,
    pub exciter_bow_level: f32,
    pub exciter_bow_timbre: f32,
    pub exciter_blow_level: f32,
    pub exciter_blow_meta: f32,
    pub exciter_blow_timbre: f32,
    pub exciter_strike_level: f32,
    pub exciter_strike_meta: f32,
    pub exciter_strike_timbre: f32,
    pub exciter_signature: f32,
    pub resonator_geometry: f32,
    pub resonator_brightness: f32,
    pub resonator_damping: f32,
    pub resonator_position: f32,
    pub space: f32,
}

impl Default for Patch {
    fn default() -> Self {
        // Part::Init defaults
        Patch {
            exciter_envelope_shape: 1.0,
            exciter_bow_level: 0.0,
            exciter_bow_timbre: 0.5,
            exciter_blow_level: 0.0,
            exciter_blow_meta: 0.5,
            exciter_blow_timbre: 0.5,
            exciter_strike_level: 0.8,
            exciter_strike_meta: 0.5,
            exciter_strike_timbre: 0.5,
            exciter_signature: 0.0,
            resonator_geometry: 0.2,
            resonator_brightness: 0.5,
            resonator_damping: 0.25,
            resonator_position: 0.3,
            space: 0.5,
        }
    }
}

pub struct Voice {
    envelope: MultistageEnvelope,
    bow: Exciter,
    blow: Exciter,
    strike: Exciter,
    tube: Tube,
    /// Granular diffuser on the blow path (fx/diffuser.h): 4 series
    /// allpasses, k = 0.625.
    diffuser: [Ap; 4],
    pub resonator: Resonator,
    previous_gate: bool,
    envelope_value: f32,
    strength: f32,
    exciter_level: f32,
    sample_rate: f32,
    bow_buffer: Vec<f32>,
    blow_buffer: Vec<f32>,
    strike_buffer: Vec<f32>,
    bow_strength: Vec<f32>,
}

impl Voice {
    pub fn new(sample_rate: f32, block: usize, seed: u32) -> Self {
        let mut bow = Exciter::new(ExciterModel::Flow, sample_rate, seed ^ 0x1111);
        bow.parameter = 0.7;
        bow.timbre = 0.5;
        let blow = Exciter::new(ExciterModel::GranularSamplePlayer, sample_rate, seed ^ 0x2222);
        let strike = Exciter::new(ExciterModel::Mallet, sample_rate, seed ^ 0x3333);
        // diffuser sizes are at the firmware's 32 kHz; scale to ours
        let s = sample_rate / 32_000.0;
        let sz = |n: f32| ((n * s).round() as usize).max(2);
        Voice {
            envelope: MultistageEnvelope::new(sample_rate),
            bow,
            blow,
            strike,
            tube: Tube::new(),
            diffuser: [
                Ap::new(sz(126.0)),
                Ap::new(sz(180.0)),
                Ap::new(sz(269.0)),
                Ap::new(sz(444.0)),
            ],
            resonator: Resonator::new(),
            previous_gate: false,
            envelope_value: 0.0,
            strength: 0.0,
            exciter_level: 0.0,
            sample_rate,
            bow_buffer: vec![0.0; block],
            blow_buffer: vec![0.0; block],
            strike_buffer: vec![0.0; block],
            bow_strength: vec![0.0; block],
        }
    }

    pub fn exciter_level(&self) -> f32 {
        self.exciter_level
    }

    fn gate_flags(&mut self, gate_in: bool) -> u8 {
        let mut flags = 0;
        if gate_in {
            if !self.previous_gate {
                flags |= FLAG_RISING;
            }
            flags |= FLAG_GATE;
        } else if self.previous_gate {
            flags |= FLAG_FALLING;
        }
        self.previous_gate = gate_in;
        flags
    }

    /// One block: `frequency` in Hz, strength 0–1 (velocity), gate.
    /// Outputs center/sides for the Part-level stereo stage, raw for
    /// the dry blend.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        patch: &Patch,
        frequency_hz: f32,
        strength: f32,
        gate_in: bool,
        raw: &mut [f32],
        center: &mut [f32],
        sides: &mut [f32],
        samples: &SampleData,
    ) {
        let size = raw.len();
        let flags = self.gate_flags(gate_in);
        let frequency = (frequency_hz / self.sample_rate).clamp(1e-5, 0.45);

        // the envelope-shape law (voice.cc)
        let shape = patch.exciter_envelope_shape;
        let mut envelope_gain = 1.0;
        if shape < 0.4 {
            let a = shape * 0.75 + 0.15;
            let dr = a * 1.8;
            self.envelope.set_adsr(a, dr, 0.0, dr);
            envelope_gain = 5.0 - shape * 10.0;
        } else if shape < 0.6 {
            let s = (shape - 0.4) * 5.0;
            self.envelope.set_adsr(0.45, 0.81, s, 0.81);
        } else {
            let a = (1.0 - shape) * 0.75 + 0.15;
            let dr = a * 1.8;
            self.envelope.set_adsr(a, dr, 1.0, dr);
        }
        let envelope_value = self.envelope.process(flags, size) * envelope_gain;
        let envelope_increment = (envelope_value - self.envelope_value) / size as f32;

        // exciter settings (voice.cc)
        let brightness_factor = 0.4 + 0.6 * patch.resonator_brightness;
        self.bow.timbre = patch.exciter_bow_timbre * brightness_factor;
        self.blow.parameter = patch.exciter_blow_meta;
        self.blow.timbre = patch.exciter_blow_timbre;
        self.blow.signature = patch.exciter_signature;
        let strike_meta = patch.exciter_strike_meta;
        self.strike.set_meta(
            if strike_meta <= 0.4 {
                strike_meta * 0.625
            } else {
                strike_meta * 1.25 - 0.25
            },
            ExciterModel::SamplePlayer,
            ExciterModel::Particles,
        );
        self.strike.timbre = patch.exciter_strike_timbre;
        self.strike.signature = patch.exciter_signature;

        self.bow.process(flags, &mut self.bow_buffer[..size], samples);

        // blow → tube (level split per voice.cc)
        let blow_level_knob = patch.exciter_blow_level * 1.5;
        let tube_level = if blow_level_knob > 1.0 {
            (blow_level_knob - 1.0) * 2.0
        } else {
            0.0
        };
        let blow_level = if blow_level_knob < 1.0 {
            blow_level_knob * 0.4
        } else {
            0.4
        };
        self.blow.process(flags, &mut self.blow_buffer[..size], samples);
        self.tube.process(
            frequency,
            envelope_value,
            patch.resonator_damping,
            tube_level,
            &mut self.blow_buffer[..size],
            tube_level * 0.5,
        );
        for v in self.blow_buffer[..size].iter_mut() {
            *v *= blow_level;
            // diffuse the blow path (voice.cc runs the granular
            // diffuser after the level scaling)
            for ap in self.diffuser.iter_mut() {
                *v = ap.process(*v, 0.625);
            }
        }

        self.strike
            .process(flags, &mut self.strike_buffer[..size], samples);

        // Past unity the strike level stops growing the exciter
        // amplitude and instead bleeds the raw exciter into the
        // resonator output (voice.cc).
        let mut strike_level = patch.exciter_strike_level * 1.25;
        let strike_bleed = if strike_level > 1.0 {
            (strike_level - 1.0) * 2.0
        } else {
            0.0
        };
        strike_level = strike_level.min(1.0) * 1.5;

        // strength smoothing + accent law (voice.cc tail)
        let strength_target = strength.clamp(0.0, 1.0);
        let strength_increment = (strength_target - self.strength) / size as f32;
        for (i, r) in raw.iter_mut().enumerate() {
            self.strength += strength_increment;
            self.envelope_value += envelope_increment;
            let mut e = self.envelope_value;
            let accent = accent_gain(self.strength);
            self.bow_strength[i] = e * patch.exciter_bow_level;

            // accent scales the strike buffer in place so the bleed
            // tap downstream picks it up too (voice.cc)
            self.strike_buffer[i] *= accent;
            e *= accent;

            let mut input_sample = 0.0;
            input_sample += self.bow_buffer[i] * self.bow_strength[i] * 0.125 * accent;
            input_sample += self.blow_buffer[i] * e;
            input_sample += self.strike_buffer[i] * strike_level;
            *r = input_sample * 0.5;
        }
        for &r in raw.iter() {
            let error = r * r - self.exciter_level;
            self.exciter_level += error * if error > 0.0 { 0.5 } else { 0.001 };
        }

        // damping feedback from the exciters (voice.cc)
        let mut damping = patch.resonator_damping;
        damping -= self.strike.damping() * strike_level * 0.125;
        damping -= (1.0 - self.bow_strength[0]) * patch.exciter_bow_level * 0.0625;
        damping = damping.max(0.0);

        self.resonator.frequency = frequency;
        self.resonator.geometry = patch.resonator_geometry;
        self.resonator.brightness = patch.resonator_brightness;
        self.resonator.position = patch.resonator_position;
        self.resonator.damping = damping;
        self.resonator.modulation_frequency = 0.5 / self.sample_rate;
        self.resonator.modulation_offset = 0.1;
        self.resonator
            .process(&self.bow_strength[..size], raw, center, sides);

        // the raw mallet signal bleeds through the exciter output
        // past unity strike level (voice.cc)
        for (c, s) in center.iter_mut().zip(self.strike_buffer[..size].iter()) {
            *c += strike_bleed * s;
        }
    }
}

// ── the space reverb (fx/reverb.h topology) ────────────────────────────────

struct Ap {
    buf: Vec<f32>,
    pos: usize,
}

impl Ap {
    fn new(len: usize) -> Self {
        Ap {
            buf: vec![0.0; len],
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

    /// Interpolated read at `offset` samples back from the write head
    /// (same convention and underflow clamp as `Delay::read_mod`).
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

    /// Write `v` at `offset` samples back from the write head
    /// (fx_engine.h `Write(line, offset, ...)` with a decrementing
    /// head maps to "write position minus offset" here).
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
            buf: vec![0.0; len],
            pos: 0,
        }
    }

    #[inline]
    fn read_tail(&self) -> f32 {
        self.buf[self.pos]
    }

    /// Interpolated read at `offset` samples back, LFO-modulated.
    /// The offset is clamped inside the line: an LFO excursion that
    /// lands exactly on the wrap boundary while the write head sits at
    /// zero underflowed the index arithmetic and panicked the audio
    /// thread — a probabilistic time bomb that killed whole takes at
    /// random times (caught by the elements_N.crash black boxes).
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

/// Stereo reverb after fx/reverb.h: 4 input allpasses, two modulated
/// delay branches with decay allpasses, LFO-smeared.
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
    /// LFO phases: \[0\] = 0.5 Hz (ap1 smear), \[1\] = 0.3 Hz (del2
    /// shimmer), both read as unipolar cosines (fx_engine.h).
    lfo_phase: [f32; 2],
    lfo_inc: [f32; 2],
    /// `sample_rate / 32_000` — converts firmware sample offsets.
    rate_scale: f32,
    pub amount: f32,
    pub diffusion: f32,
    pub time: f32,
    pub input_gain: f32,
    pub lp: f32,
}

impl Reverb {
    pub fn new(sample_rate: f32) -> Self {
        // firmware sizes are at 32 kHz; scale to ours
        let s = sample_rate / 32_000.0;
        let sz = |n: f32| ((n * s) as usize).max(16);
        Reverb {
            ap: [
                Ap::new(sz(150.0)),
                Ap::new(sz(214.0)),
                Ap::new(sz(319.0)),
                Ap::new(sz(527.0)),
            ],
            dap1a: Ap::new(sz(2182.0)),
            dap1b: Ap::new(sz(2690.0)),
            del1: Delay::new(sz(4501.0)),
            dap2a: Ap::new(sz(2525.0)),
            dap2b: Ap::new(sz(2197.0)),
            del2: Delay::new(sz(6312.0)),
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

    /// Process one block in place. `main` is the upstream main mix
    /// (the del2-modulated branch lands here, per fx/reverb.h `*left`);
    /// `aux` gets the del1 branch.
    pub fn process(&mut self, main: &mut [f32], aux: &mut [f32]) {
        let kap = self.diffusion;
        let klp = self.lp;
        let krt = self.time;
        let amount = self.amount;
        let gain = self.input_gain;
        let s = self.rate_scale;
        for i in 0..main.len() {
            // two unipolar cosine LFOs (fx_engine.h CosineOscillator):
            // 0.5 Hz smears ap1, 0.3 Hz shimmers the del2 tap
            for (phase, inc) in self.lfo_phase.iter_mut().zip(self.lfo_inc.iter()) {
                *phase += inc;
                if *phase >= 1.0 {
                    *phase -= 1.0;
                }
            }
            let lfo1 = 0.5 - 0.5 * (self.lfo_phase[0] * std::f32::consts::TAU).cos();
            let lfo2 = 0.5 - 0.5 * (self.lfo_phase[1] * std::f32::consts::TAU).cos();

            // smear AP1 inside the loop (reverb.h): re-inject a
            // modulated early tap further down the same line
            let smear = self.ap[0].read_at((10.0 + 80.0 * lfo1) * s);
            self.ap[0].write_at((100.0 * s).round() as usize, smear);

            let mut acc = (main[i] + aux[i]) * gain;
            for ap in self.ap.iter_mut() {
                acc = ap.process(acc, kap);
            }
            let apout = acc;

            // branch 1: + modulated del2 tail (upstream
            // c.Interpolate(del2, 6211, LFO_2, 100, krt)). The
            // firmware's c.Write(del1, 2.0) writes the accumulator
            // into the line FIRST and only then scales the running
            // value for the wet tap — writing the doubled value into
            // the loop doubles the loop gain per pass and the tail
            // never decays (caught by the decay test).
            let mod_offset = (6211.0 + 100.0 * lfo2) * s;
            acc = apout + self.del2.read_mod(mod_offset) * krt;
            self.lp1 += klp * (acc - self.lp1);
            let mut b = self.lp1;
            b = self.dap1a.process(b, -kap);
            b = self.dap1b.process(b, kap);
            self.del1.write(b);
            let wet_main = b * 2.0;
            main[i] += (wet_main - main[i]) * amount;

            // branch 2: + del1 tail
            acc = apout + self.del1.read_tail() * krt;
            self.lp2 += klp * (acc - self.lp2);
            let mut b = self.lp2;
            b = self.dap2a.process(b, kap);
            b = self.dap2b.process(b, -kap);
            self.del2.write(b);
            let wet_aux = b * 2.0;
            aux[i] += (wet_aux - aux[i]) * amount;
        }
    }
}

/// Part-level output stage: space → raw gain / spread / reverb params
/// (part.cc), then the soft limiter.
pub struct Part {
    pub voice: Voice,
    pub reverb: Reverb,
    samples: SampleData,
}

#[inline]
fn soft_limit(x: f32) -> f32 {
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

impl Part {
    pub fn new(sample_rate: f32, block: usize, seed: u32) -> Self {
        Part {
            voice: Voice::new(sample_rate, block, seed),
            reverb: Reverb::new(sample_rate),
            samples: SampleData::load(),
        }
    }

    /// Render one block of stereo output.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        patch: &Patch,
        frequency_hz: f32,
        strength: f32,
        gate: bool,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let size = out_l.len();
        let mut raw = vec![0.0; size];
        let mut center = vec![0.0; size];
        let mut sides = vec![0.0; size];
        self.voice.process(
            patch,
            frequency_hz,
            strength,
            gate,
            &mut raw,
            &mut center,
            &mut sides,
            &self.samples,
        );

        // space mapping (part.cc)
        let space_in = patch.space.min(1.0);
        let raw_gain = if space_in <= 0.05 {
            1.0
        } else if space_in <= 0.1 {
            2.0 - space_in * 20.0
        } else {
            0.0
        };
        let space = (space_in - 0.1).max(0.0);
        let spread = space.min(0.7);
        let reverb_amount = (space - 0.5).max(0.0);
        let reverb_time = 0.35 + 1.2 * reverb_amount;

        for i in 0..size {
            let side = sides[i] * spread;
            let r = center[i] - side;
            let l = center[i] + side;
            out_r[i] = soft_limit(r);
            out_l[i] = soft_limit(l + (raw[i] - l) * raw_gain);
        }

        self.reverb.amount = reverb_amount;
        self.reverb.diffusion = 0.625;
        self.reverb.time = reverb_time;
        self.reverb.input_gain = 0.2;
        self.reverb.lp = 0.7;
        // part.cc: reverb_.Process(main, aux) — main is the center
        // minus side mix, which this port writes to out_r
        self.reverb.process(out_r, out_l);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struck_voice_rings_and_is_bounded() {
        let mut part = Part::new(48_000.0, 64, 0xfeed);
        let patch = Patch {
            exciter_strike_level: 0.9,
            resonator_damping: 0.4,
            space: 0.3,
            ..Default::default()
        };
        let mut l = vec![0.0; 64];
        let mut r = vec![0.0; 64];
        let mut energy = 0.0_f32;
        let mut peak = 0.0_f32;
        for blk in 0..400 {
            let gate = blk < 4;
            part.process(&patch, 220.0, 0.8, gate, &mut l, &mut r);
            energy += l.iter().chain(r.iter()).map(|s| s * s).sum::<f32>();
            peak = l.iter().chain(r.iter()).fold(peak, |m, s| m.max(s.abs()));
        }
        assert!(energy > 1e-4, "the strike sounds: {energy}");
        assert!(peak.is_finite() && peak <= 1.2, "soft-limited: {peak}");
    }

    #[test]
    fn strike_bleed_reaches_center_past_unity_level() {
        let center_energy = |level: f32| -> f32 {
            let samples = SampleData::load();
            let mut voice = Voice::new(48_000.0, 64, 0xfeed);
            let patch = Patch {
                exciter_strike_level: level,
                resonator_damping: 0.9,
                ..Default::default()
            };
            let mut raw = vec![0.0; 64];
            let mut center = vec![0.0; 64];
            let mut sides = vec![0.0; 64];
            let mut energy = 0.0_f32;
            for blk in 0..8 {
                voice.process(
                    &patch,
                    220.0,
                    0.8,
                    blk < 4,
                    &mut raw,
                    &mut center,
                    &mut sides,
                    &samples,
                );
                energy += center.iter().map(|s| s * s).sum::<f32>();
            }
            energy
        };
        // levels ≥ 0.8 saturate the exciter gain (min(level·1.25, 1)),
        // so 0.8 vs 1.0 isolates the bleed term exactly
        let sub_unity = center_energy(0.6); // 0.75 after the 1.25 gain
        let saturated = center_energy(0.8); // gain saturated, bleed 0
        let bleeding = center_energy(1.0); // bleed = 0.5
        assert!(
            bleeding > sub_unity,
            "bleed beats the sub-unity level: {sub_unity} vs {bleeding}"
        );
        assert!(
            bleeding > saturated * 1.2,
            "with identical exciter gain only the bleed differs: {saturated} vs {bleeding}"
        );
    }

    #[test]
    fn bow_sustains_while_gated() {
        let mut part = Part::new(48_000.0, 64, 0xfeed);
        let patch = Patch {
            exciter_strike_level: 0.0,
            exciter_bow_level: 0.9,
            exciter_envelope_shape: 0.9, // sustained organ region
            resonator_damping: 0.6,
            space: 0.2,
            ..Default::default()
        };
        let mut l = vec![0.0; 64];
        let mut r = vec![0.0; 64];
        // let the bow build up
        for _ in 0..400 {
            part.process(&patch, 220.0, 0.9, true, &mut l, &mut r);
        }
        let sustained: f32 = l.iter().map(|s| s * s).sum();
        assert!(sustained > 1e-6, "bow sustains while gated: {sustained}");
    }

    #[test]
    fn space_widens_and_wets() {
        let render = |space: f32| -> (f32, f32) {
            let mut part = Part::new(48_000.0, 64, 7);
            let patch = Patch {
                space,
                resonator_damping: 0.3,
                ..Default::default()
            };
            let mut l = vec![0.0; 64];
            let mut r = vec![0.0; 64];
            let mut gated = 0.0_f32;
            let mut tail = 0.0_f32;
            for blk in 0..400 {
                part.process(&patch, 330.0, 0.8, blk < 4, &mut l, &mut r);
                let e: f32 = l.iter().chain(r.iter()).map(|s| s * s).sum();
                if blk < 50 {
                    gated += e;
                }
                if blk > 300 {
                    tail += e;
                }
            }
            (gated, tail)
        };
        let (_, dry_tail) = render(0.1);
        let (_, wet_tail) = render(1.0);
        assert!(
            wet_tail > dry_tail * 2.0,
            "space adds tail: {dry_tail} vs {wet_tail}"
        );
    }

    #[test]
    fn reverb_mod_read_survives_the_wrap_boundary() {
        // regression: drive the modulated read across every (pos,
        // offset) combination around the wrap — the underflow needed
        // pos == 0 with the offset at the boundary
        let mut d = Delay::new(64);
        for _ in 0..130 {
            for off in [0.0, 1.0, 62.0, 62.9, 63.0, 64.0, 200.0] {
                let v = d.read_mod(off);
                assert!(v.is_finite());
            }
            d.write(0.1);
        }
    }

    #[test]
    fn full_part_survives_ten_minutes_of_space() {
        // soak: the time bomb fired at ~90 s twice; run the reverb's
        // LFO through many full cycles with the part struck repeatedly
        let mut part = Part::new(48_000.0, 64, 99);
        let patch = Patch { space: 1.0, exciter_strike_level: 0.9, ..Default::default() };
        let mut l = vec![0.0; 64];
        let mut r = vec![0.0; 64];
        for blk in 0..450_000 / 64 {
            part.process(&patch, 220.0, 0.8, blk % 64 < 8, &mut l, &mut r);
        }
        assert!(l.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn reverb_decays_and_stays_finite() {
        let mut rv = Reverb::new(48_000.0);
        rv.amount = 0.8;
        rv.time = 0.9;
        let mut l = vec![0.0; 64];
        let mut r = vec![0.0; 64];
        l[0] = 1.0;
        r[0] = 1.0;
        rv.process(&mut l, &mut r);
        let mut tail_early = 0.0_f32;
        let mut tail_late = 0.0_f32;
        for blk in 0..1_000 {
            let mut zl = vec![0.0; 64];
            let mut zr = vec![0.0; 64];
            rv.process(&mut zl, &mut zr);
            let e: f32 = zl.iter().chain(zr.iter()).map(|s| s * s).sum();
            assert!(e.is_finite());
            if blk < 50 {
                tail_early += e;
            }
            if blk >= 950 {
                tail_late += e;
            }
        }
        assert!(tail_early > 1e-8, "reverb has a tail");
        assert!(tail_late < tail_early, "and it decays");
    }
}
