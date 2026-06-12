# The LFO — four phase-disciplined channels (after the Xaoc Batumi)

los modulates everything through the modbus, but every slow cable so
far borrowed a MATHs channel in loop mode. This module is the
dedicated answer: **four LFO channels with Batumi's discipline** —
free, quadrature, phase, and divide modes — published as eight
modulation sources and nothing else. No audio ring; it is a pure
mod-source module (the sequencer's role, for curves).

Module name: `lfo` (aliases `batumi`, `quad`).

## Modes (the hardware's four)

- **free** — four independent rates.
- **quad** — all channels at channel 1's rate, phases locked to
  0° / 90° / 180° / 270° (the barber-pole patch).
- **phase** — channel 1 is the master; 2–4 run at its rate with their
  `phase` knobs as offsets.
- **div** — channel 1 is the master; 2–4 divide its rate by the
  `phase` knob mapped to /2 … /16 (integer divisions, phase-locked,
  never drifting).

## Rows

`mode` · `rst` (trigger binding: a rising edge re-zeros every phase —
bind a sequencer track and the LFO bank snaps to the bar) · then per
channel: `freq` (log, 209 s – 50 Hz), `shape` (sine · tri · saw · sqr
· s&h), `phase` (offset in free/quad too, division pick in div).

`freq` rows are `@`-bindable (rate CV per channel). Everything else
is manual — the hardware agrees.

## Outputs (modbus claims, `routing::output_labels("lfo")` order)

`s1 s2 s3 s4` — the four sines (always sine, Batumi's fixed output)
`a1 a2 a3 a4` — the four assign outputs (the channel's `shape`)

All unipolar 0–1 (the los receiver convention). S&H steps on each
phase wrap, per-channel xorshift seeded by instance for determinism
within a run.

## Engine

A control-rate thread (~750 Hz, the modbus's native resolution):
phases advance by `freq × dt`; quad/phase/div derive 2–4 from
channel 1's phase arithmetically (never integrate separately —
that's how hardware drifts and Batumi doesn't). Reset edge re-zeros
the master phase.

Tests: mode phase relationships exact (quad = 0.25 offsets, div =
integer ratios), shapes bounded 0–1, s&h steps only on wraps,
reset re-zeros, freq mapping log-correct at both ends.
