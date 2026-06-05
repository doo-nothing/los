```
▗▖ ▄▄▄   ▄▄▄
▐▌█   █ ▀▄▄
▐▌▀▄▄▄▀ ▄▄▄▀
▐▙▄▄▖
```

**los** is a modal, terminal-based music instrument. A single statically-linked
binary that turns any Linux machine into a keyboard-driven forge for real-time
audio.

It is not a DAW. It is not a plugin host. It is not a general-purpose audio
workstation. It is an instrument — Vim-inspired, west-coast synthesis, 4-track
tape, sequencer, and effect chain, all inside your terminal. No menus. No mouse.
Every key press is a hammer strike.

- **Modal by design.** Normal, Insert, Command. Verb chaining. `c2d` to tweak
  two delay parameters. `.` to repeat. `:save` to commit.
- **Lean.** Idles under 3% CPU on a 2012 ThinkPad. One binary, no daemons.
- **Portable.** Musl static build. Drop it on any Linux kernel and play.
- **Command-line first, always.** Runs on the raw Linux console or in your
  favorite terminal emulator.
- **Internal voice engine.** Additive oscillators, ADSR envelopes, feedback
  routing matrix — modular synth thinking without the cables.
- **Tape mode.** 4-track WAV recording, overdub, looping, bounce.
- **MIDI in/out.** Sequence external gear or drive the internal voice. MIDI CC
  to any parameter.

---

## Quick start (once it builds)

```sh
los
```

It autodetects your audio device and starts with a sine wave voice waiting for
your fingers.

---

## Project status

**v1.0 — building the Rust binary.** See [DESIGN.md](DESIGN.md) for the full
architecture, philosophy, and roadmap.

**v2.0 — bootable ISO.** Minimal Linux + los binary. Plug in a USB, boot, play.

---

## License

TBD

---

*Named for William Blake's Los, the blacksmith who hammers Golgonooza — the
city of art — into being through relentless labour.*
