//! Cents-based scale engine and the built-in scale library.
//!
//! A [`Scale`] is a list of cents offsets within one repeating period —
//! usually an octave (1200¢), but nothing here assumes that: Bohlen-Pierce
//! repeats at the tritave (~1902¢), the Carlos scales at the just fifth
//! (~702¢). Sequencer tracks address pitches as *degrees*; the engine turns
//! a degree + root frequency into Hz, so any tuning the library (or a
//! Scala `.scl` file, see [`crate::theory::scl`]) can describe is playable.
//!
//! Library accuracy doctrine: cents values are never hand-typed when they
//! can be computed — 12-TET families come from semitone lists, maqamat
//! from 24-EDO quarter-tone lists, equal temperaments from their
//! definition, just intonation from exact ratios. The few literal-cents
//! entries (gamelan) carry a source comment.

/// A tuning: ascending cents offsets within one period.
///
/// Invariants (upheld by the library and the `.scl` importer):
/// `degrees[0] == 0.0`, strictly ascending, all `< period`, `period > 0`.
///
/// # Examples
///
/// ```
/// use los::theory::scales::Scale;
/// let c = Scale::chromatic();
/// // A4 = degree 0 at 440 Hz; +12 degrees = one octave up
/// assert!((c.degree_to_hz(12, 440.0) - 880.0).abs() < 1e-9);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Scale {
    /// Canonical display name (lowercase, e.g. `"dorian"`, `"22edo"`).
    pub name: String,
    /// Cents offsets within one period; `degrees[0]` is always `0.0`.
    pub degrees: Vec<f64>,
    /// Period in cents (octave = 1200, Bohlen-Pierce tritave ≈ 1902).
    pub period: f64,
}

impl Scale {
    /// Cents above the root for an arbitrary (possibly negative) degree.
    pub fn pitch_cents(&self, degree: i32) -> f64 {
        let n = self.degrees.len() as i32;
        if n == 0 {
            return 0.0;
        }
        let oct = degree.div_euclid(n);
        let idx = degree.rem_euclid(n) as usize;
        f64::from(oct) * self.period + self.degrees[idx]
    }

    /// Frequency of a degree given the root frequency.
    ///
    /// # Examples
    ///
    /// ```
    /// use los::theory::scales::lookup;
    /// let just = lookup("ptolemy intense diatonic").unwrap();
    /// // degree 2 of a just major scale is the pure third, 5/4
    /// assert!((just.degree_to_hz(2, 400.0) - 500.0).abs() < 1e-6);
    /// ```
    pub fn degree_to_hz(&self, degree: i32, root_hz: f64) -> f64 {
        root_hz * 2f64.powf(self.pitch_cents(degree) / 1200.0)
    }

    /// Number of degrees per period.
    pub fn len(&self) -> usize {
        self.degrees.len()
    }

    pub fn is_empty(&self) -> bool {
        self.degrees.is_empty()
    }

    /// Nearest degree to a pitch given in cents above the root (used to
    /// convert chromatic notes when a scale is assigned). Exact ties take
    /// the lower degree.
    pub fn quantize_cents(&self, cents: f64) -> i32 {
        let n = self.degrees.len() as i32;
        if n == 0 || self.period <= 0.0 {
            return 0;
        }
        let base = (cents / self.period).floor() as i32 * n;
        let mut best = base - n;
        let mut best_err = f64::INFINITY;
        // Scan one period below through one above — the nearest degree to
        // any pitch always lies in that window.
        for d in (base - n)..=(base + 2 * n) {
            let err = (self.pitch_cents(d) - cents).abs();
            if err < best_err {
                best_err = err;
                best = d;
            }
        }
        best
    }

    /// The 12-TET chromatic scale — the engine's identity tuning (a track
    /// with no scale behaves exactly like this one rooted anywhere).
    pub fn chromatic() -> Scale {
        Scale {
            name: String::from("chromatic"),
            degrees: (0..12).map(|i| f64::from(i) * 100.0).collect(),
            period: 1200.0,
        }
    }
}

/// MIDI note → Hz, 12-TET, A4 = 440. The crate's one canonical version.
pub fn midi_to_hz(note: u8) -> f64 {
    440.0 * 2f64.powf((f64::from(note) - 69.0) / 12.0)
}

/// 1200·log2(p/q) — exact ratios become cents in one place.
fn ratio(p: u32, q: u32) -> f64 {
    1200.0 * (f64::from(p) / f64::from(q)).log2()
}

/// Compact spec a library entry is generated from.
enum Spec {
    /// 12-TET semitone offsets (period 1200).
    Semis(&'static [u8]),
    /// 24-EDO quarter-tone offsets, ×50¢ (period 1200).
    Quarts(&'static [u8]),
    /// Full chromatic of an equal division of the octave.
    Edo(u16),
    /// A subset of an EDO: (divisions, step indices).
    EdoSteps(u16, &'static [u8]),
    /// N equal divisions of the ratio p/q (Bohlen-Pierce = 13 ed 3/1,
    /// Carlos alpha = 9 ed 3/2, …).
    EdN(u16, u32, u32),
    /// A subset of an EdN: (divisions, p, q, step indices).
    EdNSteps(u16, u32, u32, &'static [u8]),
    /// Just intonation: exact ratios within the period, plus the period
    /// ratio itself.
    Ratios(&'static [(u32, u32)], (u32, u32)),
    /// Literal cents (measured tunings) + period. Always source-commented.
    Cents(&'static [f64], f64),
}

impl Spec {
    fn build(&self, name: &str) -> Scale {
        let (degrees, period) = match *self {
            Spec::Semis(s) => (s.iter().map(|&i| f64::from(i) * 100.0).collect(), 1200.0),
            Spec::Quarts(q) => (q.iter().map(|&i| f64::from(i) * 50.0).collect(), 1200.0),
            Spec::Edo(n) => (
                (0..n).map(|i| f64::from(i) * 1200.0 / f64::from(n)).collect(),
                1200.0,
            ),
            Spec::EdoSteps(n, idx) => (
                idx.iter().map(|&i| f64::from(i) * 1200.0 / f64::from(n)).collect(),
                1200.0,
            ),
            Spec::EdN(n, p, q) => {
                let period = ratio(p, q);
                (
                    (0..n).map(|i| f64::from(i) * period / f64::from(n)).collect(),
                    period,
                )
            }
            Spec::EdNSteps(n, p, q, idx) => {
                let period = ratio(p, q);
                (
                    idx.iter().map(|&i| f64::from(i) * period / f64::from(n)).collect(),
                    period,
                )
            }
            Spec::Ratios(rs, (pp, pq)) => (
                rs.iter().map(|&(p, q)| ratio(p, q)).collect(),
                ratio(pp, pq),
            ),
            Spec::Cents(cs, period) => (cs.to_vec(), period),
        };
        Scale { name: String::from(name), degrees, period }
    }
}

/// Look a built-in scale up by name. Case-insensitive; spaces, `-` and `_`
/// are interchangeable.
pub fn lookup(name: &str) -> Option<Scale> {
    let want = fold(name);
    LIB.iter()
        .find(|(n, _)| fold(n) == want)
        .map(|(n, spec)| spec.build(n))
}

/// Every built-in scale name, sorted — feeds `:scale` tab completion.
pub fn names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = LIB.iter().map(|(n, _)| *n).collect();
    v.sort_unstable();
    v
}

fn fold(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

// ── the library ──────────────────────────────────────────────────────────
//
// Sources: 12-TET interval sets are standard theory; maqam quarter-tone
// approximations follow the common 24-EDO transcriptions (maqamworld);
// gamelan cents are the Surjodiningrat–Sudarjana–Susanto (1972) averages;
// Partch 43 is the Genesis of a Music scale; the Well-Tuned Piano ratios
// follow Kyle Gann's published analysis; Carlos alpha/beta/gamma are her
// equal divisions of the just fifth (9, 11, 20).

#[rustfmt::skip]
static LIB: &[(&str, Spec)] = &[
    // 12-TET — modes of the major scale (+ the two household aliases)
    ("chromatic",        Spec::Semis(&[0,1,2,3,4,5,6,7,8,9,10,11])),
    ("major",            Spec::Semis(&[0,2,4,5,7,9,11])),
    ("ionian",           Spec::Semis(&[0,2,4,5,7,9,11])),
    ("dorian",           Spec::Semis(&[0,2,3,5,7,9,10])),
    ("phrygian",         Spec::Semis(&[0,1,3,5,7,8,10])),
    ("lydian",           Spec::Semis(&[0,2,4,6,7,9,11])),
    ("mixolydian",       Spec::Semis(&[0,2,4,5,7,9,10])),
    ("minor",            Spec::Semis(&[0,2,3,5,7,8,10])),
    ("aeolian",          Spec::Semis(&[0,2,3,5,7,8,10])),
    ("locrian",          Spec::Semis(&[0,1,3,5,6,8,10])),
    // harmonic minor and its named modes
    ("harmonic minor",   Spec::Semis(&[0,2,3,5,7,8,11])),
    ("locrian nat6",     Spec::Semis(&[0,1,3,5,6,9,10])),
    ("ionian aug",       Spec::Semis(&[0,2,4,5,8,9,11])),
    ("ukrainian dorian", Spec::Semis(&[0,2,3,6,7,9,10])),
    ("phrygian dominant",Spec::Semis(&[0,1,4,5,7,8,10])),
    ("lydian sharp2",    Spec::Semis(&[0,3,4,6,7,9,11])),
    ("ultralocrian",     Spec::Semis(&[0,1,3,4,6,8,9])),
    // melodic minor (ascending) and its named modes
    ("melodic minor",    Spec::Semis(&[0,2,3,5,7,9,11])),
    ("dorian flat2",     Spec::Semis(&[0,1,3,5,7,9,10])),
    ("lydian augmented", Spec::Semis(&[0,2,4,6,8,9,11])),
    ("lydian dominant",  Spec::Semis(&[0,2,4,6,7,9,10])),
    ("mixolydian flat6", Spec::Semis(&[0,2,4,5,7,8,10])),
    ("half diminished",  Spec::Semis(&[0,2,3,5,6,8,10])),
    ("altered",          Spec::Semis(&[0,1,3,4,6,8,10])),
    // harmonic major & the double-harmonic family
    ("harmonic major",   Spec::Semis(&[0,2,4,5,7,8,11])),
    ("double harmonic",  Spec::Semis(&[0,1,4,5,7,8,11])),
    ("byzantine",        Spec::Semis(&[0,1,4,5,7,8,11])),
    ("hungarian minor",  Spec::Semis(&[0,2,3,6,7,8,11])),
    ("hungarian major",  Spec::Semis(&[0,3,4,6,7,9,10])),
    ("oriental",         Spec::Semis(&[0,1,4,5,6,9,10])),
    ("persian",          Spec::Semis(&[0,1,4,5,6,8,11])),
    ("neapolitan major", Spec::Semis(&[0,1,3,5,7,9,11])),
    ("neapolitan minor", Spec::Semis(&[0,1,3,5,7,8,11])),
    ("enigmatic",        Spec::Semis(&[0,1,4,6,8,10,11])),
    ("prometheus",       Spec::Semis(&[0,2,4,6,9,10])),
    ("tritone",          Spec::Semis(&[0,1,4,6,7,10])),
    // pentatonics — western names and the five Chinese modes
    ("major pentatonic", Spec::Semis(&[0,2,4,7,9])),
    ("minor pentatonic", Spec::Semis(&[0,3,5,7,10])),
    ("egyptian",         Spec::Semis(&[0,2,5,7,10])),
    ("gong",             Spec::Semis(&[0,2,4,7,9])),
    ("shang",            Spec::Semis(&[0,2,5,7,10])),
    ("jue",              Spec::Semis(&[0,3,5,8,10])),
    ("zhi",              Spec::Semis(&[0,2,5,7,9])),
    ("yu",               Spec::Semis(&[0,3,5,7,10])),
    // Japanese pentatonics
    ("hirajoshi",        Spec::Semis(&[0,2,3,7,8])),
    ("in",               Spec::Semis(&[0,1,5,7,8])),
    ("insen",            Spec::Semis(&[0,1,5,7,10])),
    ("iwato",            Spec::Semis(&[0,1,5,6,10])),
    ("yo",               Spec::Semis(&[0,2,5,7,9])),
    // blues & bebop
    ("blues",            Spec::Semis(&[0,3,5,6,7,10])),
    ("major blues",      Spec::Semis(&[0,2,3,4,7,9])),
    ("bebop dominant",   Spec::Semis(&[0,2,4,5,7,9,10,11])),
    ("bebop major",      Spec::Semis(&[0,2,4,5,7,8,9,11])),
    // "bebop minor" follows the melodic-minor reading; some references
    // use the name for the dorian-add-major-third form (bebop dorian here)
    ("bebop minor",      Spec::Semis(&[0,2,3,5,7,8,9,11])),
    ("bebop melodic minor", Spec::Semis(&[0,2,3,5,7,8,9,11])),
    ("bebop dorian",     Spec::Semis(&[0,2,3,4,5,7,9,10])),
    // symmetric scales
    ("whole tone",       Spec::Semis(&[0,2,4,6,8,10])),
    ("octatonic hw",     Spec::Semis(&[0,1,3,4,6,7,9,10])),
    ("octatonic wh",     Spec::Semis(&[0,2,3,5,6,8,9,11])),
    ("augmented",        Spec::Semis(&[0,3,4,7,8,11])),
    // Messiaen modes of limited transposition (1/2 alias whole tone/octatonic)
    ("messiaen 1",       Spec::Semis(&[0,2,4,6,8,10])),
    ("messiaen 2",       Spec::Semis(&[0,1,3,4,6,7,9,10])),
    ("messiaen 3",       Spec::Semis(&[0,2,3,4,6,7,8,10,11])),
    ("messiaen 4",       Spec::Semis(&[0,1,2,5,6,7,8,11])),
    ("messiaen 5",       Spec::Semis(&[0,1,5,6,7,11])),
    ("messiaen 6",       Spec::Semis(&[0,2,4,5,6,8,10,11])),
    ("messiaen 7",       Spec::Semis(&[0,1,2,3,5,6,7,8,9,11])),
    // maqamat — common 24-EDO quarter-tone transcriptions (units of 50¢)
    ("rast",             Spec::Quarts(&[0,4,7,10,14,18,21])),
    ("bayati",           Spec::Quarts(&[0,3,6,10,14,16,20])),
    ("hijaz",            Spec::Quarts(&[0,2,8,10,14,16,20])),
    ("hijazkar",         Spec::Quarts(&[0,2,8,10,14,16,22])),
    ("saba",             Spec::Quarts(&[0,3,6,8,14,16,20])),
    ("sikah",            Spec::Quarts(&[0,3,7,11,14,17,21])),
    ("ajam",             Spec::Quarts(&[0,4,8,10,14,18,22])),
    ("nahawand",         Spec::Quarts(&[0,4,6,10,14,16,20])),
    ("kurd",             Spec::Quarts(&[0,2,6,10,14,16,20])),
    ("nakriz",           Spec::Quarts(&[0,4,6,12,14,18,20])),
    ("jiharkah",         Spec::Quarts(&[0,4,8,10,14,18,21])),
    ("suznak",           Spec::Quarts(&[0,4,7,10,14,16,22])),
    // equal divisions of the octave — the xen workhorses
    ("5edo",   Spec::Edo(5)),
    ("6edo",   Spec::Edo(6)),
    ("7edo",   Spec::Edo(7)),
    ("8edo",   Spec::Edo(8)),
    ("9edo",   Spec::Edo(9)),
    ("10edo",  Spec::Edo(10)),
    ("11edo",  Spec::Edo(11)),
    ("13edo",  Spec::Edo(13)),
    ("14edo",  Spec::Edo(14)),
    ("15edo",  Spec::Edo(15)),
    ("16edo",  Spec::Edo(16)),
    ("17edo",  Spec::Edo(17)),
    ("18edo",  Spec::Edo(18)),
    ("19edo",  Spec::Edo(19)),
    ("20edo",  Spec::Edo(20)),
    ("21edo",  Spec::Edo(21)),
    ("22edo",  Spec::Edo(22)),
    ("23edo",  Spec::Edo(23)),
    ("24edo",  Spec::Edo(24)),
    ("quarter tone", Spec::Edo(24)),
    ("25edo",  Spec::Edo(25)),
    ("26edo",  Spec::Edo(26)),
    ("27edo",  Spec::Edo(27)),
    ("29edo",  Spec::Edo(29)),
    ("31edo",  Spec::Edo(31)),
    ("34edo",  Spec::Edo(34)),
    ("36edo",  Spec::Edo(36)),
    ("41edo",  Spec::Edo(41)),
    ("46edo",  Spec::Edo(46)),
    ("48edo",  Spec::Edo(48)),
    ("53edo",  Spec::Edo(53)),
    ("72edo",  Spec::Edo(72)),
    // famous EDO subsets (standard step patterns)
    ("19edo meantone major",  Spec::EdoSteps(19, &[0,3,6,8,11,14,17])),
    ("31edo meantone major",  Spec::EdoSteps(31, &[0,5,10,13,18,23,28])),
    ("22edo superpyth major", Spec::EdoSteps(22, &[0,4,8,9,13,17,21])),
    // Bohlen-Pierce: 13 equal divisions of the tritave (3/1)
    ("bohlen-pierce",         Spec::EdN(13, 3, 1)),
    ("bohlen-pierce lambda",  Spec::EdNSteps(13, 3, 1, &[0,2,3,5,6,8,9,11,12])),
    // Wendy Carlos alpha/beta/gamma: equal divisions of the just fifth
    ("carlos alpha", Spec::EdN(9, 3, 2)),
    ("carlos beta",  Spec::EdN(11, 3, 2)),
    ("carlos gamma", Spec::EdN(20, 3, 2)),
    // 88-cent equal temperament (Gary Morrison): one 88¢ step, no octave
    ("88cet", Spec::Cents(&[0.0], 88.0)),
    // just intonation — exact ratios
    ("ptolemy intense diatonic", Spec::Ratios(&[(1,1),(9,8),(5,4),(4,3),(3,2),(5,3),(15,8)], (2,1))),
    ("just major",               Spec::Ratios(&[(1,1),(9,8),(5,4),(4,3),(3,2),(5,3),(15,8)], (2,1))),
    ("just minor",               Spec::Ratios(&[(1,1),(9,8),(6,5),(4,3),(3,2),(8,5),(9,5)], (2,1))),
    ("pythagorean diatonic",     Spec::Ratios(&[(1,1),(9,8),(81,64),(4,3),(3,2),(27,16),(243,128)], (2,1))),
    ("pythagorean chromatic",    Spec::Ratios(&[(1,1),(256,243),(9,8),(32,27),(81,64),(4,3),(729,512),(3,2),(128,81),(27,16),(16,9),(243,128)], (2,1))),
    ("5-limit chromatic",        Spec::Ratios(&[(1,1),(16,15),(9,8),(6,5),(5,4),(4,3),(45,32),(3,2),(8,5),(5,3),(9,5),(15,8)], (2,1))),
    ("centaur",                  Spec::Ratios(&[(1,1),(21,20),(9,8),(7,6),(5,4),(4,3),(7,5),(3,2),(14,9),(5,3),(7,4),(15,8)], (2,1))),
    ("overtone",                 Spec::Ratios(&[(1,1),(9,8),(5,4),(11,8),(3,2),(13,8),(7,4)], (2,1))),
    ("harmonics 8-16",           Spec::Ratios(&[(8,8),(9,8),(10,8),(11,8),(12,8),(13,8),(14,8),(15,8)], (2,1))),
    ("harmonics 16-32",          Spec::Ratios(&[(16,16),(17,16),(18,16),(19,16),(20,16),(21,16),(22,16),(23,16),(24,16),(25,16),(26,16),(27,16),(28,16),(29,16),(30,16),(31,16)], (2,1))),
    ("subharmonics 16-8",        Spec::Ratios(&[(16,16),(16,15),(16,14),(16,13),(16,12),(16,11),(16,10),(16,9)], (2,1))),
    // Harry Partch's 43-tone scale (Genesis of a Music)
    ("partch 43", Spec::Ratios(&[
        (1,1),(81,80),(33,32),(21,20),(16,15),(12,11),(11,10),(10,9),(9,8),(8,7),
        (7,6),(32,27),(6,5),(11,9),(5,4),(14,11),(9,7),(21,16),(4,3),(27,20),
        (11,8),(7,5),(10,7),(16,11),(40,27),(3,2),(32,21),(14,9),(11,7),(8,5),
        (18,11),(5,3),(27,16),(12,7),(7,4),(16,9),(9,5),(20,11),(11,6),(15,8),
        (40,21),(64,33),(160,81)], (2,1))),
    // La Monte Young, The Well-Tuned Piano (per Kyle Gann's analysis),
    // in ascending pitch order — note-name order is non-monotonic here
    // (the G# ratio sits below G)
    ("well-tuned piano", Spec::Ratios(&[
        (1,1),(567,512),(9,8),(147,128),(1323,1024),(21,16),(189,128),
        (3,2),(49,32),(441,256),(7,4),(63,32)], (2,1))),
    // gamelan — Surjodiningrat, Sudarjana & Susanto (1972) averaged
    // measurements of central Javanese gamelan
    ("slendro", Spec::Cents(&[0.0, 231.0, 474.0, 717.0, 955.0], 1208.0)),
    ("pelog",   Spec::Cents(&[0.0, 120.0, 270.0, 540.0, 670.0, 785.0, 950.0], 1200.0)),
    ("pelog bem",    Spec::Cents(&[0.0, 120.0, 270.0, 670.0, 785.0], 1200.0)),
    ("pelog barang", Spec::Cents(&[0.0, 150.0, 550.0, 665.0, 830.0], 1200.0)),
];

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    #[test]
    fn pitch_cents_walks_periods() {
        let c = Scale::chromatic();
        assert!((c.pitch_cents(0) - 0.0).abs() < EPS);
        assert!((c.pitch_cents(1) - 100.0).abs() < EPS);
        assert!((c.pitch_cents(12) - 1200.0).abs() < EPS);
        assert!((c.pitch_cents(13) - 1300.0).abs() < EPS);
        assert!((c.pitch_cents(-1) + 100.0).abs() < EPS);
        assert!((c.pitch_cents(-12) + 1200.0).abs() < EPS);
        assert!((c.pitch_cents(-13) + 1300.0).abs() < EPS);
        // pentatonic period walk
        let p = lookup("major pentatonic").unwrap();
        assert!((p.pitch_cents(5) - 1200.0).abs() < EPS);
        assert!((p.pitch_cents(6) - 1400.0).abs() < EPS);
        assert!((p.pitch_cents(-5) + 1200.0).abs() < EPS);
    }

    #[test]
    fn degree_to_hz_octaves_and_thirds() {
        let c = Scale::chromatic();
        assert!((c.degree_to_hz(0, 440.0) - 440.0).abs() < EPS);
        assert!((c.degree_to_hz(12, 440.0) - 880.0).abs() < 1e-9);
        assert!((c.degree_to_hz(-12, 440.0) - 220.0).abs() < 1e-9);
        // the pure major third: 5/4 over the root, ~386.31 cents
        let just = lookup("ptolemy intense diatonic").unwrap();
        assert!((just.pitch_cents(2) - 386.313_713_864_835).abs() < 1e-6);
        assert!((just.degree_to_hz(2, 400.0) - 500.0).abs() < 1e-6);
    }

    #[test]
    fn midi_to_hz_anchors() {
        assert!((midi_to_hz(69) - 440.0).abs() < EPS);
        assert!((midi_to_hz(81) - 880.0).abs() < 1e-9);
        assert!((midi_to_hz(57) - 220.0).abs() < 1e-9);
        assert!((midi_to_hz(60) - 261.625_565_300_6).abs() < 1e-6);
    }

    #[test]
    fn quantize_rounds_to_nearest_degree() {
        let c = Scale::chromatic();
        assert_eq!(c.quantize_cents(0.0), 0);
        assert_eq!(c.quantize_cents(149.0), 1);
        assert_eq!(c.quantize_cents(151.0), 2);
        assert_eq!(c.quantize_cents(-49.0), 0);
        assert_eq!(c.quantize_cents(-51.0), -1);
        assert_eq!(c.quantize_cents(1201.0), 12);
        assert_eq!(c.quantize_cents(-1199.0), -12);
        // exact tie takes the lower degree
        assert_eq!(c.quantize_cents(50.0), 0);
        // major scale: 300¢ (minor third) is nearest degree 2 (400¢ is 100
        // away, 200¢ is 100 away — tie, lower wins)
        let maj = lookup("major").unwrap();
        assert_eq!(maj.quantize_cents(300.0), 1);
        assert_eq!(maj.quantize_cents(310.0), 2);
        assert_eq!(maj.quantize_cents(1190.0), 7);
    }

    #[test]
    fn lookup_is_forgiving() {
        assert!(lookup("dorian").is_some());
        assert!(lookup("DORIAN").is_some());
        assert!(lookup("Bohlen-Pierce Lambda").is_some());
        assert!(lookup("bohlen_pierce_lambda").is_some());
        assert!(lookup("bohlenpiercelambda").is_some());
        assert!(lookup("no such scale").is_none());
        assert_eq!(lookup("22edo").unwrap().len(), 22);
    }

    #[test]
    fn library_invariants_hold_for_every_scale() {
        let names = names();
        assert!(names.len() >= 130, "library has {} scales", names.len());
        for name in &names {
            let s = lookup(name).unwrap_or_else(|| panic!("{name} must build"));
            assert!(!s.is_empty(), "{name}: empty");
            assert!(s.period > 0.0, "{name}: period {}", s.period);
            assert!(s.degrees[0].abs() < EPS, "{name}: degrees[0] = {}", s.degrees[0]);
            for w in s.degrees.windows(2) {
                assert!(w[1] > w[0], "{name}: not ascending ({} -> {})", w[0], w[1]);
            }
            let last = s.degrees[s.degrees.len() - 1];
            assert!(last < s.period, "{name}: degree {last} >= period {}", s.period);
        }
        // names unique after folding
        let mut folded: Vec<String> = names.iter().map(|n| fold(n)).collect();
        folded.sort();
        let before = folded.len();
        folded.dedup();
        assert_eq!(folded.len(), before, "folded names must be unique");
    }

    #[test]
    fn famous_values_spot_checks() {
        // dorian: 0 200 300 500 700 900 1000
        let d = lookup("dorian").unwrap();
        let want = [0.0, 200.0, 300.0, 500.0, 700.0, 900.0, 1000.0];
        for (i, w) in want.iter().enumerate() {
            assert!((d.degrees[i] - w).abs() < EPS, "dorian[{i}]");
        }
        // rast's neutral third = 350¢
        assert!((lookup("rast").unwrap().degrees[2] - 350.0).abs() < EPS);
        // 22edo step = 1200/22
        let e22 = lookup("22edo").unwrap();
        assert!((e22.degrees[1] - 1200.0 / 22.0).abs() < EPS);
        // Bohlen-Pierce period is the tritave, step ~146.3¢
        let bp = lookup("bohlen-pierce").unwrap();
        assert!((bp.period - 1_901.955_000_865_387).abs() < 1e-6);
        assert!((bp.degrees[1] - bp.period / 13.0).abs() < EPS);
        // lambda is 9 notes of the 13
        assert_eq!(lookup("bohlen-pierce lambda").unwrap().len(), 9);
        // Carlos alpha step = 701.955/9 ≈ 78.0¢
        let alpha = lookup("carlos alpha").unwrap();
        assert!((alpha.degrees[1] - 77.995).abs() < 0.01);
        // gamma step ≈ 35.1¢
        let gamma = lookup("carlos gamma").unwrap();
        assert!((gamma.degrees[1] - 35.098).abs() < 0.01);
        // Partch has all 43
        assert_eq!(lookup("partch 43").unwrap().len(), 43);
        // Pythagorean major third = 81/64 ≈ 407.82¢
        let py = lookup("pythagorean diatonic").unwrap();
        assert!((py.degrees[2] - 407.82).abs() < 0.01);
        // slendro per the 1972 averages
        let sl = lookup("slendro").unwrap();
        assert_eq!(sl.len(), 5);
        assert!((sl.degrees[1] - 231.0).abs() < EPS);
        // 88cET: a single 88¢ step
        let cet = lookup("88cet").unwrap();
        assert_eq!(cet.len(), 1);
        assert!((cet.pitch_cents(3) - 264.0).abs() < EPS);
    }

    #[test]
    fn non_octave_scales_compound_correctly() {
        let bp = lookup("bohlen-pierce").unwrap();
        // one full period up = a perfect twelfth (3×)
        assert!((bp.degree_to_hz(13, 100.0) - 300.0).abs() < 1e-6);
        assert!((bp.degree_to_hz(-13, 300.0) - 100.0).abs() < 1e-6);
        let alpha = lookup("carlos alpha").unwrap();
        // 9 alpha steps = a pure 3/2
        assert!((alpha.degree_to_hz(9, 200.0) - 300.0).abs() < 1e-6);
    }

    #[test]
    fn quantize_handles_non_octave_periods() {
        let alpha = lookup("carlos alpha").unwrap();
        let step = alpha.period / 9.0;
        for d in -20..=20 {
            let cents = f64::from(d) * step;
            assert_eq!(alpha.quantize_cents(cents + 0.3 * step), d);
            assert_eq!(alpha.quantize_cents(cents - 0.3 * step), d);
        }
    }

    #[test]
    fn chromatic_matches_midi() {
        // degree d above root 60 must equal midi_to_hz(60 + d)
        let c = Scale::chromatic();
        let root = midi_to_hz(60);
        for d in -24i32..=24 {
            let want = midi_to_hz((60 + d) as u8);
            assert!(
                (c.degree_to_hz(d, root) - want).abs() < 1e-6,
                "degree {d} mismatch"
            );
        }
    }
}
