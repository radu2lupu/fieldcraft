// usefulness-bootstrap: paired bootstrap CI calculator for the G1 warm-vs-cold outcome harness.
//
// Reads a JSON array of per-task paired records (one warm + one cold observation per task,
// produced by the bash orchestrator after real codex runs + real pytest oracle checks) and
// computes warm-minus-cold deltas for resolved-rate, steps, and tokens, each with a paired
// bootstrap 95% confidence interval (resample TASKS with replacement, fixed draws, fixed seed).
//
// No floating-point nondeterminism beyond IEEE754 summation order, which is fixed because we
// always iterate resampled indices in the same order for a given seed. No system time, no OS
// randomness: the only randomness source is the user-supplied --seed, expanded via SplitMix64.
//
// Usage:
//   usefulness-bootstrap \
//     --input tasks.json \
//     --seed 20260630 \
//     --draws 10000 \
//     --out scorecard.json
//
// Input schema (array of objects), one entry per HELD-OUT task that actually completed both
// a warm and a cold run:
//   {
//     "task_id": "pytest-dev__pytest-5809",
//     "warm_resolved": true,  "warm_steps": 12, "warm_tokens": 18342,
//     "cold_resolved": false, "cold_steps": 9,  "cold_tokens": 9213
//   }

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::process::ExitCode;

#[derive(Debug, Deserialize, Clone)]
struct PairedTask {
    task_id: String,
    warm_resolved: bool,
    warm_steps: f64,
    warm_tokens: f64,
    cold_resolved: bool,
    cold_steps: f64,
    cold_tokens: f64,
}

#[derive(Debug, Serialize)]
struct MetricResult {
    point_estimate: f64,
    ci_low: f64,
    ci_high: f64,
    n: usize,
    draws: u64,
    seed: u64,
}

#[derive(Debug, Serialize)]
struct Scorecard {
    n_tasks: usize,
    seed: u64,
    draws: u64,
    resolved_delta: MetricResult,
    steps_delta: MetricResult,
    tokens_delta: MetricResult,
    per_task: Vec<PairedTaskEcho>,
}

#[derive(Debug, Serialize)]
struct PairedTaskEcho {
    task_id: String,
    warm_resolved: bool,
    cold_resolved: bool,
    warm_steps: f64,
    cold_steps: f64,
    warm_tokens: f64,
    cold_tokens: f64,
}

/// SplitMix64: deterministic, dependency-free PRNG. Same seed -> same stream, forever,
/// on any platform. This is the only source of "randomness" in the whole binary.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform index in [0, bound) via Lemire's method (no modulo bias for the bound sizes
    /// we use here; bound is always small, a handful to a few dozen tasks).
    fn next_index(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        let bound_u64 = bound as u64;
        loop {
            let r = self.next_u64();
            let m = (r as u128).wrapping_mul(bound_u64 as u128);
            let lo = m as u64;
            if lo < bound_u64.wrapping_neg() % bound_u64 {
                continue;
            }
            return (m >> 64) as usize;
        }
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Paired bootstrap: resample task INDICES with replacement (same resampled index used for
/// both warm and cold value of that draw, preserving the pairing), recompute the warm-minus-
/// cold delta on each resampled set, then take the 2.5th/97.5th percentiles of the resulting
/// distribution as the 95% CI. This is the standard paired-bootstrap design: it never mixes
/// a warm value from one task with a cold value from a different task.
fn paired_bootstrap_delta(
    warm: &[f64],
    cold: &[f64],
    seed: u64,
    draws: u64,
) -> (f64, f64, f64) {
    let n = warm.len();
    assert_eq!(n, cold.len(), "paired arrays must be equal length");
    let point = mean(warm) - mean(cold);

    if n == 0 {
        return (point, point, point);
    }

    let mut rng = SplitMix64::new(seed);
    let mut deltas: Vec<f64> = Vec::with_capacity(draws as usize);
    let mut resample_warm = vec![0.0_f64; n];
    let mut resample_cold = vec![0.0_f64; n];

    for _ in 0..draws {
        for slot in 0..n {
            let idx = rng.next_index(n);
            resample_warm[slot] = warm[idx];
            resample_cold[slot] = cold[idx];
        }
        deltas.push(mean(&resample_warm) - mean(&resample_cold));
    }

    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let lo_idx = ((draws as f64) * 0.025).floor() as usize;
    let hi_idx = (((draws as f64) * 0.975).ceil() as usize).min(deltas.len() - 1);
    (point, deltas[lo_idx], deltas[hi_idx])
}

fn parse_args() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let key = args[i].trim_start_matches('-').to_string();
        if i + 1 < args.len() && !args[i + 1].starts_with("--") {
            out.insert(key, args[i + 1].clone());
            i += 2;
        } else {
            out.insert(key, "true".to_string());
            i += 1;
        }
    }
    out
}

fn main() -> ExitCode {
    let args = parse_args();

    let input_path = match args.get("input") {
        Some(p) => p,
        None => {
            eprintln!("usage: usefulness-bootstrap --input <paired_tasks.json> --seed <u64> --draws <n> --out <scorecard.json>");
            return ExitCode::FAILURE;
        }
    };
    let seed: u64 = match args.get("seed").and_then(|s| s.parse().ok()) {
        Some(s) => s,
        None => {
            eprintln!("error: --seed <u64> is required (fixed seed, no system time/random allowed)");
            return ExitCode::FAILURE;
        }
    };
    let draws: u64 = args
        .get("draws")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    let raw = match fs::read_to_string(input_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {input_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let tasks: Vec<PairedTask> = match serde_json::from_str(&raw) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error parsing {input_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if tasks.is_empty() {
        eprintln!("error: input has zero paired tasks; nothing to compute");
        return ExitCode::FAILURE;
    }

    let warm_resolved: Vec<f64> = tasks
        .iter()
        .map(|t| if t.warm_resolved { 1.0 } else { 0.0 })
        .collect();
    let cold_resolved: Vec<f64> = tasks
        .iter()
        .map(|t| if t.cold_resolved { 1.0 } else { 0.0 })
        .collect();
    let warm_steps: Vec<f64> = tasks.iter().map(|t| t.warm_steps).collect();
    let cold_steps: Vec<f64> = tasks.iter().map(|t| t.cold_steps).collect();
    let warm_tokens: Vec<f64> = tasks.iter().map(|t| t.warm_tokens).collect();
    let cold_tokens: Vec<f64> = tasks.iter().map(|t| t.cold_tokens).collect();

    // Use distinct sub-seeds per metric (derived deterministically from the base seed) so the
    // three bootstrap distributions are independent draws, not perfectly correlated reuses of
    // the same resampled-index sequence.
    let (r_point, r_lo, r_hi) =
        paired_bootstrap_delta(&warm_resolved, &cold_resolved, seed ^ 0x5245534F4C5645, draws);
    let (s_point, s_lo, s_hi) =
        paired_bootstrap_delta(&warm_steps, &cold_steps, seed ^ 0x5354455053000000, draws);
    let (k_point, k_lo, k_hi) =
        paired_bootstrap_delta(&warm_tokens, &cold_tokens, seed ^ 0x544F4B454E530000, draws);

    let scorecard = Scorecard {
        n_tasks: tasks.len(),
        seed,
        draws,
        resolved_delta: MetricResult {
            point_estimate: r_point,
            ci_low: r_lo,
            ci_high: r_hi,
            n: tasks.len(),
            draws,
            seed,
        },
        steps_delta: MetricResult {
            point_estimate: s_point,
            ci_low: s_lo,
            ci_high: s_hi,
            n: tasks.len(),
            draws,
            seed,
        },
        tokens_delta: MetricResult {
            point_estimate: k_point,
            ci_low: k_lo,
            ci_high: k_hi,
            n: tasks.len(),
            draws,
            seed,
        },
        per_task: tasks
            .iter()
            .map(|t| PairedTaskEcho {
                task_id: t.task_id.clone(),
                warm_resolved: t.warm_resolved,
                cold_resolved: t.cold_resolved,
                warm_steps: t.warm_steps,
                cold_steps: t.cold_steps,
                warm_tokens: t.warm_tokens,
                cold_tokens: t.cold_tokens,
            })
            .collect(),
    };

    let pretty = serde_json::to_string_pretty(&scorecard).unwrap();
    println!("{pretty}");

    if let Some(out_path) = args.get("out") {
        if let Err(e) = fs::write(out_path, &pretty) {
            eprintln!("warning: failed to write {out_path}: {e}");
        }
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_is_deterministic() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn next_index_stays_in_bound() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..10_000 {
            let idx = rng.next_index(5);
            assert!(idx < 5);
        }
    }

    #[test]
    fn paired_bootstrap_zero_delta_when_warm_equals_cold() {
        let v = vec![1.0, 0.0, 1.0, 1.0, 0.0];
        let (point, lo, hi) = paired_bootstrap_delta(&v, &v, 123, 2000);
        assert_eq!(point, 0.0);
        assert!(lo <= 0.0 && hi >= 0.0);
    }

    #[test]
    fn paired_bootstrap_detects_clear_positive_delta() {
        let warm = vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        let cold = vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let (point, lo, _hi) = paired_bootstrap_delta(&warm, &cold, 99, 5000);
        assert_eq!(point, 1.0);
        // All-warm-wins, all-cold-loses with no variance: CI should be degenerate at 1.0.
        assert!(lo > 0.9);
    }

    #[test]
    fn paired_bootstrap_same_seed_same_result() {
        let warm = vec![1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0];
        let cold = vec![0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0];
        let r1 = paired_bootstrap_delta(&warm, &cold, 555, 3000);
        let r2 = paired_bootstrap_delta(&warm, &cold, 555, 3000);
        assert_eq!(r1, r2);
    }
}
