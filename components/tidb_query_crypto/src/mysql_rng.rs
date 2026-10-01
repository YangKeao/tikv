// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Shared legacy MySQL recurrence, not a cryptographically secure generator.
//! This compatibility primitive makes no FIPS claim.

const MAX_RAND_VALUE: u32 = 0x3fff_ffff;

/// Advances raw MySQL seeds using the native public setters' wrapping policy.
/// Inputs are deliberately not normalized before the step. Normalized seeds
/// make 3*seed1+seed2 <= u32::MAX-7, so this also preserves TiKV RAND's
/// original ordinary arithmetic on its narrower constructor-owned domain.
pub fn mysql_rand_step(seed1: &mut u32, seed2: &mut u32) -> f64 {
    *seed1 = seed1.wrapping_mul(3).wrapping_add(*seed2) % MAX_RAND_VALUE;
    *seed2 = seed1.wrapping_add(*seed2).wrapping_add(33) % MAX_RAND_VALUE;
    f64::from(*seed1) / f64::from(MAX_RAND_VALUE)
}

/// The normalized two-seed state shared by RAND and the legacy SQL codec.
#[derive(Clone, Copy)]
pub struct MySqlRand {
    seed1: u32,
    seed2: u32,
}

impl MySqlRand {
    /// Normalizes both seeds into the original MySQL recurrence domain.
    pub fn from_seeds(seed1: u32, seed2: u32) -> Self {
        Self {
            seed1: seed1 % MAX_RAND_VALUE,
            seed2: seed2 % MAX_RAND_VALUE,
        }
    }

    /// Advances the existing TiKV recurrence and returns its fractional value.
    pub fn next_f64(&mut self) -> f64 {
        mysql_rand_step(&mut self.seed1, &mut self.seed2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp)]
    fn original_rand_seed_zero_vectors() {
        // Original impl_math::test_rand_new_with_seed seed=0 vectors;
        // its unchanged seed constructor produces (55555555, 0).
        let mut random = MySqlRand::from_seeds(55555555, 0);
        assert_eq!(random.next_f64(), 0.15522042769493574);
        assert_eq!(random.next_f64(), 0.620881741513388);
        // Hand-derived normalization identity, not recorded provider output.
        let mut normalized = MySqlRand::from_seeds(1, 2);
        let mut wrapped = MySqlRand::from_seeds(MAX_RAND_VALUE + 1, MAX_RAND_VALUE + 2);
        assert_eq!(normalized.next_f64(), wrapped.next_f64());
        // Source-derived raw-setter boundary, hand-calculated rather than
        // recorded: wrapping 3*MAX+MAX = 4*M; 0+MAX+33 wraps to 32.
        let (mut seed1, mut seed2) = (u32::MAX, u32::MAX);
        assert_eq!(mysql_rand_step(&mut seed1, &mut seed2), 0.0);
        assert_eq!((seed1, seed2), (0, 32));
    }
}
