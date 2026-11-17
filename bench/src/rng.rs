//! Deterministic pseudo-random numbers with fixed seeds (no `rand` crate, so the
//! sequences never change with a dependency update).
//!
//! SplitMix64 seeds xoshiro256**, as recommended by their authors
//! (<https://prng.di.unimi.it/>). Integer arithmetic only: identical output on
//! every architecture.

/// Seed of everything the bench generates unless a case says otherwise.
pub const DEFAULT_SEED: u64 = 0x4e75_7669_7261_4842; // "NuviraHB"

/// SplitMix64: a 64-bit generator used to expand one seed into many.
#[derive(Clone, Debug)]
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// Stateless hash of a 64-bit value (one SplitMix64 step): per-pixel or per-index
/// noise without carrying a generator.
pub fn mix64(x: u64) -> u64 {
    SplitMix64(x).next_u64()
}

/// xoshiro256**: the bench's main generator.
#[derive(Clone, Debug)]
pub struct Xoshiro256 {
    s: [u64; 4],
}

impl Xoshiro256 {
    /// State filled from `seed` with SplitMix64.
    pub fn new(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        let s = [sm.next_u64(), sm.next_u64(), sm.next_u64(), sm.next_u64()];
        Self::from_state(s)
    }

    /// Raw state (tests against the reference implementation). Must not be all zero.
    pub fn from_state(s: [u64; 4]) -> Self {
        assert!(s.iter().any(|&x| x != 0), "xoshiro256** state must not be all zero");
        Self { s }
    }

    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform in `0..n` (Lemire's multiply-shift; the tiny bias is irrelevant here
    /// and keeps the sequence a fixed function of the seed). `n` must be > 0.
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(hi > lo);
        lo + self.below((hi - lo) as u64) as i64
    }

    /// Uniform in `[0, 1)` with 53 random bits (exact on every IEEE-754 machine).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }

    /// Fills `buf` with random bytes (little-endian words: architecture independent).
    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        let (chunks, rest) = buf.as_chunks_mut::<8>();
        for c in chunks {
            *c = self.next_u64().to_le_bytes();
        }
        if !rest.is_empty() {
            let v = self.next_u64().to_le_bytes();
            rest.copy_from_slice(&v[..rest.len()]);
        }
    }

    /// `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0; n];
        self.fill_bytes(&mut v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_reference_vector() {
        // Seed 1234567, first outputs of the reference C implementation (Vigna).
        let mut g = SplitMix64::new(1234567);
        let got: Vec<u64> = (0..5).map(|_| g.next_u64()).collect();
        assert_eq!(
            got,
            [
                6457827717110365317,
                3203168211198807973,
                9817491932198370423,
                4593380528125082431,
                16408922859458223821
            ]
        );
    }

    #[test]
    fn xoshiro256_reference_vector() {
        // State {1, 2, 3, 4}: outputs of the reference C implementation.
        let mut g = Xoshiro256::from_state([1, 2, 3, 4]);
        let got: Vec<u64> = (0..4).map(|_| g.next_u64()).collect();
        assert_eq!(got, [11520, 0, 1509978240, 1215971899390074240]);
    }

    #[test]
    fn same_seed_same_sequence() {
        let a = Xoshiro256::new(DEFAULT_SEED).bytes(1001);
        let b = Xoshiro256::new(DEFAULT_SEED).bytes(1001);
        assert_eq!(a, b);
        assert_ne!(a, Xoshiro256::new(DEFAULT_SEED + 1).bytes(1001));
    }

    #[test]
    fn below_and_f64_in_range() {
        let mut g = Xoshiro256::new(7);
        for _ in 0..10_000 {
            assert!(g.below(10) < 10);
            let f = g.next_f64();
            assert!((0.0..1.0).contains(&f));
        }
    }
}
