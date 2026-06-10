# los design language — "phosphor & ink"

The visual contract for every module, present and future. Decided in the
design interview (2026-06-09): signal-semantic color, borderless density,
1-bit whimsy through texture and glyphs, motion that always traces a real
signal. Joy is the spec.

## 1. Principles

1. **Color is a promise.** Hue means signal type, never decoration. When
   something is colored, the player can trust it.
2. **Dense, but disciplined.** No boxes inside panes. Alignment, dim rules,
   and brightness do the structuring. Every cell earns its place.
3. **Motion is signal.** Nothing moves unless a real value moved (the badge
   is the licensed exception — and even it breathes with the transport).
4. **1-bit soul.** Texture from dither (`░▒▓`), chunky glyph cells, sprite
   energy — the SNES menu feeling without the frames.
5. **Stillness when stopped.** Transport off → trails fade, meters drain,
   the badge sleeps. Silence looks like silence.

## 2. Palette

Authored in truecolor; every entry has an xterm-256 fallback (detected via
`COLORTERM`/`tmux info`, falls back automatically).

| Token | Role | Truecolor | 256 |
|---|---|---|---|
| `bg` | background | `#0d0b08` | 232 |
| `ink` | values, content | `#e8dcc8` | 223 |
| `ink-dim` | inactive content | `#7d7363` | 101 |
| `amber` | chrome: wordmarks, labels, rules | `#9a7b2d` | 136 |
| `amber-hi` | chrome emphasis (active header) | `#e3a818` | 172 |
| `NOTE` | notes/pitch/velocity | `#e0763a` (burnt ochre) | 166 |
| `CV` | modulation, bindings, ghosts | `#3fc9b0` (teal) | 79 |
| `AUDIO` | audio meters, waveforms | `#8fbf4d` (moss green) | 107 |
| `CLOCK` | transport, playhead, BPM | `#c45dd4` (orchid) | 170 |
| `alert` | errors, clipping | `#d4502e` | 160 |

State is brightness, not hue: selected = `ink` on a bone block (inverse),
active = full hue, inactive = `ink-dim`. The four signal hues never appear
as chrome.

## 2.5 The color law (identity colors)

Identity colors are **patch-cable colors**: `theme::channel_color(claimed
modbus channel)` — collision-free while ≤12 sources share a window, both
cable ends compute the same hue independently, and cross-window reuse is
fine (windows never share a screen). Where identity color may appear:

- ✓ a source's own label + out-meter in its pane (SEQ `t3`, MATHs `C2`)
- ✓ a bound param's bar + `⌁tag` (**cable wins** over page identity)
- ✓ the focused channel's page accents + its own unbound sliders
- ✓ picker rows
- ✓ pitch-class wheel on note step cells ONLY (muted, de-primaried hues;
  brightness rises with octave); CV teal ramp on mod cells only

Forbidden: identity or pitch hues on chrome, rules, borders, status lines,
help, or anything decorative. New modules derive colors from their claimed
channels via `theme::channel_color` — never invent hues.

## 3. Glyph vocabulary

```
●○        note step on/off          ◆◇   mod step on/off
▶         playhead / transport      ░▒▓  playhead wake, dither, trails
⌁         modulation / binding      ▴    ghost marker (live mod value)
♪         BPM / pitch               ∿    audio
▁▂▃▄▅▆▇█  meters & gauges           ▕▏   meter caps
·         dim separator             ─    section rule (amber-dim)
◉◎        gates hi/lo (EOR/EOC)     ↗↘―  stage arrows (rise/fall/sustain)
```

Wordmarks: caps in `amber`, one cell of brand mischief where natural —
`SEQ · MATHs · VOICE · MIX · SCOPE · LOS`. (MATHs keeps its lowercase tail.)

## 4. Pane anatomy (the module contract, visual half)

Every module pane, top to bottom:

```
WORDMARK ·context·            right-aligned: ♪BPM ▶pos   ← header (amber)
<dense content>                                          ← the instrument
──────────────────────────────────────────────          ← rule (amber-dim)
MODE · pending… · message              :ex-line         ← status (1 line)
```

- Header: wordmark + position context left; transport echo right (CLOCK
  hue) so every pane answers "where are we" without looking away.
- No interior borders. Columns aligned; sections split by dim rules or
  blank lines only.
- Status line carries the vi state: mode label, pending chords (`d…`,
  `f…`), count buffer, undo/ex messages. Ex prompt replaces it when open.
- Overlays (picker, help): floating, `Clear`-backed, amber border — the
  ONLY boxes in the system, signaling "you've left the instrument."

Required visual behaviors (new-module checklist):
- [ ] Signal hues per §2 — never decorative color
- [ ] Selected item: inverse bone block
- [ ] Modulated params: ghost marker + live/set value alternation (§5)
- [ ] Meters for anything audible/CV-visible, with peak-hold decay
- [ ] Trigger moments flash their cell (1 frame inverse)
- [ ] Stillness when transport stopped
- [ ] Header/status anatomy as above; help overlay lists keys in
      doctrine order (docs/keybindings.md is the behavioral half)

## 5. Modulation feedback (ghost + live)

The rule every bindable param honors:

```
rise   100ms ▓▓▓▴░░░░ ⌁t3
             │  └ ghost: live modulated position (CV teal, moving)
             └ set value: bone, stays put
value ticks: 100ms → ⌁88ms → 100ms   (alternates ~1s, live in teal)
```

The `⌁source` tag sits at the row's right edge in the source's hue.
Unmodulated rows show no ghost and no tag — absence is information.

## 6. Motion doctrine

| Motion | Trigger | Budget |
|---|---|---|
| Playhead wake | sequencer step | 3-cell `░▒▓` fade behind ▶ |
| Trigger flash | note/trig fires | 1 frame inverse on the cell |
| Ghost markers | bound param | every frame (it IS the signal) |
| Meter decay | audio/CV level | peak-hold tick falls ~2s |
| Badge breath | transport beat | dither wave per beat |
| Badge sleep | transport stop | freeze + dim over ~2s |

20 fps (existing 50 ms poll). No motion without a signal source.

## 7. The badge (`los badge`)

A tiny optional pane (on by default, ~22×7) — the faceplate.

```
 ▗▖ ▄▄▄   ▄▄▄
 ▐▌▒░ ░▓ ▀▒▒        body dithers with the beat
 ▐▌▀▓▒▓▀ ░▓▒▀       (breathing dither, base mood)
 ▐▙▄▄▖
 ──────────────
 my-sketch ♪120 ▶   session · bpm · transport (amber/CLOCK)
```

Moods (cycle with `m`; all read real signal):
- **breathe** (default): `░▒▓` wave rolls through the glyph each beat;
  swells on bar starts; stills + dims when stopped.
- **familiar**: a 2-cell 1-bit creature idles on the logo's ledge, hops on
  triggers (reads the event ring as consumer 15), sleeps when stopped.
- **halo**: the live mix threads a 1-row waveform under the wordmark.
- **pulse**: glyph rows shear one frame on each beat (the glitch).

Later: configurable info line (`:set line bpm|state|outputs`).

## 8. tmux shell theme

Installed at session creation (like the transport keys):
- pane borders `ink-dim`, active pane border `amber-hi`
- pane titles in brand voice: ` SEQ ` ` MATHs ` ` VOICE 1 `
- status bar: `los · <session> · ♪120 ▶ · <saved-state>` left, clock right,
  bg `bg`, fg `amber`
- truecolor when the terminal offers it; 256 palette otherwise

---

# Mockups (round 1)

All at ~60-col pane width. `←` annotations are not part of the render.
Imagine: bone values, amber labels/rules, hues as marked.

## SEQ

```
SEQ ·t1/8· bass-line                        ♪120 ▶09/16
▶t1 ●○○● ○○●○ ░▒▓▶○ ○●○●   16 P5 R0          ← ▶+wake CLOCK, ● NOTE
 t2 ○●○○ ●○○● ○○●○ ○●○○   12 P7 R2
 t3 ◆◇◇◆ ◇◇◆◇ ◆◇◇◆ ◇◆◇◇   16 ⌁MOD           ← ◆ CV teal
 t4 ●○●○ ●○●○ ●○●○ ●○●○   16 P8 R0  M       ← M = muted, dim row
─────────────────────────────────────────────
 01   02   03   04   05   06   07   08      ← selected track detail
 C4   ·    ·   ▐E4▌  ·    G4   ·    A#3     ← ▐sel▌ inverse block
 ▆    ·    ·    █    ·    ▅    ·    ▃       ← velocity (NOTE hue)
─────────────────────────────────────────────
NORMAL · 3d…                        u:Undo ×3
```

## MATHs

```
MATHs ·ch2/6·                               ♪120 ▶
 CH1⌁t1·tr   CH2 ·tr    CH3 ·gt    CH4 CYC↗   LOGIC
 rise 0ms    rise 12ms  rise 80ms  rise 2.1s  SUM ▃ +0.31
 fall 150ms  fall▓▴░90  fall 1.2s  fall 2.1s  OR  ▆ +0.62
 shap 0.85   shap 0.50  shap 0.22  shap 0.50  AND ▁ +0.04
 attn +1.00  attn +0.70 attn -0.30 attn +1.00 INV ▃ -0.31
 offs +0.00  offs +0.00 offs +0.25 offs +0.00 EOR ◉
 plck 0.80   plck 0.00  plck 0.00  plck 0.00  EOC ◎
 sig  —      sig  —     sig ⌁t3    sig  —
 out ▇↘ +0.74  out ▃↘    out ▅―     out ▂↗     ← stage arrows
─────────────────────────────────────────────
 ch2 fall ⌁t3 — ghost shows live (m: trig/gate)
```
(Column 2's fall shows the ghost `▴` riding its gauge; CH4 cycling = CYC
in CLOCK hue; EOR/EOC dots in CV teal when high.)

## VOICE

```
VOICE ·0·                                   ♪120 ▶
 shape ▓▓▓▓▓▴▓░░░ 0.55 ⌁maths/0/ch2          ← ghost + tag (CV)
 sub   ▓░░░░░░░░░ 0.10
 fm    ░░░░░░░░░░ 0.00
 out   main+sub
 amp  ⌁maths/0/ch1                           ← binding rows: CV hue
 notes⌁seq/0/t1
 lpg   ▓▓▓▓▓▓▓▓▓░ 0.90 vactrol
─────────────────────────────────────────────
 ∿ ▂▄▆█▆▄▂▁  lvl ▆▕▏ peak                    ← live out (AUDIO hue)
─────────────────────────────────────────────
NORMAL                              :w pluck-1
```

## MIX

```
MIX                                          ∿ master
 VOICE0   MATHs0   master
  ▕█▏      ▕▃▏      ▕▆▏‥                      ← ‥ = peak hold tick
  -3.0     -12.2    -4.5
  pan ·    pan ‹    ──
  ▐sel▌    M S      ♪
─────────────────────────────────────────────
NORMAL · ch1/2                       u:Level
```

## SCOPE

```
SCOPE ·mix·                                  ∿ 48k
      ▄▆█▇▅▂                    ▂▅▇█▆▄
   ▂▅▇      ▇▅▂            ▂▅▇        ▇▅▂    ← waveform AUDIO hue
 ▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▴▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔   ← trigger level line+marker
        ▂▅▇▆▃                ▃▆▇▅▂
─────────────────────────────────────────────
[Mode:Braille] Src:Mix Ch:S Zoom:2.0 Gain:1.0 Trig:+0.10
```

## LOS (conductor)

```
LOS ·states·                          Tab:modules
 ▸ bass-line.toml            today 14:02
   pluck-study.toml          today 11:30
   drone-2.toml              yesterday
─────────────────────────────────────────────
 modules: SEQ VOICE MATHs MIX SCOPE ✚        ← live glyph strip
NORMAL                          Enter:load
```

## BADGE

```
 ▗▖ ▄▄▄   ▄▄▄
 ▐▌▒░ ░▓ ▀▒▒
 ▐▌▀▓▒▓▀ ░▓▒▀
 ▐▙▄▄▖
 ─────────────
 bass-line ♪120 ▶
```

## Overlay (picker — the only box)

```
        ╔═ bind source ═══════════╗
        ║ — none —                ║
        ║ seq/0/t1   ♪            ║
        ║ seq/0/t3   ♪            ║
        ║▐maths/0/ch1 ⌁ ▂▅▃▆     ▌║   ← live preview spark!
        ║ maths/0/eoc ⌁           ║
        ╚═════════════════════════╝
```

---

# Build plan (after mockup sign-off)

1. `src/theme.rs` — palette tokens (truecolor + 256 fallback detection),
   glyph constants, shared header/status/rule renderers. Tests.
2. tmux shell theme at session creation.
3. Badge module (`los badge`): breathe mood + info line, then familiar.
4. Module-by-module redress in contract order: SEQ → MATHs → VOICE →
   MIX → SCOPE → LOS, each PR adding the §4 checklist behaviors
   (ghosts, wakes, flashes, meters, stillness).
5. DESIGN.md §6.5 grows the visual contract; keybindings.md stays the
   behavioral contract; this doc is the visual source of truth.
```
