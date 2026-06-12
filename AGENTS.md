# Composing with los, as an agent

Los is a terminal modular groovebox. You can compose complete songs for
it by writing one TOML file and running three commands — no tmux, no
keystrokes, no live session.

## The loop

```sh
cp examples/first-song.toml song.toml      # start from the annotated example
$EDITOR song.toml                          # patterns, macros, the bar-by-bar lane
los check song.toml                        # ALL problems at once — fix until clean
los render song.toml take.wav              # realtime + audible; duration from the lane
los audit take.wav --song song.toml        # RMS per bar + per-section dynamics table
# read the table, change ONE thing, render again
```

`los check` is your compiler: misspelled keys, bad cable addresses,
out-of-range values, lane slots firing undefined macros — each with the
valid alternatives spelled out. A clean check is a loadable song.
`los render` refuses to run while a `los` tmux session exists; never
kill that session — it's the human's live rig.

## Before you compose — required reading

**Read [docs/composing.md](docs/composing.md) in full. Do not skip
this.** It is the difference between a song and an error report: the
complete schema with every range *and its musical meaning*, the
modulation address tables, the form recipes (how the 128-bar house
drone is built), the dynamics lessons from the first prompted record,
and the known limits. The 20 minutes of reading replaces hours of
render-and-guess.

Ground truth, in order:

1. [docs/composing.md](docs/composing.md) — the guide (read it all)
2. [examples/first-song.toml](examples/first-song.toml) — minimal, annotated; imitate it
3. [examples/house-drone.toml](examples/house-drone.toml) — a full ~7-minute form
4. `src/session/state.rs` — the schema itself; `src/ipc/routing.rs` — address grammar

## Hard rules

- A song needs a `mixer` pane — it owns the audio device and the clock.
- Put a macro at lane slot 0 so the form opens explicitly.
- One change per render; keep numbered takes.
- Renders are realtime and audible on the default device: a 7-minute
  song takes 7 audible minutes. Compose short, lengthen late.
- If you touch the code instead of composing: `just check` must pass,
  feature branch + PR, never push to master (see CONTRIBUTING.md).
