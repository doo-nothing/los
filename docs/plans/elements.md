# Elements — a full port of Mutable Instruments Elements

A modal-synthesis voice: three exciters (bow, blow, strike) feed a
64-mode modal resonator, with a tube waveguide on the blow path and
the famous `space` stereo reverb on the way out. This is a port of
Emilie Gillet's Elements firmware DSP (pichenettes/eurorack,
MIT-licensed; copyright and permission notices preserved in every
ported file), retargeted from 32 kHz fixed-block STM32 code to los's
sample-rate-agnostic Rust modules.

Module name: `elements` (aliases `modal`, `mi`). Monophonic, like the
hardware. Notes give pitch + gate; velocity is the hardware's
`strength` input (the accent curve is the original ±3 dB/V law).

## Signal path (faithful)

- **Contour**: the multistage envelope with the original shape law —
  knob < 0.4 = percussive AD (gain boost up to 5×), 0.4–0.6 = ADSR
  with rising sustain, > 0.6 = sustained organ swell.
- **Bow**: the FLOW exciter (smoothed noise flips) through the bow
  timbre LP, driving the resonator's banded waveguides through the
  bow table (the stick-slip nonlinearity, ported verbatim).
- **Blow**: the granular noise sample player (the original flash noise
  sample, embedded) with `blow_meta` as grain restart point and
  timbre as pitch, through the **tube** waveguide (reed nonlinearity,
  ported verbatim) at high blow levels.
- **Strike**: `strike_meta` morphs SAMPLE_PLAYER → MALLET → PLECTRUM →
  PARTICLES exactly as the firmware maps it; the nine percussive
  flash samples ship in `samples.bin` (338 KB, extracted from the
  MIT-licensed resources).
- **Resonator**: the 64-mode bank — stiffness walk from the original
  geometry table (transcribed analytically from
  `resources/lookup_tables.py`), brightness/Q-loss law, damping over
  four decades, position as a cosine-oscillator comb, the 0.5 Hz
  side-channel LFO, and 8 banded waveguides for the bow path.
  Resolution 52 modes (the firmware's own setting).
- **Space**: the fx_engine reverb (4 input allpasses, two modulated
  delay branches with decay allpasses), ported with the original
  delay lengths scaled to the device rate; `space` spans dry →
  cathedral, with width handled at the center/sides sum like Part.

## Deviations (all deliberate, all documented in code)

- Sample-rate-agnostic: LUTs are computed analytically from the
  formulas in `lookup_tables.py` instead of carrying 32 kHz tables.
- The easter-egg ominous voice and the string/Rings resonator models
  are not ported (Elements' panel doesn't reach them either).
- The AGC/panic logic is replaced by the house NaN/runaway watchdog.

## Rows

`contour` · `bow` · `bow_t` · `blow` · `blow_m` · `blow_t` · `strike`
· `strike_m` · `strike_t` · `geometry` · `brightness` · `damping` ·
`position` · `space` · `level` · `notes` · `amp`(strength override).
CV (`@`): geometry, brightness, damping, position, space, contour.

## Verification

Engine tests: stiffness table endpoints vs the python, mode count vs
frequency, strike/mallet impulse decays, bow table bounds, tube
stability, envelope shape law regions, reverb tail decays and is
finite, full-voice render is bounded and non-silent, samples.bin
round-trips (boundaries monotonic, lengths match). Then the human
gate: a multi-instance gamelan piece, auditioned.
