//! Minimal seedable PRNG so [`super::select`] and [`super::rotation`] never
//! need to add an external `rand` crate dependency (none exists anywhere in
//! this workspace's `Cargo.toml` today).
//!
//! This is **not** cryptographically secure and does **not** reproduce
//! Python's Mersenne-Twister-based `random.Random` bit-for-bit — the
//! selection algorithm only uses randomness to shuffle/tie-break among
//! *already-eligible* candidates, so cross-implementation parity is judged
//! on the deterministic tiers (which account got picked, never "the same
//! shuffle order as Python for a given seed"). See the conformance suite for
//! how this is tested.

/// SplitMix64 — a small, fast, well-distributed generator good enough for
/// shuffles and tie-breaking. <https://prng.di.unimi.it/splitmix64.c>
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Seed deterministically (for reproducible tests).
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Seed from process/time entropy (for production use). Good enough for
    /// non-adversarial tie-breaking; not suitable for security purposes.
    #[must_use]
    pub fn from_entropy() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pid = u64::from(std::process::id());
        // A stack address adds a little extra per-thread jitter without
        // pulling in a random-source crate.
        let stack_addr = &nanos as *const u64 as u64;
        Self::seeded(nanos ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ stack_addr)
    }

    /// Advance and return the next `u64`.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Return a uniform value in `[0, n)`. Panics if `n == 0`.
    pub fn gen_range(&mut self, n: usize) -> usize {
        assert!(n > 0, "gen_range requires n > 0");
        (self.next_u64() % n as u64) as usize
    }

    /// Fisher-Yates in-place shuffle.
    pub fn shuffle<T>(&mut self, slice: &mut [T]) {
        if slice.len() < 2 {
            return;
        }
        for i in (1..slice.len()).rev() {
            let j = self.gen_range(i + 1);
            slice.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_is_deterministic() {
        let mut a = Rng::seeded(42);
        let mut b = Rng::seeded(42);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::seeded(1);
        let mut b = Rng::seeded(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn gen_range_stays_in_bounds() {
        let mut rng = Rng::seeded(7);
        for _ in 0..1000 {
            let v = rng.gen_range(5);
            assert!(v < 5);
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut rng = Rng::seeded(99);
        let mut v: Vec<i32> = (0..20).collect();
        let original = v.clone();
        rng.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, original);
    }

    #[test]
    fn shuffle_single_element_is_noop() {
        let mut rng = Rng::seeded(1);
        let mut v = vec![42];
        rng.shuffle(&mut v);
        assert_eq!(v, vec![42]);
    }
}
