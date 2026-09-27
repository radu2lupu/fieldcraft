//! Conjunctive cue-bundle matching — weighted geometric mean over populated
//! hard cues plus jaccard for set-valued cues, with a mismatch floor.
//!
//! Implements Tulving & Thomson (1973) encoding specificity: recall improves
//! when retrieval cues overlap with cues at encoding. Multiplicative
//! combination on hard cues enforces "conjunctive match required" without
//! brittleness — the floor prevents any populated mismatch from zeroing the
//! score, while unpopulated legacy dimensions do not drag the score down.
//!
//! Phase 1 of cue-bundle retrieval — see `docs/plans/cue-bundle-retrieval.md`.
//! This function is NOT yet wired into the main `retrieve()` pipeline;
//! Phase 3 of the plan does that.

use std::collections::HashSet;

use fieldcraft_core::CueBundle;

/// Floor applied to every per-dimension score before the geometric mean.
/// A single mismatch contributes `ln(0.3) ≈ -1.204` weighted by that
/// dimension's weight — significant but not annihilating.
pub const MISMATCH_FLOOR: f32 = 0.3;

// Per-dimension weights. Must sum to ~1.0 so an all-mismatch score lands
// exactly at `MISMATCH_FLOOR` (since `exp(sum(w_i * ln(floor))) =
// floor^sum(w_i)`).
const W_PHASE: f32 = 0.40;
const W_DOMAIN: f32 = 0.25;
const W_USER_STATE: f32 = 0.15;
const W_TOOLS: f32 = 0.10;
const W_TERMS: f32 = 0.10;

/// Weighted geometric mean of per-dimension match scores.
///
/// Returns 1.0 when bundles match on every populated weighted dimension,
/// exactly [`MISMATCH_FLOOR`] when every populated dimension disagrees or no
/// dimension is populated on both sides, and a smoothly monotonic value in
/// between. Weights are renormalized across dimensions populated in both
/// bundles, so terms-only records can span the full floor-to-1.0 range instead
/// of being capped by missing phase/domain/user-state/tool fields.
///
/// Note: only 5 of 7 [`CueBundle`] dimensions participate in scoring.
/// `active_files` overlaps with the existing `scope_match` feature in
/// `retrieval.rs` and is excluded to avoid double-counting. `session_arc` is
/// excluded for v1 — it's a weak cue that adds noise without enough signal
/// to justify a weight slot.
pub fn cue_match_factor(record_bundle: &CueBundle, query_bundle: &CueBundle) -> f32 {
    let scores = [
        (
            W_PHASE,
            populated_phase(record_bundle, query_bundle),
            match_phase(record_bundle, query_bundle),
        ),
        (
            W_DOMAIN,
            populated_domain(record_bundle, query_bundle),
            match_domain(record_bundle, query_bundle),
        ),
        (
            W_USER_STATE,
            populated_user_state(record_bundle, query_bundle),
            match_user_state(record_bundle, query_bundle),
        ),
        (
            W_TOOLS,
            populated_tools(record_bundle, query_bundle),
            match_tools(record_bundle, query_bundle),
        ),
        (
            W_TERMS,
            populated_terms(record_bundle, query_bundle),
            match_terms(record_bundle, query_bundle),
        ),
    ];
    let mut log_sum = 0.0_f32;
    let mut populated_weight = 0.0_f32;
    for (w, populated, raw) in scores {
        if !populated {
            continue;
        }
        let floored = raw.max(MISMATCH_FLOOR);
        log_sum += w * floored.ln();
        populated_weight += w;
    }
    if populated_weight == 0.0 {
        return MISMATCH_FLOOR;
    }
    (log_sum / populated_weight).exp()
}

fn populated_phase(r: &CueBundle, q: &CueBundle) -> bool {
    r.phase.is_some() && q.phase.is_some()
}

fn populated_domain(r: &CueBundle, q: &CueBundle) -> bool {
    match (r.domain.as_deref(), q.domain.as_deref()) {
        (Some(a), Some(b)) => !a.trim().is_empty() && !b.trim().is_empty(),
        _ => false,
    }
}

fn populated_user_state(r: &CueBundle, q: &CueBundle) -> bool {
    r.user_state.is_some() && q.user_state.is_some()
}

fn populated_tools(r: &CueBundle, q: &CueBundle) -> bool {
    !r.tools_involved.is_empty() && !q.tools_involved.is_empty()
}

fn populated_terms(r: &CueBundle, q: &CueBundle) -> bool {
    !r.distinctive_terms.is_empty() && !q.distinctive_terms.is_empty()
}

fn match_phase(r: &CueBundle, q: &CueBundle) -> f32 {
    match (r.phase, q.phase) {
        (Some(a), Some(b)) if a == b => 1.0,
        _ => 0.0,
    }
}

fn match_domain(r: &CueBundle, q: &CueBundle) -> f32 {
    match (r.domain.as_deref(), q.domain.as_deref()) {
        (Some(a), Some(b)) if a == b => 1.0,
        _ => 0.0,
    }
}

fn match_user_state(r: &CueBundle, q: &CueBundle) -> f32 {
    match (r.user_state, q.user_state) {
        (Some(a), Some(b)) if a == b => 1.0,
        _ => 0.0,
    }
}

fn match_tools(r: &CueBundle, q: &CueBundle) -> f32 {
    jaccard(&r.tools_involved, &q.tools_involved)
}

fn match_terms(r: &CueBundle, q: &CueBundle) -> f32 {
    jaccard(&r.distinctive_terms, &q.distinctive_terms)
}

/// Jaccard similarity on string sets. Returns 0.0 when either side is empty
/// (will be floored by the caller). Returns the standard `|A ∩ B| / |A ∪ B|`
/// otherwise.
fn jaccard(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    let intersection = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f32 / union as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use fieldcraft_core::{KnowledgePhase, SessionArc, UserState};

    fn full_bundle() -> CueBundle {
        CueBundle {
            cues: vec![],
            phase: Some(KnowledgePhase::Edit),
            domain: Some("fieldcraft_retrieval".to_string()),
            tools_involved: vec!["cargo".to_string(), "git".to_string()],
            active_files: vec!["src/retrieval.rs".to_string()],
            distinctive_terms: vec!["RRF".to_string(), "MMR".to_string()],
            user_state: Some(UserState::Committed),
            session_arc: Some(SessionArc::Mid),
        }
    }

    #[test]
    fn identical_bundles_score_one() {
        let b = full_bundle();
        let s = cue_match_factor(&b, &b);
        assert!(
            (s - 1.0).abs() < 1e-5,
            "identical bundles should score 1.0, got {s}"
        );
    }

    #[test]
    fn empty_bundles_score_at_floor() {
        let empty = CueBundle::default();
        let s = cue_match_factor(&empty, &empty);
        assert!(
            (s - MISMATCH_FLOOR).abs() < 1e-4,
            "empty bundles should score at floor, got {s}"
        );
    }

    #[test]
    fn fully_disjoint_bundles_score_at_floor() {
        let r = full_bundle();
        let q = CueBundle {
            cues: vec![],
            phase: Some(KnowledgePhase::Verify),
            domain: Some("infra".to_string()),
            tools_involved: vec!["docker".to_string(), "kubectl".to_string()],
            active_files: vec![],
            distinctive_terms: vec!["completely".to_string(), "different".to_string()],
            user_state: Some(UserState::Skeptical),
            session_arc: Some(SessionArc::Early),
        };
        let s = cue_match_factor(&r, &q);
        assert!(
            (s - MISMATCH_FLOOR).abs() < 1e-4,
            "fully disjoint should hit floor, got {s}"
        );
    }

    #[test]
    fn partial_match_is_monotonic() {
        let r = full_bundle();
        let mut q = full_bundle();

        let s_match = cue_match_factor(&r, &q);

        // Step 1: disagree on phase only.
        q.phase = Some(KnowledgePhase::Verify);
        let s_one_off = cue_match_factor(&r, &q);

        // Step 2: also disagree on domain.
        q.domain = Some("infra".to_string());
        let s_two_off = cue_match_factor(&r, &q);

        assert!(
            s_match > s_one_off,
            "perfect ({s_match}) should beat one mismatch ({s_one_off})"
        );
        assert!(
            s_one_off > s_two_off,
            "one mismatch ({s_one_off}) should beat two ({s_two_off})"
        );
        assert!(
            s_two_off >= MISMATCH_FLOOR - 1e-5,
            "still at or above floor"
        );
    }

    #[test]
    fn unpopulated_dimensions_do_not_penalize_populated_match() {
        // Only phase is present on both sides — everything else is None/empty.
        let r = CueBundle {
            phase: Some(KnowledgePhase::Edit),
            ..Default::default()
        };
        let q = CueBundle {
            phase: Some(KnowledgePhase::Edit),
            ..Default::default()
        };
        let s = cue_match_factor(&r, &q);
        assert!(
            (s - 1.0).abs() < 1e-5,
            "only populated matching dimension should score 1.0, got {s}"
        );
    }

    #[test]
    fn phase_match_dominates_other_disagreements() {
        let r = full_bundle();
        let mut q_phase_match = full_bundle();
        // Diverge on everything except phase.
        q_phase_match.domain = Some("infra".to_string());
        q_phase_match.tools_involved = vec!["docker".to_string()];
        q_phase_match.distinctive_terms = vec!["unrelated".to_string()];
        q_phase_match.user_state = Some(UserState::Skeptical);

        let mut q_phase_diff = q_phase_match.clone();
        q_phase_diff.phase = Some(KnowledgePhase::Verify); // also differ on phase

        let s_phase_match = cue_match_factor(&r, &q_phase_match);
        let s_phase_diff = cue_match_factor(&r, &q_phase_diff);
        assert!(
            s_phase_match > s_phase_diff,
            "phase agreement should dominate ({s_phase_match} vs {s_phase_diff})"
        );
    }

    #[test]
    fn jaccard_partial_overlap_scales_score() {
        // Hard cues all match; only sets differ.
        let r = CueBundle {
            phase: Some(KnowledgePhase::Edit),
            domain: Some("x".to_string()),
            user_state: Some(UserState::Committed),
            tools_involved: vec!["cargo".to_string(), "git".to_string()],
            distinctive_terms: vec!["RRF".to_string(), "MMR".to_string()],
            ..Default::default()
        };
        let q_half = CueBundle {
            phase: Some(KnowledgePhase::Edit),
            domain: Some("x".to_string()),
            user_state: Some(UserState::Committed),
            tools_involved: vec!["cargo".to_string()], // jaccard 1/2
            distinctive_terms: vec!["RRF".to_string()], // jaccard 1/2
            ..Default::default()
        };
        let q_zero = CueBundle {
            phase: Some(KnowledgePhase::Edit),
            domain: Some("x".to_string()),
            user_state: Some(UserState::Committed),
            tools_involved: vec!["zsh".to_string()], // disjoint
            distinctive_terms: vec!["other".to_string()], // disjoint
            ..Default::default()
        };
        let s_full = cue_match_factor(&r, &r);
        let s_half = cue_match_factor(&r, &q_half);
        let s_zero = cue_match_factor(&r, &q_zero);
        assert!(
            s_full > s_half && s_half > s_zero,
            "monotonic in set overlap: full={s_full} half={s_half} zero={s_zero}"
        );
    }

    #[test]
    fn terms_only_perfect_match_scores_one() {
        let r = CueBundle {
            distinctive_terms: vec!["gemini".to_string(), "embedding".to_string()],
            ..Default::default()
        };
        let q = CueBundle {
            distinctive_terms: vec!["gemini".to_string(), "embedding".to_string()],
            ..Default::default()
        };
        let s = cue_match_factor(&r, &q);
        assert!((s - 1.0).abs() < 1e-5, "expected 1.0, got {s}");
    }

    #[test]
    fn terms_only_partial_jaccard_scores_half() {
        let r = CueBundle {
            distinctive_terms: vec!["gemini".to_string(), "embedding".to_string()],
            ..Default::default()
        };
        let q = CueBundle {
            distinctive_terms: vec!["gemini".to_string()],
            ..Default::default()
        };
        let s = cue_match_factor(&r, &q);
        assert!((s - 0.5).abs() < 1e-5, "expected 0.5, got {s}");
    }

    #[test]
    fn nothing_populated_scores_floor() {
        let r = CueBundle {
            domain: Some("".to_string()),
            ..Default::default()
        };
        let q = CueBundle {
            domain: Some("".to_string()),
            ..Default::default()
        };
        let s = cue_match_factor(&r, &q);
        assert!((s - MISMATCH_FLOOR).abs() < 1e-5, "expected floor, got {s}");
    }

    #[test]
    fn one_matching_and_one_mismatching_dimension_renormalizes() {
        let r = CueBundle {
            phase: Some(KnowledgePhase::Edit),
            distinctive_terms: vec!["gemini".to_string()],
            ..Default::default()
        };
        let q = CueBundle {
            phase: Some(KnowledgePhase::Edit),
            distinctive_terms: vec!["other".to_string()],
            ..Default::default()
        };
        let s = cue_match_factor(&r, &q);
        let expected = ((W_TERMS / (W_PHASE + W_TERMS)) * MISMATCH_FLOOR.ln()).exp();
        assert!(
            (s - expected).abs() < 1e-5,
            "expected renormalized score {expected}, got {s}"
        );
    }

    #[test]
    fn jaccard_empty_set_returns_zero() {
        let empty: Vec<String> = vec![];
        let nonempty = vec!["a".to_string()];
        assert_eq!(jaccard(&empty, &nonempty), 0.0);
        assert_eq!(jaccard(&nonempty, &empty), 0.0);
        assert_eq!(jaccard(&empty, &empty), 0.0);
    }

    #[test]
    fn jaccard_identity_returns_one() {
        let s = vec!["a".to_string(), "b".to_string()];
        assert!((jaccard(&s, &s) - 1.0).abs() < 1e-5);
    }
}
