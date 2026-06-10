//! Scala `.scl` tuning file parser — the door to the multi-thousand-scale
//! Scala archive.
//!
//! Format (per the Scala docs): lines starting with `!` are comments; the
//! first non-comment line is a free-text description, the second the note
//! count N, then N pitch lines. A pitch containing a `.` is cents; anything
//! else is a ratio `p/q` (a bare integer is `p/1`). The pitches describe
//! degrees 1..=N above the root, the Nth being the period.
//!
//! Conversion to [`Scale`]: degrees = `[0.0, pitch1, …, pitch(N-1)]`,
//! period = pitchN. Scala permits non-monotonic pitch lists; we sort and
//! dedup so the [`Scale`] ascending invariant holds (documented, audible
//! difference only for deliberately scrambled files).

use std::fmt;
use std::path::Path;

use crate::theory::scales::Scale;

/// Why a `.scl` file failed to parse.
#[derive(Debug)]
pub enum SclError {
    /// The file ended before description, count, or all pitches appeared.
    Truncated,
    /// The note-count line wasn't a non-negative integer.
    BadCount(String),
    /// A pitch line couldn't be read as cents or a ratio.
    BadPitch(String),
    /// A ratio was zero or negative, or a pitch wasn't above the root.
    OutOfRange(String),
    /// The period (last pitch) wasn't positive.
    BadPeriod,
    Io(std::io::Error),
}

impl fmt::Display for SclError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SclError::Truncated => write!(f, "scl: file ends early"),
            SclError::BadCount(s) => write!(f, "scl: bad note count: {s}"),
            SclError::BadPitch(s) => write!(f, "scl: bad pitch: {s}"),
            SclError::OutOfRange(s) => write!(f, "scl: pitch out of range: {s}"),
            SclError::BadPeriod => write!(f, "scl: period must be positive"),
            SclError::Io(e) => write!(f, "scl: {e}"),
        }
    }
}

impl std::error::Error for SclError {}

impl From<std::io::Error> for SclError {
    fn from(e: std::io::Error) -> Self {
        SclError::Io(e)
    }
}

/// Parse `.scl` text into a [`Scale`].
///
/// # Examples
///
/// ```
/// use los::theory::scl::parse_scl;
/// let text = "! example\nPure fifth pair\n2\n 3/2\n 2/1\n";
/// let s = parse_scl(text).unwrap();
/// assert_eq!(s.len(), 2);
/// assert!((s.degrees[1] - 701.955).abs() < 0.001);
/// assert!((s.period - 1200.0).abs() < 1e-9);
/// ```
pub fn parse_scl(text: &str) -> Result<Scale, SclError> {
    let mut lines = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        // per the Scala spec a comment line starts with '!' in column one;
        // leading whitespace makes it data
        .filter(|l| !l.starts_with('!'));

    let description = lines.next().ok_or(SclError::Truncated)?.trim();
    let count_line = lines.next().ok_or(SclError::Truncated)?.trim();
    let count: usize = count_line
        .parse()
        .map_err(|_| SclError::BadCount(count_line.to_string()))?;

    let mut pitches = Vec::with_capacity(count);
    for _ in 0..count {
        let line = lines.next().ok_or(SclError::Truncated)?;
        // pitch is the first whitespace-separated token; the rest of the
        // line is a free-form comment
        let token = line
            .split_whitespace()
            .next()
            .ok_or_else(|| SclError::BadPitch(line.to_string()))?;
        pitches.push(parse_pitch(token)?);
    }

    if count == 0 {
        // A degenerate but legal file: nothing but the root. Treat it as
        // a one-note octave scale so it stays playable.
        return Ok(Scale {
            name: name_from(description),
            degrees: vec![0.0],
            period: 1200.0,
        });
    }

    // SAFETY of indexing: count >= 1 here, so last() exists.
    let period = pitches[count - 1];
    if period <= 0.0 {
        return Err(SclError::BadPeriod);
    }

    let mut degrees: Vec<f64> = Vec::with_capacity(count);
    degrees.push(0.0);
    degrees.extend(pitches[..count - 1].iter().copied());
    // Scala allows non-monotonic lists; our Scale requires ascending.
    degrees.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    degrees.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    if degrees.iter().any(|&d| d < 0.0 || d >= period) {
        return Err(SclError::OutOfRange(format!(
            "degrees must lie in [0, {period})"
        )));
    }

    Ok(Scale { name: name_from(description), degrees, period })
}

/// Read and parse a `.scl` file from disk.
pub fn load_scl(path: &Path) -> Result<Scale, SclError> {
    let text = std::fs::read_to_string(path)?;
    let mut scale = parse_scl(&text)?;
    // A bare description is common; the filename is the better name then.
    if scale.name == "imported" {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            scale.name = stem.to_lowercase();
        }
    }
    Ok(scale)
}

fn name_from(description: &str) -> String {
    if description.is_empty() {
        String::from("imported")
    } else {
        description.to_lowercase()
    }
}

/// One pitch token: cents if it contains `.`, otherwise `p/q` or `p`.
fn parse_pitch(token: &str) -> Result<f64, SclError> {
    if token.contains('.') {
        return token
            .parse::<f64>()
            .map_err(|_| SclError::BadPitch(token.to_string()));
    }
    let (p, q) = match token.split_once('/') {
        Some((p, q)) => (
            p.parse::<i64>().map_err(|_| SclError::BadPitch(token.to_string()))?,
            q.parse::<i64>().map_err(|_| SclError::BadPitch(token.to_string()))?,
        ),
        None => (
            token.parse::<i64>().map_err(|_| SclError::BadPitch(token.to_string()))?,
            1,
        ),
    };
    if p <= 0 || q <= 0 {
        return Err(SclError::OutOfRange(token.to_string()));
    }
    Ok(1200.0 * (p as f64 / q as f64).log2())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cents_file() {
        let text = "! comment\n\
                    Five out of nowhere\n\
                    5\n\
                    100.0\n\
                    250.5\n\
                    700.0\n\
                    1000.0\n\
                    1200.0\n";
        let s = parse_scl(text).unwrap();
        assert_eq!(s.name, "five out of nowhere");
        assert_eq!(s.degrees, vec![0.0, 100.0, 250.5, 700.0, 1000.0]);
        assert!((s.period - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn parses_ratio_and_mixed_file() {
        let text = "Just-ish\n4\n 9/8\n 386.31371\n 3/2\n 2/1\n";
        let s = parse_scl(text).unwrap();
        assert_eq!(s.len(), 4);
        assert!((s.degrees[1] - 203.910_001_730_775).abs() < 1e-6, "9/8");
        assert!((s.degrees[2] - 386.31371).abs() < 1e-9, "literal cents kept");
        assert!((s.degrees[3] - 701.955_000_865_387).abs() < 1e-6, "3/2");
        assert!((s.period - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn bare_integer_is_a_ratio() {
        let text = "Harm\n2\n 3\n 4\n";
        let s = parse_scl(text).unwrap();
        // 3/1 = 1901.955; period 4/1 = 2400
        assert!((s.degrees[1] - 1_901.955_000_865_387).abs() < 1e-6);
        assert!((s.period - 2400.0).abs() < 1e-9);
    }

    #[test]
    fn comments_blanks_and_crlf_survive() {
        let text = "! top comment\r\n\
                    ! more\r\n\
                    Windows file\r\n\
                    2\r\n\
                    ! between\r\n\
                    700.0 a fifth, roughly\r\n\
                    1200.0\r\n";
        let s = parse_scl(text).unwrap();
        assert_eq!(s.name, "windows file");
        assert_eq!(s.degrees, vec![0.0, 700.0]);
    }

    #[test]
    fn twelve_tet_roundtrip() {
        let mut text = String::from("12-TET\n12\n");
        for i in 1..=12 {
            text.push_str(&format!("{}.0\n", i * 100));
        }
        let s = parse_scl(&text).unwrap();
        let chrom = crate::theory::scales::Scale::chromatic();
        assert_eq!(s.degrees, chrom.degrees);
        assert!((s.period - chrom.period).abs() < 1e-9);
    }

    #[test]
    fn single_note_scl_is_period_only() {
        // N=1: the lone pitch is the period; degrees collapse to the root.
        let s = parse_scl("Octave drone\n1\n 2/1\n").unwrap();
        assert_eq!(s.degrees, vec![0.0]);
        assert!((s.period - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn zero_count_is_playable() {
        let s = parse_scl("Nothing\n0\n").unwrap();
        assert_eq!(s.degrees, vec![0.0]);
        assert!((s.period - 1200.0).abs() < 1e-9);
    }

    #[test]
    fn non_monotonic_input_is_sorted_and_deduped() {
        let text = "Scrambled\n4\n 700.0\n 100.0\n 100.0\n 1200.0\n";
        let s = parse_scl(text).unwrap();
        assert_eq!(s.degrees, vec![0.0, 100.0, 700.0]);
    }

    #[test]
    fn blank_description_falls_back() {
        let s = parse_scl("\n1\n2/1\n").unwrap();
        assert_eq!(s.name, "imported");
    }

    #[test]
    fn errors_are_specific() {
        assert!(matches!(parse_scl(""), Err(SclError::Truncated)));
        assert!(matches!(parse_scl("desc\n"), Err(SclError::Truncated)));
        assert!(matches!(parse_scl("desc\n3\n100.0\n"), Err(SclError::Truncated)));
        assert!(matches!(parse_scl("desc\nxyz\n"), Err(SclError::BadCount(_))));
        assert!(matches!(parse_scl("desc\n1\nbanana\n"), Err(SclError::BadPitch(_))));
        assert!(matches!(parse_scl("desc\n1\n-3/2\n"), Err(SclError::OutOfRange(_))));
        assert!(matches!(parse_scl("desc\n1\n0/5\n"), Err(SclError::OutOfRange(_))));
        // negative cents period
        assert!(matches!(parse_scl("desc\n1\n-700.0\n"), Err(SclError::BadPeriod)));
        // a degree above the period
        assert!(matches!(
            parse_scl("desc\n2\n1900.0\n1200.0\n"),
            Err(SclError::OutOfRange(_))
        ));
    }

    #[test]
    fn load_scl_reads_files_and_names_from_stem() {
        let dir = std::env::temp_dir();
        let path = dir.join("los_test_tuning.scl");
        std::fs::write(&path, "\n2\n 3/2\n 2/1\n").unwrap();
        let s = load_scl(&path).unwrap();
        assert_eq!(s.name, "los_test_tuning");
        assert_eq!(s.len(), 2);
        std::fs::remove_file(&path).ok();
        assert!(matches!(
            load_scl(Path::new("/nonexistent/los.scl")),
            Err(SclError::Io(_))
        ));
    }
}
