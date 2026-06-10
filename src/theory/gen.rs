//! Pattern generators behind the sequencer's auto-fill commands.
//!
//! Everything here is deterministic given a seed (see
//! [`crate::theory::rng::Rng`]): the same fill command with the same seed
//! always produces the same pattern, which keeps regenerate/undo musical.
//! Generators operate only on the slice they're handed — pattern length is
//! the slice length — and never panic on empty input.
//!
//! - [`mutate`] — small evolutionary nudges, for "almost the same but alive"
//! - [`density_fill`] — fill to a target density with downbeat weighting
//! - [`markov`] — learn trigger/note/velocity habits from other tracks
//! - [`lsystem`] — deterministic fractal rhythm masks ([`LRule`])

use crate::theory::rng::Rng;

/// One step as the generators see it. Mirrors the sequencer's step fields.
///
/// `note` is whatever pitch ordinal the sequencer uses (MIDI note or scale
/// degree) — generators only ever nudge or copy it, never interpret it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenStep {
    /// Whether this step triggers.
    pub active: bool,
    /// MIDI note or scale degree — generators treat it as an ordinal pitch.
    pub note: u8,
    /// Velocity, 0–127.
    pub velocity: u8,
    /// Trigger probability, 0–100.
    pub prob: u8,
}

impl Default for GenStep {
    fn default() -> Self {
        Self {
            active: false,
            note: 60,
            velocity: 100,
            prob: 100,
        }
    }
}

/// Never let [`mutate`] push trigger density above this fraction of the
/// pattern — repeated mutation should drift, not congeal into a wall of hits.
const MUTATE_DENSITY_CAP: f32 = 0.85;

/// Small evolutionary nudge to an existing pattern.
///
/// Musical intent: "play it again, slightly different". `intensity` (0..1)
/// scales how many nudges happen — roughly `intensity * len`, with a minimum
/// of 1 whenever `intensity > 0`. Each nudge is one of, chosen at random:
///
/// - flip one trigger on/off
/// - move one active step's note by ±1..=2 semitone-ordinals, clamped to
///   `[median - pitch_span, median + pitch_span]` around the pattern's
///   median active note (60 if nothing is active)
/// - swap two steps wholesale
/// - nudge a velocity by ±5..=20 (clamped to 1..=127)
/// - nudge a trigger probability by ±5..=25 (clamped to 0..=100)
///
/// Repeated calls drift rather than explode: a trigger-flip that would push
/// density above 85% turns an existing trigger off instead.
pub fn mutate(steps: &mut [GenStep], intensity: f32, pitch_span: u8, seed: u64) {
    if steps.is_empty() {
        return;
    }
    let intensity = intensity.clamp(0.0, 1.0);
    if intensity <= 0.0 {
        return;
    }
    let len = steps.len();
    let mut rng = Rng::new(seed);
    let count = ((intensity * len as f32).round() as usize).max(1);

    // Pitch bounds are anchored to the median active note at call time, so
    // a melody wanders around its own register instead of a fixed C4.
    let base = median_active_note(steps).unwrap_or(60);
    let lo = base.saturating_sub(pitch_span);
    let hi = base.saturating_add(pitch_span).min(127);

    for _ in 0..count {
        match rng.below(5) {
            0 => flip_trigger(steps, &mut rng),
            1 => nudge_note(steps, lo, hi, &mut rng),
            2 => {
                let i = rng.below(len as u64) as usize;
                let j = rng.below(len as u64) as usize;
                steps.swap(i, j);
            }
            3 => {
                let i = rng.below(len as u64) as usize;
                let delta = signed(5 + rng.below(16) as i16, &mut rng);
                steps[i].velocity =
                    (i16::from(steps[i].velocity) + delta).clamp(1, 127) as u8;
            }
            // `below(5) < 5`, so this arm is exactly kind 4.
            _ => {
                let i = rng.below(len as u64) as usize;
                let delta = signed(5 + rng.below(21) as i16, &mut rng);
                steps[i].prob = (i16::from(steps[i].prob) + delta).clamp(0, 100) as u8;
            }
        }
    }
}

/// Flip one trigger, refusing to exceed the density cap: when turning a step
/// on would push density past 85%, turn a random active step off instead.
fn flip_trigger(steps: &mut [GenStep], rng: &mut Rng) {
    let len = steps.len();
    let i = rng.below(len as u64) as usize;
    if steps[i].active {
        steps[i].active = false;
        return;
    }
    let active = steps.iter().filter(|s| s.active).count();
    if (active + 1) as f32 > MUTATE_DENSITY_CAP * len as f32 {
        let actives: Vec<usize> = (0..len).filter(|&k| steps[k].active).collect();
        if let Some(&j) = rng.pick(&actives) {
            steps[j].active = false;
        }
    } else {
        steps[i].active = true;
    }
}

/// Move one active step's note by ±1..=2, clamped to `[lo, hi]`.
fn nudge_note(steps: &mut [GenStep], lo: u8, hi: u8, rng: &mut Rng) {
    let actives: Vec<usize> = (0..steps.len()).filter(|&k| steps[k].active).collect();
    if let Some(&i) = rng.pick(&actives) {
        let delta = signed(1 + rng.below(2) as i16, &mut *rng);
        steps[i].note =
            (i16::from(steps[i].note) + delta).clamp(i16::from(lo), i16::from(hi)) as u8;
    }
}

/// Attach a random sign to a magnitude.
fn signed(magnitude: i16, rng: &mut Rng) -> i16 {
    if rng.chance(0.5) {
        magnitude
    } else {
        -magnitude
    }
}

/// Median of the active steps' notes, or `None` if nothing is active.
fn median_active_note(steps: &[GenStep]) -> Option<u8> {
    let mut notes: Vec<u8> = steps.iter().filter(|s| s.active).map(|s| s.note).collect();
    if notes.is_empty() {
        return None;
    }
    notes.sort_unstable();
    notes.get(notes.len() / 2).copied()
}

/// Clear all triggers, then re-fill to `round(density * len)` of them with
/// downbeat weighting.
///
/// Musical intent: a quick groove scaffold — beat 1 is heavily favored
/// (weight 4), quarter positions next (weight 3 where `i % 4 == 0`), then
/// eighths (weight 2 on even positions), then offbeats (weight 1). Sampling
/// is weighted and without replacement, so the trigger count is exact.
/// Note/velocity/probability of every step are left untouched: re-filling a
/// pattern doesn't destroy its melodic content. `density` is clamped to 0..=1.
pub fn density_fill(steps: &mut [GenStep], density: f32, seed: u64) {
    if steps.is_empty() {
        return;
    }
    let density = density.clamp(0.0, 1.0);
    let len = steps.len();
    let target = (density * len as f32).round() as usize;

    for s in steps.iter_mut() {
        s.active = false;
    }

    let mut rng = Rng::new(seed);
    let mut pool: Vec<(usize, u64)> = (0..len).map(|i| (i, downbeat_weight(i))).collect();
    for _ in 0..target {
        let total: u64 = pool.iter().map(|&(_, w)| w).sum();
        if total == 0 {
            break;
        }
        let mut roll = rng.below(total);
        let mut chosen = 0;
        for (k, &(_, w)) in pool.iter().enumerate() {
            if roll < w {
                chosen = k;
                break;
            }
            roll -= w;
        }
        let (idx, _) = pool.swap_remove(chosen);
        steps[idx].active = true;
    }
}

/// Downbeat weighting for [`density_fill`]: 4 on position 0, 3 on quarter
/// positions, 2 on even positions, 1 on offbeats.
fn downbeat_weight(i: usize) -> u64 {
    if i == 0 {
        4
    } else if i.is_multiple_of(4) {
        3
    } else if i.is_multiple_of(2) {
        2
    } else {
        1
    }
}

/// Generate `target` by imitating the habits of `sources` (other tracks).
///
/// Musical intent: "play something that belongs in this song". Three things
/// are learned from the sources:
///
/// 1. a first-order transition table over trigger states (on/off → on/off),
///    gathered from each source's consecutive step pairs,
/// 2. a note transition table over the active steps' notes (in pattern
///    order), and
/// 3. the mean and spread (standard deviation) of active velocities.
///
/// The target's trigger chain is walked from an initial state drawn from the
/// sources' overall density; each active step samples its note from the note
/// chain (falling back to a uniform pick over all seen notes when the
/// previous note was terminal), and its velocity from `mean ± spread`.
/// Generated active steps get `prob = 100`.
///
/// When the sources are empty or contain no active steps the generator falls
/// back to note 60, density 0.5, and velocity 100 — it never panics.
pub fn markov(target: &mut [GenStep], sources: &[Vec<GenStep>], seed: u64) {
    if target.is_empty() {
        return;
    }
    let mut rng = Rng::new(seed);

    // Learn: trigger transitions, note transitions, velocity stats.
    let mut trig = [[0u64; 2]; 2];
    let mut note_next: std::collections::BTreeMap<u8, Vec<u8>> =
        std::collections::BTreeMap::new();
    let mut notes_seen: Vec<u8> = Vec::new();
    let mut vels: Vec<f32> = Vec::new();
    let mut active_total = 0usize;
    let mut step_total = 0usize;
    for src in sources {
        step_total += src.len();
        for pair in src.windows(2) {
            trig[usize::from(pair[0].active)][usize::from(pair[1].active)] += 1;
        }
        let mut prev: Option<u8> = None;
        for s in src.iter().filter(|s| s.active) {
            active_total += 1;
            notes_seen.push(s.note);
            vels.push(f32::from(s.velocity));
            if let Some(p) = prev {
                note_next.entry(p).or_default().push(s.note);
            }
            prev = Some(s.note);
        }
    }

    // Fallbacks: sources empty or silent → density 0.5, note 60, vel 100,
    // and no learned trigger table (it would only ever say "stay off").
    let density = if active_total == 0 {
        trig = [[0; 2]; 2];
        0.5
    } else {
        active_total as f32 / step_total as f32
    };
    let (vel_mean, vel_spread) = if vels.is_empty() {
        (100.0, 0.0)
    } else {
        let mean = vels.iter().sum::<f32>() / vels.len() as f32;
        let var = vels.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / vels.len() as f32;
        (mean, var.sqrt())
    };

    // Generate: walk the trigger chain, sampling notes along the note chain.
    let mut state = rng.chance(density);
    let mut prev_note: Option<u8> = None;
    for step in target.iter_mut() {
        step.active = state;
        if state {
            let note = prev_note
                .and_then(|p| note_next.get(&p).and_then(|nexts| rng.pick(nexts).copied()))
                .or_else(|| rng.pick(&notes_seen).copied())
                .unwrap_or(60);
            step.note = note;
            prev_note = Some(note);
            let vel = vel_mean + (rng.f32() * 2.0 - 1.0) * vel_spread;
            step.velocity = (vel.round() as i32).clamp(1, 127) as u8;
            step.prob = 100;
        }
        let row = trig[usize::from(state)];
        let seen = row[0] + row[1];
        let p_on = if seen == 0 {
            density
        } else {
            row[1] as f32 / seen as f32
        };
        state = rng.chance(p_on);
    }
}

/// Which self-similar rhythm mask [`lsystem`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LRule {
    /// Cantor set rewriting: A→ABA, B→BBB from A (A = on). Sparse, gapped,
    /// symmetric — silence grows in the middle of every phrase.
    Cantor,
    /// Thue–Morse sequence: on where `popcount(n)` is even. Dense but never
    /// settles into a repeating bar — the classic "fair coin that never
    /// streaks" rhythm.
    ThueMorse,
    /// Fibonacci word (0→01, 1→0 from 0; on where 0). Quasi-periodic — feels
    /// almost like a clave but never quite loops.
    Fibonacci,
    /// Fibbinary mask (no two adjacent 1-bits in the step index): on where
    /// `(i & (i >> 1)) == 0`. A Sierpinski-flavored self-similar mask — not
    /// the rule-90 triangle itself, but the same family of bit-arithmetic
    /// self-similarity.
    Sierpinski,
}

/// Apply a deterministic fractal rhythm mask to the pattern's triggers.
///
/// Musical intent: rigid, self-similar rhythms that loop with character —
/// the opposite end of the spectrum from [`mutate`]. Only `active` is
/// touched; notes, velocities, and probabilities are preserved, so masks can
/// be layered over existing melodic content. All four rules are fully
/// deterministic; `_seed` exists only to keep the generator call signature
/// uniform.
pub fn lsystem(steps: &mut [GenStep], rule: LRule, _seed: u64) {
    if steps.is_empty() {
        return;
    }
    let len = steps.len();
    let mask: Vec<bool> = match rule {
        LRule::Cantor => cantor_mask(len),
        LRule::ThueMorse => (0..len).map(|i| i.count_ones() % 2 == 0).collect(),
        LRule::Fibonacci => fibonacci_mask(len),
        LRule::Sierpinski => (0..len).map(|i| (i & (i >> 1)) == 0).collect(),
    };
    for (step, on) in steps.iter_mut().zip(mask) {
        step.active = on;
    }
}

/// First `len` symbols of the first Cantor rewriting generation with at
/// least `len` symbols (A→ABA, B→BBB from A; `true` = A = on).
fn cantor_mask(len: usize) -> Vec<bool> {
    let mut word = vec![true];
    while word.len() < len {
        word = word
            .iter()
            .flat_map(|&a| if a { [true, false, true] } else { [false; 3] })
            .collect();
    }
    word.truncate(len);
    word
}

/// First `len` symbols of the Fibonacci word (0→01, 1→0 from 0;
/// `true` = 0 = on).
fn fibonacci_mask(len: usize) -> Vec<bool> {
    let mut word = vec![true];
    while word.len() < len {
        word = word
            .iter()
            .flat_map(|&zero| if zero { vec![true, false] } else { vec![true] })
            .collect();
    }
    word.truncate(len);
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-step pattern with varied content: triggers on quarters, rising
    /// notes, varied velocities and probabilities.
    fn pattern16() -> Vec<GenStep> {
        (0..16)
            .map(|i| GenStep {
                active: i % 4 == 0,
                note: 60 + (i as u8 % 7),
                velocity: 60 + 4 * (i as u8),
                prob: 50 + 3 * (i as u8),
            })
            .collect()
    }

    fn active_count(steps: &[GenStep]) -> usize {
        steps.iter().filter(|s| s.active).count()
    }

    #[test]
    fn genstep_default() {
        let s = GenStep::default();
        assert!(!s.active);
        assert_eq!(s.note, 60);
        assert_eq!(s.velocity, 100);
        assert_eq!(s.prob, 100);
    }

    // ----- mutate -----

    #[test]
    fn mutate_empty_no_panic() {
        let mut steps: Vec<GenStep> = vec![];
        mutate(&mut steps, 1.0, 12, 7);
        assert!(steps.is_empty());
    }

    #[test]
    fn mutate_intensity_zero_is_noop() {
        let mut steps = pattern16();
        let before = steps.clone();
        mutate(&mut steps, 0.0, 12, 7);
        assert_eq!(steps, before);
        mutate(&mut steps, -3.0, 12, 7);
        assert_eq!(steps, before);
    }

    #[test]
    fn mutate_deterministic() {
        let mut a = pattern16();
        let mut b = pattern16();
        mutate(&mut a, 0.8, 7, 12345);
        mutate(&mut b, 0.8, 7, 12345);
        assert_eq!(a, b);
    }

    #[test]
    fn mutate_seeds_differ() {
        let mut a = pattern16();
        let mut b = pattern16();
        mutate(&mut a, 1.0, 7, 1);
        mutate(&mut b, 1.0, 7, 2);
        assert_ne!(a, b);
    }

    #[test]
    fn mutate_changes_something_at_full_intensity() {
        let mut steps = pattern16();
        let before = steps.clone();
        mutate(&mut steps, 1.0, 7, 99);
        assert_ne!(steps, before);
    }

    #[test]
    fn mutate_tiny_intensity_no_panic() {
        // Rounds to zero mutations but the minimum-1 rule still applies.
        let mut steps = pattern16();
        mutate(&mut steps, 0.01, 7, 3);
        assert_eq!(steps.len(), 16);
    }

    #[test]
    fn mutate_respects_clamps() {
        // All active, all note 60: the median is 60 and span 3 bounds every
        // nudged note to 57..=63 within a single call.
        for seed in 0..50 {
            let mut steps: Vec<GenStep> = (0..16)
                .map(|_| GenStep {
                    active: true,
                    note: 60,
                    velocity: 3,
                    prob: 2,
                })
                .collect();
            mutate(&mut steps, 1.0, 3, seed);
            for s in &steps {
                assert!((57..=63).contains(&s.note), "note {} out of span", s.note);
                assert!((1..=127).contains(&s.velocity), "vel {} clamped wrong", s.velocity);
                assert!(s.prob <= 100, "prob {} out of range", s.prob);
            }
        }
    }

    #[test]
    fn mutate_density_cap() {
        // From a full 16-step pattern, repeated mutation must fall to and
        // never re-exceed floor(0.85 * 16) = 13 active steps.
        let mut steps: Vec<GenStep> = (0..16)
            .map(|_| GenStep {
                active: true,
                ..GenStep::default()
            })
            .collect();
        let mut max_after_settle = 0;
        for seed in 0..300 {
            mutate(&mut steps, 1.0, 7, seed);
            if seed >= 150 {
                max_after_settle = max_after_settle.max(active_count(&steps));
            }
        }
        assert!(
            max_after_settle <= 13,
            "density cap violated: {max_after_settle}/16 active"
        );

        // Starting exactly at the cap, no single call may exceed it.
        for seed in 0..200 {
            let mut at_cap: Vec<GenStep> = (0..16)
                .map(|i| GenStep {
                    active: i < 13,
                    ..GenStep::default()
                })
                .collect();
            mutate(&mut at_cap, 1.0, 7, seed);
            assert!(active_count(&at_cap) <= 13, "seed {seed} exceeded cap");
        }
    }

    // ----- density_fill -----

    #[test]
    fn density_fill_exact_counts() {
        for (density, len, expect) in [
            (0.5, 16, 8),
            (0.25, 16, 4),
            (0.75, 8, 6),
            (0.3, 10, 3),
            (0.2, 10, 2),
            (0.0, 16, 0),
            (1.0, 16, 16),
        ] {
            let mut steps = vec![GenStep::default(); len];
            density_fill(&mut steps, density, 42);
            assert_eq!(
                active_count(&steps),
                expect,
                "density {density} on len {len}"
            );
        }
    }

    #[test]
    fn density_fill_clamps_density() {
        let mut steps = vec![GenStep::default(); 16];
        density_fill(&mut steps, 2.0, 1);
        assert_eq!(active_count(&steps), 16);
        density_fill(&mut steps, -1.0, 1);
        assert_eq!(active_count(&steps), 0);
    }

    #[test]
    fn density_fill_empty_no_panic() {
        let mut steps: Vec<GenStep> = vec![];
        density_fill(&mut steps, 0.5, 1);
        assert!(steps.is_empty());
    }

    #[test]
    fn density_fill_clears_previous_triggers() {
        let mut steps: Vec<GenStep> = (0..16)
            .map(|_| GenStep {
                active: true,
                ..GenStep::default()
            })
            .collect();
        density_fill(&mut steps, 0.25, 9);
        assert_eq!(active_count(&steps), 4);
    }

    #[test]
    fn density_fill_preserves_melodic_content() {
        let mut steps = pattern16();
        let before = steps.clone();
        density_fill(&mut steps, 0.5, 17);
        for (after, orig) in steps.iter().zip(&before) {
            assert_eq!(after.note, orig.note);
            assert_eq!(after.velocity, orig.velocity);
            assert_eq!(after.prob, orig.prob);
        }
    }

    #[test]
    fn density_fill_deterministic() {
        let mut a = vec![GenStep::default(); 16];
        let mut b = vec![GenStep::default(); 16];
        density_fill(&mut a, 0.5, 777);
        density_fill(&mut b, 0.5, 777);
        assert_eq!(a, b);
        let mut c = vec![GenStep::default(); 16];
        density_fill(&mut c, 0.5, 778);
        // Different seeds should (for these seeds) place triggers elsewhere.
        assert_ne!(a, c);
    }

    #[test]
    fn density_fill_favors_downbeat() {
        // With weights 4/3/2/1 and 4 picks from 16 steps, position 0 lands
        // in roughly half of all fills while a given odd position lands in
        // ~15%. Assert that strong separation statistically.
        let trials = 400;
        let mut pos0_hits = 0usize;
        let mut odd_hits = 0usize; // across all 8 odd positions
        for seed in 0..trials {
            let mut steps = vec![GenStep::default(); 16];
            density_fill(&mut steps, 0.25, seed);
            if steps[0].active {
                pos0_hits += 1;
            }
            odd_hits += steps
                .iter()
                .enumerate()
                .filter(|(i, s)| i % 2 == 1 && s.active)
                .count();
        }
        let pos0_rate = pos0_hits as f32 / trials as f32;
        let odd_rate = odd_hits as f32 / (trials * 8) as f32;
        assert!(pos0_rate > 0.35, "position 0 rate too low: {pos0_rate}");
        assert!(
            pos0_rate > 2.0 * odd_rate,
            "position 0 ({pos0_rate}) not strongly favored over odd ({odd_rate})"
        );
    }

    // ----- markov -----

    #[test]
    fn markov_empty_target_no_panic() {
        let mut target: Vec<GenStep> = vec![];
        markov(&mut target, &[pattern16()], 1);
        assert!(target.is_empty());
    }

    #[test]
    fn markov_empty_sources_falls_back() {
        let mut target = vec![GenStep::default(); 16];
        markov(&mut target, &[], 5);
        let actives: Vec<&GenStep> = target.iter().filter(|s| s.active).collect();
        // Fallback density 0.5: all-off or all-on are astronomically
        // unlikely; this seed produces a mix.
        assert!(!actives.is_empty(), "fallback produced silence");
        assert!(actives.len() < 16, "fallback produced a wall of triggers");
        for s in &actives {
            assert_eq!(s.note, 60, "fallback note should be 60");
            assert_eq!(s.velocity, 100, "fallback velocity should be 100");
            assert_eq!(s.prob, 100);
        }
    }

    #[test]
    fn markov_silent_sources_fall_back() {
        // Sources exist but contain no active steps: same fallbacks, and the
        // learned all-off trigger table must not force eternal silence.
        let sources = vec![vec![GenStep::default(); 16], vec![GenStep::default(); 8]];
        let mut target = vec![GenStep::default(); 16];
        markov(&mut target, &sources, 5);
        assert!(active_count(&target) > 0, "silent sources froze the chain");
        for s in target.iter().filter(|s| s.active) {
            assert_eq!(s.note, 60);
            assert_eq!(s.velocity, 100);
        }
    }

    #[test]
    fn markov_notes_subset_of_source_alphabet() {
        let alphabet = [62u8, 65, 69];
        let source: Vec<GenStep> = (0..16)
            .map(|i| GenStep {
                active: i % 2 == 0,
                note: alphabet[i % 3],
                velocity: 90,
                prob: 100,
            })
            .collect();
        for seed in 0..20 {
            let mut target = vec![GenStep::default(); 16];
            markov(&mut target, std::slice::from_ref(&source), seed);
            for s in target.iter().filter(|s| s.active) {
                assert!(
                    alphabet.contains(&s.note),
                    "note {} not in source alphabet",
                    s.note
                );
            }
        }
    }

    #[test]
    fn markov_alternating_source_alternates() {
        // on/off alternation has a deterministic transition table, so any
        // 16-step generation lands at exactly 8 triggers.
        let source: Vec<GenStep> = (0..16)
            .map(|i| GenStep {
                active: i % 2 == 0,
                note: 64,
                velocity: 100,
                prob: 100,
            })
            .collect();
        for seed in 0..10 {
            let mut target = vec![GenStep::default(); 16];
            markov(&mut target, std::slice::from_ref(&source), seed);
            assert_eq!(active_count(&target), 8, "seed {seed}");
        }
    }

    #[test]
    fn markov_all_on_source_saturates() {
        let source: Vec<GenStep> = (0..8)
            .map(|_| GenStep {
                active: true,
                note: 48,
                velocity: 80,
                prob: 100,
            })
            .collect();
        let mut target = vec![GenStep::default(); 16];
        markov(&mut target, std::slice::from_ref(&source), 3);
        assert_eq!(active_count(&target), 16);
        for s in &target {
            assert_eq!(s.note, 48);
            // Zero velocity spread: generated velocity is exactly the mean.
            assert_eq!(s.velocity, 80);
            assert_eq!(s.prob, 100);
        }
    }

    #[test]
    fn markov_velocity_in_legal_range() {
        let source: Vec<GenStep> = (0..16)
            .map(|i| GenStep {
                active: true,
                note: 60,
                velocity: if i % 2 == 0 { 10 } else { 120 },
                prob: 100,
            })
            .collect();
        for seed in 0..30 {
            let mut target = vec![GenStep::default(); 16];
            markov(&mut target, std::slice::from_ref(&source), seed);
            for s in target.iter().filter(|s| s.active) {
                assert!((1..=127).contains(&s.velocity));
            }
        }
    }

    #[test]
    fn markov_deterministic() {
        let sources = vec![pattern16()];
        let mut a = vec![GenStep::default(); 16];
        let mut b = vec![GenStep::default(); 16];
        markov(&mut a, &sources, 31337);
        markov(&mut b, &sources, 31337);
        assert_eq!(a, b);
    }

    #[test]
    fn markov_density_in_plausible_band() {
        // Source density 0.25 with strongly off-biased transitions: target
        // should be sparse, never saturated.
        let source = pattern16(); // quarters: density 0.25
        let mut total_active = 0usize;
        for seed in 0..50 {
            let mut target = vec![GenStep::default(); 16];
            markov(&mut target, std::slice::from_ref(&source), seed);
            total_active += active_count(&target);
        }
        let mean_density = total_active as f32 / (50.0 * 16.0);
        assert!(
            (0.05..=0.55).contains(&mean_density),
            "implausible density {mean_density} from 0.25-density source"
        );
    }

    // ----- lsystem -----

    /// Expand an L-system from `axiom` with `rewrite` until at least `len`
    /// symbols, then truncate — computed here so the expected masks come
    /// from the rewrite rules rather than hand-typing.
    fn expand(axiom: &[u8], rewrite: impl Fn(u8) -> Vec<u8>, len: usize) -> Vec<u8> {
        let mut word = axiom.to_vec();
        while word.len() < len {
            word = word.iter().flat_map(|&s| rewrite(s)).collect();
        }
        word.truncate(len);
        word
    }

    fn mask_of(steps: &[GenStep]) -> Vec<bool> {
        steps.iter().map(|s| s.active).collect()
    }

    #[test]
    fn lsystem_empty_no_panic() {
        for rule in [LRule::Cantor, LRule::ThueMorse, LRule::Fibonacci, LRule::Sierpinski] {
            let mut steps: Vec<GenStep> = vec![];
            lsystem(&mut steps, rule, 0);
            assert!(steps.is_empty());
        }
    }

    #[test]
    fn lsystem_thue_morse_prefix() {
        let mut steps = vec![GenStep::default(); 8];
        lsystem(&mut steps, LRule::ThueMorse, 0);
        assert_eq!(
            mask_of(&steps),
            [true, false, false, true, false, true, true, false]
        );
    }

    #[test]
    fn lsystem_thue_morse_recurrence() {
        // t(2n) = t(n), t(2n+1) = !t(n).
        let mut steps = vec![GenStep::default(); 64];
        lsystem(&mut steps, LRule::ThueMorse, 0);
        let m = mask_of(&steps);
        for n in 0..32 {
            assert_eq!(m[2 * n], m[n], "t(2n) != t(n) at n={n}");
            assert_eq!(m[2 * n + 1], !m[n], "t(2n+1) != !t(n) at n={n}");
        }
    }

    #[test]
    fn lsystem_cantor_len9() {
        let mut steps = vec![GenStep::default(); 9];
        lsystem(&mut steps, LRule::Cantor, 0);
        assert_eq!(
            mask_of(&steps),
            [true, false, true, false, false, false, true, false, true]
        );
    }

    #[test]
    fn lsystem_cantor_matches_rewrite() {
        // A→ABA, B→BBB from A; A (1) = on.
        let expected: Vec<bool> = expand(
            &[1],
            |s| if s == 1 { vec![1, 0, 1] } else { vec![0, 0, 0] },
            27,
        )
        .into_iter()
        .map(|s| s == 1)
        .collect();
        let mut steps = vec![GenStep::default(); 27];
        lsystem(&mut steps, LRule::Cantor, 0);
        assert_eq!(mask_of(&steps), expected);
    }

    #[test]
    fn lsystem_fibonacci_matches_rewrite() {
        // 0→01, 1→0 from 0; 0 = on. Expected mask computed from the rules.
        let expected: Vec<bool> = expand(
            &[0],
            |s| if s == 0 { vec![0, 1] } else { vec![0] },
            21,
        )
        .into_iter()
        .map(|s| s == 0)
        .collect();
        let mut steps = vec![GenStep::default(); 21];
        lsystem(&mut steps, LRule::Fibonacci, 0);
        assert_eq!(mask_of(&steps), expected);
        // Sanity: the canonical fibonacci word starts 01001010.
        assert_eq!(
            &expected[..8],
            [true, false, true, true, false, true, false, true]
        );
    }

    #[test]
    fn lsystem_sierpinski_fibbinary() {
        let mut steps = vec![GenStep::default(); 64];
        lsystem(&mut steps, LRule::Sierpinski, 0);
        let m = mask_of(&steps);
        // On exactly where the index has no two adjacent 1-bits.
        for (i, &on) in m.iter().enumerate() {
            assert_eq!(on, (i & (i >> 1)) == 0, "fibbinary mismatch at {i}");
        }
        assert_eq!(
            &m[..8],
            [true, true, true, false, true, true, false, false]
        );
    }

    #[test]
    fn lsystem_only_touches_active() {
        for rule in [LRule::Cantor, LRule::ThueMorse, LRule::Fibonacci, LRule::Sierpinski] {
            let mut steps = pattern16();
            let before = steps.clone();
            lsystem(&mut steps, rule, 1);
            for (after, orig) in steps.iter().zip(&before) {
                assert_eq!(after.note, orig.note, "{rule:?} touched a note");
                assert_eq!(after.velocity, orig.velocity, "{rule:?} touched a velocity");
                assert_eq!(after.prob, orig.prob, "{rule:?} touched a prob");
            }
        }
    }

    #[test]
    fn lsystem_seed_is_ignored() {
        for rule in [LRule::Cantor, LRule::ThueMorse, LRule::Fibonacci, LRule::Sierpinski] {
            let mut a = vec![GenStep::default(); 32];
            let mut b = vec![GenStep::default(); 32];
            lsystem(&mut a, rule, 0);
            lsystem(&mut b, rule, u64::MAX);
            assert_eq!(a, b, "{rule:?} depended on the seed");
        }
    }
}
