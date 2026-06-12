// wasp — the filter core, after the Doepfer A-124 VCF5 (itself after
// the EDP Wasp). A 12 dB/oct state-variable filter whose loop runs
// through a soft CMOS-ish nonlinearity — the famous dirty rasp — with
// the A-124's two outputs: bandpass, and the LP<->HP mix knob (notch
// at noon).
//
// Params (alphabetical ParamIndex order, pinned by tests):
//   dirt (0..1) - how hard the loop leans on the nonlinearity
//   freq (0..1) - cutoff, exponential 30 Hz - 12 kHz
//   mix  (0..1) - 0 = lowpass, 0.5 = notch, 1 = highpass
//   res  (0..1) - resonance up to the edge of self-oscillation
//
// Two outputs: [0] = the mix out (LP/HP blend), [1] = bandpass.
// Regenerate the committed wasp_gen.rs with `just dsp`.
declare name "wasp";
declare author "doo-nothing / AU Supply";
declare license "AGPL-3.0-or-later";

import("stdfaust.lib");

dirt = hslider("dirt", 0.5, 0.0, 1.0, 0.001) : si.smoo;
freq = hslider("freq", 0.5, 0.0, 1.0, 0.001) : si.smoo;
mix = hslider("mix", 0.0, 0.0, 1.0, 0.001) : si.smoo;
res = hslider("res", 0.3, 0.0, 1.0, 0.001) : si.smoo;

fc = 30.0 * pow(400.0, freq);
q = 0.5 + res * 19.5;

// the CMOS inverter pair biased linear: a soft tanh stage. dirt
// drives INTO the filter and saturates coming out — wrapping the SVF
// rather than living inside each integrator (the library SVF's loop
// is closed to us), which still rasps convincingly at high res.
grit(x) = x * (1.0 - dirt) + ma.tanh(x * (1.0 + dirt * 3.0)) * dirt;

nf = min(fc, 14000.0) / ma.SR;

driven(x) = grit(x * (1.0 + dirt * 1.5));

process(x) = mixout, bpout
with {
    lpo = driven(x) : ve.oberheimLPF(nf, q);
    hpo = driven(x) : ve.oberheimHPF(nf, q);
    bpo = driven(x) : ve.oberheimBPF(nf, q) : grit;
    mixout = (lpo * (1.0 - mix) + hpo * mix) : grit;
    bpout = bpo;
};
