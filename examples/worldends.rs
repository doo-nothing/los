//! worldends — a one-off patch, written as code.
//!
//! "Till the World Ends" reimagined as a glacial four-voice fugue in
//! E♭ minor at 46 BPM (Vangelis × Cortini). The chant hook becomes the
//! fugue subject; the answer enters a fifth below; a tenor carries the
//! subject in AUGMENTATION (half speed, 32 steps against 16); the bass
//! holds the pedal. The macro lane stages the entries — exposition,
//! development (subject varied, answer in reverse), climax, unwinding,
//! a ghost outro.
//!
//! It only writes module state files:
//!     cargo run --example worldends
//! Then either reload a live session (SIGUSR2 the modules + `los add
//! voice 2`, `los add voice 3`, `los add delay 1`) or save from the
//! conductor and `los load` it later. The tape arrives armed: `r` is
//! the take (~5.6 minutes).
//!
//! This file doubles as the "patch-as-code" pattern: every knob the
//! TUIs expose is a serde field here, and the compiler keeps the patch
//! honest against the state schema.

use los::state::{
    self, CycleMode, DelayTapParam, DelayUnit, EnvelopeChannelParams, MacroCmd, MacroParam, Quant,
    SlotParam, StepParam, TapeTrackParam, TrackMode, TrackParam,
};

const REST: StepParam = StepParam {
    active: false,
    note: 60,
    velocity: 100,
    mod_value: 0.0,
    prob: 100,
    bind: None,
    delay: 0.0,
    delay_unit: DelayUnit::Ms,
    delay_prob: 100,
    repeats: 1,
    repeat_prob: 100,
};

fn n(note: u8, velocity: u8) -> StepParam {
    StepParam { active: true, note, velocity, ..REST }
}

fn p(note: u8, velocity: u8, prob: u8) -> StepParam {
    StepParam { prob, ..n(note, velocity) }
}

fn track(steps: Vec<StepParam>, len: usize) -> TrackParam {
    TrackParam {
        steps,
        length: Some(len),
        pulses: None,
        rotation: None,
        muted: false,
        mode: TrackMode::Note,
        cycle: CycleMode::Forward,
        scale: None,
        scale_cents: vec![],
        scale_period: None,
        root: None,
        active_slot: 0,
        slots: vec![],
        swing: 50,
        groove: None,
        humanize: 2.0,
        ratchet_decay: 0,
    }
}

fn steps16(notes: &[(usize, StepParam)]) -> Vec<StepParam> {
    let mut v = vec![REST; 16];
    for (i, s) in notes {
        v[*i] = s.clone();
    }
    v
}

fn main() -> anyhow::Result<()> {
    state::ensure_dirs()?;

    // ── the sequencer: the fugue ───────────────────────────────────────
    // E♭ minor. MIDI: Bb1=34 Db2=37 Eb2=39 Bb2=46 Db3=49 Eb3=51 F3=53
    // Ab3=56 Bb3=58 C4=60 Db4=61 Eb4=63 F4=65 Gb4=66

    // t1 — the SUBJECT (the chant, slowed into liturgy)
    let subject = steps16(&[
        (0, n(63, 95)),
        (2, n(61, 78)),
        (4, n(63, 85)),
        (6, n(65, 92)),
        (8, n(63, 80)),
        (10, n(61, 72)),
        (12, n(58, 88)),
        (13, n(61, 64)), // the chant's quick double-hit tail
        (14, n(58, 74)),
    ]);
    // development: the chorus line itself — "keep on dancing till the
    // world ends", 5–5–4–♭3–♭3–2–1, stepping home to E♭
    let subject_dev = steps16(&[
        (0, n(70, 92)),  // Bb4  "keep"
        (2, n(70, 76)),  //      "on"
        (4, n(68, 84)),  // Ab4  "dan-"
        (6, n(66, 88)),  // Gb4  "-cing"
        (8, n(66, 72)),  //      "till the"
        (10, n(65, 68)), // F4   "world"
        (12, n(63, 95)), // Eb4  "ends"
        (14, p(63, 60, 65)),
    ]);
    // the fragment, for the unwinding
    let fragment = steps16(&[(0, p(63, 55, 90)), (2, p(61, 45, 80)), (4, p(58, 50, 85))]);
    // the ghost, for the end of the world
    let ghost = steps16(&[(0, n(63, 30))]);
    let mut t1 = track(subject, 16);
    t1.muted = true; // the exposition opens on the pedal alone
    t1.slots = vec![
        SlotParam { slot: 1, steps: subject_dev, length: Some(16), pulses: None, rotation: None },
        SlotParam { slot: 2, steps: fragment, length: Some(16), pulses: None, rotation: None },
        SlotParam { slot: 3, steps: ghost, length: Some(16), pulses: None, rotation: None },
    ];

    // t2 — the drunk modulation track strolling the filterbank window
    let mut t2 = track(
        [0.2, 0.55, 0.35, 0.7, 0.45, 0.25, 0.6, 0.4]
            .iter()
            .map(|m| StepParam { active: true, mod_value: *m, ..REST })
            .collect(),
        8,
    );
    t2.mode = TrackMode::Modulation;
    t2.cycle = CycleMode::Drunk;
    t2.humanize = 0.0;

    // t3 — the pedal: E♭ held against everything. Two weights: the
    // full pedal, and a whisper for the exposition and the outro — the
    // pedal is a voice in the drama, not a constant.
    let mut t3 =
        track(steps16(&[(0, n(39, 100)), (8, p(34, 62, 70)), (12, p(37, 50, 45))]), 16);
    t3.humanize = 1.5;
    t3.active_slot = 1; // the piece opens on the whisper
    t3.slots = vec![SlotParam {
        slot: 1,
        steps: steps16(&[(0, n(39, 52)), (8, p(34, 34, 60))]),
        length: Some(16),
        pulses: None,
        rotation: None,
    }];

    // t4 — silence (placeholder so t5/t7 keep their labels)
    let mut t4 = track(vec![REST; 16], 16);
    t4.muted = true;

    // t5 — the ANSWER, a fifth below the subject
    let mut t5 = track(
        steps16(&[
            (0, n(58, 85)),
            (2, n(56, 70)),
            (4, n(58, 78)),
            (6, n(60, 84)),
            (8, n(58, 74)),
            (10, n(56, 66)),
            (12, n(53, 80)),
        ]),
        16,
    );
    t5.muted = true;

    // t6 — silence
    let mut t6 = track(vec![REST; 16], 16);
    t6.muted = true;

    // t7 — the tenor: the subject in AUGMENTATION, 32 steps against 16
    let mut aug = vec![REST; 32];
    for (i, note, vel) in [
        (0usize, 51u8, 70u8),
        (4, 49, 62),
        (8, 51, 66),
        (12, 53, 72),
        (16, 51, 64),
        (20, 49, 58),
        (24, 46, 68),
    ] {
        aug[i] = n(note, vel);
    }
    let mut t7 = track(aug, 32);
    t7.muted = true;
    t7.humanize = 3.0;

    // ── the form: entries staged like a fugue ──────────────────────────
    let mac =
        |id: &str, cmds: Vec<MacroCmd>| MacroParam { id: id.into(), quant: Quant::Bar, cmds };
    let macros = vec![
        // a — exposition opens: the pedal alone, whispering
        mac("a", vec![
            MacroCmd::SetMute { track: 0, muted: true },
            MacroCmd::SetMute { track: 4, muted: true },
            MacroCmd::SetMute { track: 6, muted: true },
            MacroCmd::SetMute { track: 2, muted: false },
            MacroCmd::SwitchPattern { track: 2, slot: 1 },
            MacroCmd::SetBpm { bpm: 46.0 },
        ]),
        // b — the subject enters
        mac("b", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 0 },
            MacroCmd::SetMute { track: 0, muted: false },
        ]),
        // c — the answer, a fifth below; the full pedal arrives with it
        mac("c", vec![
            MacroCmd::SetMute { track: 4, muted: false },
            MacroCmd::SwitchPattern { track: 2, slot: 0 },
        ]),
        // d — the tenor in augmentation: the texture is full
        mac("d", vec![MacroCmd::SetMute { track: 6, muted: false }]),
        // e — development: subject varied, answer in retrograde motion
        mac("e", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 1 },
            MacroCmd::SetCycle { track: 4, mode: CycleMode::Reverse },
            MacroCmd::SetBpm { bpm: 50.0 },
        ]),
        // f — climax: everything leaning forward
        mac("f", vec![
            MacroCmd::SetCycle { track: 6, mode: CycleMode::PingPong },
            MacroCmd::SetBpm { bpm: 52.0 },
        ]),
        // g — unwinding: voices leave, the fragment remains
        mac("g", vec![
            MacroCmd::SetMute { track: 4, muted: true },
            MacroCmd::SetMute { track: 6, muted: true },
            MacroCmd::SwitchPattern { track: 0, slot: 2 },
            MacroCmd::SwitchPattern { track: 2, slot: 1 },
            MacroCmd::SetCycle { track: 4, mode: CycleMode::Forward },
            MacroCmd::SetBpm { bpm: 42.0 },
        ]),
        // h — the ghost and the pedal, till the world ends
        mac("h", vec![
            MacroCmd::SwitchPattern { track: 0, slot: 3 },
            MacroCmd::SetBpm { bpm: 38.0 },
        ]),
    ];
    let mut lane = vec![String::new(); 64];
    for (bar, m) in
        [(0, "a"), (4, "b"), (12, "c"), (20, "d"), (28, "e"), (40, "f"), (48, "g"), (56, "h")]
    {
        lane[bar] = m.to_string();
    }

    let seq = state::SequencerParams {
        bpm_src: None,
        bpm: Some(46.0),
        playing: Some(true),
        euclidean_pulses: None,
        euclidean_length: None,
        euclidean_rotation: None,
        steps: vec![],
        tracks: vec![t1, t2, t3, t4, t5, t6, t7],
        macros,
        lane,
        lane_len: Some(64),
    };
    state::save_module_state("sequencer", 0, &seq)?;

    // ── voices: four registers of the same dark organ ──────────────────
    let voice = |shape: f32, sub: f32, lpg: f32, amp: &str, notes: &str| state::VoiceParams {
        format: state::STATE_FORMAT,
        shape: Some(shape),
        sub: Some(sub),
        lpg: Some(lpg),
        amp_src: Some(amp.into()),
        notes_src: Some(notes.into()),
        ..Default::default()
    };
    state::save_module_state(
        "voice", 0, &voice(0.55, 0.15, 0.6, "envelope/0/ch1", "sequencer/0/t1"),
    )?;
    state::save_module_state(
        "voice", 1, &voice(0.08, 0.9, 0.25, "envelope/0/ch3", "sequencer/0/t3"),
    )?;
    state::save_module_state(
        "voice", 2, &voice(0.42, 0.2, 0.5, "envelope/0/ch5", "sequencer/0/t5"),
    )?;
    state::save_module_state(
        "voice", 3, &voice(0.3, 0.35, 0.4, "envelope/2/ch1", "sequencer/0/t7"),
    )?;

    // ── envelopes: six amp channels on MATHs 0 (voice 2 rides ch5);
    // the tenor swells on MATHs 2 ch1 ──────────────────────────────────
    let ch = |rise: f32, fall: f32, trig: &str| EnvelopeChannelParams {
        rise,
        fall,
        trigger_src: Some(trig.into()),
        ..Default::default()
    };
    state::save_module_state(
        "envelope",
        0,
        &state::EnvelopeParams {
            format: state::STATE_FORMAT,
            channels: vec![
                ch(0.28, 0.7, "sequencer/0/t1"),
                ch(0.5, 0.5, "sequencer/0/t2"),
                ch(0.2, 0.85, "sequencer/0/t3"),
                ch(0.5, 0.5, "sequencer/0/t4"),
                ch(0.3, 0.68, "sequencer/0/t5"),
                ch(0.5, 0.5, "sequencer/0/t6"),
            ],
            logic_outputs: Default::default(),
        },
    )?;
    state::save_module_state(
        "envelope",
        2,
        &state::EnvelopeParams {
            format: state::STATE_FORMAT,
            channels: vec![
                ch(0.45, 0.85, "sequencer/0/t7"),
                EnvelopeChannelParams::default(),
                EnvelopeChannelParams::default(),
                EnvelopeChannelParams::default(),
            ],
            logic_outputs: Default::default(),
        },
    )?;

    // ── delays: the send echo goes glacial; a SECOND delay rides the
    // tenor as an INSERT (its strip leaves the console; the delay's
    // return carries voice and echoes together) ────────────────────────
    let levels = [0.8_f32, 0.45, 0.6, 0.35, 0.5, 0.3, 0.4, 0.7];
    let pans = [-0.2_f32, 0.25, -0.3, 0.35, -0.4, 0.45, -0.5, 0.55];
    state::save_module_state(
        "delay",
        0,
        &state::DelayParams {
            format: state::STATE_FORMAT,
            time: Some(0.24),
            regen: Some(0.6),
            shim: Some(0.0),
            wash: Some(0.22),
            dry: Some(0.0),
            taps: Some(8),
            input: Some("send/0".into()),
            tap: (0..8)
                .map(|i| DelayTapParam {
                    level: levels[i],
                    pan: pans[i],
                    phase: "+".into(),
                    pan_src: None,
                    level_src: None,
                })
                .collect(),
            ..Default::default()
        },
    )?;
    state::save_module_state(
        "delay",
        1,
        &state::DelayParams {
            format: state::STATE_FORMAT,
            time: Some(0.18),
            regen: Some(0.5),
            shim: Some(0.0),
            wash: Some(0.1),
            dry: Some(0.6), // insert: the tenor stays present under its echoes
            taps: Some(5),
            input: Some("voice/3".into()),
            ..Default::default()
        },
    )?;

    // ── the filterbank: tilted into the dark, smeared in time ──────────
    state::save_module_state(
        "filterbank",
        0,
        &state::FilterbankParams {
            format: state::STATE_FORMAT,
            bank_a: vec![
                1.0, 0.95, 0.9, 0.85, 0.8, 0.7, 0.6, 0.5, 0.42, 0.35, 0.28, 0.22, 0.18, 0.12,
                0.08, 0.05,
            ],
            morph: Some(0.0),
            wwidth: Some(0.4),
            split: Some(0.5),
            spread: Some(0.22),
            dry: Some(0.0),
            decay: Some(0.62),
            input: Some("send/1".into()),
            morph_src: Some("envelope/1/ch1".into()),
            wcent_src: Some("sequencer/0/t2".into()),
            ..Default::default()
        },
    )?;

    // ── the tape: armed over the whole rendition ───────────────────────
    state::save_module_state(
        "tape",
        0,
        &state::TapeParams {
            format: state::STATE_FORMAT,
            speed: Some(1.0),
            loop_on: Some(false),
            loop_in: Some(0),
            loop_out: Some(0),
            speed_src: None,
            tracks: (0..los::tape::TRACKS)
                .map(|i| TapeTrackParam {
                    input: None,
                    fader: 0.8,
                    pan: 0.0,
                    armed: i == 0,
                    muted: false,
                    reversed: false,
                    monitor: true,
                    fader_src: None,
                    pan_src: None,
                    auto: vec![],
                })
                .collect(),
        },
    )?;

    println!("worldends: patch states written to ~/.config/los/tmp/");
    println!("  live rig: SIGUSR2 the modules, `los add voice 2|voice 3|delay 1`");
    println!("  then `r` on the TAPE — ~5.6 minutes till the world ends");
    Ok(())
}
