# Tape — the record window (Tascam 4-track × OP-1 tape)

The fourth window: a tape deck for capturing the rig into a song.
Interview-locked decisions: **hybrid sourcing** (tracks default to the
mix, re-armable to any audio source), **optional RAVE helper** for
offline neural processing, **both automation styles** (mod-bindings +
recorded fader moves), and the full tape-feel set (varispeed, loop +
overdub, reverse, bounce + export).

## 1. The deck

One `tape` module (window "tape", beside MATHs 3): **6 tracks** on a
**3:00 tape** at the device rate, stereo i16 in RAM (~104 MB all six),
persisted as WAVs under `~/.config/los/tape/` with the session.

- **Transport**: the tape follows the global play flag; position
  advances by `speed × dt`. `r` arms record; recording runs only at 1×
  (varispeed is a playback instrument; the v1 constraint keeps capture
  sample-locked to the rig).
- **Per track**: input (mix | any audio source), arm, monitor, fader,
  pan, reverse, mute, a RAVE slot, and a drawn **waveform overview**
  (meter_frac per chunk — the tape is the picture).
- **GLOBAL**: speed (0.25×–2×, bindable — the Cortini knob), loop
  in/out + on, position, tape length, bounce, export.

## 2. Signal architecture

- **The print bus**: the mixer grows `/los_mix_print`, advertised as
  `mix/0` — the sum of every strip EXCEPT tape returns (post-strip,
  pre-master-color), exactly the send-bus pattern with name-based loop
  protection. Tracks armed to "mix" consume it; overdubs hear earlier
  takes through the console but never re-record them.
- **Per-source arming** reuses the fx input claim verbatim: arming t3
  to `voice/1` pulls that cable out of the console; the deck monitors
  armed inputs through its own output while armed (Tascam-style input
  monitoring).
- **Playback**: unmuted tracks sum (automation gain × fader, pan,
  reverse, varispeed via linearly-interpolated read) into the tape's
  out ring — the console adopts a `Tape 0` return strip like any
  source. The print bus skips it by name.
- **Sync**: at 1× the tape position derives from the transport clock —
  sample-locked overdubs. Under varispeed it free-runs by design.

## 3. Automation

- Every continuous tape param is **`@`-bindable** (MATHs 3 lives on the
  window for exactly this).
- **Write mode** (`w` on a track): while the tape rolls, fader moves
  append `(pos, value)` points to that track's lane; playback replays
  them (sample-held between points). `W` clears the lane. Lanes save
  with the tape. v1 scope: track faders.

## 4. RAVE (optional, offline)

`tools/los-rave` — a uv-managed Python helper (torch only; models are
TorchScript exports). Never required: the deck's RAVE row lights up
only when the helper and a model directory (`~/.config/los/rave/*.ts`)
exist.

- `los-rave process --model <m.ts> in.wav out.wav [--progress]`
- `los-rave fetch vintage|percussion|...` (IRCAM pretrained; `vintage`
  is the closest stock "cassette/lofi" character)
- Deck flow: `R` on a track with a take → helper runs async (progress
  in the status line) → processed take swaps in, original kept for
  undo/A-B.

## 5. House wiring ("hit record, get a song")

Window 4 "tape": TAPE + MATHs 3. Fresh sessions arm track 1 to the
mix with the loop set to the house drone's 16-bar form; the lane
already evolves the piece, so `r` + 52 seconds = a take. MATHs 3 ch1
arrives bound to the tape speed at 1× (atten 0 — wiggle the
attenuverter and the tape starts breathing). Export drops
`~/Music/los/<name>.wav`.

Plumbing fix that rides along: envelope event-consumer IDs grow to
instances 0–5 (8..13; 12–13 were reserved) so MATHs 3 consumes notes
without colliding with MATHs 2.

## 6. Out of scope (v1)

Recording under varispeed · per-step tape splices/cuts · automation on
params beyond track faders · RAVE prior sampling (generation) · tape
saturation DSP (the RAVE vintage model carries the character for now).
