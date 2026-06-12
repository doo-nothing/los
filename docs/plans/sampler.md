# The sampler — a-u.supply reels + a Morphagene-ish microsound designer

Two instruments in one module:

1. **A proper sampler** — eight slots, each a sample with playback
   modes (one-shot · loop · gated · hold), splice windows, pitch
   tracking, varispeed (negative = reverse), and an internal AD
   envelope. **Kit mode** maps notes to slots (C→a, C#→b, … G→h), so
   one sequencer track plays a whole drum kit — finally, real drums.
2. **A microsound tape tool** (the Morphagene's cues): when `gene` is
   raised, playback becomes windowed grains of gene-length cycling
   inside the splice, `slide` scrubs the grain position through the
   reel, and varispeed/`pitch` repitch the grains. Bind `slide` to a
   drunk CV track and the reel strolls; bind it to an envelope and a
   single word becomes a gesture.

## Where samples come from

- **The cache**: `~/.config/los/samples/` — WAVs (anything `afconvert`
  can read gets converted on ingest; macOS ships the converter).
- **The a-u.supply search engine** (private API, key in
  `~/.config/a-u.suppl/env` as `AU_SUPPLY_KEY`, **never committed**):
  - `samples-bored` output index — one-shot drum/perc material
  - `__inputs__` with `media_types=["audio"]` — raw, long, found
    audio (the TikTok-length reels)
  - In-module **browser**: `/` opens a search row; type, Enter,
    arrow through results (filename · duration · tags), Enter
    downloads into the selected slot. `Tab` flips drums ↔ raw.
  - **CLI**: `los samples pull <query> [--raw] [--n N]` prefetches
    into the cache; `los samples ls` lists it.
  - All HTTP via `curl` subprocesses on a worker thread — los keeps
    zero HTTP dependencies, and the UI never blocks on the network.
- **Build flag**: the `au-api` cargo feature (default on) gates every
  network path; built without it — or run without a key file — the
  module is local-cache-only and the browser row says so.

## Per-slot rows (the designer)

| row | range | meaning |
|---|---|---|
| `sample` | — | the loaded file (browser loads here; `x` unloads) |
| `mode` | oneshot · loop · gated · hold | trigger behavior |
| `start`/`len` | 0–1 | the splice window into the reel |
| `pitch` | ±24 st | offset; notes track relative to C4 in single mode |
| `speed` | −2…+2 | varispeed; negative plays the splice backwards |
| `gene` | 0–1 | 0 = tape playback; up = grain size 1 s → 10 ms (log) |
| `slide` | 0–1 | grain position in the splice (the scrub) |
| `atk`/`dec` | 0–1 | internal AD per trigger (log time, vactrol-free) |
| `level` | 0–1 | slot gain |

Global rows: `kit` (kit ↔ single) · `slot` (the edit/load target) ·
`notes` (track binding) · `amp` (optional external amp source).
Bindables: `pitch`, `speed`, `gene`, `slide`, `level` per the usual
`@` convention; `slide` + `gene` are where the Morphagene lives.

Polyphony: 6 voices, round-robin; in kit mode each note owns its slot.
Consumer ID: sampler shares the voice range from below swarm
(sampler N → slot 5−N; collision matrix documented in shm.rs).
Modbus out: `env` — the loudest live voice's envelope.

## Engine notes

- Reels decode to mono f32 at the device rate via `afconvert` into the
  cache (`<id>.wav` + `<id>.json` metadata sidecar), 120 s/slot cap.
- Grain playback: two alternating heads, raised-cosine windows, 50%
  overlap — no grain edge ever clicks (same religion as the DLD: if a
  seam is audible, it's a bug).
- Varispeed via linear-interp fractional read; reverse = negative
  increment (the splice wraps in loop/gene modes, ends in one-shot).
- The AD envelope is per-voice; `gated` sustains while the note holds,
  `hold` ignores note-off and decays on its own clock.

## Verification

Unit: splice math, mode lifecycles, grain window continuity (no step
greater than window slope), kit-mode note→slot mapping, pitch ratios.
Live: a kit sketch (kick/hat/snare one-shots from samples-bored) and a
gene sketch (a raw reel strolled by a drunk track) — **auditioned by a
human before anything scores for them.**
