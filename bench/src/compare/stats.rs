//! Statistics of the A/B comparison (plan section 9.3): paired round ratios,
//! bootstrap confidence interval of their median, noise from an A-vs-A run.

use crate::measure::{median, percentile};

/// Resamples of the bootstrap (plan: 10,000, fixed seed from thresholds.json).
pub const BOOTSTRAP_RESAMPLES: usize = 10_000;

/// A tiny SplitMix64, local so the bootstrap never depends on another module's
/// generator choices (the confidence intervals must be reproducible forever).
struct Mix(u64);

impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        // n is tiny (rounds): the modulo bias is irrelevant here.
        (self.next() % n as u64) as usize
    }
}

/// 95% bootstrap confidence interval of the median of `values` (percentile
/// method, `resamples` resamples, generator seeded with `seed`). One value gives
/// `[v, v]`; none gives NaN.
pub fn bootstrap_ci95_median(values: &[f64], resamples: usize, seed: u64) -> (f64, f64) {
    let v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    match v.len() {
        0 => return (f64::NAN, f64::NAN),
        1 => return (v[0], v[0]),
        _ => {}
    }
    let mut rng = Mix(seed);
    let mut medians = Vec::with_capacity(resamples);
    let mut sample = vec![0.0; v.len()];
    for _ in 0..resamples {
        for s in sample.iter_mut() {
            *s = v[rng.below(v.len())];
        }
        medians.push(median(&sample));
    }
    (percentile(&medians, 2.5), percentile(&medians, 97.5))
}

/// Median absolute deviation (raw).
pub fn mad(values: &[f64]) -> f64 {
    let m = median(values);
    let dev: Vec<f64> = values.iter().filter(|x| x.is_finite()).map(|x| (x - m).abs()).collect();
    median(&dev)
}

/// Ratio new/base per paired round. Rounds present on one side only are left
/// out; a base of 0 gives no ratio.
pub fn paired_ratios(base: &[(u32, f64)], new: &[(u32, f64)]) -> Vec<f64> {
    let mut out = Vec::new();
    for (k, b) in base {
        if let Some((_, n)) = new.iter().find(|(kn, _)| kn == k)
            && b.is_finite()
            && n.is_finite()
            && *b != 0.0
        {
            out.push(n / b);
        }
    }
    out
}

/// Ratios when rounds cannot be paired (different round numbers): every new
/// value against the base median.
pub fn unpaired_ratios(base: &[(u32, f64)], new: &[(u32, f64)]) -> Vec<f64> {
    let b = median(&base.iter().map(|x| x.1).collect::<Vec<_>>());
    if !b.is_finite() || b == 0.0 {
        return Vec::new();
    }
    new.iter().filter(|x| x.1.is_finite()).map(|x| x.1 / b).collect()
}

/// Noise of one metric from an A-vs-A' run, in percent: the larger of the 95th
/// percentile of |ratio - 1| and 2 x MAD / median of the pooled values.
pub fn noise_pct(ratios: &[f64], pooled: &[f64]) -> f64 {
    let dev: Vec<f64> = ratios.iter().filter(|x| x.is_finite()).map(|r| (r - 1.0).abs()).collect();
    let a = if dev.is_empty() { 0.0 } else { percentile(&dev, 95.0) };
    let m = median(pooled);
    let b = if m.is_finite() && m != 0.0 { 2.0 * mad(pooled) / m.abs() } else { 0.0 };
    100.0 * a.max(b)
}

/// Absolute noise (same unit as the metric): for metrics compared by absolute
/// difference (PSNR in dB, SSIM, fractions).
pub fn noise_abs(base: &[f64], new: &[f64]) -> f64 {
    let mut pooled: Vec<f64> = base.to_vec();
    pooled.extend_from_slice(new);
    let d = (median(base) - median(new)).abs();
    let spread = 2.0 * mad(&pooled);
    d.max(spread)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_of_constant_values_is_a_point() {
        let (lo, hi) = bootstrap_ci95_median(&[1.0, 1.0, 1.0, 1.0, 1.0], 1000, 1);
        assert_eq!((lo, hi), (1.0, 1.0));
    }

    #[test]
    fn ci_contains_the_median_and_is_reproducible() {
        let v = [1.01, 0.99, 1.05, 1.02, 0.98, 1.03, 1.00];
        let (lo, hi) = bootstrap_ci95_median(&v, BOOTSTRAP_RESAMPLES, 1);
        assert!(lo <= median(&v) && median(&v) <= hi, "{lo} {hi}");
        assert_eq!(bootstrap_ci95_median(&v, BOOTSTRAP_RESAMPLES, 1), (lo, hi));
    }

    #[test]
    fn ratios_pair_by_round() {
        let a = [(1, 10.0), (2, 20.0), (3, 30.0)];
        let b = [(2, 22.0), (1, 11.0), (4, 1.0)];
        assert_eq!(paired_ratios(&a, &b), vec![1.1, 1.1]);
        assert_eq!(unpaired_ratios(&a, &b).len(), 3);
    }

    #[test]
    fn noise_takes_the_larger_estimate() {
        // |r - 1| at most 2%; pooled spread 0 -> 2%.
        let n = noise_pct(&[1.0, 1.02, 0.99], &[5.0, 5.0, 5.0, 5.0]);
        assert!((n - 2.0).abs() < 0.1, "{n}");
        // pooled MAD 1 over median 10 -> 20%.
        let n = noise_pct(&[1.0], &[9.0, 10.0, 11.0]);
        assert!((n - 20.0).abs() < 1e-9, "{n}");
    }
}
