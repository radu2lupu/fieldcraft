//! Retrieval pipeline: query construction → hybrid search → RRF fusion → scoring → MMR → token budget.

use chrono::Utc;
use fieldcraft_core::{
    ClaimPayload, ContextPayload, CueBundle, DirectivePayload, EpisodePayload, KnowledgeKind,
    KnowledgeOrigin, KnowledgePhase, KnowledgeRecord, KnowledgeTier, ProcedurePayload,
    SourceFilter, TrError, cue_extract,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::sqlite_search::SqliteHit;
use crate::{encode_dense, internal};

// ---------------------------------------------------------------------------
// Public API types
// ---------------------------------------------------------------------------

/// What kind of lesson the agent wants prioritized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LessonPreference {
    /// Agent needs to understand how something works (boost context).
    Context,
    /// Agent needs a specific fix or action recipe (boost directive).
    Directive,
    /// Agent needs both equally (system decides).
    Any,
}

#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    pub project_id: String,
    /// What the agent is working on (free-form context).
    pub context: String,
    /// Currently open files.
    pub files: Vec<String>,
    /// Optional task description.
    pub task: Option<String>,
    /// Agent's preferred lesson types with relative weights.
    /// E.g. [("context", 0.7), ("directive", 0.3)] means "mostly context, some directives."
    /// Empty = system decides based on query keywords.
    pub preferred_types: Vec<(LessonPreference, f32)>,
    /// Which tier to retrieve. Defaults to Active so archive records are never
    /// auto-injected into coding context.
    #[allow(dead_code)]
    pub tier: KnowledgeTier,
    /// Eval/debug filter for comparing learned records to external-derived records.
    pub source_filter: SourceFilter,
}

#[derive(Debug, Clone)]
pub struct RetrievedLesson {
    pub id: String,
    pub kind: String,
    pub title: String,
    /// Full detail or summary format depending on rank position.
    pub content: String,
    pub score: f32,
    pub confidence: f32,
    /// Which project this lesson belongs to.
    pub project_id: String,
    /// When this lesson was created (ISO 8601).
    pub created_at: String,
    /// User who wrote this lesson (token label at write time). None for legacy records.
    pub owner: Option<String>,
    /// Classified provenance bucket for retrieval analysis.
    pub source_origin: String,
}

#[derive(Debug, Clone)]
pub struct TokenReceipt {
    pub lessons_count: usize,
    pub total_tokens: usize,
    pub context_percentage: f32,
}

#[derive(Debug, Clone)]
pub struct RetrievalResponse {
    pub lessons: Vec<RetrievedLesson>,
    pub token_receipt: TokenReceipt,
    /// Serialized JSON of the query-side `CueBundle` used during this
    /// retrieval. Callers can pass it through to `RetrievalLogEntry` so the
    /// closed-loop feedback system can later compare it to record bundles.
    /// Empty when the request had nothing to extract from.
    pub query_cue_bundle_json: String,
    pub raw_candidate_ids: Vec<String>,
    pub raw_candidate_scores: Vec<f32>,
    pub raw_candidate_titles: Vec<String>,
    pub raw_candidate_origins: Vec<String>,
    /// Dense embedding of the query itself. Exposed so callers can run
    /// post-retrieval passes (e.g. the L3 resurrection scan over archived
    /// records) without paying a second encode.
    pub query_dense: Vec<f32>,
}

// ---------------------------------------------------------------------------
// Internal scoring types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ScoringFeatures {
    rrf_rank: f32,
    dense_similarity: f32,
    sparse_bm25_score: f32,
    node_type_claim: f32,
    node_type_procedure: f32,
    node_type_episode: f32,
    confidence: f32,
    state_active: f32,
    state_provisional: f32,
    recency_decay: f32,
    title_match_ratio: f32,
    scope_match: f32,
}

/// Base weights — node_type weights are overridden per-query by `detect_query_intent`.
const P0_WEIGHTS: [f32; 12] = [
    0.30,  // rrf_rank
    0.15,  // dense_similarity
    0.10,  // sparse_bm25_score
    0.05,  // slot 3: directive/claim (base — overridden per query)
    0.10,  // slot 4: context/procedure (boosted — module knowledge transfers better)
    0.02,  // slot 5: episode (legacy)
    0.05,  // confidence
    0.06,  // state_active
    -0.03, // state_provisional
    0.08,  // recency_decay
    0.10,  // title_match_ratio
    0.25,  // scope_match
];

impl ScoringFeatures {
    fn weighted_sum(&self, weights: &[f32; 12]) -> f32 {
        let features = [
            self.rrf_rank,
            self.dense_similarity,
            self.sparse_bm25_score,
            self.node_type_claim,
            self.node_type_procedure,
            self.node_type_episode,
            self.confidence,
            self.state_active,
            self.state_provisional,
            self.recency_decay,
            self.title_match_ratio,
            self.scope_match,
        ];
        features
            .iter()
            .zip(weights.iter())
            .map(|(f, w)| f * w)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Internal candidate type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Candidate {
    knowledge_id: String,
    project_id: String,
    kind: String,
    title: String,
    retrieval_text: String,
    payload_json: String,
    confidence: f32,
    state: String,
    created_at: String,
    affected_files: Vec<String>,
    dense_similarity: f32,
    bm25_score: f32,
    dense_vector: Vec<f32>,
    /// Subtask phase classification (Analyze/Reproduce/Edit/Verify) or None for legacy records.
    phase: Option<KnowledgePhase>,
    /// Session start time for recency scoring; prefers session time over `created_at`.
    session_date: Option<String>,
    /// User who wrote this lesson (token label at write time). None for legacy records.
    owner: Option<String>,
    source_origin: KnowledgeOrigin,
    /// Stored cue bundle for conjunctive matching; None for legacy / unbackfilled records.
    cue_bundle: Option<CueBundle>,
}

// ---------------------------------------------------------------------------
// Query construction (single well-constructed query — replaces 3-query ladder)
// ---------------------------------------------------------------------------

/// Heuristic stop words — tokens that carry no retrieval signal.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "are", "was", "have", "has", "had", "can",
    "will", "would", "could", "should", "not", "but", "its", "all", "any", "our", "your", "their",
    "also",
];

fn is_stop_word(token: &str) -> bool {
    STOP_WORDS.contains(&token)
}

fn extract_meaningful_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .filter(|t| !is_stop_word(t))
        .collect()
}

fn identity_expansion_tokens(text: &str) -> Vec<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
    let mut out = BTreeSet::new();

    for window_len in 2..=4 {
        for window in words.windows(window_len) {
            let compact = window.join("");
            if compact.len() >= 5 {
                insert_identity_token_variants(&mut out, &compact);
            }

            let without_stop = window
                .iter()
                .filter(|w| !is_stop_word(w))
                .cloned()
                .collect::<Vec<_>>()
                .join("");
            if without_stop.len() >= 5 {
                insert_identity_token_variants(&mut out, &without_stop);
            }
        }
    }

    for token in &words {
        if token.contains('_') || token.contains('-') {
            out.insert(token.replace(['_', '-'], ""));
            out.insert(token.replace('_', "-"));
            out.insert(token.replace('-', "_"));
        }
    }

    out.into_iter().collect()
}

fn insert_identity_token_variants(out: &mut BTreeSet<String>, token: &str) {
    out.insert(token.to_string());
    if let Some(stem) = token.strip_suffix("ios")
        && !stem.is_empty()
    {
        out.insert(format!("{stem}_ios"));
        out.insert(format!("{stem}-ios"));
    }
}

fn identity_cue_terms(project_id: &str) -> Vec<String> {
    std::iter::once(project_id.to_string())
        .chain(extract_meaningful_tokens(project_id))
        .chain(identity_expansion_tokens(project_id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn file_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build a single well-constructed query from a RetrievalRequest.
/// Combines context + task + file basenames into one query string.
pub fn build_query(request: &RetrievalRequest) -> String {
    let mut tokens: Vec<String> = extract_meaningful_tokens(&request.project_id);
    tokens.extend(identity_expansion_tokens(&request.project_id));
    tokens.extend(extract_meaningful_tokens(&request.context));
    if let Some(task) = &request.task {
        tokens.extend(extract_meaningful_tokens(task));
    }
    // Add file basenames (without extension) for scope matching
    for file in &request.files {
        let base = file_basename(file);
        tokens.extend(extract_meaningful_tokens(base));
    }
    let mut seen = BTreeSet::new();
    tokens
        .into_iter()
        .filter(|token| seen.insert(token.clone()))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Query intent detection for node_type weight adjustment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum QueryIntent {
    /// "how do I", "fix", "error", "why" → boost directives
    ActionOriented,
    /// "what is", "how does", "architecture", "design" → boost context
    KnowledgeOriented,
    /// No clear signal
    General,
}

fn detect_query_intent(request: &RetrievalRequest) -> QueryIntent {
    let combined = format!(
        "{} {}",
        request.context,
        request.task.as_deref().unwrap_or("")
    )
    .to_lowercase();

    let action_terms = [
        "how to",
        "how do",
        "fix",
        "error",
        "bug",
        "fail",
        "cause",
        "broken",
        "crash",
        "issue",
        "problem",
        "deploy",
        "setup",
        "install",
        "configure",
        "steps to",
        "run",
        "why",
    ];
    let knowledge_terms = [
        "what is",
        "how does",
        "architecture",
        "design",
        "explain",
        "what are",
        "convention",
        "pattern",
        "constraint",
        "why is this",
    ];

    let action_score: usize = action_terms
        .iter()
        .filter(|t| combined.contains(*t))
        .count();
    let knowledge_score: usize = knowledge_terms
        .iter()
        .filter(|t| combined.contains(*t))
        .count();

    if action_score > knowledge_score && action_score > 0 {
        QueryIntent::ActionOriented
    } else if knowledge_score > 0 {
        QueryIntent::KnowledgeOriented
    } else {
        QueryIntent::General
    }
}

fn weights_for_intent(intent: QueryIntent) -> [f32; 12] {
    let mut w = P0_WEIGHTS;
    match intent {
        QueryIntent::ActionOriented => {
            w[3] = 0.08; // directive/claim (boosted for action queries)
            w[4] = 0.08; // context still valuable for action queries
            w[5] = 0.02; // episode
        }
        QueryIntent::KnowledgeOriented => {
            w[3] = 0.03; // directive/claim (reduced)
            w[4] = 0.15; // context (heavily boosted for knowledge queries)
            w[5] = 0.02; // episode
        }
        QueryIntent::General => {} // use base weights — directives dominate by distribution
    }
    w
}

// ---------------------------------------------------------------------------
// Query phase inference (arXiv:2602.21611 — subtask-scoped lesson)
// ---------------------------------------------------------------------------

/// Infer the subtask phase the agent is currently in based on query text.
/// Returns None when there's no clear phase signal (retrieval then considers
/// all phases as a fallback to avoid starving results).
///
/// Category isolation — preferring same-phase lessons — eliminates
/// cross-stage noise. In the paper, this single mechanism accounts for
/// most of the +3.9pp improvement over instance-level lesson.
pub(crate) fn detect_query_phase(request: &RetrievalRequest) -> Option<KnowledgePhase> {
    let combined = format!(
        "{} {}",
        request.context,
        request.task.as_deref().unwrap_or("")
    )
    .to_lowercase();

    // Reproduce phase: reproducing a bug, setting up a failing test.
    let reproduce_terms = [
        "reproduce",
        "repro",
        "failing test",
        "trigger the",
        "how to trigger",
        "how do i reproduce",
        "minimal example",
        "simplest case",
        "recreate the bug",
    ];
    // Verify phase: confirming a fix, running tests, regression guards.
    let verify_terms = [
        "run the test",
        "run tests",
        "pytest",
        "cargo test",
        "how do i verify",
        "how to verify",
        "regression test",
        "test command",
        "confirm the fix",
        "validate the fix",
        "check that",
        "make sure the",
    ];
    // Edit phase: fixing, changing, implementing.
    let edit_terms = [
        "fix",
        "fixing",
        "bug",
        "error",
        "broken",
        "crash",
        "implement",
        "add",
        "change",
        "modify",
        "update",
        "refactor",
        "how do i fix",
        "how to fix",
        "patch",
    ];
    // Analyze phase: understanding code, architecture, invariants.
    let analyze_terms = [
        "what is",
        "how does",
        "what does",
        "explain",
        "architecture",
        "design",
        "why does",
        "why is",
        "convention",
        "pattern",
        "invariant",
        "structure",
        "how is",
        "where is",
    ];

    let repro_hits = reproduce_terms
        .iter()
        .filter(|t| combined.contains(*t))
        .count();
    let verify_hits = verify_terms
        .iter()
        .filter(|t| combined.contains(*t))
        .count();
    let edit_hits = edit_terms.iter().filter(|t| combined.contains(*t)).count();
    let analyze_hits = analyze_terms
        .iter()
        .filter(|t| combined.contains(*t))
        .count();

    // Take the strongest signal. Ties break toward Edit (the most common phase
    // in bug-fixing workflows).
    let max_hits = repro_hits.max(verify_hits).max(edit_hits).max(analyze_hits);
    if max_hits == 0 {
        return None;
    }
    if repro_hits == max_hits {
        return Some(KnowledgePhase::Reproduce);
    }
    if verify_hits == max_hits {
        return Some(KnowledgePhase::Verify);
    }
    if edit_hits == max_hits {
        return Some(KnowledgePhase::Edit);
    }
    Some(KnowledgePhase::Analyze)
}

// ---------------------------------------------------------------------------
// RRF fusion
// ---------------------------------------------------------------------------

/// Reciprocal Rank Fusion over multiple ranked lists of (id, score) pairs.
pub fn rrf_fuse(ranked_lists: &[Vec<(String, f32)>], k: f32) -> Vec<(String, f32)> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    for list in ranked_lists {
        for (rank, (id, _score)) in list.iter().enumerate() {
            let rrf = 1.0 / (k + (rank as f32 + 1.0));
            *scores.entry(id.clone()).or_insert(0.0) += rrf;
        }
    }
    let mut fused: Vec<(String, f32)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

fn title_match_ratio(query: &str, title: &str) -> f32 {
    let query_tokens: std::collections::HashSet<String> =
        extract_meaningful_tokens(query).into_iter().collect();
    let title_tokens: std::collections::HashSet<String> =
        extract_meaningful_tokens(title).into_iter().collect();
    if query_tokens.is_empty() || title_tokens.is_empty() {
        return 0.0;
    }
    let intersection = query_tokens.intersection(&title_tokens).count();
    let denominator = query_tokens.len().min(title_tokens.len());
    intersection as f32 / denominator as f32
}

fn recency_decay(created_at_str: &str) -> f32 {
    let Ok(created_at) = created_at_str.parse::<chrono::DateTime<Utc>>() else {
        return 0.5;
    };
    let age_days = (Utc::now() - created_at).num_days().max(0) as f32;
    (-age_days / 90.0_f32).exp()
}

/// Compute scope_match: how well the candidate's affected_files overlap with request files.
fn compute_scope_match(request_files: &[String], candidate_files: &[String]) -> f32 {
    if request_files.is_empty() || candidate_files.is_empty() {
        return 0.0;
    }
    // Normalize to basenames for comparison
    let req_basenames: std::collections::HashSet<&str> =
        request_files.iter().map(|f| file_basename(f)).collect();
    let cand_basenames: std::collections::HashSet<&str> =
        candidate_files.iter().map(|f| file_basename(f)).collect();
    let intersection = req_basenames.intersection(&cand_basenames).count();
    if intersection > 0 {
        // Any overlap is a strong signal — return proportion matched
        (intersection as f32 / req_basenames.len().min(cand_basenames.len()) as f32).min(1.0)
    } else {
        0.0
    }
}

fn compute_score(
    candidate: &Candidate,
    combined_query: &str,
    rrf_rank: usize,
    max_bm25: f32,
    request_files: &[String],
    weights: &[f32; 12],
) -> f32 {
    let rrf_rank_norm = 1.0 / (1.0 + rrf_rank as f32);
    let bm25_norm = if max_bm25 > 0.0 {
        (candidate.bm25_score / max_bm25).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let features = ScoringFeatures {
        rrf_rank: rrf_rank_norm,
        dense_similarity: candidate.dense_similarity.clamp(0.0, 1.0),
        sparse_bm25_score: bm25_norm,
        // Slot 3: directive + legacy claim
        node_type_claim: if candidate.kind == "directive" || candidate.kind == "claim" {
            1.0
        } else {
            0.0
        },
        // Slot 4: context + legacy procedure
        node_type_procedure: if candidate.kind == "context" || candidate.kind == "procedure" {
            1.0
        } else {
            0.0
        },
        // Slot 5: legacy episode
        node_type_episode: if candidate.kind == "episode" {
            1.0
        } else {
            0.0
        },
        confidence: candidate.confidence.clamp(0.0, 1.0),
        state_active: if candidate.state == "active" {
            1.0
        } else {
            0.0
        },
        state_provisional: if candidate.state == "provisional" {
            1.0
        } else {
            0.0
        },
        recency_decay: recency_decay(
            candidate
                .session_date
                .as_deref()
                .unwrap_or(&candidate.created_at),
        ),
        title_match_ratio: title_match_ratio(
            combined_query,
            &format!("{} {}", candidate.title, candidate.retrieval_text),
        ),
        scope_match: compute_scope_match(request_files, &candidate.affected_files),
    };
    features.weighted_sum(weights)
}

// ---------------------------------------------------------------------------
// SqliteHit → Candidate conversion
// ---------------------------------------------------------------------------

fn sqlite_hit_to_candidate(h: &SqliteHit) -> Candidate {
    // Parse affected_files from the retrieval_text if present
    let affected_files = extract_affected_files_from_record(&h.knowledge_id, &h.retrieval_text);
    let phase = h.phase.as_deref().and_then(KnowledgePhase::parse);

    Candidate {
        knowledge_id: h.knowledge_id.clone(),
        project_id: h.project_id.clone(),
        kind: h.kind.clone(),
        title: h.title.clone(),
        retrieval_text: h.retrieval_text.clone(),
        payload_json: h.payload_json.clone(),
        confidence: h.confidence,
        state: h.state.clone(),
        created_at: h.created_at.clone(),
        affected_files,
        session_date: h.session_date.clone(),
        dense_similarity: h.dense_similarity,
        bm25_score: h.bm25_score,
        dense_vector: h.dense_vector.clone(),
        phase,
        owner: h.owner.clone(),
        source_origin: h.source_origin,
        cue_bundle: h.cue_bundle.clone(),
    }
}

fn knowledge_kind_label(kind: KnowledgeKind) -> String {
    match kind {
        KnowledgeKind::Claim => "claim",
        KnowledgeKind::Procedure => "procedure",
        KnowledgeKind::Episode => "episode",
        KnowledgeKind::Directive => "directive",
        KnowledgeKind::Context => "context",
    }
    .to_string()
}

fn knowledge_state_label(state: fieldcraft_core::KnowledgeState) -> String {
    match state {
        fieldcraft_core::KnowledgeState::Active => "active".to_string(),
        fieldcraft_core::KnowledgeState::Provisional => "provisional".to_string(),
        fieldcraft_core::KnowledgeState::Superseded => "superseded".to_string(),
        fieldcraft_core::KnowledgeState::Retired => "retired".to_string(),
    }
}

fn record_to_candidate(
    record: KnowledgeRecord,
    dense_vector: Vec<f32>,
    dense_similarity: f32,
    bm25_score: f32,
    confidence: f32,
    cue_bundle: Option<CueBundle>,
) -> Candidate {
    Candidate {
        affected_files: extract_affected_files_from_record(&record.id, &record.retrieval_text),
        knowledge_id: record.id,
        project_id: record.project_id,
        kind: knowledge_kind_label(record.kind),
        title: record.title,
        retrieval_text: record.retrieval_text,
        payload_json: record.payload_json,
        confidence,
        state: knowledge_state_label(record.state),
        created_at: record.created_at.to_rfc3339(),
        dense_similarity,
        bm25_score,
        dense_vector,
        phase: record.phase,
        session_date: record.session_date.map(|d| d.to_rfc3339()),
        owner: record.owner,
        source_origin: KnowledgeOrigin::from_source(&record.source),
        cue_bundle,
    }
}

fn extract_skill_markers(text: &str) -> Vec<String> {
    let mut markers = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[skill:") {
        let after_start = &rest[start..];
        let Some(end) = after_start.find(']') else {
            break;
        };
        let marker = &after_start[..=end];
        if marker.len() > "[skill:]".len()
            && marker
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '[' | ']' | ':' | '-' | '_'))
            && !markers.iter().any(|existing| existing == marker)
        {
            markers.push(marker.to_string());
        }
        rest = &after_start[end + 1..];
    }
    markers
}

fn auto_expandable_skill_markers(candidates: &[(Candidate, f32)]) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let best_score = candidates[0].1;
    let strong_score_floor = (best_score * 0.85).max(0.1);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut strong_markers = Vec::new();

    for (rank, (candidate, score)) in candidates.iter().take(10).enumerate() {
        for marker in extract_skill_markers(&candidate.retrieval_text) {
            *counts.entry(marker.clone()).or_insert(0) += 1;
            if rank < 5 && *score >= strong_score_floor {
                strong_markers.push(marker);
            }
        }
    }

    let mut markers = Vec::new();
    for marker in strong_markers {
        if !markers.contains(&marker) {
            markers.push(marker);
        }
    }
    for (marker, count) in counts {
        if count >= 2 && !markers.contains(&marker) {
            markers.push(marker);
        }
    }
    markers
}

struct CandidateScoringContext<'a> {
    max_bm25: f32,
    combined_query: &'a str,
    project_id: &'a str,
    request_files: &'a [String],
    weights: &'a [f32; 12],
    query_phase: Option<KnowledgePhase>,
    query_cue_bundle: &'a CueBundle,
    query_vector: &'a [f32],
}

fn score_candidate(
    candidate: &Candidate,
    rank: usize,
    scoring: &CandidateScoringContext<'_>,
) -> f32 {
    let mut score = compute_score(
        candidate,
        scoring.combined_query,
        rank,
        scoring.max_bm25,
        scoring.request_files,
        scoring.weights,
    );
    if let (Some(qp), Some(cp)) = (scoring.query_phase, candidate.phase) {
        if qp == cp {
            score += 0.25;
        } else {
            score -= 0.15;
        }
    }
    let final_score = score
        + task_grounding_adjustment(candidate, scoring.combined_query, scoring.project_id)
        + cue_match_adjustment(candidate.cue_bundle.as_ref(), scoring.query_cue_bundle);
    apply_dynamic_confidence_multiplier(
        final_score,
        candidate.confidence,
        dynamic_confidence_multiplier_enabled(),
    )
}

fn dynamic_confidence_multiplier_enabled() -> bool {
    dynamic_confidence_multiplier_enabled_for(std::env::var("FIELDCRAFT_DYNAMIC_CONF").ok())
}

fn dynamic_confidence_multiplier_enabled_for(value: Option<String>) -> bool {
    matches!(value.as_deref(), Some("1" | "true"))
}

fn apply_dynamic_confidence_multiplier(
    score: f32,
    effective_confidence: f32,
    enabled: bool,
) -> f32 {
    if enabled {
        score * effective_confidence.clamp(0.0, 1.0)
    } else {
        score
    }
}

fn task_grounding_adjustment(candidate: &Candidate, query: &str, project_id: &str) -> f32 {
    task_grounding_score(query, project_id, candidate).min(6.0) * 0.12
}

fn task_overlap_count(query: &str, project_id: &str, candidate: &Candidate) -> usize {
    task_grounding_terms(query, project_id, candidate).len()
}

fn task_grounding_score(query: &str, project_id: &str, candidate: &Candidate) -> f32 {
    task_overlap_count(query, project_id, candidate) as f32
}

fn task_grounding_terms(query: &str, project_id: &str, candidate: &Candidate) -> Vec<String> {
    let identity_terms: BTreeSet<String> = extract_meaningful_tokens(project_id)
        .into_iter()
        .chain(identity_expansion_tokens(project_id))
        .collect();
    let normalized_identity_terms: BTreeSet<String> = identity_terms
        .iter()
        .map(|term| cue_extract::normalize_identity(term))
        .collect();
    let haystack = format!(
        "{} {}",
        candidate.title.to_lowercase(),
        candidate.retrieval_text.to_lowercase()
    );
    extract_meaningful_tokens(query)
        .into_iter()
        .filter(|term| {
            !identity_terms.contains(term)
                && !normalized_identity_terms.contains(&cue_extract::normalize_identity(term))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|term| haystack.contains(term))
        .collect()
}

fn keep_grounded_candidates(
    candidates: Vec<(Candidate, f32)>,
    query: &str,
    project_id: &str,
) -> Vec<(Candidate, f32)> {
    let grounding_scores: Vec<f32> = candidates
        .iter()
        .map(|(candidate, _)| task_grounding_score(query, project_id, candidate))
        .collect();
    if grounding_scores.iter().copied().fold(0.0, f32::max) < 2.0 {
        return candidates;
    }
    let filtered: Vec<(Candidate, f32)> = candidates
        .iter()
        .cloned()
        .zip(grounding_scores)
        .filter_map(|(candidate, score)| (score >= 2.0).then_some(candidate))
        .collect();
    if filtered.len() < 3 {
        return candidates;
    }
    filtered
}

fn expand_skill_bundles(
    repo: &fieldcraft_storage::KnowledgeRepository,
    candidates: &mut Vec<(Candidate, f32)>,
    request: &RetrievalRequest,
    tier: &str,
    scoring: &CandidateScoringContext<'_>,
) -> Result<bool, TrError> {
    let markers = auto_expandable_skill_markers(candidates);
    if markers.is_empty() {
        return Ok(false);
    }

    let mut existing: std::collections::HashSet<String> = candidates
        .iter()
        .map(|(candidate, _)| candidate.knowledge_id.clone())
        .collect();
    let mut expanded = false;

    for marker in markers {
        let ids =
            repo.list_knowledge_ids_by_retrieval_marker(&request.project_id, &marker, tier)?;
        let signals = repo.load_retrieval_signals_for(&ids)?;
        let seed_rank = candidates
            .iter()
            .position(|(candidate, _)| {
                extract_skill_markers(&candidate.retrieval_text).contains(&marker)
            })
            .unwrap_or(0);
        for id in ids {
            if existing.contains(&id) {
                continue;
            }
            let Some(record) = repo
                .get_knowledge(&id)
                .map_err(|e| internal(format!("get_knowledge: {e}")))?
            else {
                continue;
            };
            let source_origin = KnowledgeOrigin::from_source(&record.source);
            if !request.source_filter.matches(source_origin) {
                continue;
            }
            let dense_vector = repo
                .load_dense_vector(&id)
                .map_err(|e| internal(format!("load_dense_vector: {e}")))?
                .unwrap_or_default();
            let dense_vector = comparable_dense_vector(scoring.query_vector, dense_vector);
            let confidence = signals
                .effective_confidence
                .get(&id)
                .copied()
                .unwrap_or(record.confidence);
            let cue_bundle = signals.cue_bundles.get(&id).cloned();
            let dense_similarity = cosine_similarity(scoring.query_vector, &dense_vector);
            let candidate = record_to_candidate(
                record,
                dense_vector,
                dense_similarity,
                0.0,
                confidence,
                cue_bundle,
            );
            let score = score_candidate(&candidate, seed_rank, scoring);
            existing.insert(candidate.knowledge_id.clone());
            candidates.push((candidate, score));
            expanded = true;
        }
    }

    Ok(expanded)
}

/// Build the query-side `CueBundle` from a [`RetrievalRequest`] for conjunctive
/// scoring. Combines the deterministic extractors with the heuristically
/// inferred query phase. `user_state` stays `None` until an LLM classifier
/// fills it in (Phase 3 of the plan).
pub(crate) fn build_query_cue_bundle(
    request: &RetrievalRequest,
    query_phase: Option<KnowledgePhase>,
) -> CueBundle {
    let combined = format!(
        "{} {}",
        request.context,
        request.task.as_deref().unwrap_or("")
    );
    let mut bundle = cue_extract::extract_deterministic(&combined, &request.files, None);
    cue_extract::add_identity_cue(&mut bundle, &request.project_id, "request_project_id");
    bundle.phase = query_phase;
    bundle
}

/// Compute the conjunctive cue-match adjustment for a candidate. Returns 0.0
/// when either bundle is missing or empty — legacy records aren't penalised
/// by the introduction of cue scoring.
///
/// Otherwise returns `(cue_match_factor - 1.0) * CUE_ADJUSTMENT_WEIGHT`, an
/// additive correction so the new signal composes cleanly with the existing
/// linear scoring sum and additive phase boost.
fn cue_match_adjustment(record: Option<&CueBundle>, query: &CueBundle) -> f32 {
    let Some(record_bundle) = record else {
        return 0.0;
    };
    if record_bundle.is_empty() || query.is_empty() {
        return 0.0;
    }
    let factor = crate::cue_match::cue_match_factor(record_bundle, query);
    (factor - 1.0) * CUE_ADJUSTMENT_WEIGHT
}

/// Multiplier applied to `(cue_match_factor - 1.0)` when blending into the
/// additive score. A perfect cue match contributes 0, full mismatch
/// contributes `(MISMATCH_FLOOR - 1.0) * WEIGHT ≈ -0.21` — comparable in
/// magnitude to the existing phase boost (+0.25 / -0.15).
const CUE_ADJUSTMENT_WEIGHT: f32 = 0.30;

/// Extract affected files from retrieval text.
/// Looks for patterns like "scope: path/to/file.rs" or file-like tokens.
fn extract_affected_files_from_record(_id: &str, retrieval_text: &str) -> Vec<String> {
    let mut files = Vec::new();
    // Match "scope: some/path.ext" and "files: a.ext, path/b.ext" patterns.
    for segment in retrieval_text.split('|') {
        let trimmed = segment.trim();
        if let Some(paths) = trimmed
            .strip_prefix("scope:")
            .or_else(|| trimmed.strip_prefix("scope :"))
            .or_else(|| trimmed.strip_prefix("files:"))
            .or_else(|| trimmed.strip_prefix("files :"))
        {
            for path in paths.split(',').map(str::trim) {
                if !path.is_empty() && path.contains('/') {
                    files.push(path.to_string());
                }
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// MMR diversity reranking
// ---------------------------------------------------------------------------

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn comparable_dense_vector(query_vector: &[f32], dense_vector: Vec<f32>) -> Vec<f32> {
    if !query_vector.is_empty()
        && !dense_vector.is_empty()
        && query_vector.len() == dense_vector.len()
    {
        dense_vector
    } else {
        Vec::new()
    }
}

fn mmr_rerank(
    candidates: &[(Candidate, f32)],
    max_results: usize,
    lambda: f32,
) -> Vec<(Candidate, f32)> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut selected: Vec<usize> = Vec::new();
    let mut remaining: Vec<usize> = (0..candidates.len()).collect();

    while selected.len() < max_results && !remaining.is_empty() {
        let mut best_idx = 0;
        let mut best_mmr = f32::NEG_INFINITY;

        for &idx in &remaining {
            let (candidate, score) = &candidates[idx];
            let max_sim = if selected.is_empty() {
                0.0
            } else {
                selected
                    .iter()
                    .map(|&sel_idx| {
                        cosine_similarity(
                            &candidate.dense_vector,
                            &candidates[sel_idx].0.dense_vector,
                        )
                    })
                    .fold(0.0_f32, f32::max)
            };
            let mmr = lambda * score - (1.0 - lambda) * max_sim;
            if mmr > best_mmr {
                best_mmr = mmr;
                best_idx = idx;
            }
        }

        selected.push(best_idx);
        remaining.retain(|&idx| idx != best_idx);
    }

    selected
        .into_iter()
        .map(|idx| candidates[idx].clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Token budget enforcement
// ---------------------------------------------------------------------------

const CHARS_PER_TOKEN: usize = 4;
const DEFAULT_TOKEN_BUDGET: usize = 60_000;
const CONTEXT_WINDOW: usize = 200_000;

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(CHARS_PER_TOKEN)
}

fn escape_memory_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_memory_attr(text: &str) -> String {
    escape_memory_text(text)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn remembered_note_open(candidate: &Candidate, summary: bool) -> String {
    let summary_attr = if summary { r#" summary="true""# } else { "" };
    format!(
        r#"<remembered_note id="lesson:{}" kind="{}" title="{}"{}>"#,
        escape_memory_attr(&candidate.knowledge_id),
        escape_memory_attr(&candidate.kind),
        escape_memory_attr(&candidate.title),
        summary_attr
    )
}

fn remembered_note_title_only(candidate: &Candidate) -> String {
    format!(
        r#"<remembered_note id="lesson:{}" kind="{}" title="{}" />"#,
        escape_memory_attr(&candidate.knowledge_id),
        escape_memory_attr(&candidate.kind),
        escape_memory_attr(&candidate.title)
    )
}

fn format_full(candidate: &Candidate) -> String {
    match candidate.kind.as_str() {
        "directive" => {
            if let Ok(p) = serde_json::from_str::<DirectivePayload>(&candidate.payload_json) {
                let mut out = format!("{}\n", remembered_note_open(candidate, false));
                out.push_str(&format!(
                    "What worked before: {}\n",
                    escape_memory_text(&p.action)
                ));
                if !p.avoid.is_empty() {
                    out.push_str(&format!(
                        "What to avoid from that case: {}\n",
                        escape_memory_text(&p.avoid.join("; "))
                    ));
                }
                out.push_str(&format!(
                    "Why it mattered then: {}\n",
                    escape_memory_text(&p.reason)
                ));
                if let Some(ref v) = p.verify {
                    out.push_str(&format!(
                        "How it was checked then: {}\n",
                        escape_memory_text(v)
                    ));
                }
                if !p.files.is_empty() {
                    out.push_str(&format!(
                        "Files involved then: {}\n",
                        escape_memory_text(&p.files.join(", "))
                    ));
                }
                out.push_str("</remembered_note>");
                return out;
            }
        }
        "context" => {
            if let Ok(p) = serde_json::from_str::<ContextPayload>(&candidate.payload_json) {
                let mut out = format!("{}\n", remembered_note_open(candidate, false));
                out.push_str(&format!(
                    "Previously observed: {}\n",
                    escape_memory_text(&p.statement)
                ));
                out.push_str(&format!(
                    "Where it applied then: {}\n",
                    escape_memory_text(&p.scope)
                ));
                if !p.implications.is_empty() {
                    out.push_str(&format!(
                        "What that suggested then: {}\n",
                        escape_memory_text(&p.implications.join("; "))
                    ));
                }
                out.push_str("</remembered_note>");
                return out;
            }
        }
        _ => {}
    }
    // Fallback: emit a generic remembered_note block for unknown kinds.
    // `lesson:<id>` in the id attribute lets the trace analyzer attribute later citations.
    format!(
        "{}\nPreviously remembered:\n{}\n</remembered_note>",
        remembered_note_open(candidate, false),
        escape_memory_text(&candidate.retrieval_text)
    )
}

fn format_summary(candidate: &Candidate) -> String {
    let first_sentence = candidate
        .retrieval_text
        .split('.')
        .next()
        .unwrap_or(&candidate.retrieval_text)
        .trim()
        .to_string();
    format!(
        "{}\nShort memory: {}\n</remembered_note>",
        remembered_note_open(candidate, true),
        escape_memory_text(&first_sentence)
    )
}

fn apply_token_budget(
    ranked: Vec<(Candidate, f32)>,
    token_budget: usize,
) -> (Vec<RetrievedLesson>, usize) {
    let mut lessons = Vec::new();
    let mut total_chars = 0usize;
    let budget_chars = token_budget * CHARS_PER_TOKEN;

    for (position, (candidate, score)) in ranked.into_iter().enumerate() {
        let content = if position < 3 {
            format_full(&candidate)
        } else if position < 8 {
            format_summary(&candidate)
        } else {
            remembered_note_title_only(&candidate)
        };

        let content_chars = content.len();
        if total_chars + content_chars > budget_chars {
            break;
        }
        total_chars += content_chars;

        let created_date = candidate
            .created_at
            .split('T')
            .next()
            .unwrap_or(&candidate.created_at)
            .to_string();
        lessons.push(RetrievedLesson {
            id: candidate.knowledge_id,
            kind: candidate.kind,
            title: candidate.title,
            content,
            score,
            confidence: candidate.confidence,
            project_id: candidate.project_id,
            created_at: created_date,
            owner: candidate.owner.clone(),
            source_origin: candidate.source_origin.as_str().to_string(),
        });
    }

    let total_tokens = estimate_tokens(
        &lessons
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join(""),
    );
    (lessons, total_tokens)
}

// ---------------------------------------------------------------------------
// Main retrieval function (SQLite only)
// ---------------------------------------------------------------------------

pub async fn retrieve(
    request: RetrievalRequest,
    db_path: &std::path::Path,
) -> Result<RetrievalResponse, TrError> {
    let repo = fieldcraft_storage::KnowledgeRepository::open(db_path)
        .map_err(|e| internal(format!("sqlite open: {e}")))?;
    retrieve_from_repository(request, &repo)
}

/// Embed the request's query text. Needs no repository, so callers can run it
/// without holding any database handle or lock. An empty vector means
/// "no dense signal" and makes retrieval BM25-only.
pub fn embed_query(request: &RetrievalRequest) -> Vec<f32> {
    let query = build_query(request);
    match encode_dense(std::slice::from_ref(&query)) {
        Ok(vectors) => vectors.into_iter().next().unwrap_or_default(),
        Err(err) => {
            eprintln!(
                "WARNING: dense query embedding failed; continuing sparse-only BM25 retrieval: {err}"
            );
            Vec::new()
        }
    }
}

pub fn retrieve_from_repository(
    request: RetrievalRequest,
    repo: &fieldcraft_storage::KnowledgeRepository,
) -> Result<RetrievalResponse, TrError> {
    let query_vector = embed_query(&request);
    retrieve_with_query_vector(request, repo, query_vector)
}

/// Retrieval with an already-computed query vector (see [`embed_query`]).
/// An empty `query_vector` runs the BM25-only path.
pub fn retrieve_with_query_vector(
    request: RetrievalRequest,
    repo: &fieldcraft_storage::KnowledgeRepository,
    query_vector: Vec<f32>,
) -> Result<RetrievalResponse, TrError> {
    let query = build_query(&request);
    let intent = detect_query_intent(&request);
    let mut weights = weights_for_intent(intent);

    // Agent preferences override keyword-based intent for type boosting.
    // Multiple preferences are blended by their weights.
    if !request.preferred_types.is_empty() {
        let mut directive_w = 0.0_f32;
        let mut context_w = 0.0_f32;
        for (pref, w) in &request.preferred_types {
            match pref {
                LessonPreference::Directive => directive_w += w,
                LessonPreference::Context => context_w += w,
                LessonPreference::Any => {
                    directive_w += w * 0.5;
                    context_w += w * 0.5;
                }
            }
        }
        let total = (directive_w + context_w).max(0.01);
        weights[3] = 0.18 * (directive_w / total); // directive/claim
        weights[4] = 0.18 * (context_w / total); // context
        weights[5] = 0.01; // episode
    }

    // Run hybrid search without treating `project_id` as a recall boundary.
    // Project/app identity is noisy human language: it belongs in query/cue
    // evidence, not as a hard SQLite partition.
    let tier_str = request.tier.as_str();
    let results = if query_vector.is_empty() {
        crate::sqlite_search::sparse_search_for_query_unscoped(
            repo,
            &query,
            tier_str,
            request.source_filter,
        )?
    } else {
        let identity_cues = identity_cue_terms(&request.project_id);
        crate::sqlite_search::hybrid_search_for_query_unscoped(
            repo,
            &query,
            &identity_cues,
            &query_vector,
            tier_str,
            request.source_filter,
        )?
    };

    // Build hit map
    let mut hit_map: HashMap<String, SqliteHit> = HashMap::new();
    let fused: Vec<(String, f32)> = results
        .iter()
        .map(|(id, score, _)| (id.clone(), *score))
        .collect();
    for (_, _, h) in results {
        hit_map.entry(h.knowledge_id.clone()).or_insert(h);
    }

    let top30 = &fused[..fused.len().min(30)];

    let combined_query = format!(
        "{} {}",
        request.context,
        request.task.as_deref().unwrap_or("")
    );

    // Find max BM25 score in results for normalization
    let max_bm25 = hit_map
        .values()
        .map(|h| h.bm25_score)
        .fold(0.0_f32, f32::max);

    // Detect the subtask phase the agent is currently in, for category-filtered
    // retrieval (Shen et al. 2026, arXiv:2602.21611). Category isolation
    // eliminates cross-stage noise — the paper's dominant effect (+3.9pp).
    let query_phase = detect_query_phase(&request);

    // Phase 2 of cue-bundle retrieval: build the query-side cue bundle from
    // the current request and apply a conjunctive match adjustment per
    // candidate alongside the existing additive phase boost.
    let query_cue_bundle = build_query_cue_bundle(&request, query_phase);
    let scoring = CandidateScoringContext {
        max_bm25,
        combined_query: &combined_query,
        project_id: &request.project_id,
        request_files: &request.files,
        weights: &weights,
        query_phase,
        query_cue_bundle: &query_cue_bundle,
        query_vector: &query_vector,
    };

    let mut candidates: Vec<(Candidate, f32)> = top30
        .iter()
        .enumerate()
        .filter_map(|(rank, (id, _))| {
            let h = hit_map.get(id)?;
            let candidate = sqlite_hit_to_candidate(h);
            let score = score_candidate(&candidate, rank, &scoring);
            Some((candidate, score))
        })
        .collect();

    // Re-sort after phase adjustment.
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let expanded_bundles =
        expand_skill_bundles(repo, &mut candidates, &request, tier_str, &scoring)?;
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let candidates = keep_grounded_candidates(candidates, &combined_query, &request.project_id);
    let raw_candidate_ids: Vec<String> = candidates
        .iter()
        .map(|(candidate, _)| candidate.knowledge_id.clone())
        .collect();
    let raw_candidate_scores: Vec<f32> = candidates.iter().map(|(_, score)| *score).collect();
    let raw_candidate_titles: Vec<String> = candidates
        .iter()
        .map(|(candidate, _)| candidate.title.clone())
        .collect();
    let raw_candidate_origins: Vec<String> = candidates
        .iter()
        .map(|(candidate, _)| candidate.source_origin.as_str().to_string())
        .collect();

    let token_budget = DEFAULT_TOKEN_BUDGET;
    let max_results = (token_budget / 500).clamp(3, 30);
    let reranked = if expanded_bundles {
        // Keep expanded skill bundles together; MMR can split required sibling records.
        candidates.iter().take(max_results).cloned().collect()
    } else {
        mmr_rerank(&candidates, max_results, 0.7)
    };
    let (lessons, total_tokens) = apply_token_budget(reranked, token_budget);
    let lessons_count = lessons.len();
    let context_percentage = (total_tokens as f32 / CONTEXT_WINDOW as f32) * 100.0;

    let query_cue_bundle_json = serde_json::to_string(&query_cue_bundle).unwrap_or_default();
    Ok(RetrievalResponse {
        lessons,
        token_receipt: TokenReceipt {
            lessons_count,
            total_tokens,
            context_percentage,
        },
        query_cue_bundle_json,
        raw_candidate_ids,
        raw_candidate_scores,
        raw_candidate_titles,
        raw_candidate_origins,
        query_dense: query_vector,
    })
}

// ---------------------------------------------------------------------------
// SQLite indexing
// ---------------------------------------------------------------------------

/// Index a knowledge record in SQLite (store dense vector).
pub fn index_knowledge_sqlite(
    repo: &fieldcraft_storage::KnowledgeRepository,
    record: &KnowledgeRecord,
) -> Result<(), TrError> {
    let retrieval_text = if record.retrieval_text.is_empty() {
        build_embedding_text(record)
    } else {
        record.retrieval_text.clone()
    };

    let embeddings = encode_dense(&[retrieval_text])
        .map_err(|e| internal(format!("encode_dense failed: {e}")))?;
    let dense_vec = embeddings.into_iter().next().unwrap_or_default();

    repo.store_dense_vector(&record.id, &dense_vec)
        .map_err(|e| internal(format!("store vector failed: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Embedding text construction
// ---------------------------------------------------------------------------

pub fn build_embedding_text(record: &KnowledgeRecord) -> String {
    match record.kind {
        KnowledgeKind::Claim => {
            let payload: ClaimPayload =
                serde_json::from_str(&record.payload_json).unwrap_or_default();
            let mechanism = payload.mechanism.as_deref().unwrap_or("");
            let scope = payload.scope.as_deref().unwrap_or("");
            let questions = payload.retrieval_questions.join("; ");
            format!(
                "{} | {} | scope: {} | {}",
                payload.statement, mechanism, scope, questions
            )
        }
        KnowledgeKind::Procedure => {
            let payload: ProcedurePayload =
                serde_json::from_str(&record.payload_json).unwrap_or_default();
            let steps_preview = payload.steps[..payload.steps.len().min(3)].join("; ");
            let preconditions = payload.preconditions.join("; ");
            let questions = payload.retrieval_questions.join("; ");
            format!(
                "{} | steps: {} | preconditions: {} | {}",
                payload.goal, steps_preview, preconditions, questions
            )
        }
        KnowledgeKind::Episode => {
            let payload: EpisodePayload =
                serde_json::from_str(&record.payload_json).unwrap_or_default();
            let trigger = payload.trigger.as_deref().unwrap_or("");
            let outcome = payload.outcome.as_deref().unwrap_or("");
            let obs_preview = payload.observations[..payload.observations.len().min(2)].join("; ");
            format!("{} | {} | {}", trigger, outcome, obs_preview)
        }
        KnowledgeKind::Directive => {
            let payload: DirectivePayload =
                serde_json::from_str(&record.payload_json).unwrap_or_default();
            let avoid = if payload.avoid.is_empty() {
                String::new()
            } else {
                format!(" | avoid: {}", payload.avoid.join("; "))
            };
            let queries = payload.retrieval_queries.join("; ");
            format!(
                "{} | {} | trigger: {}{}| {}",
                payload.action, payload.reason, payload.trigger, avoid, queries
            )
        }
        KnowledgeKind::Context => {
            let payload: ContextPayload =
                serde_json::from_str(&record.payload_json).unwrap_or_default();
            let implications = payload.implications.join("; ");
            let queries = payload.retrieval_queries.join("; ");
            format!(
                "{} | scope: {} | {} | {}",
                payload.statement, payload.scope, implications, queries
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use fieldcraft_core::KnowledgeSource;

    fn make_request(context: &str, files: Vec<&str>, task: Option<&str>) -> RetrievalRequest {
        RetrievalRequest {
            project_id: "test-project".to_string(),
            context: context.to_string(),
            files: files.into_iter().map(|s| s.to_string()).collect(),
            task: task.map(|s| s.to_string()),
            preferred_types: vec![],
            tier: KnowledgeTier::Active,
            source_filter: SourceFilter::Any,
        }
    }

    // ------------------------------------------------------------------
    // Query construction tests
    // ------------------------------------------------------------------

    #[test]
    fn test_build_query_extracts_key_terms() {
        let req = make_request(
            "authentication flow middleware login",
            vec![],
            Some("fix JWT token validation"),
        );
        let query = build_query(&req);
        assert!(
            query.contains("authentication")
                || query.contains("middleware")
                || query.contains("login"),
            "query should contain key terms, got: {query}"
        );
    }

    #[test]
    fn test_build_query_includes_file_basenames() {
        let req = make_request(
            "fix the auth bug",
            vec!["src/middleware/auth.rs", "src/routes/login.rs"],
            None,
        );
        let query = build_query(&req);
        assert!(
            query.contains("auth") || query.contains("login"),
            "query should include file basenames, got: {query}"
        );
    }

    #[test]
    fn test_build_query_expands_project_identity_variants() {
        let mut req = make_request(
            "Oh a potato ingredient display strings are inconsistent",
            vec![],
            Some("make recipe ingredient surfaces consistent"),
        );
        req.project_id = "ohapotato_ios".to_string();

        let query = build_query(&req);

        assert!(query.contains("ohapotato"), "query was: {query}");
        assert!(query.contains("ohapotato_ios"), "query was: {query}");
        assert!(query.contains("ohapotato-ios"), "query was: {query}");
    }

    // ------------------------------------------------------------------
    // Query intent detection tests
    // ------------------------------------------------------------------

    #[test]
    fn test_detect_intent_action_oriented() {
        let req = make_request("how do I deploy to production", vec![], None);
        assert!(matches!(
            detect_query_intent(&req),
            QueryIntent::ActionOriented
        ));
        let req2 = make_request("why does the auth middleware fail", vec![], None);
        assert!(matches!(
            detect_query_intent(&req2),
            QueryIntent::ActionOriented
        ));
    }

    #[test]
    fn test_detect_intent_knowledge_oriented() {
        let req = make_request("what is the architecture of the auth system", vec![], None);
        assert!(matches!(
            detect_query_intent(&req),
            QueryIntent::KnowledgeOriented
        ));
    }

    #[test]
    fn test_detect_intent_general() {
        let req = make_request("authentication middleware order", vec![], None);
        assert!(matches!(detect_query_intent(&req), QueryIntent::General));
    }

    // ------------------------------------------------------------------
    // Scope match tests
    // ------------------------------------------------------------------

    #[test]
    fn test_scope_match_exact_file_overlap() {
        let req_files = vec!["src/middleware/auth.rs".to_string()];
        let cand_files = vec!["src/middleware/auth.rs".to_string()];
        let score = compute_scope_match(&req_files, &cand_files);
        assert!(score > 0.9, "exact match should score high, got {score}");
    }

    #[test]
    fn test_scope_match_basename_overlap() {
        let req_files = vec!["src/middleware/auth.rs".to_string()];
        let cand_files = vec!["lib/middleware/auth.rs".to_string()];
        let score = compute_scope_match(&req_files, &cand_files);
        assert!(score > 0.9, "basename match should score high, got {score}");
    }

    #[test]
    fn test_scope_match_no_overlap() {
        let req_files = vec!["src/middleware/auth.rs".to_string()];
        let cand_files = vec!["src/db/pool.rs".to_string()];
        let score = compute_scope_match(&req_files, &cand_files);
        assert_eq!(score, 0.0, "no overlap should score 0");
    }

    #[test]
    fn test_scope_match_empty_files() {
        let score = compute_scope_match(&[], &["src/auth.rs".to_string()]);
        assert_eq!(score, 0.0);
        let score = compute_scope_match(&["src/auth.rs".to_string()], &[]);
        assert_eq!(score, 0.0);
    }

    // ------------------------------------------------------------------
    // RRF fusion tests
    // ------------------------------------------------------------------

    #[test]
    fn test_rrf_fuse_single_list() {
        let list = vec![
            ("a".to_string(), 1.0),
            ("b".to_string(), 0.9),
            ("c".to_string(), 0.8),
        ];
        let fused = rrf_fuse(&[list], 60.0);
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].0, "a");
        assert_eq!(fused[1].0, "b");
        assert_eq!(fused[2].0, "c");
    }

    #[test]
    fn test_rrf_fuse_merges_two_lists() {
        let list1 = vec![
            ("a".to_string(), 1.0),
            ("b".to_string(), 0.9),
            ("c".to_string(), 0.8),
        ];
        let list2 = vec![
            ("b".to_string(), 1.0),
            ("a".to_string(), 0.9),
            ("d".to_string(), 0.8),
        ];
        let fused = rrf_fuse(&[list1, list2], 60.0);
        assert!(fused.len() >= 3);
        let ids: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&"d"));
    }

    #[test]
    fn test_rrf_fuse_item_in_all_lists_ranked_highest() {
        let shared = "x".to_string();
        let list1 = vec![(shared.clone(), 1.0), ("a".to_string(), 0.5)];
        let list2 = vec![(shared.clone(), 1.0), ("b".to_string(), 0.5)];
        let list3 = vec![(shared.clone(), 1.0), ("c".to_string(), 0.5)];
        let fused = rrf_fuse(&[list1, list2, list3], 60.0);
        assert_eq!(fused[0].0, shared);
    }

    #[test]
    fn test_rrf_fuse_empty_inputs() {
        let fused = rrf_fuse(&[], 60.0);
        assert!(fused.is_empty());
        let fused2 = rrf_fuse(&[vec![]], 60.0);
        assert!(fused2.is_empty());
    }

    // ------------------------------------------------------------------
    // Scoring function tests
    // ------------------------------------------------------------------

    fn make_candidate(kind: &str, state: &str, confidence: f32, title: &str) -> Candidate {
        Candidate {
            knowledge_id: "test-id".to_string(),
            project_id: "test-project".to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            retrieval_text: "some retrieval text about the topic".to_string(),
            payload_json: "{}".to_string(),
            confidence,
            state: state.to_string(),
            created_at: Utc::now().to_rfc3339(),
            affected_files: vec![],
            session_date: None,
            dense_similarity: 0.8,
            bm25_score: 5.0,
            dense_vector: vec![0.1, 0.2, 0.3],
            phase: None,
            owner: None,
            source_origin: KnowledgeOrigin::Learned,
            cue_bundle: None,
        }
    }

    #[test]
    fn test_scoring_claim_scores_higher_than_episode() {
        let claim = make_candidate("claim", "active", 0.9, "auth middleware order");
        let episode = make_candidate("episode", "active", 0.9, "auth middleware order");
        let weights = P0_WEIGHTS;
        let claim_score = compute_score(&claim, "auth middleware", 0, 10.0, &[], &weights);
        let episode_score = compute_score(&episode, "auth middleware", 0, 10.0, &[], &weights);
        assert!(
            claim_score > episode_score,
            "claim ({claim_score}) should score higher than episode ({episode_score})"
        );
    }

    #[test]
    fn test_scoring_active_scores_higher_than_provisional() {
        let active = make_candidate("claim", "active", 0.8, "some claim");
        let provisional = make_candidate("claim", "provisional", 0.8, "some claim");
        let weights = P0_WEIGHTS;
        let active_score = compute_score(&active, "some query", 0, 10.0, &[], &weights);
        let provisional_score = compute_score(&provisional, "some query", 0, 10.0, &[], &weights);
        assert!(
            active_score > provisional_score,
            "active ({active_score}) should score higher than provisional ({provisional_score})"
        );
    }

    #[test]
    fn test_scoring_title_match_boosts_score() {
        let matching = make_candidate("claim", "active", 0.8, "authentication flow");
        let non_matching = make_candidate("claim", "active", 0.8, "database migrations");
        let weights = P0_WEIGHTS;
        let matching_score =
            compute_score(&matching, "authentication flow", 0, 10.0, &[], &weights);
        let non_matching_score =
            compute_score(&non_matching, "authentication flow", 0, 10.0, &[], &weights);
        assert!(
            matching_score > non_matching_score,
            "title-matching ({matching_score}) should score higher than non-matching ({non_matching_score})"
        );
    }

    #[test]
    fn test_dynamic_confidence_multiplier_is_flag_gated() {
        let candidate = make_candidate("claim", "active", 0.8, "authentication flow");
        let weights = P0_WEIGHTS;
        let score = compute_score(&candidate, "authentication flow", 0, 10.0, &[], &weights);

        assert_eq!(
            apply_dynamic_confidence_multiplier(score, 0.25, false),
            score
        );
        assert!(
            (apply_dynamic_confidence_multiplier(score, 0.25, true) - score * 0.25).abs() < 1e-6
        );
        assert!(
            (apply_dynamic_confidence_multiplier(score, candidate.confidence, true)
                - score * candidate.confidence)
                .abs()
                < 1e-6
        );

        assert!(!dynamic_confidence_multiplier_enabled_for(None));
        assert!(dynamic_confidence_multiplier_enabled_for(Some(
            "1".to_string()
        )));
        assert!(dynamic_confidence_multiplier_enabled_for(Some(
            "true".to_string()
        )));
        assert!(!dynamic_confidence_multiplier_enabled_for(Some(
            "TRUE".to_string()
        )));
        assert!(!dynamic_confidence_multiplier_enabled_for(Some(
            "0".to_string()
        )));
    }

    #[test]
    fn test_scoring_weighted_sum_structure() {
        let features = ScoringFeatures {
            rrf_rank: 1.0,
            dense_similarity: 1.0,
            sparse_bm25_score: 1.0,
            node_type_claim: 1.0,
            node_type_procedure: 0.0,
            node_type_episode: 0.0,
            confidence: 1.0,
            state_active: 1.0,
            state_provisional: 0.0,
            recency_decay: 1.0,
            title_match_ratio: 1.0,
            scope_match: 1.0,
        };
        let score = features.weighted_sum(&P0_WEIGHTS);
        let expected: f32 = P0_WEIGHTS
            .iter()
            .zip([
                1.0f32, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0,
            ])
            .map(|(w, f)| w * f)
            .sum();
        assert!(
            (score - expected).abs() < 1e-5,
            "weighted sum mismatch: {score} vs {expected}"
        );
    }

    #[test]
    fn test_extract_skill_markers() {
        let markers = extract_skill_markers(
            "Applies broadly. [skill:design-engineering] More text [skill:swiftui-ui]",
        );
        assert_eq!(
            markers,
            vec![
                "[skill:design-engineering]".to_string(),
                "[skill:swiftui-ui]".to_string()
            ]
        );
    }

    #[test]
    fn test_auto_expandable_skill_markers_requires_strong_evidence() {
        let mut top = make_candidate("procedure", "active", 0.9, "animation rules");
        top.retrieval_text = "landing page motion [skill:design-engineering]".to_string();
        let mut weak = make_candidate("procedure", "active", 0.9, "other");
        weak.retrieval_text = "other unrelated [skill:other]".to_string();

        let markers = auto_expandable_skill_markers(&[(top.clone(), 1.0), (weak, 0.2)]);
        assert_eq!(markers, vec!["[skill:design-engineering]".to_string()]);

        let markers = auto_expandable_skill_markers(&[(top, 1.0)]);
        assert_eq!(markers, vec!["[skill:design-engineering]".to_string()]);
    }

    #[test]
    fn test_expand_skill_bundles_adds_marker_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let repo =
            fieldcraft_storage::KnowledgeRepository::open(&dir.path().join("memory.sqlite3"))
                .unwrap();
        let marker = "[skill:design-engineering]";
        for id in ["seed", "sibling-a", "sibling-b"] {
            let mut record = make_record(
                KnowledgeKind::Procedure,
                r#"{"trigger":"ui","steps":["animate"],"success_criteria":["polished"]}"#,
            );
            record.id = id.to_string();
            record.title = id.to_string();
            record.retrieval_text = format!("UI animation guidance {marker}");
            repo.insert_knowledge(&record).unwrap();
            repo.store_dense_vector(id, &[1.0, 0.0]).unwrap();
        }

        let request = make_request("animated landing page", vec![], None);
        let mut seed = make_candidate("procedure", "active", 0.9, "seed");
        seed.knowledge_id = "seed".to_string();
        seed.retrieval_text = format!("UI animation guidance {marker}");
        let query_phase = detect_query_phase(&request);
        let query_cue_bundle = build_query_cue_bundle(&request, query_phase);
        let query_vector = vec![1.0, 0.0];
        let scoring = CandidateScoringContext {
            max_bm25: 0.0,
            combined_query: "animated landing page",
            project_id: &request.project_id,
            request_files: &request.files,
            weights: &P0_WEIGHTS,
            query_phase,
            query_cue_bundle: &query_cue_bundle,
            query_vector: &query_vector,
        };
        let mut candidates = vec![(seed, 1.0)];

        let expanded = expand_skill_bundles(
            &repo,
            &mut candidates,
            &request,
            KnowledgeTier::Active.as_str(),
            &scoring,
        )
        .unwrap();

        assert!(expanded);
        let ids: std::collections::HashSet<&str> = candidates
            .iter()
            .map(|(candidate, _)| candidate.knowledge_id.as_str())
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("seed"));
        assert!(ids.contains("sibling-a"));
        assert!(ids.contains("sibling-b"));
    }

    #[test]
    fn test_intent_weights_action_oriented() {
        let action_weights = weights_for_intent(QueryIntent::ActionOriented);
        let general_weights = weights_for_intent(QueryIntent::General);
        // Action-oriented should boost claim weight (index 3) compared to general
        assert!(
            action_weights[3] >= general_weights[3],
            "action-oriented should boost claim weight: action={} general={}",
            action_weights[3],
            general_weights[3]
        );
    }

    // ------------------------------------------------------------------
    // MMR diversity reranking tests
    // ------------------------------------------------------------------

    fn make_candidate_with_vec(id: &str, score: f32, vec: Vec<f32>) -> (Candidate, f32) {
        let mut c = make_candidate("claim", "active", 0.8, id);
        c.knowledge_id = id.to_string();
        c.dense_vector = vec;
        (c, score)
    }

    #[test]
    fn test_mmr_selects_top_k() {
        let candidates: Vec<(Candidate, f32)> = (0..10)
            .map(|i| {
                let vec: Vec<f32> = (0..8).map(|j| if j == i % 8 { 1.0 } else { 0.0 }).collect();
                make_candidate_with_vec(&format!("id-{i}"), 1.0 - 0.05 * i as f32, vec)
            })
            .collect();

        let result = mmr_rerank(&candidates, 5, 0.7);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn test_mmr_diversity_avoids_duplicates() {
        let vec_a = vec![1.0_f32, 0.0, 0.0, 0.0];
        let vec_b = vec![1.0_f32, 0.0, 0.0, 0.0];
        let vec_c = vec![0.0_f32, 1.0, 0.0, 0.0];

        let candidates = vec![
            make_candidate_with_vec("a", 1.0, vec_a),
            make_candidate_with_vec("b", 0.95, vec_b),
            make_candidate_with_vec("c", 0.90, vec_c),
        ];

        let result = mmr_rerank(&candidates, 2, 0.7);
        assert_eq!(result.len(), 2);
        let ids: Vec<&str> = result
            .iter()
            .map(|(c, _)| c.knowledge_id.as_str())
            .collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"c"));
    }

    #[test]
    fn test_mmr_empty_input() {
        let result = mmr_rerank(&[], 5, 0.7);
        assert!(result.is_empty());
    }

    // ------------------------------------------------------------------
    // Token budget enforcement tests
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Phase detection tests (arXiv:2602.21611 subtask-scoped lesson)
    // ------------------------------------------------------------------

    fn make_phase_request(ctx: &str, task: Option<&str>) -> RetrievalRequest {
        RetrievalRequest {
            project_id: "proj".to_string(),
            context: ctx.to_string(),
            files: vec![],
            task: task.map(String::from),
            preferred_types: vec![],
            tier: KnowledgeTier::Active,
            source_filter: SourceFilter::Any,
        }
    }

    #[test]
    fn test_detect_query_phase_edit() {
        let req = make_phase_request(
            "how do I fix the bug in the authentication middleware",
            None,
        );
        assert_eq!(detect_query_phase(&req), Some(KnowledgePhase::Edit));
    }

    #[test]
    fn test_detect_query_phase_analyze() {
        let req = make_phase_request("what is the architecture of the query planner", None);
        assert_eq!(detect_query_phase(&req), Some(KnowledgePhase::Analyze));
    }

    #[test]
    fn test_detect_query_phase_verify() {
        let req = make_phase_request(
            "how do I run the tests for this module to confirm the fix",
            None,
        );
        assert_eq!(detect_query_phase(&req), Some(KnowledgePhase::Verify));
    }

    #[test]
    fn test_detect_query_phase_reproduce() {
        let req = make_phase_request(
            "how do I reproduce this failing test in a minimal example",
            None,
        );
        assert_eq!(detect_query_phase(&req), Some(KnowledgePhase::Reproduce));
    }

    #[test]
    fn test_detect_query_phase_none_for_neutral_query() {
        let req = make_phase_request("hello world", None);
        assert_eq!(detect_query_phase(&req), None);
    }

    #[test]
    fn task_overlap_excludes_identity_terms_without_substring_matches() {
        let query = "Oh a potato ingredient display strings are inconsistent across surfaces";
        let mut app_icon = make_candidate_for_budget("app-icon", "claim", 0);
        app_icon.title =
            "For OH a potato UI copy, avoid em dashes in user-facing strings".to_string();
        app_icon.retrieval_text =
            "For OH a potato UI copy, use commas and simpler sentence breaks.".to_string();

        let mut ingredient = make_candidate_for_budget("ingredient", "claim", 0);
        ingredient.title =
            "In ohapotato, ingredient display consistency should preserve original lines"
                .to_string();
        ingredient.retrieval_text =
            "Route recipe detail, edit, meal-plan missing ingredients, shopping, and fridge surfaces through QuantityDisplayFormatter.".to_string();

        assert_eq!(task_overlap_count(query, "ohapotato", &app_icon), 2);
        assert!(
            task_overlap_count(query, "ohapotato", &ingredient) >= 3,
            "ingredient memory should have task grounding beyond identity"
        );

        let mut translink = make_candidate_for_budget("translink", "claim", 0);
        translink.title = "Deep link routing should stay stable".to_string();
        translink.retrieval_text = "Keep link parsing behavior covered by tests.".to_string();
        assert_eq!(
            task_overlap_count("Fix link routing", "translink", &translink),
            2
        );
    }

    #[test]
    fn grounded_filter_keeps_candidates_when_no_strong_grounding_exists() {
        let mut one_overlap = make_candidate_for_budget("one-overlap", "claim", 0);
        one_overlap.title = "Ingredient note".to_string();
        one_overlap.retrieval_text = "Use the ingredient formatter.".to_string();
        let mut unrelated = make_candidate_for_budget("unrelated", "claim", 0);
        unrelated.title = "Onboarding note".to_string();
        unrelated.retrieval_text = "Handle notification permission timing.".to_string();

        let candidates = vec![(one_overlap, 1.0), (unrelated, 0.5)];
        let filtered = keep_grounded_candidates(candidates, "ingredient display", "ohapotato");

        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn grounded_filter_keeps_candidates_when_filter_would_leave_too_few() {
        let mut strong = make_candidate_for_budget("strong", "claim", 0);
        strong.title = "Ingredient display strings".to_string();
        strong.retrieval_text = "Keep ingredient display strings consistent.".to_string();
        let mut weak = make_candidate_for_budget("weak", "claim", 0);
        weak.title = "Ingredient note".to_string();
        weak.retrieval_text = "Use the ingredient formatter.".to_string();

        let candidates = vec![(strong, 1.0), (weak, 0.5)];
        let filtered =
            keep_grounded_candidates(candidates, "ingredient display strings", "ohapotato");

        assert_eq!(filtered.len(), 2);
    }

    fn make_candidate_for_budget(id: &str, kind: &str, text_len: usize) -> Candidate {
        Candidate {
            knowledge_id: id.to_string(),
            project_id: "test-project".to_string(),
            kind: kind.to_string(),
            title: format!("Title {id}"),
            retrieval_text: "x".repeat(text_len),
            payload_json: "{}".to_string(),
            confidence: 0.8,
            state: "active".to_string(),
            created_at: Utc::now().to_rfc3339(),
            affected_files: vec![],
            session_date: None,
            dense_similarity: 0.8,
            bm25_score: 5.0,
            dense_vector: vec![],
            phase: None,
            owner: None,
            source_origin: KnowledgeOrigin::Learned,
            cue_bundle: None,
        }
    }

    #[test]
    fn test_token_budget_truncates_at_limit() {
        let candidates: Vec<(Candidate, f32)> = (0..20)
            .map(|i| {
                (
                    make_candidate_for_budget(&format!("id-{i}"), "claim", 500),
                    1.0 - i as f32 * 0.01,
                )
            })
            .collect();

        let (lessons, total_tokens) = apply_token_budget(candidates, 1_000);
        assert!(
            total_tokens <= 1_100,
            "total tokens should respect budget, got {total_tokens}"
        );
        assert!(!lessons.is_empty());
    }

    #[test]
    fn test_token_budget_full_detail_for_top_3() {
        let candidates: Vec<(Candidate, f32)> = (0..5)
            .map(|i| {
                (
                    make_candidate_for_budget(&format!("id-{i}"), "claim", 50),
                    1.0,
                )
            })
            .collect();

        let (lessons, _) = apply_token_budget(candidates, 100_000);
        for (i, m) in lessons.iter().enumerate() {
            if i < 3 {
                assert!(m.content.contains(r#"<remembered_note id="lesson:id-"#));
                assert!(m.content.contains(r#"kind="claim""#));
                assert!(m.content.contains("Previously remembered:"));
                assert!(!m.content.contains("Treat it as a hypothesis"));
                assert!(!m.content.contains("(summary)"));
            }
        }
        if lessons.len() > 3 {
            assert!(lessons[3].content.contains(r#"summary="true""#));
            assert!(lessons[3].content.contains("Short memory:"));
            assert!(!lessons[3].content.contains("Treat it as a hypothesis"));
        }
    }

    #[test]
    fn test_directive_retrieval_is_framed_as_prior_memory() {
        let mut candidate = make_candidate_for_budget("id-directive", "directive", 50);
        candidate.title = "Use server-backed retrieval".to_string();
        candidate.payload_json = serde_json::json!({
            "action": "Route through the shared server when configured.",
            "avoid": ["treating local results as authoritative"],
            "reason": "The shared server is the durable source of truth.",
            "verify": "Check FIELDCRAFT_SERVER_URL before assuming local mode.",
            "files": ["install/instructions/codex.md"],
            "trigger": "A coding agent starts work in Fieldcraft.",
            "invalid_if": null,
            "retrieval_queries": []
        })
        .to_string();

        let content = format_full(&candidate);

        assert!(content.contains(r#"<remembered_note id="lesson:id-directive""#));
        assert!(content.contains(r#"kind="directive""#));
        assert!(content.contains("What worked before: Route through the shared server"));
        assert!(
            content
                .contains("What to avoid from that case: treating local results as authoritative")
        );
        assert!(
            content.contains(
                "Why it mattered then: The shared server is the durable source of truth."
            )
        );
        assert!(content.contains("How it was checked then: Check FIELDCRAFT_SERVER_URL"));
        assert!(content.contains("Files involved then: install/instructions/codex.md"));
        assert!(content.contains("</remembered_note>"));
        assert!(!content.contains("Treat it as a hypothesis"));
        assert!(!content.contains("DO:"));
        assert!(!content.contains("AVOID:"));
        assert!(!content.contains("\nFILES:"));
    }

    #[test]
    fn test_context_retrieval_is_framed_as_prior_memory() {
        let mut candidate = make_candidate_for_budget("id-context", "context", 50);
        candidate.title = "Server URL controls routing".to_string();
        candidate.payload_json = serde_json::json!({
            "statement": "FIELDCRAFT_SERVER_URL routes all queries to the shared server.",
            "scope": "multi-machine setups",
            "files": [],
            "implications": ["local results may be stale"],
            "invalid_if": null,
            "retrieval_queries": []
        })
        .to_string();

        let content = format_full(&candidate);

        assert!(content.contains(r#"<remembered_note id="lesson:id-context""#));
        assert!(content.contains(r#"kind="context""#));
        assert!(content.contains("Previously observed: FIELDCRAFT_SERVER_URL routes all queries"));
        assert!(content.contains("Where it applied then: multi-machine setups"));
        assert!(content.contains("What that suggested then: local results may be stale"));
        assert!(!content.contains("Treat it as a hypothesis"));
        assert!(!content.contains("\nKNOW:"));
        assert!(!content.contains("\nSCOPE:"));
    }

    #[test]
    fn test_minimal_budget_entries_keep_remembered_note_marker() {
        let candidates: Vec<(Candidate, f32)> = (0..9)
            .map(|i| {
                (
                    make_candidate_for_budget(&format!("id-{i}"), "claim", 50),
                    1.0,
                )
            })
            .collect();

        let (lessons, _) = apply_token_budget(candidates, 100_000);

        assert!(
            lessons[8]
                .content
                .contains(r#"<remembered_note id="lesson:id-8""#)
        );
        assert!(lessons[8].content.contains(r#"kind="claim""#));
        assert!(!lessons[8].content.contains("[CLAIM]"));
    }

    #[test]
    fn test_title_only_memory_block_escapes_xml_attributes() {
        let candidates: Vec<(Candidate, f32)> = (0..9)
            .map(|i| {
                let mut candidate = make_candidate_for_budget(&format!("id-{i}"), "claim", 50);
                if i == 8 {
                    candidate.knowledge_id = r#"id-"8"&<tag>'"#.to_string();
                    candidate.kind = r#"claim"<kind>'"#.to_string();
                    candidate.title = r#"Use "quoted" <server> & it's ok"#.to_string();
                }
                (candidate, 1.0)
            })
            .collect();

        let (lessons, _) = apply_token_budget(candidates, 100_000);

        assert!(
            lessons[8]
                .content
                .contains(r#"id="lesson:id-&quot;8&quot;&amp;&lt;tag&gt;&apos;""#)
        );
        assert!(
            lessons[8]
                .content
                .contains(r#"kind="claim&quot;&lt;kind&gt;&apos;""#)
        );
        assert!(
            lessons[8]
                .content
                .contains(r#"title="Use &quot;quoted&quot; &lt;server&gt; &amp; it&apos;s ok""#)
        );
        assert!(lessons[8].content.ends_with(" />"));
    }

    #[test]
    fn test_memory_block_format_escapes_xml_markup_characters() {
        let mut candidate = make_candidate_for_budget("id-escape", "context", 50);
        candidate.title = r#"Use "quoted" <server>"#.to_string();
        candidate.payload_json = serde_json::json!({
            "statement": "A < B & C > D.",
            "scope": "quotes \" and apostrophes '",
            "files": [],
            "implications": [],
            "invalid_if": null,
            "retrieval_queries": []
        })
        .to_string();

        let content = format_full(&candidate);

        assert!(content.contains(r#"title="Use &quot;quoted&quot; &lt;server&gt;""#));
        assert!(content.contains("A &lt; B &amp; C &gt; D."));
        // Quotes do not need escaping in XML body text, only in attributes.
        assert!(content.contains("quotes \" and apostrophes '"));
    }

    #[test]
    fn test_summary_memory_block_escapes_xml_markup_characters() {
        let mut candidate = make_candidate_for_budget("id-summary", "claim", 50);
        candidate.retrieval_text = "A < B & C > D. Second sentence.".to_string();

        let content = format_summary(&candidate);

        assert!(content.contains(r#"<remembered_note id="lesson:id-summary""#));
        assert!(content.contains(r#"summary="true""#));
        assert!(content.contains("Short memory: A &lt; B &amp; C &gt; D"));
        assert!(!content.contains("Second sentence"));
    }

    #[test]
    fn test_malformed_structured_payload_falls_back_to_generic_memory_block() {
        let mut candidate = make_candidate_for_budget("id-malformed", "directive", 50);
        candidate.payload_json = "{not valid json".to_string();
        candidate.retrieval_text = "Fallback <memory> & text.".to_string();

        let content = format_full(&candidate);

        assert!(content.contains(r#"<remembered_note id="lesson:id-malformed""#));
        assert!(content.contains(r#"kind="directive""#));
        assert!(content.contains("Previously remembered:"));
        assert!(content.contains("Fallback &lt;memory&gt; &amp; text."));
    }

    // ------------------------------------------------------------------
    // Embedding text construction tests
    // ------------------------------------------------------------------

    fn make_record(kind: KnowledgeKind, payload_json: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            protected: false,
            id: "test-id".to_string(),
            project_id: "test-project".to_string(),
            kind,
            title: "Test record".to_string(),
            payload_json: payload_json.to_string(),
            state: fieldcraft_core::KnowledgeState::Active,
            confidence: 0.8,
            source: KnowledgeSource::Manual,
            superseded_by: None,
            project_specific: false,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: String::new(),
            schema_version: 2,
            phase: None,
            tier: KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
        }
    }

    #[test]
    fn test_embedding_text_claim() {
        let payload = ClaimPayload {
            statement: "PostgreSQL advisory locks deadlock when pool > 20".to_string(),
            mechanism: Some("circular wait under high concurrency".to_string()),
            scope: Some("src/db/pool.rs".to_string()),
            ..Default::default()
        };
        let record = make_record(
            KnowledgeKind::Claim,
            &serde_json::to_string(&payload).unwrap(),
        );
        let text = build_embedding_text(&record);
        assert!(text.contains("PostgreSQL advisory locks"));
        assert!(text.contains("circular wait"));
        assert!(text.contains("src/db/pool.rs"));
        assert!(text.contains("| scope: "));
    }

    #[test]
    fn test_embedding_text_procedure() {
        let payload = ProcedurePayload {
            goal: "Deploy to production".to_string(),
            steps: vec![
                "Run init.sh".to_string(),
                "Health check port 3000".to_string(),
                "Verify deployment".to_string(),
                "Extra step not included".to_string(),
            ],
            preconditions: vec!["Docker daemon running".to_string()],
            ..Default::default()
        };
        let record = make_record(
            KnowledgeKind::Procedure,
            &serde_json::to_string(&payload).unwrap(),
        );
        let text = build_embedding_text(&record);
        assert!(text.contains("Deploy to production"));
        assert!(text.contains("Run init.sh"));
        assert!(text.contains("Docker daemon running"));
    }

    #[test]
    fn test_bm25_normalization_uses_max() {
        let candidate = make_candidate("claim", "active", 0.8, "test");
        let weights = P0_WEIGHTS;
        // With max_bm25=5.0, a candidate with bm25=5.0 should get normalized to 1.0
        let score_high = compute_score(&candidate, "test", 0, 5.0, &[], &weights);
        // With max_bm25=50.0, same candidate should get lower normalized BM25
        let score_low = compute_score(&candidate, "test", 0, 50.0, &[], &weights);
        assert!(
            score_high > score_low,
            "higher max_bm25 should reduce normalized score: {score_high} vs {score_low}"
        );
    }
}
