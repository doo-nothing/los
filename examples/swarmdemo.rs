//! swarmdemo — a tiny patch showing off the swarm voice.
//!
//! Four open-fifth chords at 34 BPM (C — Ab — Bb — G, the Blade Runner
//! changes), each held for seven steps and released for one: that gap
//! is the point — the swarm's filter swell relaxes in the rest and
//! blooms again on the next chord. A drunk mod track strolls the
//! cutoff underneath the swell so no two passes match.
//!
//! It only writes module state files:
//!     cargo run --example swarmdemo
//! Then, in a fresh session: SIGUSR2 sequencer 0 / envelope 0 / the
//! voices, and `los add swarm 0`. The house voices are muted by this
//! patch — the swarm is the whole show.

use los::state::{self, CycleMode, DelayUnit, EnvelopeChannelParams, StepParam, TrackMode, TrackParam};

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

fn track(steps: Vec<StepParam>, len: usize, mode: TrackMode, cycle: CycleMode) -> TrackParam {
    TrackParam {
        steps,
        length: Some(len),
        pulses: None,
        rotation: None,
        muted: false,
        mode,
        cycle,
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

fn main() -> anyhow::Result<()> {
    // ── the chord track: roots held 7 steps, released 1 ────────────────
    // Same note repeated on consecutive steps = a held gate (the off/on
    // flip at each boundary lands inside one audio block); the rest step
    // is what lets the swell breathe.
    let hold = |root: u8| -> Vec<StepParam> {
        (0..8)
            .map(|i| {
                if i < 7 {
                    StepParam {
                        active: true,
                        note: root,
                        velocity: 92,
                        ..REST
                    }
                } else {
                    REST
                }
            })
            .collect()
    };
    let mut chord_steps = Vec::new();
    for root in [36u8, 32, 34, 31] {
        // C2 Ab1 Bb1 G1
        chord_steps.extend(hold(root));
    }
    let t1 = track(chord_steps, 32, TrackMode::Note, CycleMode::Forward);

    // ── a drunk stroll for the cutoff ──────────────────────────────────
    let t2 = track(
        (0..16)
            .map(|i| StepParam {
                active: true,
                mod_value: 0.3 + 0.05 * (i % 7) as f32, // 0.30..0.60
                ..REST
            })
            .collect(),
        16,
        TrackMode::Modulation,
        CycleMode::Drunk,
    );

    let seq = state::SequencerParams {
        bpm: Some(34.0),
        playing: Some(true),
        euclidean_pulses: None,
        euclidean_length: None,
        euclidean_rotation: None,
        steps: vec![],
        tracks: vec![t1, t2],
        macros: vec![],
        lane: vec![],
        lane_len: None,
    };
    state::save_module_state("sequencer", 0, &seq)?;

    // ── one envelope channel rings the pad across the rest steps ──────
    state::save_module_state(
        "envelope",
        0,
        &state::EnvelopeParams {
            format: state::STATE_FORMAT,
            channels: vec![EnvelopeChannelParams {
                rise: 0.5,
                fall: 1.6,
                trigger_src: Some("sequencer/0/t1".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
    )?;

    // ── the swarm itself ───────────────────────────────────────────────
    state::save_module_state(
        "swarm",
        0,
        &state::SwarmParams {
            format: state::STATE_FORMAT,
            chord: Some("5th".into()),
            detune: Some(0.45),
            cutoff: Some(0.5), // replaced by the drunk track below
            res: Some(0.35),
            swell: Some(0.7),
            glide: Some(0.2),
            level: Some(0.85),
            cutoff_src: Some("sequencer/0/t2".into()),
            amp_src: Some("envelope/0/ch1".into()),
            notes_src: Some("sequencer/0/t1".into()),
            ..Default::default()
        },
    )?;

    // ── mute the house voices: the swarm is the whole show ─────────────
    // The voice rule: a bound-but-unresolvable amp source means SILENCE
    // (level isn't a voice UI row, and an unbound amp is a drone — the
    // one thing a mute must never be). envelope/7 doesn't exist.
    for i in 0..2 {
        state::save_module_state(
            "voice",
            i,
            &state::VoiceParams {
                format: state::STATE_FORMAT,
                amp_src: Some("envelope/7/ch1".into()),
                notes_src: Some("sequencer/0/t8".into()),
                ..Default::default()
            },
        )?;
    }

    println!("swarmdemo written: sequencer 0, envelope 0, swarm 0, voices muted");
    println!("in a session: kill+re-add sequencer 0 / envelope 0, `los add swarm`, voices reload via save/load");
    Ok(())
}
