//! Tiny deterministic PRNG (splitmix64).
//!
//! The pattern generators in [`crate::theory::gen`] need a stream of
//! reproducible randomness so that the same seed always yields the same
//! fill — that's what makes "regenerate" and undo/redo behave musically
//! instead of chaotically. The crate intentionally has no `rand`
//! dependency; splitmix64 is tiny, fast, and statistically more than
//! good enough for picking steps and nudging velocities.

/// Deterministic splitmix64 PRNG — the crate intentionally has no `rand` dep.
///
/// Same seed, same sequence, on every platform. Cloning an [`Rng`] forks
/// the stream at its current position.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Create a generator from a seed. Any seed (including 0) is fine.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next raw 64-bit output of the splitmix64 sequence.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `[0, n)`.
    ///
    /// Uses a widening multiply (`(next_u64 * n) >> 64`), which is free of
    /// modulo bias for our purposes (`n` is always tiny next to 2^64 —
    /// step counts, weight totals, and the like). `n == 0` returns 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// `true` with probability `p`, clamped to `[0, 1]`.
    ///
    /// `chance(0.0)` is always `false`, `chance(1.0)` is always `true` —
    /// handy for probability-gated triggers at the extremes.
    pub fn chance(&mut self, p: f32) -> bool {
        let p = p.clamp(0.0, 1.0);
        self.f32() < p
    }

    /// Uniform `f32` in `[0, 1)` (24 bits of precision).
    pub fn f32(&mut self) -> f32 {
        // Top 24 bits scaled by 2^-24: every value is exactly representable
        // and the result is strictly below 1.0.
        (self.next_u64() >> 40) as f32 * (1.0 / 16_777_216.0)
    }

    /// Pick a uniformly random element of `xs`, or `None` if it's empty.
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> Option<&'a T> {
        if xs.is_empty() {
            None
        } else {
            xs.get(self.below(xs.len() as u64) as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_splitmix64_vectors() {
        // Reference outputs computed independently from the splitmix64 spec.
        let mut r = Rng::new(1234567);
        assert_eq!(r.next_u64(), 6457827717110365317);
        assert_eq!(r.next_u64(), 3203168211198807973);
        assert_eq!(r.next_u64(), 9817491932198370423);
        assert_eq!(r.next_u64(), 4593380528125082431);
        assert_eq!(r.next_u64(), 16408922859458223821);

        let mut r0 = Rng::new(0);
        assert_eq!(r0.next_u64(), 16294208416658607535);
        assert_eq!(r0.next_u64(), 7960286522194355700);
        assert_eq!(r0.next_u64(), 487617019471545679);
    }

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(0xDEAD_BEEF);
        let mut b = Rng::new(0xDEAD_BEEF);
        let xs: Vec<u64> = (0..100).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..100).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);
    }

    #[test]
    fn different_seeds_differ() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let xs: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        assert_ne!(xs, ys);
    }

    #[test]
    fn clone_forks_stream() {
        let mut a = Rng::new(7);
        a.next_u64();
        let mut b = a.clone();
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn below_zero_is_zero() {
        let mut r = Rng::new(42);
        assert_eq!(r.below(0), 0);
        assert_eq!(r.below(0), 0);
    }

    #[test]
    fn below_one_is_zero() {
        let mut r = Rng::new(42);
        for _ in 0..50 {
            assert_eq!(r.below(1), 0);
        }
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = Rng::new(99);
        for n in [2u64, 3, 7, 16, 100, 1 << 40] {
            for _ in 0..200 {
                assert!(r.below(n) < n, "below({n}) escaped its range");
            }
        }
    }

    #[test]
    fn below_covers_all_values() {
        let mut r = Rng::new(5);
        let mut seen = [false; 4];
        for _ in 0..200 {
            seen[r.below(4) as usize] = true;
        }
        assert!(
            seen.iter().all(|&s| s),
            "below(4) never produced some value"
        );
    }

    #[test]
    fn chance_extremes() {
        let mut r = Rng::new(11);
        assert!((0..100).all(|_| !r.chance(0.0)));
        assert!((0..100).all(|_| r.chance(1.0)));
        // Out-of-range probabilities clamp.
        assert!((0..100).all(|_| !r.chance(-3.0)));
        assert!((0..100).all(|_| r.chance(2.0)));
    }

    #[test]
    fn chance_half_is_roughly_half() {
        let mut r = Rng::new(13);
        let hits = (0..2000).filter(|_| r.chance(0.5)).count();
        assert!((800..1200).contains(&hits), "chance(0.5) hit {hits}/2000");
    }

    #[test]
    fn f32_in_unit_interval() {
        let mut r = Rng::new(17);
        for _ in 0..1000 {
            let x = r.f32();
            assert!((0.0..1.0).contains(&x), "f32() produced {x}");
        }
    }

    #[test]
    fn f32_varies() {
        let mut r = Rng::new(19);
        let xs: Vec<f32> = (0..32).map(|_| r.f32()).collect();
        assert!(xs.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn pick_empty_is_none() {
        let mut r = Rng::new(23);
        let empty: [u8; 0] = [];
        assert_eq!(r.pick(&empty), None);
    }

    #[test]
    fn pick_singleton() {
        let mut r = Rng::new(23);
        assert_eq!(r.pick(&[42u8]), Some(&42));
    }

    #[test]
    fn pick_stays_in_slice_and_covers_it() {
        let mut r = Rng::new(29);
        let xs = [10u8, 20, 30];
        let mut seen = [false; 3];
        for _ in 0..200 {
            let p = r.pick(&xs);
            assert!(p.is_some_and(|v| xs.contains(v)));
            if let Some(&v) = p {
                seen[(v / 10 - 1) as usize] = true;
            }
        }
        assert!(seen.iter().all(|&s| s), "pick never returned some element");
    }
}
