// swarm — the brass machine's core: seven detuned sawtooths drifting
// against each other, summed into a resonant ladder filter. The CS-80
// move (filter swell, chord spreads, paraphony) lives in Rust; this
// core is one swarm note. Three instances make the chord stack.
//
// Params (ParamIndex order pinned by the swarm module's tests):
//   freq (Hz) · detune (0..1) · cutoff (0..1, exponential 60 Hz–12 kHz)
//   res (0..1) · level (0..1)
//
// Regenerate the committed swarm_gen.rs with `just dsp`.
declare name "swarm";
declare author "doo-nothing / AU Supply";
declare license "AGPL-3.0-or-later";

import("stdfaust.lib");

freq = hslider("freq", 110.0, 20.0, 4000.0, 0.01) : si.smoo;
detune = hslider("detune", 0.3, 0.0, 1.0, 0.001) : si.smoo;
cutoff = hslider("cutoff", 0.5, 0.0, 1.0, 0.001) : si.smoo;
res = hslider("res", 0.25, 0.0, 1.0, 0.001) : si.smoo;
level = hslider("level", 0.8, 0.0, 1.0, 0.001) : si.smoo;

// seven saws fanned across ±0.9% (≈15 cents) at full detune, each with
// its own slow drift so the swarm never phase-locks — the ensemble
// shimmer is the instrument. saw3 (3rd-order polynomial transition
// region) keeps the top end silk instead of foldback grit; the wider
// ±24-cent fan of the first cut read as detuned-broken, not lush.
drift(i) = os.osc(0.05 + 0.011 * i) * 0.0009;
ratio(i) = 1.0 + detune * 0.009 * (i - 3.0) / 3.0 + drift(i) * detune;
saw(i) = os.saw3(max(20.0, freq * ratio(i)));
bank = sum(i, 7, saw(i)) / 7.0 : fi.lowpass(1, 11000.0);

// the ladder: cutoff sweeps exponentially, resonance stays musical.
// moog_vcf_2b takes Hz + res 0..1 (moogLadder wants a NORMALIZED freq
// and NaNs if you hand it Hz — found the hard way, pinned by test)
fc = 60.0 * pow(200.0, cutoff);
ladder = ve.moog_vcf_2b(res * 0.92, min(fc, 14000.0));

process = bank : ladder : *(level);
