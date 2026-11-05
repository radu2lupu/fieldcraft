//! Ranking metrics and the paired bootstrap. Pure functions over ranked id lists.

/// Per-query scores for one arm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueryScore {
    pub hit1: f64,
    pub hit3: f64,
    pub hit5: f64,
    pub hit10: f64,
    pub rr: f64,
    pub recall10: f64,
}

pub const METRIC_NAMES: [&str; 6] = ["hit@1", "hit@3", "hit@5", "hit@10", "MRR", "Recall@10"];

impl QueryScore {
    pub fn as_array(&self) -> [f64; 6] {
        [self.hit1, self.hit3, self.hit5, self.hit10, self.rr, self.recall10]
    }
}

/// 1-based rank of the best cited id in `ranked`, if any.
pub fn best_rank(ranked: &[String], cited: &[String]) -> Option<usize> {
    ranked
        .iter()
        .position(|id| cited.contains(id))
        .map(|p| p + 1)
}

/// Score one query. `ranked` is the arm's full ranking (deepest cutoff is its length).
pub fn score_query(ranked: &[String], cited: &[String]) -> QueryScore {
    let best = best_rank(ranked, cited);
    let at = |k: usize| f64::from(u8::from(best.is_some_and(|r| r <= k)));
    let found10 = cited
        .iter()
        .filter(|c| ranked.iter().take(10).any(|r| r == *c))
        .count();
    QueryScore {
        hit1: at(1),
        hit3: at(3),
        hit5: at(5),
        hit10: at(10),
        rr: best.map_or(0.0, |r| 1.0 / r as f64),
        recall10: if cited.is_empty() {
            0.0
        } else {
            found10 as f64 / cited.len() as f64
        },
    }
}

/// Column means of the six metrics.
pub fn mean(scores: &[QueryScore]) -> [f64; 6] {
    let mut sum = [0.0; 6];
    for s in scores {
        for (acc, v) in sum.iter_mut().zip(s.as_array()) {
            *acc += v;
        }
    }
    let n = scores.len().max(1) as f64;
    sum.map(|v| v / n)
}

/// SplitMix64: tiny deterministic PRNG so the bootstrap is reproducible with no dependency.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Paired bootstrap of `arm - base` per metric: the mean delta and a percentile 95% CI.
/// Both slices are per-query and index-aligned. Same resampled indices for every metric.
pub fn paired_bootstrap(
    arm: &[QueryScore],
    base: &[QueryScore],
    draws: usize,
    seed: u64,
) -> [(f64, f64, f64); 6] {
    assert_eq!(arm.len(), base.len(), "paired bootstrap needs aligned rows");
    let n = arm.len();
    let diffs: Vec<[f64; 6]> = arm
        .iter()
        .zip(base)
        .map(|(a, b)| {
            let (a, b) = (a.as_array(), b.as_array());
            std::array::from_fn(|i| a[i] - b[i])
        })
        .collect();
    let point: [f64; 6] = std::array::from_fn(|i| diffs.iter().map(|d| d[i]).sum::<f64>() / n.max(1) as f64);
    let mut rng = SplitMix64(seed);
    let mut samples: Vec<Vec<f64>> = (0..6).map(|_| Vec::with_capacity(draws)).collect();
    for _ in 0..draws {
        let mut sum = [0.0; 6];
        for _ in 0..n {
            let d = &diffs[rng.below(n)];
            for (acc, v) in sum.iter_mut().zip(d) {
                *acc += v;
            }
        }
        for (i, s) in samples.iter_mut().enumerate() {
            s.push(sum[i] / n as f64);
        }
    }
    std::array::from_fn(|i| {
        samples[i].sort_by(|a, b| a.total_cmp(b));
        let lo = samples[i][(draws as f64 * 0.025) as usize];
        let hi = samples[i][((draws as f64 * 0.975) as usize).min(draws - 1)];
        (point[i], lo, hi)
    })
}

/// Percentile (nearest-rank) of a sample, `p` in 0..=100.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[idx.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hit_mrr_recall_on_toy_case() {
        let ranked = ids(&["a", "b", "c", "d", "e"]);
        let s = score_query(&ranked, &ids(&["c", "z"]));
        assert_eq!((s.hit1, s.hit3, s.hit5, s.hit10), (0.0, 1.0, 1.0, 1.0));
        assert!((s.rr - 1.0 / 3.0).abs() < 1e-12);
        assert!((s.recall10 - 0.5).abs() < 1e-12);
        let top = score_query(&ranked, &ids(&["a"]));
        assert_eq!((top.hit1, top.rr, top.recall10), (1.0, 1.0, 1.0));
        let miss = score_query(&ranked, &ids(&["zz"]));
        assert_eq!((miss.hit10, miss.rr, miss.recall10), (0.0, 0.0, 0.0));
    }

    #[test]
    fn rank_beyond_ten_counts_for_mrr_not_hit10() {
        let ranked: Vec<String> = (0..20).map(|i| format!("x{i}")).collect();
        let s = score_query(&ranked, &ids(&["x14"]));
        assert_eq!((s.hit10, s.recall10), (0.0, 0.0));
        assert!((s.rr - 1.0 / 15.0).abs() < 1e-12);
    }

    #[test]
    fn bootstrap_identical_arms_is_zero_and_better_arm_is_positive() {
        let hit = QueryScore { hit1: 1.0, hit3: 1.0, hit5: 1.0, hit10: 1.0, rr: 1.0, recall10: 1.0 };
        let miss = QueryScore { hit1: 0.0, hit3: 0.0, hit5: 0.0, hit10: 0.0, rr: 0.0, recall10: 0.0 };
        let base: Vec<_> = (0..50).map(|i| if i % 2 == 0 { hit } else { miss }).collect();
        for (d, lo, hi) in paired_bootstrap(&base, &base, 200, 7) {
            assert_eq!((d, lo, hi), (0.0, 0.0, 0.0));
        }
        let better = vec![hit; 50];
        let res = paired_bootstrap(&better, &base, 2000, 7);
        assert!((res[0].0 - 0.5).abs() < 1e-12);
        assert!(res[0].1 > 0.3 && res[0].2 <= 0.7, "CI {:?}", res[0]);
        // Deterministic for a fixed seed.
        assert_eq!(res, paired_bootstrap(&better, &base, 2000, 7));
    }

    #[test]
    fn percentile_nearest_rank() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&v, 50.0), 5.0);
        assert_eq!(percentile(&v, 95.0), 10.0);
    }
}
