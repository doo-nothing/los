//! Shared key-handling helpers implementing the keybinding doctrine
//! (docs/keybindings.md): vi-style count prefixes and coarse adjustments.

/// Accumulates digit presses into a vi-style count prefix.
///
/// Digits push into the buffer; `take()` consumes it (defaulting to 1) when
/// the counted key arrives. A leading `0` is not a count (vi reserves it as
/// a motion), so callers should only `push` a `'0'` when a count is pending.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Count(Option<String>);

impl Count {
    /// Returns true if the digit was consumed as part of a count.
    /// A leading '0' is refused (it's a motion, not a count).
    pub fn push(&mut self, c: char) -> bool {
        if !c.is_ascii_digit() {
            return false;
        }
        if c == '0' && self.0.is_none() {
            return false;
        }
        self.0.get_or_insert_with(String::new).push(c);
        true
    }

    /// Consume the count; defaults to 1 and is never 0.
    #[must_use]
    pub fn take(&mut self) -> usize {
        self.0
            .take()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .max(1)
    }

    pub fn clear(&mut self) {
        self.0 = None;
    }

    pub fn is_pending(&self) -> bool {
        self.0.is_some()
    }

    /// The raw pending digits, for status-line display.
    pub fn pending(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

/// Step a float param by `steps` increments of `fine` (×10 when `coarse`),
/// clamped to `[min, max]`. The standard doctrine adjustment for h/l + H/L.
pub fn step_f32(value: f32, steps: i32, fine: f32, coarse: bool, min: f32, max: f32) -> f32 {
    let unit = if coarse { fine * 10.0 } else { fine };
    (value + unit * steps as f32).clamp(min, max)
}

/// Cycle an enum-like usize through `len` variants by `steps` (negative ok).
pub fn cycle(value: usize, steps: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = len as i64;
    (((value as i64 + steps as i64) % len_i + len_i) % len_i) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_accumulates_digits() {
        let mut c = Count::default();
        assert!(c.push('1'));
        assert!(c.push('2'));
        assert!(c.is_pending());
        assert_eq!(c.pending(), Some("12"));
        assert_eq!(c.take(), 12);
        assert!(!c.is_pending());
    }

    #[test]
    fn count_defaults_to_one() {
        let mut c = Count::default();
        assert_eq!(c.take(), 1);
    }

    #[test]
    fn leading_zero_is_not_a_count() {
        let mut c = Count::default();
        assert!(!c.push('0'));
        assert!(c.push('1'));
        assert!(c.push('0'));
        assert_eq!(c.take(), 10);
    }

    #[test]
    fn non_digit_refused() {
        let mut c = Count::default();
        assert!(!c.push('x'));
        assert!(!c.is_pending());
    }

    #[test]
    fn clear_discards() {
        let mut c = Count::default();
        c.push('5');
        c.clear();
        assert_eq!(c.take(), 1);
    }

    #[test]
    fn step_f32_fine_and_coarse() {
        assert_eq!(step_f32(0.5, 1, 0.05, false, 0.0, 1.0), 0.55);
        assert_eq!(step_f32(0.5, -2, 0.05, false, 0.0, 1.0), 0.4);
        assert_eq!(step_f32(0.5, 1, 0.05, true, 0.0, 1.0), 1.0); // 0.5 + 0.5
        assert_eq!(step_f32(0.9, 5, 0.05, false, 0.0, 1.0), 1.0); // clamped
        assert_eq!(step_f32(0.1, -5, 0.05, false, 0.0, 1.0), 0.0); // clamped
    }

    #[test]
    fn cycle_wraps_both_directions() {
        assert_eq!(cycle(3, 1, 4), 0);
        assert_eq!(cycle(0, -1, 4), 3);
        assert_eq!(cycle(1, 6, 4), 3);
        assert_eq!(cycle(1, -6, 4), 3);
        assert_eq!(cycle(0, 0, 0), 0);
    }
}
