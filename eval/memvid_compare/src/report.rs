//! Markdown tables shared by every eval command: arm means and paired-bootstrap deltas.

use crate::metrics::{self, METRIC_NAMES, QueryScore};
use std::collections::HashMap;

pub const BOOTSTRAP_DRAWS: usize = 2000;
pub const BOOTSTRAP_SEED: u64 = 20_261_027;

pub type Scores<'a> = HashMap<&'a str, Vec<QueryScore>>;

/// `(delta, lo, hi)` of a paired-bootstrap CI for each of the six metrics.
pub type MetricCis = [(f64, f64, f64); 6];

/// One arm name with its CIs against the base arm.
pub type ArmDelta = (String, MetricCis);

fn header(first: &str) -> String {
    format!("| {first} | {} |\n|---|{}\n", METRIC_NAMES.join(" | "), "---|".repeat(METRIC_NAMES.len()))
}

/// Mean of each metric per arm, in the order of `arms`.
pub fn mean_table(title: &str, n: usize, arms: &[&str], scores: &Scores) -> String {
    let mut md = format!("\n### {title} (n = {n})\n\n{}", header("arm"));
    for a in arms {
        let m = metrics::mean(&scores[a]);
        md.push_str(&format!("| {a} | {} |\n", m.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(" | ")));
    }
    md
}

/// Per-metric deltas against `base` as `(delta, lo, hi)` for every arm except `base`.
pub fn deltas(arms: &[&str], scores: &Scores, base: &str) -> Vec<ArmDelta> {
    arms.iter()
        .filter(|a| **a != base)
        .map(|a| (a.to_string(), metrics::paired_bootstrap(&scores[a], &scores[base], BOOTSTRAP_DRAWS, BOOTSTRAP_SEED)))
        .collect()
}

pub fn delta_table(deltas: &[ArmDelta], base: &str) -> String {
    let mut md = format!(
        "\nPaired bootstrap vs {base} ({BOOTSTRAP_DRAWS} draws, seed {BOOTSTRAP_SEED}): delta [95% CI]\n\n{}",
        header("arm")
    );
    for (a, ci) in deltas {
        md.push_str(&format!("| {a} | {} |\n", ci.iter().map(|(d, lo, hi)| format!("{d:+.3} [{lo:+.3}, {hi:+.3}]")).collect::<Vec<_>>().join(" | ")));
    }
    md
}

/// "nav better" / "hybrid better" / "no difference" from the CI of `arm - base`.
pub fn verdict(ci: (f64, f64, f64), arm_name: &str, base_name: &str) -> String {
    if ci.1 > 0.0 {
        format!("{arm_name} better")
    } else if ci.2 < 0.0 {
        format!("{base_name} better")
    } else {
        "no difference".to_string()
    }
}

pub fn verdict_table(deltas: &[ArmDelta], base: &str) -> String {
    let mut md = format!("\nVerdict per metric (95% CI of the paired delta excludes 0, else \"no difference\"; base = {base}):\n\n{}", header("arm"));
    for (a, ci) in deltas {
        md.push_str(&format!("| {a} | {} |\n", ci.iter().map(|c| verdict(*c, a, base)).collect::<Vec<_>>().join(" | ")));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_follows_ci_sign() {
        assert_eq!(verdict((0.05, 0.01, 0.09), "nav", "hybrid"), "nav better");
        assert_eq!(verdict((-0.05, -0.09, -0.01), "nav", "hybrid"), "hybrid better");
        assert_eq!(verdict((0.0, -0.02, 0.02), "nav", "hybrid"), "no difference");
    }

    #[test]
    fn header_has_one_separator_per_column() {
        let h = header("arm");
        let seps = h.lines().nth(1).unwrap();
        assert_eq!(seps.matches("---").count(), 1 + METRIC_NAMES.len());
    }
}
