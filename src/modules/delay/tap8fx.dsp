// tap8fx — the delay's feedback characters, after the Verbos MDP's
// internal DSP on tap 8: one input (tap 8's audio), two outputs —
// a +1-octave pitch shift (the "shim" channel) and a small-room
// reverb wash (the "wash" channel). Amounts are mixed in Rust; this
// core has no parameters on purpose, keeping the generated interface
// trivial (see docs/writing-dsp.md).
//
// Regenerate the committed tap8fx_gen.rs with `just dsp`.
declare name "tap8fx";
declare author "doo-nothing / AU Supply";
declare license "AGPL-3.0-or-later";

import("stdfaust.lib");

// Octave-up granular transposer: 50ms window, 12ms crossfade — big
// enough to track pitched material, small enough to smear percussives
// into the classic shimmer blur.
shim = ef.transpose(2400, 600, 12);

// Freeverb tuned small and dark: the wash should sit behind the
// repeats, not become the patch (raise the wash fader for that).
// mono_freeverb returns the RAW comb sum (classic Freeverb applies a
// ~0.015 wet scaler outside the kernel) — without the 0.04 here the
// wash path has ~+28 dB of loop gain and the feedback self-oscillates.
wash = re.mono_freeverb(0.70, 0.6, 0.7, 0) * 0.04;

process = _ <: shim, wash;
