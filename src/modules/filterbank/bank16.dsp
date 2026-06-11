// bank16 — the filterbank's 16 fixed bands, after the Buchla 296e's
// analysis curve (docs/plans/filterbank-296e.md). One input, sixteen
// outputs: the RAW band signals, pre-VCA — all gains, morphing,
// vocoder transfer, freezing, spreading, and panning happen in Rust,
// where they integrate with the modbus and the UI. Zero widgets on
// purpose (docs/writing-dsp.md).
//
// Band 1 is a lowpass (<100 Hz), band 16 a highpass (>10 kHz); the
// fourteen between are three stagger-tuned resonant bandpasses in
// series (the original 296 built each band exactly this way: skewed
// centers make a flat top with steep skirts).
//
// Regenerate the committed bank16_gen.rs with `just dsp`.
declare name "bank16";
declare author "doo-nothing / AU Supply";
declare license "AGPL-3.0-or-later";

import("stdfaust.lib");

centers = (150, 250, 350, 500, 630, 800, 1000, 1300, 1600, 2000, 2600, 3500, 5000, 8000);

q = 6.0;
// three stagger-tuned sections. fi.resonbp's peak gain grows with q,
// so the per-section makeup is well under 1 — 0.19 lands the cascade
// near unity at center (pinned by the band_gains test).
bp(f) = fi.resonbp(f*0.94, q, 0.19) : fi.resonbp(f, q, 0.19) : fi.resonbp(f*1.06, q, 0.19);

band(0)  = fi.lowpass(3, 100.0);
band(15) = fi.highpass(3, 10000.0);
band(i)  = bp(ba.take(i, centers));

process = _ <: par(i, 16, band(i));
