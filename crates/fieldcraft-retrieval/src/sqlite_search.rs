//! SQLite-backed search: brute-force cosine similarity + FTS5 BM25.

use fieldcraft_core::{CueBundle, KnowledgeOrigin, KnowledgeSource, SourceFilter, TrError};
use fieldcraft_storage::KnowledgeRepository;
use std::collections::HashMap;

use crate::internal;

/// A search hit from SQLite.
#[derive(Debug, Clone)]
pub(crate) struct SqliteHit {
    pub knowledge_id: String,
    pub project_id: String,
    pub kind: String,
    pub title: String,
    pub retrieval_text: String,
    pub payload_json: String,
    pub confidence: f32,
    pub state: String,
    pub created_at: String,
    pub dense_similarity: f32,
    pub bm25_score: f32,
    pub dense_vector: Vec<f32>,
    /// Subtask phase ("analyze", "reproduce", "edit", "verify") or None for legacy records.
    pub phase: Option<String>,
    /// Session start time (source session `started_at`) in RFC3339 form.
    pub session_date: Option<String>,
    /// User who wrote this lesson (token label at write time). None for legacy records.
    pub owner: Option<String>,
    pub source_origin: KnowledgeOrigin,
    /// Stored cue bundle for conjunctive matching. Populated by hybrid_search
    /// after RRF fusion via a batch fetch; None for records without a bundle
    /// (very legacy or pending backfill).
    pub cue_bundle: Option<CueBundle>,
}

enum SearchScope {
    #[cfg(test)]
    Project(String),
    Unscoped,
}

pub(crate) fn parse_source(source_json: &str) -> KnowledgeSource {
    serde_json::from_str(source_json).unwrap_or(KnowledgeSource::Manual)
}

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

fn dimensions_match(query_vector: &[f32], vector: &[f32]) -> bool {
    !query_vector.is_empty() && !vector.is_empty() && query_vector.len() == vector.len()
}

/// Brute-force dense cosine search over all vectors in a project.
#[cfg(test)]
pub(crate) fn dense_search(
    repo: &KnowledgeRepository,
    project_id: &str,
    query_vector: &[f32],
    limit: usize,
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<SqliteHit>, TrError> {
    dense_search_inner(
        repo,
        SearchScope::Project(project_id.to_string()),
        query_vector,
        limit,
        tier,
        source_filter,
    )
}

/// Dense search across every record in the requested tier. Project identity is
/// handled later as cue/ranking evidence instead of as a hard filter.
///
/// Ranks from the in-memory vector cache (same scan order and cosine as the
/// old full-table read), then loads full rows only for the winners.
pub(crate) fn dense_search_unscoped(
    repo: &KnowledgeRepository,
    query_vector: &[f32],
    limit: usize,
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<SqliteHit>, TrError> {
    let top: Vec<(String, f32, Vec<f32>, KnowledgeOrigin)> =
        crate::record_cache::with_record_cache(repo, |cache| {
            let mut scored: Vec<(&crate::record_cache::CachedRecord, &Vec<f32>, f32)> = cache
                .iter()
                .filter(|entry| entry.tier.as_deref().is_none_or(|t| t == tier))
                .filter(|entry| source_filter.matches(entry.origin))
                .filter_map(|entry| entry.vector.as_ref().map(|vector| (entry, vector)))
                .filter(|(_, vector)| dimensions_match(query_vector, vector))
                .map(|(entry, vector)| (entry, vector, cosine_similarity(query_vector, vector)))
                .collect();
            scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(limit);
            scored
                .into_iter()
                .map(|(entry, vector, sim)| (entry.id.clone(), sim, vector.clone(), entry.origin))
                .collect()
        })?;
    let ids: Vec<String> = top.iter().map(|(id, ..)| id.clone()).collect();
    let mut rows = repo
        .load_dense_hit_rows(&ids)
        .map_err(|e| internal(format!("load_dense_hit_rows: {e}")))?;
    Ok(top
        .into_iter()
        .filter_map(|(id, sim, vector, source_origin)| {
            let row = rows.remove(&id)?;
            Some(SqliteHit {
                knowledge_id: row.id,
                project_id: row.project_id,
                kind: row.kind,
                title: row.title,
                retrieval_text: row.retrieval_text,
                payload_json: row.payload_json,
                confidence: row.confidence,
                state: row.state,
                created_at: row.created_at,
                dense_similarity: sim,
                bm25_score: 0.0,
                dense_vector: vector,
                phase: row.phase,
                session_date: row.session_date,
                owner: row.owner,
                source_origin,
                cue_bundle: None,
            })
        })
        .collect())
}

#[cfg(test)]
fn dense_search_inner(
    repo: &KnowledgeRepository,
    scope: SearchScope,
    query_vector: &[f32],
    limit: usize,
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<SqliteHit>, TrError> {
    let all_records = match scope {
        #[cfg(test)]
        SearchScope::Project(project_id) => repo
            .load_all_vectors(&project_id, tier)
            .map_err(|e| internal(format!("load_all_vectors: {e}")))?,
        SearchScope::Unscoped => unreachable!("unscoped dense search reads the vector cache"),
    };
    let mut scored: Vec<(SqliteHit, f32)> = all_records
        .into_iter()
        .filter_map(
            |(
                id,
                kind,
                title,
                retrieval_text,
                payload_json,
                confidence,
                state,
                created_at,
                vector,
                phase,
                owner,
                session_date,
                source_json,
                project_id,
            )| {
                let source = parse_source(&source_json);
                let source_origin = KnowledgeOrigin::from_source(&source);
                if !source_filter.matches(source_origin) {
                    return None;
                }
                if !dimensions_match(query_vector, &vector) {
                    return None;
                }
                let sim = cosine_similarity(query_vector, &vector);
                Some((
                    SqliteHit {
                        knowledge_id: id,
                        project_id,
                        kind,
                        title,
                        retrieval_text,
                        payload_json,
                        confidence,
                        state,
                        created_at,
                        dense_similarity: sim,
                        bm25_score: 0.0,
                        dense_vector: vector,
                        phase,
                        session_date,
                        owner,
                        source_origin,
                        cue_bundle: None,
                    },
                    sim,
                ))
            },
        )
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(hit, _)| hit).collect())
}

/// FTS5 BM25 sparse search across every record in the requested tier.
pub(crate) fn sparse_search_unscoped(
    repo: &KnowledgeRepository,
    query: &str,
    limit: usize,
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<SqliteHit>, TrError> {
    sparse_search_inner(
        repo,
        SearchScope::Unscoped,
        query,
        limit,
        tier,
        source_filter,
    )
}

pub(crate) fn identity_search_unscoped(
    repo: &KnowledgeRepository,
    identity_cues: &[String],
    query: &str,
    query_vector: &[f32],
    limit: usize,
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<SqliteHit>, TrError> {
    let terms = query_terms(query);
    // Legacy candidates and every candidate's lowercased text come from the
    // record cache, so the identity query reads only ids and identity values.
    let ranked = crate::record_cache::with_record_cache(repo, |cache| {
        let legacy = if identity_cues.is_empty() || terms.is_empty() {
            Vec::new()
        } else {
            cache.legacy_identity_candidates(tier)
        };
        repo.identity_cue_search(identity_cues, &terms, limit, &legacy, &|id| {
            cache.admissible_haystack(id, tier)
        })
    })?
    .map_err(|e| internal(format!("identity_cue_search: {e}")))?;
    let ids: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
    let records = repo
        .get_knowledge_batch(&ids)
        .map_err(|e| internal(format!("get_knowledge_batch: {e}")))?;
    let vectors = repo
        .load_dense_vectors_for(&ids)
        .map_err(|e| internal(format!("load_dense_vectors_for: {e}")))?;
    let mut hits = Vec::new();
    for (id, _) in ranked {
        if let Some(record) = records.get(&id) {
            let source_origin = KnowledgeOrigin::from_source(&record.source);
            if !source_filter.matches(source_origin) {
                continue;
            }
            let vector = vectors.get(&id).cloned().unwrap_or_default();
            let (vector, dense_similarity) = if dimensions_match(query_vector, &vector) {
                let dense_similarity = cosine_similarity(query_vector, &vector);
                (vector, dense_similarity)
            } else {
                (Vec::new(), 0.0)
            };
            hits.push(record_to_hit(record.clone(), vector, dense_similarity, 0.0));
        }
    }
    Ok(hits)
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .filter(|t| !FTS_STOP_WORDS.contains(&t.as_str()))
        .collect()
}

/// Stop words to filter from FTS5 queries (same list as retrieval.rs).
const FTS_STOP_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "are", "was", "have", "has", "had", "can",
    "will", "would", "could", "should", "not", "but", "its", "all", "any", "our", "your", "their",
    "also", "how", "does", "what", "when", "where", "which", "who", "why", "been", "being", "did",
    "does", "doing", "into", "more", "most", "other", "some", "such", "than", "them", "then",
    "these", "those", "very", "just", "about", "each", "use", "using", "used",
];

fn sparse_search_inner(
    repo: &KnowledgeRepository,
    scope: SearchScope,
    query: &str,
    limit: usize,
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<SqliteHit>, TrError> {
    // FTS5 query: tokenize, remove stop words, join with OR
    let fts_query = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .filter(|t| !FTS_STOP_WORDS.contains(&t.as_str()))
        .collect::<Vec<_>>()
        .join(" OR ");

    if fts_query.is_empty() {
        return Ok(vec![]);
    }

    let fts_results = match scope {
        #[cfg(test)]
        SearchScope::Project(project_id) => repo
            .fts_search(&project_id, &fts_query, limit, tier)
            .map_err(|e| internal(format!("fts_search: {e}")))?,
        SearchScope::Unscoped => repo
            .fts_search_unscoped(&fts_query, limit, tier)
            .map_err(|e| internal(format!("fts_search_unscoped: {e}")))?,
    };

    // Load full records for FTS hits
    let mut hits = Vec::new();
    for (id, bm25_score) in fts_results {
        if let Some(record) = repo
            .get_knowledge(&id)
            .map_err(|e| internal(format!("get_knowledge: {e}")))?
        {
            let source_origin = KnowledgeOrigin::from_source(&record.source);
            if !source_filter.matches(source_origin) {
                continue;
            }
            let vector = repo
                .load_dense_vector(&id)
                .map_err(|e| internal(format!("load_dense_vector: {e}")))?
                .unwrap_or_default();
            hits.push(record_to_hit(
                record,
                vector,
                0.0,
                bm25_score.abs() as f32, // FTS5 bm25() returns negative scores
            ));
        }
    }
    Ok(hits)
}

fn record_to_hit(
    record: fieldcraft_core::KnowledgeRecord,
    vector: Vec<f32>,
    dense_similarity: f32,
    bm25_score: f32,
) -> SqliteHit {
    let phase_str = record.phase.map(|p| p.as_str().to_string());
    let source_origin = KnowledgeOrigin::from_source(&record.source);
    SqliteHit {
        knowledge_id: record.id,
        project_id: record.project_id,
        kind: match record.kind {
            fieldcraft_core::KnowledgeKind::Claim => "claim".to_string(),
            fieldcraft_core::KnowledgeKind::Procedure => "procedure".to_string(),
            fieldcraft_core::KnowledgeKind::Episode => "episode".to_string(),
            fieldcraft_core::KnowledgeKind::Directive => "directive".to_string(),
            fieldcraft_core::KnowledgeKind::Context => "context".to_string(),
        },
        title: record.title,
        retrieval_text: record.retrieval_text,
        payload_json: record.payload_json,
        confidence: record.confidence,
        state: match record.state {
            fieldcraft_core::KnowledgeState::Active => "active".to_string(),
            fieldcraft_core::KnowledgeState::Provisional => "provisional".to_string(),
            fieldcraft_core::KnowledgeState::Superseded => "superseded".to_string(),
            fieldcraft_core::KnowledgeState::Retired => "retired".to_string(),
        },
        created_at: record.created_at.to_rfc3339(),
        dense_similarity,
        bm25_score,
        dense_vector: vector,
        session_date: record.session_date.map(|d| d.to_rfc3339()),
        phase: phase_str,
        owner: record.owner,
        source_origin,
        cue_bundle: None,
    }
}

/// Hybrid search across every record. This is the cue-first entrypoint: it
/// refuses to let a human-visible project key decide recall up front.
pub(crate) fn hybrid_search_for_query_unscoped(
    repo: &KnowledgeRepository,
    query: &str,
    identity_cues: &[String],
    query_vector: &[f32],
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<(String, f32, SqliteHit)>, TrError> {
    let dense_hits = dense_search_unscoped(repo, query_vector, 80, tier, source_filter)?;
    let sparse_hits = sparse_search_unscoped(repo, query, 80, tier, source_filter)?;
    let identity_hits = identity_search_unscoped(
        repo,
        identity_cues,
        query,
        query_vector,
        80,
        tier,
        source_filter,
    )?;

    let dense_ranked: Vec<(String, f32)> = dense_hits
        .iter()
        .map(|h| (h.knowledge_id.clone(), h.dense_similarity))
        .collect();
    let sparse_ranked: Vec<(String, f32)> = sparse_hits
        .iter()
        .map(|h| (h.knowledge_id.clone(), h.bm25_score))
        .collect();
    let identity_ranked: Vec<(String, f32)> = identity_hits
        .iter()
        .enumerate()
        .map(|(idx, h)| (h.knowledge_id.clone(), 1.0 / (idx as f32 + 1.0)))
        .collect();

    let fused = crate::retrieval::rrf_fuse(&[dense_ranked, sparse_ranked, identity_ranked], 60.0);
    let top80 = &fused[..fused.len().min(80)];

    let mut hit_map: HashMap<String, SqliteHit> = HashMap::new();
    for h in dense_hits {
        hit_map.entry(h.knowledge_id.clone()).or_insert(h);
    }
    for h in sparse_hits {
        hit_map.entry(h.knowledge_id.clone()).or_insert(h);
    }
    for h in identity_hits {
        hit_map.entry(h.knowledge_id.clone()).or_insert(h);
    }

    let mut results: Vec<(String, f32, SqliteHit)> = top80
        .iter()
        .filter_map(|(id, fused_score)| {
            hit_map.remove(id).map(|mut h| {
                clear_mismatched_vector(&mut h, query_vector);
                (id.clone(), *fused_score, h)
            })
        })
        .collect();

    enrich_with_cue_bundles(repo, &mut results)?;

    Ok(results)
}

/// BM25-only search for retrieval degradation when the dense query encoder is
/// unavailable. Dense vectors are cleared so later MMR does not use embeddings
/// for a request that explicitly fell back to sparse-only ranking.
pub(crate) fn sparse_search_for_query_unscoped(
    repo: &KnowledgeRepository,
    query: &str,
    tier: &str,
    source_filter: SourceFilter,
) -> Result<Vec<(String, f32, SqliteHit)>, TrError> {
    let sparse_hits = sparse_search_unscoped(repo, query, 80, tier, source_filter)?;
    let sparse_ranked: Vec<(String, f32)> = sparse_hits
        .iter()
        .map(|h| (h.knowledge_id.clone(), h.bm25_score))
        .collect();
    let fused = crate::retrieval::rrf_fuse(&[sparse_ranked], 60.0);
    let top80 = &fused[..fused.len().min(80)];

    let mut hit_map: HashMap<String, SqliteHit> = HashMap::new();
    for mut h in sparse_hits {
        h.dense_similarity = 0.0;
        h.dense_vector.clear();
        hit_map.entry(h.knowledge_id.clone()).or_insert(h);
    }

    let mut results: Vec<(String, f32, SqliteHit)> = top80
        .iter()
        .filter_map(|(id, fused_score)| hit_map.remove(id).map(|h| (id.clone(), *fused_score, h)))
        .collect();
    enrich_with_cue_bundles(repo, &mut results)?;
    Ok(results)
}

fn clear_mismatched_vector(hit: &mut SqliteHit, query_vector: &[f32]) {
    if !hit.dense_vector.is_empty() && !dimensions_match(query_vector, &hit.dense_vector) {
        hit.dense_similarity = 0.0;
        hit.dense_vector.clear();
    }
}

/// Populate each hit's retrieval-time signals (cue bundle + effective
/// confidence) via a single batched DB query. Effective confidence is
/// `COALESCE(dynamic_confidence, confidence)` so records evolved by feedback
/// are scored with their updated value.
fn enrich_with_cue_bundles(
    repo: &KnowledgeRepository,
    results: &mut [(String, f32, SqliteHit)],
) -> Result<(), TrError> {
    if results.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = results.iter().map(|(id, _, _)| id.clone()).collect();
    let signals = repo.load_retrieval_signals_for(&ids)?;
    for (id, _, hit) in results.iter_mut() {
        if let Some(b) = signals.cue_bundles.get(id) {
            hit.cue_bundle = Some(b.clone());
        }
        if let Some(conf) = signals.effective_confidence.get(id) {
            hit.confidence = *conf;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use fieldcraft_core::{
        AgentTool, KnowledgeKind, KnowledgeRecord, KnowledgeSource, KnowledgeState, KnowledgeTier,
    };
    use tempfile::tempdir;

    fn record(
        id: &str,
        project_id: &str,
        project_specific: bool,
        tier: KnowledgeTier,
        source: KnowledgeSource,
    ) -> KnowledgeRecord {
        KnowledgeRecord {
            protected: false,
            id: id.to_string(),
            project_id: project_id.to_string(),
            kind: KnowledgeKind::Claim,
            title: id.to_string(),
            payload_json: r#"{"statement":"routing sentinel"}"#.to_string(),
            state: KnowledgeState::Active,
            confidence: 0.8,
            source,
            superseded_by: None,
            project_specific,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: "routing sentinel".to_string(),
            schema_version: 2,
            phase: None,
            tier,
            owner: None,
            session_date: None,
            source_refs: vec![],
        }
    }

    #[test]
    fn active_tier_omits_archived_external_records() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let external = record(
            "external-archive",
            "global",
            false,
            KnowledgeTier::Archive,
            KnowledgeSource::External {
                uri: "file:///skill".to_string(),
                title: "Skill".to_string(),
                source_hash: Some("hash".to_string()),
            },
        );
        repo.insert_knowledge(&external).unwrap();
        repo.store_dense_vector("external-archive", &[1.0, 0.0])
            .unwrap();

        let active =
            dense_search(&repo, "proj", &[1.0, 0.0], 10, "active", SourceFilter::Any).unwrap();
        assert!(active.is_empty());

        let archived = dense_search(
            &repo,
            "proj",
            &[1.0, 0.0],
            10,
            "archive",
            SourceFilter::ExternalOnly,
        )
        .unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].source_origin, KnowledgeOrigin::ExternalDerived);
    }

    #[test]
    fn source_filters_split_learned_and_external_records_without_rank_bias() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let learned = record(
            "learned-active",
            "proj",
            true,
            KnowledgeTier::Active,
            KnowledgeSource::Session {
                session_id: "s1".to_string(),
                tool: AgentTool::Codex,
            },
        );
        let external = record(
            "external-active",
            "global",
            false,
            KnowledgeTier::Active,
            KnowledgeSource::External {
                uri: "file:///skill".to_string(),
                title: "Skill".to_string(),
                source_hash: Some("hash".to_string()),
            },
        );
        repo.insert_knowledge(&learned).unwrap();
        repo.insert_knowledge(&external).unwrap();
        repo.store_dense_vector("learned-active", &[1.0, 0.0])
            .unwrap();
        repo.store_dense_vector("external-active", &[1.0, 0.0])
            .unwrap();

        let any =
            dense_search(&repo, "proj", &[1.0, 0.0], 10, "active", SourceFilter::Any).unwrap();
        assert_eq!(any.len(), 2);

        let learned_only = dense_search(
            &repo,
            "proj",
            &[1.0, 0.0],
            10,
            "active",
            SourceFilter::LearnedOnly,
        )
        .unwrap();
        assert_eq!(learned_only.len(), 1);
        assert_eq!(learned_only[0].knowledge_id, "learned-active");

        let external_only = dense_search(
            &repo,
            "proj",
            &[1.0, 0.0],
            10,
            "active",
            SourceFilter::ExternalOnly,
        )
        .unwrap();
        assert_eq!(external_only.len(), 1);
        assert_eq!(external_only[0].knowledge_id, "external-active");
    }

    #[test]
    fn dense_search_skips_mismatched_vector_dimensions() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let matching = record(
            "matching-dim",
            "proj",
            true,
            KnowledgeTier::Active,
            KnowledgeSource::Manual,
        );
        let stale = record(
            "stale-dim",
            "proj",
            true,
            KnowledgeTier::Active,
            KnowledgeSource::Manual,
        );
        repo.insert_knowledge(&matching).unwrap();
        repo.insert_knowledge(&stale).unwrap();
        repo.store_dense_vector("matching-dim", &[1.0, 0.0])
            .unwrap();
        repo.store_dense_vector("stale-dim", &[1.0, 0.0, 0.0])
            .unwrap();

        let hits =
            dense_search(&repo, "proj", &[1.0, 0.0], 10, "active", SourceFilter::Any).unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].knowledge_id, "matching-dim");
    }

    #[test]
    fn sparse_only_search_clears_dense_vectors() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let record = record(
            "sparse-hit",
            "proj",
            true,
            KnowledgeTier::Active,
            KnowledgeSource::Manual,
        );
        repo.insert_knowledge(&record).unwrap();
        repo.store_dense_vector("sparse-hit", &[1.0, 0.0]).unwrap();

        let hits = sparse_search_for_query_unscoped(
            &repo,
            "routing sentinel",
            "active",
            SourceFilter::Any,
        )
        .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].2.knowledge_id, "sparse-hit");
        assert!(hits[0].2.dense_vector.is_empty());
        assert_eq!(hits[0].2.dense_similarity, 0.0);
    }
}
