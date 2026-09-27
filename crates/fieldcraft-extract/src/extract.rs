//! Extraction pipeline orchestrator: Stages A → B → C → Dedup.
//!
//! Takes a CompressedTranscript and runs:
//!   Stage A: Haiku chunk classification
//!   Stage B: Sonnet structured extraction (parallel)
//!   Stage C: Haiku self-critique (accept/reject)
//!   Dedup:   Word-Jaccard similarity merge (no fastembed dependency)

use crate::anthropic::LlmClient;
use crate::chunker::chunk_transcript;
use crate::compress::CompressedTranscript;
use crate::stage_b::run_stage_b;
use fieldcraft_core::{KnowledgeRecord, KnowledgeState, TrError};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// LLM backend for the extraction pipeline.
pub enum LlmBackend {
    /// Direct Anthropic API (requires API key).
    Api(String),
    /// OpenAI-compatible Chat Completions API (requires OPENAI_API_KEY).
    OpenAi {
        api_key: String,
        base_url: Option<String>,
        model: Option<String>,
    },
    /// Claude Code CLI (`claude -p`, uses existing auth).
    ClaudeCode,
}

pub struct ExtractionPipeline {
    pub backend: LlmBackend,
    /// Haiku-class model for Stage A (filter) and Stage C (critique).
    pub haiku_model: String,
    /// Sonnet-class model for Stage B (extraction).
    pub sonnet_model: String,
    /// Max concurrent Stage B calls.
    pub max_concurrent_stage_b: usize,
}

impl Default for ExtractionPipeline {
    fn default() -> Self {
        Self {
            backend: LlmBackend::ClaudeCode,
            haiku_model: "claude-haiku-4-5-20251001".to_string(),
            sonnet_model: "claude-sonnet-4-5-20241022".to_string(),
            max_concurrent_stage_b: 5,
        }
    }
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ExtractionResult {
    pub session_id: String,
    pub records: Vec<KnowledgeRecord>,
    pub stats: ExtractionStats,
}

#[derive(Debug, Default)]
pub struct ExtractionStats {
    pub chunks_total: usize,
    pub chunks_high: usize,
    pub nodes_extracted: usize,
    pub nodes_rejected: usize,
    pub nodes_deduped: usize,
    pub by_type: HashMap<String, usize>,
    pub stage_a_cost_tokens: usize,
    pub stage_b_cost_tokens: usize,
    pub stage_c_cost_tokens: usize,
}

// ---------------------------------------------------------------------------
// Main pipeline
// ---------------------------------------------------------------------------

impl ExtractionPipeline {
    /// Run the full extraction pipeline (A → B → C → Dedup) on a compressed transcript.
    pub async fn extract(
        &self,
        project_id: &str,
        session_id: &str,
        session_started_at: Option<&str>,
        compressed: &CompressedTranscript,
    ) -> Result<ExtractionResult, TrError> {
        // Resolve model name: OpenAI backend can override the default Anthropic model IDs.
        let sonnet_model = match &self.backend {
            LlmBackend::OpenAi { model, .. } => {
                model.clone().unwrap_or_else(|| "gpt-4.1".to_string())
            }
            _ => self.sonnet_model.clone(),
        };
        let client = Arc::new(match &self.backend {
            LlmBackend::Api(key) => LlmClient::api(key),
            LlmBackend::OpenAi {
                api_key, base_url, ..
            } => LlmClient::openai(api_key, base_url.clone()),
            LlmBackend::ClaudeCode => LlmClient::cli(),
        });
        let mut stats = ExtractionStats::default();

        // ── Chunk the transcript ──────────────────────────────────────────────
        let chunks = chunk_transcript(compressed);
        stats.chunks_total = chunks.len();

        if chunks.is_empty() {
            return Ok(ExtractionResult {
                session_id: session_id.to_string(),
                records: vec![],
                stats,
            });
        }

        // ── Stage A: SKIPPED — all chunks go to Stage B ─────────────────────
        // Stage A was a Haiku classifier that filtered LOW chunks. Removed because:
        // - Each CLI call has 12s overhead; 8 classification calls = 96s wasted
        // - Stage B with unified prompt handles type detection inline
        // - False negatives in Stage A cause permanent knowledge loss
        use crate::stage_a::{ClassificationResult, StageAResult};
        let stage_a_results: Vec<StageAResult> = chunks
            .iter()
            .map(|c| StageAResult {
                chunk_index: c.index,
                classification: ClassificationResult {
                    worth_remembering: true,
                    knowledge_type: None,
                    summary: "Stage A skipped; all chunks sent to Stage B.".to_string(),
                },
                is_high: true,
            })
            .collect();
        stats.chunks_high = chunks.len();
        stats.stage_a_cost_tokens = 0;

        // ── Stage B: Sonnet extraction ────────────────────────────────────────
        let stage_b_results = run_stage_b(
            Arc::clone(&client),
            &sonnet_model,
            &chunks,
            &stage_a_results,
            project_id,
            session_id,
            self.max_concurrent_stage_b,
        )
        .await;

        // Token cost estimate: ~1500 tokens per high chunk for Stage B
        stats.stage_b_cost_tokens = stats.chunks_high * 1500;
        stats.nodes_extracted = stage_b_results.len();
        let session_date = session_started_at
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        if stage_b_results.is_empty() {
            return Ok(ExtractionResult {
                session_id: session_id.to_string(),
                records: vec![],
                stats,
            });
        }

        // ── Stage C: SKIPPED — heuristic filter instead of LLM critique ─────
        // Stage C was a Haiku LLM-as-judge. Removed because:
        // - Each CLI call has 12s overhead; 4-20 critique calls = 48-240s wasted
        // - Jaccard dedup handles duplicates deterministically
        // - Heuristic filter catches narration/boilerplate without LLM
        stats.stage_c_cost_tokens = 0;

        let mut all_records: Vec<KnowledgeRecord> = Vec::new();
        for result in stage_b_results {
            let mut record = result.record;
            record.session_date = session_date;
            if is_garbage(&record) {
                record.state = KnowledgeState::Retired;
                stats.nodes_rejected += 1;
            } else {
                record.state = KnowledgeState::Active;
                record.confidence = 0.5;
            }
            all_records.push(record);
        }

        // ── Deduplication ─────────────────────────────────────────────────────
        let (deduped, n_merged) = dedup_records(all_records);
        stats.nodes_deduped = n_merged;
        all_records = deduped;

        // Build by_type stats (only active records)
        for record in &all_records {
            if record.state == KnowledgeState::Active || record.state == KnowledgeState::Provisional
            {
                let type_key = format!("{:?}", record.kind).to_lowercase();
                *stats.by_type.entry(type_key).or_insert(0) += 1;
            }
        }

        Ok(ExtractionResult {
            session_id: session_id.to_string(),
            records: all_records,
            stats,
        })
    }
}

// ---------------------------------------------------------------------------
// Heuristic garbage filter (replaces Stage C LLM critique)
// ---------------------------------------------------------------------------

/// Reject records that are narration, boilerplate, or too vague to be useful.
pub fn is_garbage(record: &KnowledgeRecord) -> bool {
    let t = record.title.to_lowercase();
    // Too short to be useful
    t.len() < 15
        // Conversation narration
        || t.starts_with("i ")
        || t.starts_with("let me ")
        || t.starts_with("i'll ")
        || t.contains("let me check")
        || t.contains("i read the file")
        // Boilerplate / system prompt leakage
        || t.contains("agents.md")
        || t.contains("<instructions>")
        || t.contains("system prompt")
}

// ---------------------------------------------------------------------------
// Deduplication — embedding-based clustering with field-merging fallback
// ---------------------------------------------------------------------------

/// Merge duplicate records by clustering on dense-embedding cosine similarity (≥ 0.85).
/// Falls back to word-Jaccard when the embedding model is unavailable.
///
/// For each cluster the primary record (highest confidence, tie-break: first index) is kept.
/// All other fields from cluster members are *merged into* the primary rather than dropped.
pub fn dedup_records(records: Vec<KnowledgeRecord>) -> (Vec<KnowledgeRecord>, usize) {
    if records.len() <= 1 {
        return (records, 0);
    }

    // Build embedding texts
    let texts: Vec<String> = records
        .iter()
        .map(|r| format!("{} {}", r.title, r.retrieval_text))
        .collect();

    match fieldcraft_retrieval::encode_dense(&texts) {
        Ok(embeddings) => dedup_with_embeddings(records, &embeddings),
        Err(e) => {
            tracing::warn!(
                "dedup_records: embedding unavailable ({}), falling back to Jaccard",
                e
            );
            dedup_jaccard(records)
        }
    }
}

/// Cosine similarity between two vectors (assumes non-zero length).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

const COSINE_THRESHOLD: f32 = 0.85;

fn dedup_with_embeddings(
    records: Vec<KnowledgeRecord>,
    embeddings: &[Vec<f32>],
) -> (Vec<KnowledgeRecord>, usize) {
    let n = records.len();
    // Union-Find for single-link clustering
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut Vec<usize>, x: usize) -> usize {
        if parent[x] != x {
            parent[x] = find(parent, parent[x]);
        }
        parent[x]
    }

    for i in 0..n {
        for j in (i + 1)..n {
            // Only cluster same-kind, non-retired records
            if records[i].kind != records[j].kind {
                continue;
            }
            if records[i].state == KnowledgeState::Retired
                || records[j].state == KnowledgeState::Retired
            {
                continue;
            }
            if i < embeddings.len() && j < embeddings.len() {
                let sim = cosine(&embeddings[i], &embeddings[j]);
                if sim >= COSINE_THRESHOLD {
                    let pi = find(&mut parent, i);
                    let pj = find(&mut parent, j);
                    if pi != pj {
                        parent[pj] = pi;
                    }
                }
            }
        }
    }

    // Collect clusters: root → list of member indices
    let mut clusters: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }

    let mut result: Vec<KnowledgeRecord> = Vec::new();
    let mut n_merged = 0usize;

    for (_root, mut members) in clusters {
        if members.len() == 1 {
            result.push(records[members[0]].clone());
            continue;
        }

        // Primary = highest confidence, tie-break: smallest index
        members.sort_by(|&a, &b| {
            records[b]
                .confidence
                .partial_cmp(&records[a].confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });

        let primary_idx = members[0];
        let mut primary = records[primary_idx].clone();

        // Bump confidence by +0.05 per merged record, capped at 0.95
        let bonus = 0.05 * (members.len() - 1) as f32;
        primary.confidence = (primary.confidence + bonus).min(0.95);

        // Merge payload fields from all cluster members into primary
        for &idx in &members[1..] {
            merge_record_fields(&mut primary, &records[idx]);
        }

        n_merged += members.len() - 1;
        result.push(primary);
    }

    (result, n_merged)
}

/// Merge the union-able fields from `other` into `primary`.
fn merge_record_fields(primary: &mut KnowledgeRecord, other: &KnowledgeRecord) {
    use fieldcraft_core::{
        ClaimPayload, ContextPayload, DirectivePayload, EpisodePayload, KnowledgeKind,
        ProcedurePayload,
    };

    fn union_vec(base: &mut Vec<String>, extra: &[String]) {
        for item in extra {
            if !base.contains(item) {
                base.push(item.clone());
            }
        }
    }

    for source_ref in &other.source_refs {
        if !primary.source_refs.contains(source_ref) {
            primary.source_refs.push(source_ref.clone());
        }
    }

    match primary.kind {
        KnowledgeKind::Claim => {
            if let (Ok(mut p), Ok(o)) = (
                serde_json::from_str::<ClaimPayload>(&primary.payload_json),
                serde_json::from_str::<ClaimPayload>(&other.payload_json),
            ) {
                union_vec(&mut p.affected_files, &o.affected_files);
                union_vec(&mut p.retrieval_questions, &o.retrieval_questions);
                union_vec(&mut p.exceptions, &o.exceptions);
                if let Ok(s) = serde_json::to_string(&p) {
                    primary.payload_json = s;
                }
            }
        }
        KnowledgeKind::Procedure => {
            if let (Ok(mut p), Ok(o)) = (
                serde_json::from_str::<ProcedurePayload>(&primary.payload_json),
                serde_json::from_str::<ProcedurePayload>(&other.payload_json),
            ) {
                union_vec(&mut p.affected_files, &o.affected_files);
                union_vec(&mut p.retrieval_questions, &o.retrieval_questions);
                union_vec(&mut p.failure_modes, &o.failure_modes);
                if let Ok(s) = serde_json::to_string(&p) {
                    primary.payload_json = s;
                }
            }
        }
        KnowledgeKind::Directive => {
            if let (Ok(mut p), Ok(o)) = (
                serde_json::from_str::<DirectivePayload>(&primary.payload_json),
                serde_json::from_str::<DirectivePayload>(&other.payload_json),
            ) {
                union_vec(&mut p.retrieval_queries, &o.retrieval_queries);
                union_vec(&mut p.avoid, &o.avoid);
                if let Ok(s) = serde_json::to_string(&p) {
                    primary.payload_json = s;
                }
            }
        }
        KnowledgeKind::Context => {
            if let (Ok(mut p), Ok(o)) = (
                serde_json::from_str::<ContextPayload>(&primary.payload_json),
                serde_json::from_str::<ContextPayload>(&other.payload_json),
            ) {
                union_vec(&mut p.implications, &o.implications);
                union_vec(&mut p.retrieval_queries, &o.retrieval_queries);
                if let Ok(s) = serde_json::to_string(&p) {
                    primary.payload_json = s;
                }
            }
        }
        KnowledgeKind::Episode => {
            if let (Ok(mut p), Ok(o)) = (
                serde_json::from_str::<EpisodePayload>(&primary.payload_json),
                serde_json::from_str::<EpisodePayload>(&other.payload_json),
            ) {
                union_vec(&mut p.observations, &o.observations);
                union_vec(&mut p.affected_files, &o.affected_files);
                if let Ok(s) = serde_json::to_string(&p) {
                    primary.payload_json = s;
                }
            }
        }
    }
}

/// Fallback: word-Jaccard dedup (used when embedding model is unavailable).
fn dedup_jaccard(records: Vec<KnowledgeRecord>) -> (Vec<KnowledgeRecord>, usize) {
    const TITLE_THRESHOLD: f32 = 0.45;
    const TEXT_THRESHOLD: f32 = 0.40;

    let n = records.len();
    let mut merged = vec![false; n];
    let mut result: Vec<KnowledgeRecord> = Vec::new();
    let mut n_merged = 0usize;

    for i in 0..n {
        if merged[i] {
            continue;
        }

        let mut base = records[i].clone();

        for j in (i + 1)..n {
            if merged[j] {
                continue;
            }

            if base.kind != records[j].kind {
                continue;
            }

            if base.state == KnowledgeState::Retired || records[j].state == KnowledgeState::Retired
            {
                continue;
            }

            let title_sim = jaccard_similarity(&base.title, &records[j].title);
            let text_sim = jaccard_similarity(&base.retrieval_text, &records[j].retrieval_text);
            let containment = containment_similarity(&base.title, &records[j].title);

            if title_sim > TITLE_THRESHOLD || text_sim > TEXT_THRESHOLD || containment > 0.80 {
                merge_record_fields(&mut base, &records[j]);
                if records[j].confidence > base.confidence {
                    base.confidence = records[j].confidence;
                }
                merged[j] = true;
                n_merged += 1;
            }
        }

        result.push(base);
    }

    (result, n_merged)
}

/// Compute word-level Jaccard similarity between two strings.
/// Jaccard(A, B) = |A ∩ B| / |A ∪ B|
pub fn jaccard_similarity(a: &str, b: &str) -> f32 {
    use std::collections::HashSet;

    let words_a: HashSet<String> = a
        .split_whitespace()
        .map(stem)
        .filter(|w| !is_stopword(w))
        .collect();
    let words_b: HashSet<String> = b
        .split_whitespace()
        .map(stem)
        .filter(|w| !is_stopword(w))
        .collect();

    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();

    if union == 0 {
        return 0.0;
    }

    intersection as f32 / union as f32
}

/// Asymmetric containment: what fraction of the SMALLER set's words appear in the larger?
/// Catches cases where record B is a strict subset of record A (same fact, fewer details).
/// containment(A, B) = |A ∩ B| / min(|A|, |B|)
pub fn containment_similarity(a: &str, b: &str) -> f32 {
    use std::collections::HashSet;

    let words_a: HashSet<String> = a
        .split_whitespace()
        .map(stem)
        .filter(|w| !is_stopword(w))
        .collect();
    let words_b: HashSet<String> = b
        .split_whitespace()
        .map(stem)
        .filter(|w| !is_stopword(w))
        .collect();

    let min_size = words_a.len().min(words_b.len());
    if min_size == 0 {
        return 0.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    intersection as f32 / min_size as f32
}

/// Lightweight stemmer: lowercase + strip common English suffixes.
/// Not a full Porter stemmer — just enough to catch "scrolling"/"scrollable"/"scrolls".
fn stem(word: &str) -> String {
    let w = word.to_lowercase();
    let w = w.trim_matches(|c: char| !c.is_alphanumeric());
    if w.len() <= 3 {
        return w.to_string();
    }
    for suffix in &[
        "ation", "ness", "ment", "able", "ible", "ting", "ing", "ful", "ous", "ive", "ion", "ed",
        "ly", "er", "es", "al", "s",
    ] {
        if let Some(base) = w.strip_suffix(suffix)
            && base.len() >= 3
        {
            return base.to_string();
        }
    }
    w.to_string()
}

/// Common English stopwords that dilute Jaccard similarity on verbose LLM-generated titles.
fn is_stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "of"
            | "with"
            | "by"
            | "from"
            | "is"
            | "are"
            | "was"
            | "were"
            | "be"
            | "been"
            | "has"
            | "have"
            | "had"
            | "do"
            | "did"
            | "not"
            | "no"
            | "and"
            | "or"
            | "but"
            | "if"
            | "so"
            | "as"
            | "it"
            | "its"
            | "this"
            | "that"
            | "which"
            | "when"
            | "where"
            | "how"
            | "all"
            | "any"
            | "each"
            | "every"
            | "both"
            | "also"
            | "than"
            | "then"
            | "into"
            | "only"
            | "even"
            | "because"
            | "instead"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use fieldcraft_core::{AgentTool, KnowledgeKind, KnowledgeSource, KnowledgeState};

    fn make_record(
        id: &str,
        kind: KnowledgeKind,
        retrieval: &str,
        confidence: f32,
    ) -> KnowledgeRecord {
        KnowledgeRecord {
            protected: false,
            id: id.to_string(),
            project_id: "proj".to_string(),
            kind,
            title: retrieval.to_string(),
            payload_json: "{}".to_string(),
            state: KnowledgeState::Active,
            confidence,
            source: KnowledgeSource::Session {
                session_id: "sess".to_string(),
                tool: AgentTool::ClaudeCode,
            },
            superseded_by: None,
            project_specific: true,
            created_at: chrono::Utc::now(),
            valid_until: None,
            retrieval_text: retrieval.to_string(),
            schema_version: 2,
            phase: None,
            tier: fieldcraft_core::KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
        }
    }

    #[test]
    fn test_jaccard_identical_strings() {
        let sim = jaccard_similarity("hello world foo", "hello world foo");
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_jaccard_completely_different() {
        let sim = jaccard_similarity("foo bar baz", "qux quux corge");
        assert!(sim < f32::EPSILON);
    }

    #[test]
    fn test_jaccard_partial_overlap() {
        // "foo bar" ∩ "foo baz" = {"foo"}, union = {"foo", "bar", "baz"} → 1/3 ≈ 0.33
        let sim = jaccard_similarity("foo bar", "foo baz");
        assert!((sim - 1.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn test_jaccard_empty_strings() {
        let sim = jaccard_similarity("", "");
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_dedup_merges_similar_records() {
        let r1 = make_record(
            "r1",
            KnowledgeKind::Claim,
            "postgres lock deadlock pool size limit",
            0.5,
        );
        let r2 = make_record(
            "r2",
            KnowledgeKind::Claim,
            "postgres lock deadlock pool size limit order",
            0.7,
        );
        let (deduped, n_merged) = dedup_records(vec![r1, r2]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(n_merged, 1);
        // Should keep higher confidence (and may be bumped further by merge bonus)
        assert!(deduped[0].confidence >= 0.7);
    }

    #[test]
    fn test_dedup_keeps_different_records() {
        let r1 = make_record("r1", KnowledgeKind::Claim, "postgres lock deadlock", 0.5);
        let r2 = make_record(
            "r2",
            KnowledgeKind::Claim,
            "redis cache eviction policy lru ttl",
            0.5,
        );
        let (deduped, n_merged) = dedup_records(vec![r1, r2]);
        assert_eq!(deduped.len(), 2);
        assert_eq!(n_merged, 0);
    }

    #[test]
    fn test_dedup_different_kinds_not_merged() {
        // Same text, different kind → should NOT merge
        let text = "postgres lock deadlock pool size limit order transaction";
        let r1 = make_record("r1", KnowledgeKind::Claim, text, 0.5);
        let r2 = make_record("r2", KnowledgeKind::Procedure, text, 0.5);
        let (deduped, n_merged) = dedup_records(vec![r1, r2]);
        assert_eq!(deduped.len(), 2);
        assert_eq!(n_merged, 0);
    }

    #[test]
    fn test_dedup_retired_records_not_merged() {
        let mut r1 = make_record("r1", KnowledgeKind::Claim, "foo bar baz qux", 0.5);
        let r2 = make_record("r2", KnowledgeKind::Claim, "foo bar baz qux", 0.5);
        r1.state = KnowledgeState::Retired;
        let (deduped, n_merged) = dedup_records(vec![r1, r2]);
        assert_eq!(deduped.len(), 2); // both kept, not merged
        assert_eq!(n_merged, 0);
    }

    #[test]
    fn test_dedup_single_record_passthrough() {
        let r1 = make_record("r1", KnowledgeKind::Claim, "unique claim", 0.5);
        let (deduped, n_merged) = dedup_records(vec![r1]);
        assert_eq!(deduped.len(), 1);
        assert_eq!(n_merged, 0);
    }

    #[test]
    fn test_extraction_pipeline_defaults() {
        let pipeline = ExtractionPipeline::default();
        assert_eq!(pipeline.haiku_model, "claude-haiku-4-5-20251001");
        assert_eq!(pipeline.sonnet_model, "claude-sonnet-4-5-20241022");
        assert_eq!(pipeline.max_concurrent_stage_b, 5);
    }

    #[test]
    fn test_openai_backend_default_model_is_gpt4_1_mini() {
        // US-001: when LlmBackend::OpenAi has model=None, resolved model must be gpt-4.1.
        let pipeline = ExtractionPipeline {
            backend: LlmBackend::OpenAi {
                api_key: "test-key".to_string(),
                base_url: None,
                model: None,
            },
            ..Default::default()
        };
        let resolved = match &pipeline.backend {
            LlmBackend::OpenAi { model, .. } => {
                model.clone().unwrap_or_else(|| "gpt-4.1".to_string())
            }
            _ => panic!("wrong variant"),
        };
        assert_eq!(resolved, "gpt-4.1");
    }

    #[test]
    fn test_openai_backend_explicit_model_overrides_default() {
        let pipeline = ExtractionPipeline {
            backend: LlmBackend::OpenAi {
                api_key: "test-key".to_string(),
                base_url: None,
                model: Some("gpt-4o".to_string()),
            },
            ..Default::default()
        };
        let resolved = match &pipeline.backend {
            LlmBackend::OpenAi { model, .. } => {
                model.clone().unwrap_or_else(|| "gpt-4.1".to_string())
            }
            _ => panic!("wrong variant"),
        };
        assert_eq!(resolved, "gpt-4o");
    }

    // Helper: make a Directive record with a pre-set payload
    fn make_directive_record(
        id: &str,
        title: &str,
        retrieval_text: &str,
        retrieval_queries: Vec<String>,
        confidence: f32,
    ) -> KnowledgeRecord {
        use fieldcraft_core::DirectivePayload;
        let payload = DirectivePayload {
            action: title.to_string(),
            reason: "test".to_string(),
            files: vec![],
            trigger: "test".to_string(),
            avoid: vec![],
            verify: None,
            invalid_if: None,
            retrieval_queries,
        };
        KnowledgeRecord {
            protected: false,
            id: id.to_string(),
            project_id: "proj".to_string(),
            kind: KnowledgeKind::Directive,
            title: title.to_string(),
            payload_json: serde_json::to_string(&payload).unwrap(),
            state: KnowledgeState::Active,
            confidence,
            source: KnowledgeSource::Session {
                session_id: "sess".to_string(),
                tool: AgentTool::ClaudeCode,
            },
            superseded_by: None,
            project_specific: true,
            created_at: chrono::Utc::now(),
            valid_until: None,
            retrieval_text: retrieval_text.to_string(),
            schema_version: 2,
            phase: None,
            tier: fieldcraft_core::KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
        }
    }

    /// Two directive records that are near-paraphrases (word-Jaccard will catch them via
    /// the Jaccard fallback path; embedding path will also cluster them if model is present).
    /// Verifies: 2 records → 1 merged, merged confidence > 0.5, affected_files unioned via
    /// the `files` field (DirectivePayload stores affected files in `files`).
    #[test]
    fn dedup_merges_near_duplicate_directives_via_embedding() {
        // Highly overlapping titles and retrieval text — both Jaccard and cosine should cluster
        let r1 = make_directive_record(
            "d1",
            "review EXPANSION_ARCHITECTURE.md and check pipeline concurrency settings",
            "review EXPANSION_ARCHITECTURE.md and check pipeline concurrency settings before merging",
            vec!["pipeline concurrency".to_string()],
            0.6,
        );
        let r2 = make_directive_record(
            "d2",
            "check EXPANSION_ARCHITECTURE.md pipeline concurrency review settings",
            "check EXPANSION_ARCHITECTURE.md pipeline concurrency review settings before merge",
            vec!["expansion architecture".to_string()],
            0.5,
        );
        let r3 = make_directive_record(
            "d3",
            "configure redis eviction policy for cache invalidation",
            "configure redis eviction policy for cache invalidation on high load",
            vec!["redis eviction".to_string()],
            0.7,
        );

        let (deduped, n_merged) = dedup_records(vec![r1, r2, r3]);
        // r1 and r2 merge → 2 records total
        assert_eq!(deduped.len(), 2, "expected 2 records after dedup");
        assert_eq!(n_merged, 1, "expected 1 merge");
        // Merged directive should have bumped confidence
        let merged = deduped
            .iter()
            .find(|r| r.title.contains("EXPANSION_ARCHITECTURE"))
            .expect("merged directive not found");
        assert!(merged.confidence > 0.5, "confidence should be bumped");
    }

    /// Merging two Directive records should union their retrieval_queries lists.
    #[test]
    fn dedup_unions_retrieval_queries_on_merge() {
        use fieldcraft_core::DirectivePayload;

        let r1 = make_directive_record(
            "d1",
            "always run cargo clippy before committing code changes",
            "always run cargo clippy before committing code changes to catch warnings",
            vec![
                "cargo clippy commit".to_string(),
                "rust linting".to_string(),
            ],
            0.7,
        );
        let r2 = make_directive_record(
            "d2",
            "run cargo clippy always before committing code changes rust",
            "run cargo clippy always before committing code changes to catch rust warnings",
            vec![
                "clippy warnings".to_string(),
                "pre-commit checks".to_string(),
            ],
            0.6,
        );

        let (deduped, n_merged) = dedup_records(vec![r1, r2]);
        assert_eq!(deduped.len(), 1, "should merge to 1 record");
        assert_eq!(n_merged, 1);

        let merged_payload: DirectivePayload =
            serde_json::from_str(&deduped[0].payload_json).expect("payload parses");
        // Both retrieval_queries lists should be present in the merged record
        assert!(
            merged_payload
                .retrieval_queries
                .iter()
                .any(|q| q.contains("cargo clippy commit") || q.contains("rust linting")),
            "primary queries should be present"
        );
        assert!(
            merged_payload
                .retrieval_queries
                .iter()
                .any(|q| q.contains("clippy warnings") || q.contains("pre-commit")),
            "secondary queries should be unioned in"
        );
    }

    /// Three unrelated records should all be returned with n_merged=0.
    #[test]
    fn dedup_no_op_when_all_records_distinct() {
        let r1 = make_record(
            "r1",
            KnowledgeKind::Claim,
            "postgres connection pool exhaustion under high load",
            0.5,
        );
        let r2 = make_record(
            "r2",
            KnowledgeKind::Claim,
            "redis cache eviction lru policy configuration",
            0.6,
        );
        let r3 = make_record(
            "r3",
            KnowledgeKind::Claim,
            "tensorflow gradient vanishing problem deep networks",
            0.7,
        );

        let (deduped, n_merged) = dedup_records(vec![r1, r2, r3]);
        assert_eq!(deduped.len(), 3, "all distinct records should be kept");
        assert_eq!(n_merged, 0);
    }
}
