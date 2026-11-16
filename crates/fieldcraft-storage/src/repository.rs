use chrono::Utc;
use fieldcraft_core::{
    CueBundle, KnowledgeKind, KnowledgePhase, KnowledgeRecord, KnowledgeSource, KnowledgeState,
    KnowledgeTier, TrError,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Outcome of a feedback event for a lesson.
///
/// Drives [`KnowledgeRepository::apply_feedback_outcome`] which updates the
/// per-record `dynamic_confidence` and appends a row to `lesson_outcome_events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackOutcome {
    /// The agent visibly used this record (cited content, applied procedure).
    Confirmed,
    /// The record was retrieved but the agent did not visibly use it.
    Ignored,
    /// The agent explicitly stated the record was wrong / outdated / inapplicable.
    Contradicted,
}

impl FeedbackOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Ignored => "ignored",
            Self::Contradicted => "contradicted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "confirmed" => Some(Self::Confirmed),
            "ignored" => Some(Self::Ignored),
            "contradicted" => Some(Self::Contradicted),
            _ => None,
        }
    }
}

/// Batched retrieval-time enrichment signals fetched in one SQL round-trip
/// after RRF fusion. Powers cue-aware scoring (cue_bundles) and outcome-
/// adjusted confidence (effective_confidence = COALESCE(dynamic, static)).
#[derive(Debug, Default, Clone)]
pub struct RetrievalSignals {
    pub cue_bundles: HashMap<String, CueBundle>,
    pub effective_confidence: HashMap<String, f32>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClosedLoopStats {
    pub window_days: u32,
    pub confirmed_recent: i64,
    pub ignored_recent: i64,
    pub contradicted_recent: i64,
    pub dynamic_confidence_non_null: i64,
    pub retrieval_log_with_query_cue_bundle: i64,
    pub retrieval_log_without_query_cue_bundle: i64,
}

/// Pure function implementing the testing-effect outcome update rule.
/// Exposed so it can be unit-tested without a live DB.
pub fn compute_outcome_update(current: f32, outcome: FeedbackOutcome) -> f32 {
    let updated = match outcome {
        FeedbackOutcome::Confirmed => current + 0.05 * (1.0 - current),
        FeedbackOutcome::Ignored => current - 0.08 * current,
        FeedbackOutcome::Contradicted => current - 0.12 * current,
    };
    updated.clamp(0.05, 0.95)
}

use crate::leiden::leiden_cpm;
use crate::schema::{canonical_project_id_from_existing, open_catalog, sqlite_error};

pub struct KnowledgeRepository {
    conn: Connection,
    /// Tag of the vectors this connection reads and writes by default (see
    /// `fieldcraft_core::embedding`). `None` is BM25-only mode: no vector is
    /// read, and storing one is an error.
    vector_tag: Option<String>,
}

pub const DERIVED_CLAIM_MIN_SOURCES: usize = 3;
pub const DERIVED_CLAIM_MAX_SOURCES: usize = 24;

/// Parameters for [`KnowledgeRepository::log_retrieval_entry`]. Construct with
/// only the fields you need and rely on `Default` for the rest:
///
/// ```ignore
/// repo.log_retrieval_entry(&RetrievalLogEntry {
///     project_id: "p",
///     query_text: "q",
///     result_ids: &ids,
///     result_scores: &scores,
///     latency_ms: 42,
///     ..Default::default()
/// })?;
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct RetrievalLogEntry<'a> {
    pub project_id: &'a str,
    pub query_text: &'a str,
    /// Optional richer retrieval context used only to build telemetry cues
    /// when the caller did not pass a precomputed query cue bundle.
    pub query_context: &'a str,
    /// Optional active files for query telemetry. Basenames are persisted in
    /// the query-side cue bundle so trace analysis is stable across machines.
    pub query_files: &'a [String],
    pub result_ids: &'a [String],
    pub result_scores: &'a [f32],
    pub latency_ms: u64,
    pub raw_candidate_ids: &'a [String],
    pub raw_candidate_scores: &'a [f32],
    pub raw_candidate_titles: &'a [String],
    pub result_origins: &'a [String],
    pub raw_candidate_origins: &'a [String],
    pub link_ids_surfaced: &'a [String],
    /// Serialized JSON of the query-side `CueBundle` for this retrieval.
    /// Empty string when not yet computed (legacy / pre-Phase-2 callers).
    pub query_cue_bundle: &'a str,
    /// Optional session id. NULL/empty if the caller did not provide one
    /// (legacy retrievals + mid-session HTTP calls that don't know their
    /// session id yet). Used by the trace analyzer for exact session
    /// correlation when present; falls back to time-window+project otherwise.
    pub session_id: &'a str,
}

/// Row returned by [`KnowledgeRepository::retrievals_for_project_window`]. Holds
/// just what the trace analyzer needs to attribute Ignored events.
#[derive(Debug, Clone)]
pub struct RetrievalLogRow {
    pub id: i64,
    pub query_cue_bundle: Option<String>,
    pub result_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct KnowledgeNeighbor {
    pub id: String,
    pub project_id: String,
    pub kind: String,
    pub title: String,
    pub relation_kind: String,
    pub relation_confidence: f32,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct MergeQueueItem {
    pub id: i64,
    pub lesson_id: String,
    pub attempts: i64,
}

#[derive(Debug, Clone)]
pub struct MergeCandidate {
    pub canonical_id: String,
    pub duplicate_id: String,
    pub score: f32,
}

fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn file_basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).trim().to_string()
}

fn build_retrieval_log_query_cue_bundle(
    entry: &RetrievalLogEntry<'_>,
) -> Result<Option<String>, TrError> {
    let mut text_parts = Vec::new();
    if let Some(query_text) = non_empty_str(entry.query_text) {
        text_parts.push(query_text);
    }
    if let Some(query_context) = non_empty_str(entry.query_context)
        && Some(query_context) != non_empty_str(entry.query_text)
    {
        text_parts.push(query_context);
    }

    let file_basenames: Vec<String> = entry
        .query_files
        .iter()
        .map(|path| file_basename(path))
        .filter(|path| !path.is_empty())
        .collect();
    let mut combined = text_parts.join(" ");
    if !file_basenames.is_empty() {
        if !combined.is_empty() {
            combined.push(' ');
        }
        combined.push_str(&file_basenames.join(" "));
    }

    let mut bundle =
        fieldcraft_core::cue_extract::extract_deterministic(&combined, &file_basenames, None);
    fieldcraft_core::cue_extract::add_identity_cue(&mut bundle, entry.project_id, "project_id");
    if bundle.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(&bundle)
        .map(Some)
        .map_err(|e| TrError::Storage {
            message: format!("query cue bundle serialize: {e}"),
        })
}

/// A record proposed for reversible auto-archival by the Memory Lifecycle
/// eviction pass (L2). Read-only: producing one of these never mutates the
/// store. See docs/plans/memory-lifecycle.md.
#[derive(Debug, Clone)]
pub struct ArchiveCandidate {
    pub id: String,
    /// `"ghost"` (old, never served, unconfirmed) or `"contradicted_loser"`
    /// (repeatedly contradicted and net-negative).
    pub category: &'static str,
    /// Human-readable justification carrying the signal values; this becomes
    /// the `knowledge_events` audit note when the candidate is archived.
    pub reason: String,
}

impl KnowledgeRepository {
    pub fn open(path: &Path) -> Result<Self, TrError> {
        Ok(Self::from_connection(open_catalog(path)?))
    }

    /// Wrap a catalog connection; the vector tag comes from the configured
    /// embedding provider.
    pub fn from_connection(conn: Connection) -> Self {
        Self {
            conn,
            vector_tag: fieldcraft_core::embedding::active_tag(),
        }
    }

    /// Tag of the vectors this connection reads and writes by default.
    pub fn vector_tag(&self) -> Option<&str> {
        self.vector_tag.as_deref()
    }

    /// Read and write vectors of another tag (reindex, evaluation, tests).
    pub fn set_vector_tag(&mut self, tag: Option<String>) {
        self.vector_tag = tag;
    }

    /// Cosine thresholds for this connection's vectors.
    pub fn cosine_thresholds(&self) -> fieldcraft_core::embedding::CosineThresholds {
        fieldcraft_core::embedding::thresholds_for_tag(self.vector_tag())
    }

    /// SQL expression for the vector of `id_expr` under this connection's tag
    /// (`NULL` in BM25-only mode). The tag is a quoted literal so callers can
    /// keep positional parameters.
    pub(crate) fn vector_sql(&self, id_expr: &str) -> String {
        match self.vector_tag() {
            Some(tag) => format!(
                "(SELECT v.vector FROM knowledge_vectors v WHERE v.knowledge_id = {id_expr} AND v.tag = '{}')",
                tag.replace('\'', "''")
            ),
            None => "NULL".to_string(),
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn known_project_ids(&self) -> Result<Vec<String>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT project_id FROM knowledge
                 UNION SELECT DISTINCT project_id FROM imports
                 UNION SELECT DISTINCT project_id FROM retrieval_log
                 UNION SELECT DISTINCT project_id FROM lesson_feedback
                 ORDER BY project_id",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    pub fn canonical_project_id(&self, project_id: &str) -> Result<String, TrError> {
        let existing = self.known_project_ids()?;
        Ok(canonical_project_id_from_existing(project_id, existing))
    }

    pub fn closed_loop_stats(&self, days: u32) -> Result<ClosedLoopStats, TrError> {
        let confirmed_recent = self.count_recent_outcomes("confirmed", days)?;
        let ignored_recent = self.count_recent_outcomes("ignored", days)?;
        let contradicted_recent = self.count_recent_outcomes("contradicted", days)?;
        let dynamic_confidence_non_null = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM knowledge WHERE dynamic_confidence IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        let retrieval_log_with_query_cue_bundle = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM retrieval_log \
                 WHERE query_cue_bundle IS NOT NULL AND TRIM(query_cue_bundle) <> ''",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        let retrieval_log_without_query_cue_bundle = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM retrieval_log \
                 WHERE query_cue_bundle IS NULL OR TRIM(query_cue_bundle) = ''",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;

        Ok(ClosedLoopStats {
            window_days: days,
            confirmed_recent,
            ignored_recent,
            contradicted_recent,
            dynamic_confidence_non_null,
            retrieval_log_with_query_cue_bundle,
            retrieval_log_without_query_cue_bundle,
        })
    }

    fn count_recent_outcomes(&self, outcome: &str, days: u32) -> Result<i64, TrError> {
        let sql = format!(
            "SELECT COUNT(*) FROM lesson_outcome_events \
             WHERE outcome = ?1 AND created_at >= datetime('now', '-{days} days')"
        );
        self.conn
            .query_row(&sql, [outcome], |row| row.get(0))
            .map_err(sqlite_error)
    }

    pub fn insert_knowledge(&self, record: &KnowledgeRecord) -> Result<(), TrError> {
        let kind_str = knowledge_kind_str(record.kind);
        let state_str = knowledge_state_str(record.state);
        let source_json = serde_json::to_string(&record.source).map_err(|e| TrError::Storage {
            message: e.to_string(),
        })?;
        let source_refs_json =
            serde_json::to_string(&record.source_refs).map_err(|e| TrError::Storage {
                message: e.to_string(),
            })?;
        let affected_files_json: Option<String> = extract_affected_files(&record.payload_json);
        let project_specific: i64 = if record.project_specific { 1 } else { 0 };
        let created_at = record.created_at.to_rfc3339();
        let valid_until = record.valid_until.map(|dt| dt.to_rfc3339());
        let session_date = record.session_date.map(|dt| dt.to_rfc3339());
        let now = Utc::now().to_rfc3339();

        let phase_str: Option<&str> = record.phase.map(KnowledgePhase::as_str);
        let tier_str = record.tier.as_str();
        self.conn
            .execute(
                "INSERT INTO knowledge
                 (id, project_id, kind, state, title, payload_json, confidence, source_json,
                  superseded_by, project_specific, created_at, valid_until, updated_at,
                  affected_files_json, retrieval_text, schema_version, phase, tier, owner,
                  session_date, source_refs_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
                 ON CONFLICT(id) DO UPDATE SET
                   project_id=excluded.project_id,
                   kind=excluded.kind,
                   state=excluded.state,
                   title=excluded.title,
                   payload_json=excluded.payload_json,
                   confidence=excluded.confidence,
                   source_json=excluded.source_json,
                   superseded_by=excluded.superseded_by,
                   project_specific=excluded.project_specific,
                   valid_until=excluded.valid_until,
                   updated_at=excluded.updated_at,
                   affected_files_json=excluded.affected_files_json,
                   retrieval_text=excluded.retrieval_text,
                   schema_version=excluded.schema_version,
                   phase=excluded.phase,
                   tier=excluded.tier,
                   owner=excluded.owner,
                   session_date=excluded.session_date,
                   source_refs_json=excluded.source_refs_json",
                params![
                    record.id,
                    record.project_id,
                    kind_str,
                    state_str,
                    record.title,
                    record.payload_json,
                    record.confidence as f64,
                    source_json,
                    record.superseded_by,
                    project_specific,
                    created_at,
                    valid_until,
                    now,
                    affected_files_json,
                    record.retrieval_text,
                    record.schema_version,
                    phase_str,
                    tier_str,
                    record.owner.as_deref(),
                    session_date,
                    source_refs_json,
                ],
            )
            .map_err(sqlite_error)?;

        // Phase 1 of cue-bundle retrieval: derive a deterministic cue_bundle
        // from the record's title + retrieval_text + affected_files, store it
        // on the row, and rebuild the cue_index inverted table for this
        // record. LLM-classified cues (phase, user_state) stay None at write
        // time — they're filled in later by a separate classifier worker.
        let files: Vec<String> = affected_files_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default();
        let combined = format!("{} {}", record.title, record.retrieval_text);
        let mut bundle =
            fieldcraft_core::cue_extract::extract_deterministic(&combined, &files, None);
        fieldcraft_core::cue_extract::add_identity_cue(
            &mut bundle,
            &record.project_id,
            "project_id",
        );
        let bundle_json = serde_json::to_string(&bundle).map_err(|e| TrError::Storage {
            message: format!("bundle serialize: {e}"),
        })?;
        self.conn
            .execute(
                "UPDATE knowledge SET cue_bundle = ?1 WHERE id = ?2",
                params![bundle_json, record.id],
            )
            .map_err(sqlite_error)?;
        // Idempotent on re-insert: drop the prior rows, then write fresh.
        self.conn
            .execute(
                "DELETE FROM cue_index WHERE record_id = ?1",
                params![record.id],
            )
            .map_err(sqlite_error)?;
        for (dim_name, dim_value) in bundle.to_index_rows() {
            self.conn
                .execute(
                    "INSERT INTO cue_index (record_id, dim_name, dim_value) \
                     VALUES (?1, ?2, ?3)",
                    params![record.id, dim_name, dim_value],
                )
                .map_err(sqlite_error)?;
        }
        Ok(())
    }

    pub fn get_knowledge(&self, id: &str) -> Result<Option<KnowledgeRecord>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project_id, kind, state, title, payload_json, confidence,
                        source_json, superseded_by, project_specific, created_at,
                        valid_until, updated_at, affected_files_json, retrieval_text,
                        schema_version, phase, tier, owner, session_date, source_refs_json,
                        protected
                 FROM knowledge WHERE id = ?1",
            )
            .map_err(sqlite_error)?;
        stmt.query_row([id], row_to_knowledge)
            .optional()
            .map_err(sqlite_error)
    }

    pub fn get_knowledge_batch(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, KnowledgeRecord>, TrError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders: String = (0..ids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, project_id, kind, state, title, payload_json, confidence,
                    source_json, superseded_by, project_specific, created_at,
                    valid_until, updated_at, affected_files_json, retrieval_text,
                    schema_version, phase, tier, owner, session_date, source_refs_json,
                    protected
             FROM knowledge WHERE id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let params_iter: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_iter), row_to_knowledge)
            .map_err(sqlite_error)?;
        let mut out = HashMap::new();
        for row in rows {
            let record = row.map_err(sqlite_error)?;
            out.insert(record.id.clone(), record);
        }
        Ok(out)
    }

    pub fn list_knowledge_ids_by_retrieval_marker(
        &self,
        project_id: &str,
        marker: &str,
        tier: &str,
    ) -> Result<Vec<String>, TrError> {
        let like_marker = format!("%{}%", escape_sqlite_like(marker));
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id
                 FROM knowledge
                 WHERE retrieval_text LIKE ?1 ESCAPE '\\'
                   AND (project_id = ?2 OR project_specific = 0)
                   AND state IN ('active', 'provisional')
                   AND (tier = ?3 OR tier IS NULL)
                 ORDER BY created_at ASC, id ASC
                 LIMIT 50",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![like_marker, project_id, tier], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    pub fn list_knowledge(
        &self,
        project_id: &str,
        kind: Option<KnowledgeKind>,
        state: Option<KnowledgeState>,
    ) -> Result<Vec<KnowledgeRecord>, TrError> {
        let sql = match (kind, state) {
            (Some(_), Some(_)) => {
                "SELECT id, project_id, kind, state, title, payload_json, confidence,
                        source_json, superseded_by, project_specific, created_at,
                        valid_until, updated_at, affected_files_json, retrieval_text,
                        schema_version, phase, tier, owner, session_date, source_refs_json,
                        protected
                 FROM knowledge WHERE project_id = ?1 AND kind = ?2 AND state = ?3
                 ORDER BY created_at DESC"
            }
            (Some(_), None) => {
                "SELECT id, project_id, kind, state, title, payload_json, confidence,
                        source_json, superseded_by, project_specific, created_at,
                        valid_until, updated_at, affected_files_json, retrieval_text,
                        schema_version, phase, tier, owner, session_date, source_refs_json,
                        protected
                 FROM knowledge WHERE project_id = ?1 AND kind = ?2
                 ORDER BY created_at DESC"
            }
            (None, Some(_)) => {
                "SELECT id, project_id, kind, state, title, payload_json, confidence,
                        source_json, superseded_by, project_specific, created_at,
                        valid_until, updated_at, affected_files_json, retrieval_text,
                        schema_version, phase, tier, owner, session_date, source_refs_json,
                        protected
                 FROM knowledge WHERE project_id = ?1 AND state = ?2
                 ORDER BY created_at DESC"
            }
            (None, None) => {
                "SELECT id, project_id, kind, state, title, payload_json, confidence,
                        source_json, superseded_by, project_specific, created_at,
                        valid_until, updated_at, affected_files_json, retrieval_text,
                        schema_version, phase, tier, owner, session_date, source_refs_json,
                        protected
                 FROM knowledge WHERE project_id = ?1
                 ORDER BY created_at DESC"
            }
        };

        let mut stmt = self.conn.prepare(sql).map_err(sqlite_error)?;

        let rows = match (kind, state) {
            (Some(k), Some(s)) => stmt.query_map(
                params![project_id, knowledge_kind_str(k), knowledge_state_str(s)],
                row_to_knowledge,
            ),
            (Some(k), None) => {
                stmt.query_map(params![project_id, knowledge_kind_str(k)], row_to_knowledge)
            }
            (None, Some(s)) => stmt.query_map(
                params![project_id, knowledge_state_str(s)],
                row_to_knowledge,
            ),
            (None, None) => stmt.query_map(params![project_id], row_to_knowledge),
        }
        .map_err(sqlite_error)?;

        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    pub fn update_state(&self, id: &str, state: KnowledgeState) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE knowledge SET state = ?1, updated_at = ?2 WHERE id = ?3",
                params![knowledge_state_str(state), now, id],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    pub fn update_superseded_by(&self, id: &str, superseded_by: &str) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE knowledge SET superseded_by = ?1, updated_at = ?2 WHERE id = ?3",
                params![superseded_by, now, id],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    pub fn enqueue_merge_candidate(&self, lesson_id: &str) -> Result<bool, TrError> {
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM knowledge_merge_queue
                 WHERE lesson_id = ?1 AND status IN ('pending', 'running')
                 LIMIT 1",
                params![lesson_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        if existing.is_some() {
            return Ok(false);
        }

        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO knowledge_merge_queue
                 (lesson_id, status, attempts, created_at, updated_at)
                 VALUES (?1, 'pending', 0, ?2, ?2)",
                params![lesson_id, now],
            )
            .map_err(sqlite_error)?;
        Ok(true)
    }

    pub fn claim_merge_candidates(&self, limit: usize) -> Result<Vec<MergeQueueItem>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, lesson_id, attempts
                 FROM knowledge_merge_queue
                 WHERE status = 'pending'
                 ORDER BY id
                 LIMIT ?1",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(MergeQueueItem {
                    id: row.get(0)?,
                    lesson_id: row.get(1)?,
                    attempts: row.get(2)?,
                })
            })
            .map_err(sqlite_error)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?;
        drop(stmt);

        let now = Utc::now().to_rfc3339();
        for item in &items {
            self.conn
                .execute(
                    "UPDATE knowledge_merge_queue
                     SET status = 'running', attempts = attempts + 1, updated_at = ?1
                     WHERE id = ?2 AND status = 'pending'",
                    params![now, item.id],
                )
                .map_err(sqlite_error)?;
        }

        Ok(items
            .into_iter()
            .map(|mut item| {
                item.attempts += 1;
                item
            })
            .collect())
    }

    pub fn complete_merge_candidate(&self, queue_id: i64) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE knowledge_merge_queue
                 SET status = 'done', updated_at = ?1, processed_at = ?1
                 WHERE id = ?2",
                params![now, queue_id],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    pub fn fail_merge_candidate(
        &self,
        queue_id: i64,
        attempts: i64,
        error: &str,
    ) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        let status = if attempts >= 3 { "failed" } else { "pending" };
        self.conn
            .execute(
                "UPDATE knowledge_merge_queue
                 SET status = ?1, last_error = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![status, error, now, queue_id],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    pub fn obvious_merge_candidate(
        &self,
        lesson_id: &str,
    ) -> Result<Option<MergeCandidate>, TrError> {
        let Some(candidate) = self.get_knowledge(lesson_id)? else {
            return Ok(None);
        };
        if !matches!(
            candidate.state,
            KnowledgeState::Active | KnowledgeState::Provisional
        ) || candidate.tier != KnowledgeTier::Active
        {
            return Ok(None);
        }

        let candidate_dense = self.load_dense_vector(&candidate.id)?;
        let candidate_cue_bundle = self.load_cue_bundle(&candidate.id)?;
        let kind = knowledge_kind_str(candidate.kind);
        let sql = format!(
            "SELECT id, project_id, kind, state, title, payload_json, confidence,
                    source_json, superseded_by, project_specific, created_at,
                    valid_until, updated_at, affected_files_json, retrieval_text,
                    schema_version, phase, tier, owner, session_date, source_refs_json,
                    protected, {}, cue_bundle
             FROM knowledge
             WHERE project_id = ?1
               AND kind = ?2
               AND id != ?3
               AND state IN ('active', 'provisional')
               AND (tier = 'active' OR tier IS NULL)
             ORDER BY created_at ASC
             LIMIT 200",
            self.vector_sql("knowledge.id")
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![&candidate.project_id, kind, &candidate.id], |row| {
                let record = row_to_knowledge(row)?;
                let dense = row
                    .get::<_, Option<Vec<u8>>>(22)?
                    .map(|bytes| blob_to_vector(&bytes));
                let cue_bundle = row
                    .get::<_, Option<String>>(23)?
                    .and_then(|json| serde_json::from_str(&json).ok());
                Ok((record, dense, cue_bundle))
            })
            .map_err(sqlite_error)?;

        let mut best: Option<MergeCandidate> = None;
        for row in rows {
            let (neighbor, neighbor_dense, neighbor_cue_bundle) = row.map_err(sqlite_error)?;
            let score = duplicate_score(
                self.cosine_thresholds().duplicate_min,
                &candidate,
                &neighbor,
                candidate_dense.as_deref(),
                neighbor_dense.as_deref(),
                candidate_cue_bundle.as_ref(),
                neighbor_cue_bundle.as_ref(),
            );
            if score >= DUPLICATE_MERGE_THRESHOLD && best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(MergeCandidate {
                    canonical_id: neighbor.id,
                    duplicate_id: candidate.id.clone(),
                    score,
                });
            }
        }
        Ok(best)
    }

    pub fn merge_duplicate_into_canonical(
        &self,
        canonical_id: &str,
        duplicate_id: &str,
        score: f32,
    ) -> Result<(), TrError> {
        let Some(mut canonical) = self.get_knowledge(canonical_id)? else {
            return Err(TrError::NotFound {
                message: format!("canonical lesson {canonical_id}"),
            });
        };
        let Some(duplicate) = self.get_knowledge(duplicate_id)? else {
            return Err(TrError::NotFound {
                message: format!("duplicate lesson {duplicate_id}"),
            });
        };

        for source_ref in duplicate.source_refs.iter().cloned() {
            if !canonical.source_refs.contains(&source_ref) {
                canonical.source_refs.push(source_ref);
            }
        }
        let source_refs_json =
            serde_json::to_string(&canonical.source_refs).map_err(|e| TrError::Storage {
                message: e.to_string(),
            })?;
        let duplicate_refs_json =
            serde_json::to_string(&duplicate.source_refs).map_err(|e| TrError::Storage {
                message: e.to_string(),
            })?;
        let now = Utc::now().to_rfc3339();
        let note = format!(
            "Merged duplicate lesson {duplicate_id} into {canonical_id} with deterministic score {score:.2}: {}",
            duplicate.title
        );

        self.conn
            .execute(
                "UPDATE knowledge SET source_refs_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![source_refs_json, now, canonical_id],
            )
            .map_err(sqlite_error)?;
        self.conn
            .execute(
                "UPDATE knowledge
                 SET state = 'superseded',
                     tier = 'archive',
                     superseded_by = ?1,
                     updated_at = ?2
                 WHERE id = ?3",
                params![canonical_id, now, duplicate_id],
            )
            .map_err(sqlite_error)?;
        self.conn
            .execute(
                "INSERT INTO knowledge_events
                 (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                 VALUES (?1, 'duplicate_merged', ?2, ?3, ?4, ?5)",
                params![canonical_id, duplicate_id, duplicate_refs_json, note, now],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Memory Lifecycle L1: retention/protection, served-activation, and
    // reversible archive/restore. See docs/plans/memory-lifecycle.md.
    // -----------------------------------------------------------------

    /// Set (or clear) the explicit retention pin on a record and write a
    /// `knowledge_events` audit row. This is the *explicit* protection
    /// override; see [`KnowledgeRepository::is_protected`] for the full
    /// (explicit OR implicit) protection check used by eviction.
    pub fn set_protected(&self, id: &str, value: bool, reason: &str) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        let updated = self
            .conn
            .execute(
                "UPDATE knowledge SET protected = ?1, updated_at = ?2 WHERE id = ?3",
                params![value as i64, now, id],
            )
            .map_err(sqlite_error)?;
        if updated == 0 {
            return Err(TrError::NotFound {
                message: format!("knowledge record {id}"),
            });
        }
        let event_type = if value {
            "protected_set"
        } else {
            "protected_cleared"
        };
        self.conn
            .execute(
                "INSERT INTO knowledge_events
                 (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                 VALUES (?1, ?2, NULL, '[]', ?3, ?4)",
                params![id, event_type, reason, now],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// True if `id` must never be auto-archived: explicit `protected` pin,
    /// OR `kind` is `directive`/`context` (rare, hand-authored), OR static
    /// `confidence >= 0.9`. Conjunction-free by design — any one reason is
    /// sufficient, per the Memory Lifecycle guardrail that protection is a
    /// disjunction of safety conditions, never a single confidence gate.
    pub fn is_protected(&self, id: &str) -> Result<bool, TrError> {
        let row: Option<(i64, String, f64)> = self
            .conn
            .query_row(
                "SELECT protected, kind, confidence FROM knowledge WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sqlite_error)?;
        let Some((protected_flag, kind_str, confidence_f64)) = row else {
            return Err(TrError::NotFound {
                message: format!("knowledge record {id}"),
            });
        };
        if protected_flag != 0 {
            return Ok(true);
        }
        if matches!(kind_str.as_str(), "directive" | "context") {
            return Ok(true);
        }
        // `confidence` is stored as SQLite REAL (f64) but the canonical type
        // throughout the codebase (KnowledgeRecord.confidence, the 0.0-1.0
        // contract, compute_outcome_update) is f32 — round-trip through f32
        // before comparing so a record built with `confidence: 0.9_f32` (which
        // becomes 0.899999976... once widened to f64) still compares as
        // exactly 0.9, not silently 0.89999997615814208984 < 0.9.
        let confidence = confidence_f64 as f32;
        if confidence >= 0.9 {
            return Ok(true);
        }
        Ok(false)
    }

    /// Served-activation signal: how recently and how often `id` has been
    /// *served* by retrieval (NOT proof of use — see the module-level note
    /// in docs/plans/memory-lifecycle.md on why this is named
    /// `served_activation` and not `usage`).
    ///
    /// `served_activation(id) = ln(Σ over each appearance of id in
    /// retrieval_log.result_ids of max(1.0, days_since(that row's
    /// created_at))^-0.5)`.
    ///
    /// Returns `None` when `id` never appears in any `result_ids` — this is
    /// the eviction sentinel: a record that has literally never been served
    /// is the strongest possible "never used" signal, distinct from "served
    /// long ago" (which yields a small-but-finite value via the decay).
    pub fn served_activation(&self, id: &str) -> Result<Option<f64>, TrError> {
        let mut stmt = self
            .conn
            .prepare("SELECT result_ids, created_at FROM retrieval_log")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| {
                let ids_json: String = row.get(0)?;
                let created_at: String = row.get(1)?;
                Ok((ids_json, created_at))
            })
            .map_err(sqlite_error)?;

        let now = Utc::now();
        let mut sum = 0.0_f64;
        let mut appearances = 0_u64;
        for row in rows {
            let (ids_json, created_at_str) = row.map_err(sqlite_error)?;
            let result_ids: Vec<String> = serde_json::from_str(&ids_json).unwrap_or_default();
            if !result_ids.iter().any(|rid| rid == id) {
                continue;
            }
            let created_at = created_at_str
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap_or(now);
            let days_since = (now - created_at).num_seconds() as f64 / 86_400.0;
            let days_since = days_since.max(1.0);
            sum += days_since.powf(-0.5);
            appearances += 1;
        }
        if appearances == 0 {
            return Ok(None);
        }
        Ok(Some(sum.ln()))
    }

    /// Reversibly archive a record: `tier = 'archive'`, and write a
    /// `knowledge_events` audit row carrying `reason`. Reuses the same
    /// `tier`/`state` shape the merge worker already writes
    /// (`merge_duplicate_into_canonical`), but does NOT touch `state` or
    /// `superseded_by` — those mark "replaced by another record", which is
    /// a different fact from "archived, may still be the only copy of this
    /// knowledge". Never DELETEs. See [`KnowledgeRepository::restore_record`]
    /// for the inverse.
    pub fn archive_record(&self, id: &str, reason: &str) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        let updated = self
            .conn
            .execute(
                "UPDATE knowledge SET tier = 'archive', updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(sqlite_error)?;
        if updated == 0 {
            return Err(TrError::NotFound {
                message: format!("knowledge record {id}"),
            });
        }
        self.conn
            .execute(
                "INSERT INTO knowledge_events
                 (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                 VALUES (?1, 'archived', NULL, '[]', ?2, ?3)",
                params![id, reason, now],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Inverse of [`KnowledgeRepository::archive_record`]: flips `tier`
    /// back to `'active'` and writes a `knowledge_events` audit row. Only
    /// plain archives are reversible. Superseded records are canonical
    /// tombstones: they remain available for provenance, but cannot be
    /// restored into the serving pool.
    pub fn restore_record(&self, id: &str, reason: &str) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        let Some((state, superseded_by)) = self
            .conn
            .query_row(
                "SELECT state, superseded_by FROM knowledge WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?
        else {
            return Err(TrError::NotFound {
                message: format!("knowledge record {id}"),
            });
        };
        if state == "superseded" || superseded_by.is_some() {
            let target = superseded_by
                .as_deref()
                .map(|id| format!(" by {id}"))
                .unwrap_or_default();
            return Err(TrError::Conflict {
                message: format!(
                    "knowledge record {id} is superseded{target}; write a new canonical record instead of restoring it"
                ),
            });
        }
        let updated = self
            .conn
            .execute(
                "UPDATE knowledge
                 SET tier = 'active',
                     updated_at = ?1
                 WHERE id = ?2",
                params![now, id],
            )
            .map_err(sqlite_error)?;
        if updated == 0 {
            return Err(TrError::NotFound {
                message: format!("knowledge record {id}"),
            });
        }
        self.conn
            .execute(
                "INSERT INTO knowledge_events
                 (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                 VALUES (?1, 'restored', NULL, '[]', ?2, ?3)",
                params![id, reason, now],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Compute the set of records eligible for reversible auto-archival under
    /// the Memory Lifecycle conjunctive predicate (L2). READ-ONLY: this never
    /// mutates the store — the caller decides dry-run (print) vs apply (call
    /// [`KnowledgeRepository::archive_record`] per candidate).
    ///
    /// Batched by design: one pass over `retrieval_log` to build the served-id
    /// set, one grouped pass over `lesson_outcome_events`, then a single scan
    /// of active knowledge. No per-record subqueries (16k records must not each
    /// re-scan `retrieval_log`).
    ///
    /// A record is a GHOST candidate iff ALL hold: `age > min_age_days` AND it
    /// has never been served (never appears in any `result_ids`) AND it has
    /// zero `confirmed` outcome events AND it is not protected. A record is a
    /// CONTRADICTED-loser candidate iff `contradicted >= 3 AND contradicted >
    /// confirmed` AND it is not protected. Protection (explicit pin,
    /// `directive`/`context` kind, or `confidence >= 0.9`) is the L1
    /// disjunctive safety guard, applied inline from row fields so a
    /// per-candidate `is_protected` query is unnecessary.
    pub fn archive_candidates(&self, min_age_days: i64) -> Result<Vec<ArchiveCandidate>, TrError> {
        // 1. Served set: every id that ever appeared in retrieval_log.result_ids.
        let mut served: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT result_ids FROM retrieval_log")
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite_error)?;
            for row in rows {
                let ids_json = row.map_err(sqlite_error)?;
                let ids: Vec<String> = serde_json::from_str(&ids_json).unwrap_or_default();
                for id in ids {
                    served.insert(id);
                }
            }
        }

        // 2. Confirmed / contradicted counts per knowledge_id (one grouped pass).
        let mut confirmed: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        let mut contradicted: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT knowledge_id, outcome, COUNT(*) FROM lesson_outcome_events
                     WHERE outcome IN ('confirmed', 'contradicted')
                     GROUP BY knowledge_id, outcome",
                )
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (kid, outcome, n) = row.map_err(sqlite_error)?;
                match outcome.as_str() {
                    "confirmed" => {
                        confirmed.insert(kid, n);
                    }
                    "contradicted" => {
                        contradicted.insert(kid, n);
                    }
                    _ => {}
                }
            }
        }

        // 3. Single scan of active (non-archived) records.
        let now = Utc::now();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, created_at, protected, confidence FROM knowledge
                 WHERE state IN ('active', 'provisional') AND (tier = 'active' OR tier IS NULL)",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                ))
            })
            .map_err(sqlite_error)?;

        let mut candidates = Vec::new();
        for row in rows {
            let (id, kind, created_at_str, protected_flag, confidence_f64) =
                row.map_err(sqlite_error)?;
            // Inline protection check (mirrors is_protected): any one reason is
            // sufficient. Round-trip confidence through f32 to match the
            // canonical type, exactly as is_protected does.
            let protected = protected_flag != 0
                || matches!(kind.as_str(), "directive" | "context")
                || (confidence_f64 as f32) >= 0.9;
            if protected {
                continue;
            }
            let created_at = created_at_str
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap_or(now);
            let age_days = (now - created_at).num_seconds() / 86_400;
            let confirmed_n = confirmed.get(&id).copied().unwrap_or(0);
            let contradicted_n = contradicted.get(&id).copied().unwrap_or(0);
            let never_served = !served.contains(&id);

            if age_days > min_age_days && never_served && confirmed_n == 0 {
                candidates.push(ArchiveCandidate {
                    reason: format!(
                        "ghost: age {age_days}d > {min_age_days}d, never served, 0 confirmations"
                    ),
                    category: "ghost",
                    id,
                });
            } else if contradicted_n >= 3 && contradicted_n > confirmed_n {
                candidates.push(ArchiveCandidate {
                    reason: format!(
                        "contradicted-loser: {contradicted_n} contradictions > {confirmed_n} confirmations"
                    ),
                    category: "contradicted_loser",
                    id,
                });
            }
        }
        Ok(candidates)
    }

    /// Retrieval-induced forgetting (L4): find near-twin pairs — dense cosine
    /// in `[0.85, 0.92)`, i.e. related but below the merge threshold — that are
    /// repeatedly served together, and propose the *loser* of each pair for
    /// reversible archival. The loser is decided by observed retrieval
    /// competition: strictly fewer `confirmed` outcomes than its twin. A record
    /// that is protected, or that ever *wins* a pair, is never proposed. Ties
    /// yield no loser. READ-ONLY. See docs/plans/memory-lifecycle.md.
    pub fn rif_demotion_candidates(
        &self,
        min_cooccur: i64,
    ) -> Result<Vec<ArchiveCandidate>, TrError> {
        // 1. Co-occurrence counts over served slates (small top-k per row).
        let mut cooccur: std::collections::HashMap<(String, String), i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT result_ids FROM retrieval_log")
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite_error)?;
            for row in rows {
                let ids_json = row.map_err(sqlite_error)?;
                let mut ids: Vec<String> = serde_json::from_str(&ids_json).unwrap_or_default();
                ids.sort();
                ids.dedup();
                for i in 0..ids.len() {
                    for j in (i + 1)..ids.len() {
                        *cooccur.entry((ids[i].clone(), ids[j].clone())).or_insert(0) += 1;
                    }
                }
            }
        }

        // 2. Confirmed counts per id.
        let mut confirmed: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT knowledge_id, COUNT(*) FROM lesson_outcome_events
                     WHERE outcome = 'confirmed' GROUP BY knowledge_id",
                )
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (id, n) = row.map_err(sqlite_error)?;
                confirmed.insert(id, n);
            }
        }

        // 3. Pick a loser per frequently co-occurring near-twin pair.
        let mut losers: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut winners: std::collections::HashSet<String> = std::collections::HashSet::new();
        for ((a, b), n) in &cooccur {
            if *n < min_cooccur {
                continue;
            }
            let (Some(va), Some(vb)) = (self.load_dense_vector(a)?, self.load_dense_vector(b)?)
            else {
                continue;
            };
            let Some(sim) = cosine_similarity(&va, &vb) else {
                continue;
            };
            // Near-twin band: related, but not a merge-grade duplicate.
            let thresholds = self.cosine_thresholds();
            if !(thresholds.near_twin_min..thresholds.duplicate_min).contains(&sim) {
                continue;
            }
            let ca = confirmed.get(a).copied().unwrap_or(0);
            let cb = confirmed.get(b).copied().unwrap_or(0);
            let (winner, loser, cw, cl) = if ca > cb {
                (a, b, ca, cb)
            } else if cb > ca {
                (b, a, cb, ca)
            } else {
                continue; // tie: no clear loser
            };
            // is_protected returns Err on unknown id; treat that as protected
            // (skip) so a lookup failure can never demote a record.
            if self.is_protected(loser).unwrap_or(true) {
                continue;
            }
            winners.insert(winner.clone());
            losers.entry(loser.clone()).or_insert_with(|| {
                format!(
                    "rif-loser: near-twin (cosine {sim:.3}) co-served {n}x, \
                     {cl} confirmations vs winner's {cw}"
                )
            });
        }

        // A record that wins any pair is never demoted, even if it also loses
        // another — conservative by design.
        let candidates = losers
            .into_iter()
            .filter(|(id, _)| !winners.contains(id))
            .map(|(id, reason)| ArchiveCandidate {
                id,
                category: "rif_loser",
                reason,
            })
            .collect();
        Ok(candidates)
    }

    /// Record that an archived record matched a live query (a resurrection
    /// signal), and auto-restore it once it has accumulated `threshold` such
    /// matches. Returns `true` iff this call restored the record. The counter
    /// is the durable, inspectable count of `archive_match` audit events. This
    /// self-corrects over-forgetting: a record wrongly archived but still
    /// relevant to real queries comes back on its own, and only after
    /// *repeated* pressure (never a single stray match).
    pub fn note_archive_match(&self, id: &str, threshold: i64) -> Result<bool, TrError> {
        let now = Utc::now().to_rfc3339();
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT state, superseded_by FROM knowledge WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        if matches!(row, Some((ref state, _)) if state == "superseded")
            || matches!(row, Some((_, Some(_))))
        {
            return Ok(false);
        }
        self.conn
            .execute(
                "INSERT INTO knowledge_events
                 (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                 VALUES (?1, 'archive_match', NULL, '[]', 'resurrection signal', ?2)",
                params![id, now],
            )
            .map_err(sqlite_error)?;
        let hits: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id = ?1 AND event_type = 'archive_match'",
                params![id],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        if hits >= threshold {
            // Restore only if still archived (idempotent, and avoids touching a
            // record that was already restored or never existed).
            let tier: Option<String> = self
                .conn
                .query_row(
                    "SELECT tier FROM knowledge WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sqlite_error)?;
            if tier.as_deref() == Some("archive") {
                self.restore_record(id, &format!("resurrected after {hits} archive matches"))?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Dream-tier clustering (L5): find dense CPM communities of
    /// related-but-distinct active lessons — same `(project_id, kind)`,
    /// pairwise dense cosine in `[0.80, 0.92)` (below the merge threshold,
    /// above topical noise) — that carry real confirmation signal (>=
    /// `min_confirmed` confirmed outcomes across members). These are
    /// candidates for LLM abstraction into a derived Claim. READ-ONLY; returns
    /// member ids per cluster, best-signal first, capped at `max_clusters`.
    /// See docs/plans/memory-lifecycle.md.
    pub fn dream_clusters(
        &self,
        min_size: usize,
        min_confirmed: i64,
        max_clusters: usize,
    ) -> Result<Vec<Vec<String>>, TrError> {
        // Confirmed counts per id (one grouped pass).
        let mut confirmed: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT knowledge_id, COUNT(*) FROM lesson_outcome_events
                     WHERE outcome = 'confirmed' GROUP BY knowledge_id",
                )
                .map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (id, n) = row.map_err(sqlite_error)?;
                confirmed.insert(id, n);
            }
        }

        // Active, embedded records grouped by (project, kind).
        type VectorGroups = std::collections::HashMap<(String, String), Vec<(String, Vec<f32>)>>;
        let mut groups: VectorGroups = std::collections::HashMap::new();
        {
            let sql = format!(
                "SELECT id, project_id, kind, vector FROM (
                   SELECT id, project_id, kind, rowid AS rid, {} AS vector FROM knowledge
                   WHERE state = 'active' AND (tier = 'active' OR tier IS NULL)
                 ) WHERE vector IS NOT NULL ORDER BY rid",
                self.vector_sql("knowledge.id")
            );
            let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (id, project, kind, blob) = row.map_err(sqlite_error)?;
                groups
                    .entry((project, kind))
                    .or_default()
                    .push((id, blob_to_vector(&blob)));
            }
        }

        let thresholds = self.cosine_thresholds();
        // CPM communities over near-twin edges inside each group.
        let mut clusters: Vec<(i64, Vec<String>)> = Vec::new();
        for (_key, members) in groups {
            if members.len() < min_size {
                continue;
            }
            let n = members.len();
            let mut edges = Vec::new();
            for i in 0..n {
                for j in (i + 1)..n {
                    if let Some(similarity) = cosine_similarity(&members[i].1, &members[j].1)
                        .filter(|s| (thresholds.dream_min..thresholds.duplicate_min).contains(s))
                    {
                        edges.push((i, j, similarity));
                    }
                }
            }
            for community in leiden_cpm(n, &edges, DREAM_CPM_RESOLUTION) {
                let ids: Vec<String> = community
                    .into_iter()
                    .map(|index| members[index].0.clone())
                    .collect();
                if ids.len() < min_size {
                    continue;
                }
                let signal: i64 = ids
                    .iter()
                    .map(|id| confirmed.get(id).copied().unwrap_or(0))
                    .sum();
                if signal >= min_confirmed {
                    clusters.push((signal, ids));
                }
            }
        }
        clusters.sort_by(|(left_signal, left_ids), (right_signal, right_ids)| {
            right_signal
                .cmp(left_signal)
                .then_with(|| left_ids.cmp(right_ids))
        });
        Ok(clusters
            .into_iter()
            .take(max_clusters)
            .map(|(_, ids)| ids)
            .collect())
    }

    /// Insert an LLM-abstracted derived Claim (L5 dreaming) under the hard
    /// guardrails from docs/plans/memory-lifecycle.md: it MUST cite >= 3
    /// source lessons (provenance), it lands `state='provisional'` (served
    /// with lower weight; promoted to active only by a real confirmed
    /// outcome — see `apply_feedback_outcome`), originals are retained
    /// untouched, and every source link is audited via a `derived_from`
    /// `knowledge_events` row. Returns the new record id.
    pub fn insert_derived_claim(
        &self,
        source_ids: &[String],
        project_id: &str,
        title: &str,
        statement: &str,
    ) -> Result<String, TrError> {
        if source_ids.len() < DERIVED_CLAIM_MIN_SOURCES {
            return Err(TrError::Storage {
                message: format!(
                    "derived claim requires {}..={} source lessons, got {}",
                    DERIVED_CLAIM_MIN_SOURCES,
                    DERIVED_CLAIM_MAX_SOURCES,
                    source_ids.len()
                ),
            });
        }
        if source_ids.len() > DERIVED_CLAIM_MAX_SOURCES {
            return Err(TrError::Storage {
                message: format!(
                    "derived claim refuses {} source lessons; expected {}..={} source lessons",
                    source_ids.len(),
                    DERIVED_CLAIM_MIN_SOURCES,
                    DERIVED_CLAIM_MAX_SOURCES
                ),
            });
        }
        if title.trim().is_empty() || statement.trim().is_empty() {
            return Err(TrError::Storage {
                message: "derived claim requires a non-empty title and statement".into(),
            });
        }
        let id = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(project_id.as_bytes());
            h.update(b"|dream|");
            h.update(statement.as_bytes());
            format!("{:x}", h.finalize())
        };
        let payload = serde_json::json!({
            "statement": statement,
            "derived_from": source_ids,
        });
        let record = KnowledgeRecord {
            id: id.clone(),
            project_id: project_id.to_string(),
            kind: KnowledgeKind::Claim,
            state: KnowledgeState::Provisional,
            title: title.to_string(),
            payload_json: payload.to_string(),
            confidence: 0.6,
            source: fieldcraft_core::KnowledgeSource::Manual,
            superseded_by: None,
            project_specific: true,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: format!("{title}. {statement}"),
            schema_version: 2,
            phase: None,
            tier: fieldcraft_core::KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
            protected: false,
        };
        self.insert_knowledge(&record)?;
        let now = Utc::now().to_rfc3339();
        for src in source_ids {
            self.conn
                .execute(
                    "INSERT INTO knowledge_events
                     (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                     VALUES (?1, 'derived_from', ?2, '[]', 'dream-tier abstraction source', ?3)",
                    params![id, src, now],
                )
                .map_err(sqlite_error)?;
        }
        Ok(id)
    }

    /// True when one live derived claim already cites a majority of this
    /// cluster's members. Archived or superseded derived claims do not shadow
    /// future dream clusters.
    pub fn dream_cluster_already_dreamed(&self, member_ids: &[String]) -> Result<bool, TrError> {
        if member_ids.is_empty() {
            return Ok(false);
        }

        let placeholders = vec!["?"; member_ids.len()].join(",");
        let sql = format!(
            "SELECT e.knowledge_id, COUNT(DISTINCT e.source_knowledge_id) AS overlap
             FROM knowledge_events e
             JOIN knowledge k ON k.id = e.knowledge_id
             WHERE e.event_type = 'derived_from'
               AND e.source_knowledge_id IN ({placeholders})
               AND k.state != 'superseded'
               AND k.tier != 'archive'
             GROUP BY e.knowledge_id
             HAVING overlap > ?"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = member_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let majority_threshold = (member_ids.len() / 2) as i64;
        params.push(&majority_threshold);
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let found = stmt
            .exists(rusqlite::params_from_iter(params))
            .map_err(sqlite_error)?;
        Ok(found)
    }

    /// Find all knowledge records with schema_version below target.
    pub fn find_outdated(&self, target_version: u32) -> Result<Vec<KnowledgeRecord>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project_id, kind, state, title, payload_json, confidence,
                        source_json, superseded_by, project_specific, created_at,
                        valid_until, updated_at, affected_files_json, retrieval_text,
                        schema_version, phase, tier, owner, session_date, source_refs_json,
                        protected
                 FROM knowledge WHERE schema_version < ?1
                 ORDER BY created_at DESC",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![target_version], row_to_knowledge)
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// Delete all knowledge records whose source matches a given session_id.
    /// Returns the number of deleted rows.
    pub fn delete_by_source_session(&self, session_id: &str) -> Result<usize, TrError> {
        let pattern = format!("%\"session_id\":\"{}\"%", session_id);
        let count = self
            .conn
            .execute(
                "DELETE FROM knowledge WHERE source_json LIKE ?1",
                params![pattern],
            )
            .map_err(sqlite_error)?;
        Ok(count)
    }

    pub fn insert_relation(
        &self,
        id: &str,
        from_id: &str,
        to_id: &str,
        kind: &str,
    ) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO knowledge_relations (id, from_id, to_id, kind, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO NOTHING",
                params![id, from_id, to_id, kind, now],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    pub fn insert_relation_with_metadata(
        &self,
        id: &str,
        from_id: &str,
        to_id: &str,
        kind: &str,
        confidence: f32,
        valid_until: Option<&str>,
    ) -> Result<(), TrError> {
        let normalized_kind = normalize_relation_kind(kind)?;
        let now = Utc::now().to_rfc3339();
        let updated = self
            .conn
            .execute(
                "UPDATE knowledge_relations
                 SET id = ?1, confidence = ?2, valid_until = ?3
                 WHERE from_id = ?4 AND to_id = ?5 AND kind = ?6",
                params![
                    id,
                    confidence.clamp(0.0, 1.0) as f64,
                    valid_until,
                    from_id,
                    to_id,
                    normalized_kind,
                ],
            )
            .map_err(sqlite_error)?;
        if updated > 0 {
            return Ok(());
        }
        self.conn
            .execute(
                "INSERT INTO knowledge_relations
                 (id, from_id, to_id, kind, confidence, valid_until, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                   kind=excluded.kind,
                   confidence=excluded.confidence,
                   valid_until=excluded.valid_until",
                params![
                    id,
                    from_id,
                    to_id,
                    normalized_kind,
                    confidence.clamp(0.0, 1.0) as f64,
                    valid_until,
                    now
                ],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Returns (to_id, kind, created_at) tuples for all relations originating from `from_id`.
    pub fn get_relations_from(
        &self,
        from_id: &str,
    ) -> Result<Vec<(String, String, String)>, TrError> {
        let mut stmt = self
            .conn
            .prepare("SELECT to_id, kind, created_at FROM knowledge_relations WHERE from_id = ?1")
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([from_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    pub fn get_active_neighbors(
        &self,
        from_id: &str,
        edge_kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<KnowledgeNeighbor>, TrError> {
        let allowed: std::collections::HashSet<&str> = edge_kinds.iter().copied().collect();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT k.id, k.project_id, k.kind, k.title, r.kind, r.confidence, r.created_at
                 FROM knowledge_relations r
                 JOIN knowledge k ON k.id = r.to_id
                 WHERE r.from_id = ?1
                   AND k.state = 'active'
                   AND (k.tier = 'active' OR k.tier IS NULL)
                   AND (r.valid_until IS NULL OR r.valid_until > ?2)
                 ORDER BY
                   CASE r.kind
                     WHEN 'WARNS_ABOUT' THEN 0
                     WHEN 'REQUIRES' THEN 1
                     WHEN 'DEMONSTRATES' THEN 2
                     WHEN 'SUPERSEDES' THEN 3
                     ELSE 9
                   END,
                   r.confidence DESC,
                   r.created_at DESC
                 LIMIT ?3",
            )
            .map_err(sqlite_error)?;
        let now = Utc::now().to_rfc3339();
        let rows = stmt
            .query_map(params![from_id, now, (limit.max(1) * 4) as i64], |row| {
                Ok(KnowledgeNeighbor {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    kind: row.get(2)?,
                    title: row.get(3)?,
                    relation_kind: row.get(4)?,
                    relation_confidence: row.get::<_, f64>(5)? as f32,
                    created_at: row.get(6)?,
                })
            })
            .map_err(sqlite_error)?;
        let mut neighbors = Vec::new();
        for row in rows {
            let neighbor = row.map_err(sqlite_error)?;
            if allowed.is_empty() || allowed.contains(neighbor.relation_kind.as_str()) {
                neighbors.push(neighbor);
            }
        }
        sort_neighbors(&mut neighbors);
        neighbors.truncate(limit);
        Ok(neighbors)
    }

    pub fn delete_knowledge(&self, id: &str) -> Result<(), TrError> {
        self.conn
            .execute("DELETE FROM knowledge WHERE id = ?1", [id])
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Store a dense vector for a knowledge record under this connection's
    /// tag, as raw little-endian f32 bytes. Errors in BM25-only mode.
    pub fn store_dense_vector(&self, id: &str, vector: &[f32]) -> Result<(), TrError> {
        let tag = self.vector_tag().ok_or_else(|| TrError::Storage {
            message: "no embedding provider configured (BM25-only mode); not storing a vector"
                .into(),
        })?;
        self.store_vector_for_tag(id, tag, vector)
    }

    /// Store `vector` for `id` under `tag`, replacing that tag's previous
    /// vector for the record. Vectors of other tags are kept. A no-op for an
    /// id that is not in `knowledge`.
    pub fn store_vector_for_tag(&self, id: &str, tag: &str, vector: &[f32]) -> Result<(), TrError> {
        let bytes = vector_to_blob(vector);
        self.conn
            .execute(
                "INSERT INTO knowledge_vectors (knowledge_id, tag, dims, vector, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5 WHERE EXISTS (SELECT 1 FROM knowledge WHERE id = ?1)
                 ON CONFLICT (knowledge_id, tag) DO UPDATE SET
                   dims = excluded.dims, vector = excluded.vector,
                   created_at = excluded.created_at",
                params![id, tag, vector.len() as i64, bytes, Utc::now().to_rfc3339()],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Vector count per tag, largest first.
    pub fn vector_tag_counts(&self) -> Result<Vec<(String, i64)>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT tag, COUNT(*) FROM knowledge_vectors GROUP BY tag ORDER BY COUNT(*) DESC",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// Records with no vector under `tag`, as `(id, retrieval_text)`: the
    /// ones retrieval can serve (active/provisional, active tier) first, then
    /// the rest, each by rowid. `fieldcraft reindex` works through this list,
    /// so an interrupted run resumes where it stopped.
    pub fn records_missing_vector(
        &self,
        tag: &str,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT k.id, k.retrieval_text FROM knowledge k
                 WHERE NOT EXISTS (
                   SELECT 1 FROM knowledge_vectors v WHERE v.knowledge_id = k.id AND v.tag = ?1
                 )
                   AND (?3 IS NULL OR k.project_id = ?3)
                 ORDER BY (k.state IN ('active', 'provisional')
                           AND (k.tier = 'active' OR k.tier IS NULL)) DESC, k.rowid
                 LIMIT ?2",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![tag, limit as i64, project_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// How many records (optionally of one project) have no vector under `tag`.
    pub fn count_records_missing_vector(
        &self,
        tag: &str,
        project_id: Option<&str>,
    ) -> Result<i64, TrError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM knowledge k
                 WHERE NOT EXISTS (
                   SELECT 1 FROM knowledge_vectors v WHERE v.knowledge_id = k.id AND v.tag = ?1
                 )
                   AND (?2 IS NULL OR k.project_id = ?2)",
                params![tag, project_id],
                |row| row.get(0),
            )
            .map_err(sqlite_error)
    }

    /// `(serving records missing a vector under tag, serving records)`, where
    /// serving means active/provisional in the active tier.
    pub fn vector_coverage(&self, tag: &str) -> Result<(i64, i64), TrError> {
        self.conn
            .query_row(
                "SELECT
                   SUM(NOT EXISTS (SELECT 1 FROM knowledge_vectors v
                                   WHERE v.knowledge_id = k.id AND v.tag = ?1)),
                   COUNT(*)
                 FROM knowledge k
                 WHERE k.state IN ('active', 'provisional')
                   AND (k.tier = 'active' OR k.tier IS NULL)",
                params![tag],
                |row| Ok((row.get::<_, Option<i64>>(0)?.unwrap_or(0), row.get(1)?)),
            )
            .map_err(sqlite_error)
    }

    /// Load a record's dense vector under this connection's tag.
    pub fn load_dense_vector(&self, id: &str) -> Result<Option<Vec<f32>>, TrError> {
        let Some(tag) = self.vector_tag() else {
            return Ok(None);
        };
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT vector FROM knowledge_vectors WHERE knowledge_id = ?1 AND tag = ?2",
                params![id, tag],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?
            .flatten();
        Ok(blob.map(|bytes| blob_to_vector(&bytes)))
    }

    fn load_cue_bundle(&self, id: &str) -> Result<Option<CueBundle>, TrError> {
        let bundle_json: Option<String> = self
            .conn
            .query_row(
                "SELECT cue_bundle FROM knowledge WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?
            .flatten();
        Ok(bundle_json.and_then(|json| serde_json::from_str(&json).ok()))
    }

    pub fn load_dense_vectors_for(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, TrError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders: String = (0..ids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT k.id, {} FROM knowledge k WHERE k.id IN ({placeholders})",
            self.vector_sql("k.id")
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let params_iter: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_iter), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
            })
            .map_err(sqlite_error)?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, blob) = row.map_err(sqlite_error)?;
            if let Some(bytes) = blob {
                out.insert(id, blob_to_vector(&bytes));
            }
        }
        Ok(out)
    }

    /// Load all dense vectors for a project (for brute-force cosine search).
    /// Returns (id, kind, title, retrieval_text, payload_json, confidence, state,
    /// created_at, dense_vector, phase, owner, session_date, source_json).
    /// `tier` param filters by the tier column: `WHERE (tier = tier OR tier IS NULL)`.
    /// Default callers pass `"active"` so archive records are never auto-retrieved.
    #[allow(clippy::type_complexity)]
    pub fn load_all_vectors(
        &self,
        project_id: &str,
        tier: &str,
    ) -> Result<
        Vec<(
            String,
            String,
            String,
            String,
            String,
            f32,
            String,
            String,
            Vec<f32>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
        )>,
        TrError,
    > {
        let sql = format!(
            "SELECT * FROM (
               SELECT id, kind, title, retrieval_text, payload_json, confidence, state, created_at,
                      {} AS vector, phase, owner, session_date, source_json, project_id
               FROM knowledge
               WHERE (project_id = ?1 OR project_specific = 0)
                 AND state IN ('active', 'provisional')
                 AND (tier = ?2 OR tier IS NULL)
             ) WHERE vector IS NOT NULL",
            self.vector_sql("knowledge.id")
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![project_id, tier], |row| {
                let blob: Vec<u8> = row.get(8)?;
                let vector: Vec<f32> = blob
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| f32::from_le_bytes(*chunk))
                    .collect();
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)? as f32,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    vector,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                ))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// Batch-fetch cue bundles + effective confidence for the given record IDs
    /// in a single SQL round-trip. The canonical retrieval-time enrichment path.
    ///
    /// `effective_confidence` is `COALESCE(dynamic_confidence, confidence)` —
    /// records that have been touched by feedback use their evolved value,
    /// untouched records fall back to the static write-time confidence.
    pub fn load_retrieval_signals_for(&self, ids: &[String]) -> Result<RetrievalSignals, TrError> {
        if ids.is_empty() {
            return Ok(RetrievalSignals::default());
        }
        let placeholders: String = (0..ids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, cue_bundle, COALESCE(dynamic_confidence, confidence) AS effective_confidence \
             FROM knowledge WHERE id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let params_iter: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_iter), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, f64>(2)? as f32,
                ))
            })
            .map_err(sqlite_error)?;
        let mut signals = RetrievalSignals::default();
        for row in rows {
            let (id, bundle_json, conf) = row.map_err(sqlite_error)?;
            signals.effective_confidence.insert(id.clone(), conf);
            if let Some(b) = bundle_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<CueBundle>(s).ok())
            {
                signals.cue_bundles.insert(id, b);
            }
        }
        Ok(signals)
    }

    /// Apply a feedback outcome to a knowledge record: update its
    /// `dynamic_confidence` and append a row to `lesson_outcome_events`.
    /// Returns `(confidence_before, confidence_after)`.
    ///
    /// Update rule (testing-effect-style asymptotic Bayesian — Roediger &
    /// Karpicke 2006, d ≈ 0.80):
    /// - `Confirmed`: c += 0.05 * (1 - c)
    /// - `Ignored`:   c -= 0.08 * c
    /// - `Contradicted`: c -= 0.12 * c
    ///
    /// Floored at 0.05 (never delete records from retrieval), capped at 0.95
    /// (no record is ever certain).
    ///
    /// Returns true when an outcome event already exists for the given
    /// (knowledge_id, session_id, source) triple. The trace analyzer uses this
    /// to stay idempotent across repeated runs over the same transcript.
    pub fn outcome_event_exists(
        &self,
        knowledge_id: &str,
        session_id: &str,
        source: &str,
    ) -> Result<bool, TrError> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM lesson_outcome_events
                 WHERE knowledge_id = ?1 AND session_id = ?2 AND source = ?3",
                params![knowledge_id, session_id, source],
                |r| r.get(0),
            )
            .map_err(|e| TrError::Storage {
                message: format!("outcome_event_exists: {e}"),
            })?;
        Ok(n > 0)
    }

    pub fn apply_feedback_outcome(
        &self,
        knowledge_id: &str,
        outcome: FeedbackOutcome,
        session_id: Option<&str>,
        source: &str,
        note: Option<&str>,
    ) -> Result<(f32, f32), TrError> {
        let row: (f32, Option<f32>) = self
            .conn
            .query_row(
                "SELECT confidence, dynamic_confidence FROM knowledge WHERE id = ?1",
                params![knowledge_id],
                |row| {
                    Ok((
                        row.get::<_, f64>(0)? as f32,
                        row.get::<_, Option<f64>>(1)?.map(|x| x as f32),
                    ))
                },
            )
            .optional()
            .map_err(sqlite_error)?
            .ok_or_else(|| TrError::NotFound {
                message: format!("knowledge record {knowledge_id}"),
            })?;
        let before = row.1.unwrap_or(row.0);
        let after = compute_outcome_update(before, outcome);
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE knowledge SET dynamic_confidence = ?1, updated_at = ?2 WHERE id = ?3",
                params![after as f64, now, knowledge_id],
            )
            .map_err(sqlite_error)?;
        // A real confirmed outcome is what promotes a provisional record to
        // active. This is the L5 dream-tier activation gate (a derived claim
        // lands provisional and only goes active by earning a confirm), and it
        // is the correct semantics for every provisional record.
        if matches!(outcome, FeedbackOutcome::Confirmed) {
            let promoted = self
                .conn
                .execute(
                    "UPDATE knowledge SET state = 'active', updated_at = ?1
                     WHERE id = ?2 AND state = 'provisional'",
                    params![now, knowledge_id],
                )
                .map_err(sqlite_error)?;
            if promoted > 0 {
                self.conn
                    .execute(
                        "INSERT INTO knowledge_events
                         (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                         VALUES (?1, 'promoted_active', NULL, '[]', 'confirmed outcome promoted provisional record', ?2)",
                        params![knowledge_id, now],
                    )
                    .map_err(sqlite_error)?;
                self.demote_derived_sources_on_promotion(knowledge_id, &now)?;
            }
        }
        self.conn
            .execute(
                "INSERT INTO lesson_outcome_events \
                 (knowledge_id, session_id, outcome, source, \
                  confidence_before, confidence_after, note, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    knowledge_id,
                    session_id,
                    outcome.as_str(),
                    source,
                    before as f64,
                    after as f64,
                    note,
                    now,
                ],
            )
            .map_err(sqlite_error)?;
        Ok((before, after))
    }

    fn demote_derived_sources_on_promotion(
        &self,
        derived_knowledge_id: &str,
        now: &str,
    ) -> Result<(), TrError> {
        let source_ids = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT DISTINCT source_knowledge_id
                     FROM knowledge_events
                     WHERE knowledge_id = ?1
                       AND event_type = 'derived_from'
                       AND source_knowledge_id IS NOT NULL",
                )
                .map_err(sqlite_error)?;
            stmt.query_map(params![derived_knowledge_id], |row| row.get::<_, String>(0))
                .map_err(sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sqlite_error)?
        };

        for source_id in source_ids {
            let Some((state, tier)) = self
                .conn
                .query_row(
                    "SELECT state, tier FROM knowledge WHERE id = ?1",
                    params![source_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?
            else {
                return Err(TrError::NotFound {
                    message: format!("source knowledge record {source_id}"),
                });
            };
            if state == "superseded" || tier == "archive" || self.is_protected(&source_id)? {
                continue;
            }

            self.conn
                .execute(
                    "UPDATE knowledge
                     SET state = 'superseded',
                         tier = 'archive',
                         superseded_by = ?1,
                         updated_at = ?2
                     WHERE id = ?3",
                    params![derived_knowledge_id, now, source_id],
                )
                .map_err(sqlite_error)?;
            self.conn
                .execute(
                    "INSERT INTO knowledge_events
                     (knowledge_id, event_type, source_knowledge_id, source_refs_json, note, created_at)
                     VALUES (?1, 'source_demoted', ?2, '[]', ?3, ?4)",
                    params![
                        derived_knowledge_id,
                        source_id,
                        "demote-on-promotion: source superseded by promoted derived claim",
                        now,
                    ],
                )
                .map_err(sqlite_error)?;
        }
        Ok(())
    }

    /// Batch-fetch parsed `CueBundle` JSON for the given record IDs.
    ///
    /// Returns a map containing only the records that have a non-null,
    /// parseable `cue_bundle` column — records without one (e.g. legacy or
    /// pending backfill) are silently omitted from the result.
    ///
    /// Phase 2 of cue-bundle retrieval — used by `retrieve()` after RRF
    /// fusion to compute conjunctive cue scores against the query bundle.
    pub fn load_cue_bundles_for(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, CueBundle>, TrError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders: String = (0..ids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, cue_bundle FROM knowledge \
             WHERE id IN ({placeholders}) AND cue_bundle IS NOT NULL AND cue_bundle <> ''"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let params_iter: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_iter), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, json) = row.map_err(sqlite_error)?;
            if let Ok(bundle) = serde_json::from_str::<CueBundle>(&json) {
                out.insert(id, bundle);
            }
        }
        Ok(out)
    }

    /// FTS5 BM25 search. Returns (id, bm25_score).
    /// `tier` param: `WHERE (tier = tier OR tier IS NULL)`. Default = "active".
    pub fn fts_search(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
        tier: &str,
    ) -> Result<Vec<(String, f64)>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT f.id, bm25(knowledge_fts) as score
                 FROM knowledge_fts f
                 JOIN knowledge k ON k.id = f.id
                 WHERE knowledge_fts MATCH ?1
                   AND (f.project_id = ?2 OR k.project_specific = 0)
                   AND (k.tier = ?4 OR k.tier IS NULL)
                 ORDER BY score
                 LIMIT ?3",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![query, project_id, limit as i64, tier], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// FTS5 BM25 search across all records in the requested tier.
    pub fn fts_search_unscoped(
        &self,
        query: &str,
        limit: usize,
        tier: &str,
    ) -> Result<Vec<(String, f64)>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT f.id, bm25(knowledge_fts) as score
                 FROM knowledge_fts f
                 JOIN knowledge k ON k.id = f.id
                 WHERE knowledge_fts MATCH ?1
                   AND k.state IN ('active', 'provisional')
                   AND (k.tier = ?3 OR k.tier IS NULL)
                 ORDER BY score
                 LIMIT ?2",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![query, limit as i64, tier], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// `haystack_of(id)` is the record's `record_haystack` when the record is
    /// admissible (state active or provisional, in the requested tier) and `None`
    /// otherwise; the record cache answers it from memory, so the identity
    /// query never touches `knowledge` rows.
    pub fn identity_cue_search<'h>(
        &self,
        cues: &[String],
        query_terms: &[String],
        limit: usize,
        legacy_candidates: &[(String, String)],
        haystack_of: &dyn Fn(&str) -> Option<&'h str>,
    ) -> Result<Vec<(String, f64)>, TrError> {
        if cues.is_empty() || query_terms.is_empty() {
            return Ok(vec![]);
        }
        let normalized_cues: Vec<String> = cues
            .iter()
            .map(|cue| fieldcraft_core::cue_extract::normalize_identity(cue))
            .filter(|cue| cue.len() >= 5)
            .collect();
        if normalized_cues.is_empty() {
            return Ok(vec![]);
        }

        let normalized_query_terms: Vec<(String, String)> = query_terms
            .iter()
            .filter(|term| term.len() >= 4)
            .map(|term| {
                (
                    term.to_lowercase(),
                    fieldcraft_core::cue_extract::normalize_identity(term),
                )
            })
            .filter(|(_, normalized)| normalized.len() >= 4)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let identity_expr = "replace(replace(lower(d.v), '_', ''), '-', '')";
        let identity_predicates = normalized_cues
            .iter()
            .map(|_| {
                format!(
                    "({identity_expr} = ? OR {identity_expr} LIKE '%' || ? || '%' OR ? LIKE '%' || {identity_expr} || '%' OR (length(?) >= 6 AND {identity_expr} GLOB ?))"
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            // The predicates run once per distinct identity value (~3k on the
            // 2026-11 store), not once per cue row (~57k): each evaluation
            // allocates strings, and under concurrent queries SQLite's
            // allocator lock made the per-row form the server's bottleneck.
            "WITH matched AS (
               SELECT d.v FROM (SELECT DISTINCT dim_value AS v FROM cue_index
                                WHERE dim_name = 'identity') d
               WHERE {identity_predicates}
             )
             SELECT DISTINCT c.record_id, c.dim_value
             FROM matched m
             JOIN cue_index c ON c.dim_name = 'identity' AND c.dim_value = m.v"
        );
        let fuzzy_globs: Vec<String> = normalized_cues
            .iter()
            .map(|cue| {
                let suffix_start = cue.len().saturating_sub(4);
                let suffix = &cue[suffix_start..];
                format!("{}*{}*", &cue[..1], suffix)
            })
            .collect();
        let mut cue_params: Vec<&dyn rusqlite::ToSql> = Vec::new();
        for (cue, fuzzy_glob) in normalized_cues.iter().zip(&fuzzy_globs) {
            cue_params.push(cue);
            cue_params.push(cue);
            cue_params.push(cue);
            cue_params.push(cue);
            cue_params.push(fuzzy_glob);
        }

        // A record matches through several identity rows (aliases); it scores by
        // its best row, independent of the order rows come back in.
        let mut best: HashMap<String, f64> = HashMap::new();
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(cue_params), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_error)?;
        for row in rows {
            let (id, identity_value) = row.map_err(sqlite_error)?;
            let identity_norm = fieldcraft_core::cue_extract::normalize_identity(&identity_value);
            let identity_score = normalized_cues
                .iter()
                .map(|cue| identity_similarity(cue, &identity_norm))
                .fold(0.0_f64, f64::max);
            if identity_score == 0.0 {
                continue;
            }

            let Some(haystack) = haystack_of(&id) else {
                continue;
            };
            let overlap = normalized_query_terms
                .iter()
                .filter(|(term, normalized)| {
                    identity_similarity(normalized, &identity_norm) == 0.0
                        && haystack.contains(term.as_str())
                })
                .count() as f64;
            let score = identity_score * 10.0 + overlap;
            let entry = best.entry(id).or_insert(score);
            *entry = entry.max(score);
        }
        let mut seen: std::collections::HashSet<String> = best.keys().cloned().collect();
        let mut scored: Vec<(String, f64)> = best.into_iter().collect();

        // Compatibility for databases that have not been cue-backfilled yet:
        // admit legacy rows by their stored project_id only when no identity
        // cue row exists for that record. Backfill removes this path naturally.
        // The caller supplies those rows as `(id, project_id)` in scan order
        // (state, rowid) from the record cache; only rows whose project can
        // match are read from SQLite, for their text.
        let mut project_scores: HashMap<&str, (String, f64)> = HashMap::new();
        let mut admitted: Vec<(&str, &str)> = Vec::new();
        for (id, project_id) in legacy_candidates {
            if seen.contains(id) {
                continue;
            }
            let (_, identity_score) =
                project_scores
                    .entry(project_id.as_str())
                    .or_insert_with(|| {
                        let norm = fieldcraft_core::cue_extract::normalize_identity(project_id);
                        let score = normalized_cues
                            .iter()
                            .map(|cue| identity_similarity(cue, &norm))
                            .fold(0.0_f64, f64::max);
                        (norm, score)
                    });
            if *identity_score == 0.0 {
                continue;
            }
            seen.insert(id.clone());
            admitted.push((id.as_str(), project_id.as_str()));
        }
        for (id, project_id) in admitted {
            let Some(haystack) = haystack_of(id) else {
                continue;
            };
            let (identity_norm, identity_score) = &project_scores[project_id];
            let overlap = normalized_query_terms
                .iter()
                .filter(|(term, normalized)| {
                    identity_similarity(normalized, identity_norm) == 0.0
                        && haystack.contains(term.as_str())
                })
                .count() as f64;
            scored.push((id.to_string(), identity_score * 10.0 + overlap));
        }

        // Scores are small integers plus a few identity levels, so ties are the
        // norm; break them by id, not by the order SQLite happened to return
        // rows in (which depends on the query plan).
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    /// FTS5 BM25 search across all projects (cross-project, project_specific = 0).
    /// `tier` param: `WHERE (tier = tier OR tier IS NULL)`. Default = "active".
    pub fn fts_search_cross_project(
        &self,
        query: &str,
        limit: usize,
        tier: &str,
    ) -> Result<Vec<(String, f64)>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT f.id, bm25(knowledge_fts) as score
                 FROM knowledge_fts f
                 JOIN knowledge k ON k.id = f.id
                 WHERE knowledge_fts MATCH ?1
                   AND k.project_specific = 0
                   AND (k.tier = ?3 OR k.tier IS NULL)
                 ORDER BY score
                 LIMIT ?2",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![query, limit as i64, tier], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    pub fn count_by_project(&self, project_id: &str) -> Result<usize, TrError> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM knowledge WHERE project_id = ?1",
                [project_id],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        Ok(count as usize)
    }

    /// Log a retrieval event for the feedback loop.
    ///
    /// Convenience wrapper for the common case (no raw candidates, no link tracking).
    /// Call [`log_retrieval_entry`](Self::log_retrieval_entry) directly when you need to
    /// record raw pre-gate candidates or surfaced link IDs.
    pub fn log_retrieval(
        &self,
        project_id: &str,
        query_text: &str,
        result_ids: &[String],
        result_scores: &[f32],
        latency_ms: u64,
    ) -> Result<(), TrError> {
        self.log_retrieval_entry(&RetrievalLogEntry {
            project_id,
            query_text,
            result_ids,
            result_scores,
            latency_ms,
            ..Default::default()
        })
    }

    /// Log a full retrieval event including raw pre-gate candidates and surfaced links.
    /// Canonical entry point — both [`log_retrieval`](Self::log_retrieval) and external
    /// callers should funnel through here.
    pub fn log_retrieval_entry(&self, entry: &RetrievalLogEntry<'_>) -> Result<(), TrError> {
        let ids_json = serde_json::to_string(entry.result_ids).unwrap_or_default();
        let scores_json = serde_json::to_string(entry.result_scores).unwrap_or_default();
        let raw_ids_json = serde_json::to_string(entry.raw_candidate_ids).unwrap_or_default();
        let raw_scores_json = serde_json::to_string(entry.raw_candidate_scores).unwrap_or_default();
        let raw_titles_json = serde_json::to_string(entry.raw_candidate_titles).unwrap_or_default();
        let result_origins_json = serde_json::to_string(entry.result_origins).unwrap_or_default();
        let raw_origins_json =
            serde_json::to_string(entry.raw_candidate_origins).unwrap_or_default();
        let links_json = serde_json::to_string(entry.link_ids_surfaced).unwrap_or_default();
        let computed_cue_bundle;
        let cue_bundle_param: Option<&str> =
            if let Some(query_cue_bundle) = non_empty_str(entry.query_cue_bundle) {
                Some(query_cue_bundle)
            } else {
                computed_cue_bundle = build_retrieval_log_query_cue_bundle(entry)?;
                computed_cue_bundle.as_deref()
            };
        let session_id_param: Option<&str> = if entry.session_id.is_empty() {
            None
        } else {
            Some(entry.session_id)
        };
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO retrieval_log (
                    project_id, query_text, result_ids, result_scores, result_count, latency_ms, created_at,
                    raw_candidate_ids, raw_candidate_scores, raw_candidate_titles, result_origins,
                    raw_candidate_origins, link_ids_surfaced,
                    query_cue_bundle, session_id
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    entry.project_id,
                    entry.query_text,
                    ids_json,
                    scores_json,
                    entry.result_ids.len() as i64,
                    entry.latency_ms as i64,
                    now,
                    raw_ids_json,
                    raw_scores_json,
                    raw_titles_json,
                    result_origins_json,
                    raw_origins_json,
                    links_json,
                    cue_bundle_param,
                    session_id_param,
                ],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Fetch retrieval-log rows that the trace analyzer needs to correlate
    /// session activity. When `session_id` is provided, return only rows tagged
    /// with that session — exact attribution. Otherwise fall back to the
    /// project + half-open time window `[since, until)` (RFC3339 strings) so
    /// legacy retrievals without `session_id` can still be analyzed.
    ///
    /// The branches are mutually exclusive: previously they were OR'd, which
    /// pulled cross-session retrieval rows into the candidate pool whenever a
    /// session_id was supplied and the time window happened to overlap.
    pub fn retrievals_for_project_window(
        &self,
        project_id: &str,
        since_rfc3339: &str,
        until_rfc3339: &str,
        session_id: Option<&str>,
    ) -> Result<Vec<RetrievalLogRow>, TrError> {
        let sql = "SELECT id, query_cue_bundle, result_ids FROM retrieval_log
                   WHERE (?4 IS NOT NULL AND session_id = ?4)
                      OR (?4 IS NULL AND project_id = ?1 AND created_at >= ?2 AND created_at < ?3)
                   ORDER BY id ASC";
        let mut stmt = self.conn.prepare(sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![project_id, since_rfc3339, until_rfc3339, session_id],
                |row| {
                    let id: i64 = row.get(0)?;
                    let qcb: Option<String> = row.get(1)?;
                    let ids_json: String = row.get(2)?;
                    Ok((id, qcb, ids_json))
                },
            )
            .map_err(sqlite_error)?;
        let mut out = Vec::new();
        for r in rows {
            let (id, qcb, ids_json) = r.map_err(sqlite_error)?;
            let result_ids: Vec<String> = serde_json::from_str(&ids_json).unwrap_or_default();
            out.push(RetrievalLogRow {
                id,
                query_cue_bundle: qcb,
                result_ids,
            });
        }
        Ok(out)
    }

    pub fn affected_files_for_knowledge_ids(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>, TrError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders: String = (0..ids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT id, affected_files_json FROM knowledge WHERE id IN ({placeholders})");
        let params_iter = ids.iter().map(|s| s as &dyn rusqlite::ToSql);
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_iter), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(sqlite_error)?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, files_json) = row.map_err(sqlite_error)?;
            let files = files_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .unwrap_or_default();
            out.insert(id, files);
        }
        Ok(out)
    }

    /// Record a feedback signal for a knowledge record.
    pub fn record_feedback(
        &self,
        knowledge_id: &str,
        signal: &str,
        retrieval_id: Option<i64>,
        project_id: &str,
    ) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO lesson_feedback (knowledge_id, signal, retrieval_id, project_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![knowledge_id, signal, retrieval_id, project_id, now],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Find the most recent retrieval in this project within the last N minutes.
    /// Returns (retrieval_log.id, result_ids JSON) if found.
    pub fn recent_retrieval(&self, project_id: &str, minutes: i64) -> Option<(i64, String)> {
        let cutoff = (Utc::now() - chrono::Duration::minutes(minutes)).to_rfc3339();
        self.conn
            .query_row(
                "SELECT id, result_ids FROM retrieval_log
                 WHERE project_id = ?1 AND created_at > ?2
                 ORDER BY created_at DESC LIMIT 1",
                params![project_id, cutoff],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()
    }

    pub fn record_link_revealed(&self, project_id: &str, link_id: &str) -> Result<(), TrError> {
        let cutoff = (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        let Some((retrieval_id, existing_json)) = self
            .conn
            .query_row(
                "SELECT id, link_ids_revealed FROM retrieval_log
                 WHERE project_id = ?1 AND created_at > ?2
                 ORDER BY created_at DESC LIMIT 1",
                params![project_id, cutoff],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .ok()
        else {
            return Ok(());
        };
        let mut revealed: Vec<String> =
            serde_json::from_str(&existing_json).unwrap_or_else(|_| Vec::new());
        if !revealed.iter().any(|id| id == link_id) {
            revealed.push(link_id.to_string());
        }
        let revealed_json = serde_json::to_string(&revealed).unwrap_or_default();
        self.conn
            .execute(
                "UPDATE retrieval_log SET link_ids_revealed = ?1 WHERE id = ?2",
                params![revealed_json, retrieval_id],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }
}

fn row_to_knowledge(row: &rusqlite::Row<'_>) -> rusqlite::Result<KnowledgeRecord> {
    let kind_str: String = row.get(2)?;
    let state_str: String = row.get(3)?;
    let source_json: String = row.get(7)?;
    let project_specific_i: i64 = row.get(9)?;
    let created_at_str: String = row.get(10)?;
    let valid_until_str: Option<String> = row.get(11)?;

    let kind = parse_knowledge_kind(&kind_str);
    let state = parse_knowledge_state(&state_str);
    let source: KnowledgeSource =
        serde_json::from_str(&source_json).unwrap_or(KnowledgeSource::Manual);
    let created_at = created_at_str
        .parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap_or_else(|_| chrono::Utc::now());
    let valid_until = valid_until_str.and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());

    let schema_version: u32 = row.get::<_, i64>(15).unwrap_or(2) as u32;
    let phase_str: Option<String> = row.get(16).ok();
    let phase = phase_str.and_then(|s| KnowledgePhase::parse(&s));
    // Column 17 = tier. NULL (pre-migration rows) defaults to Active.
    let tier_str: Option<String> = row.get(17).ok().flatten();
    let tier = tier_str
        .as_deref()
        .map(KnowledgeTier::parse)
        .unwrap_or_default();
    // Column 18 = owner. NULL for legacy records.
    let owner: Option<String> = row.get(18).ok().flatten();
    let session_date_str: Option<String> = row.get(19).ok().flatten();
    let session_date = session_date_str
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));
    let source_refs_json: Option<String> = row.get(20).ok().flatten();
    let source_refs = source_refs_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    // Column 21 = protected. NULL/missing (pre-migration rows, or queries
    // that haven't picked up the new column yet) defaults to false.
    let protected: i64 = row.get(21).unwrap_or(0);

    Ok(KnowledgeRecord {
        id: row.get(0)?,
        project_id: row.get(1)?,
        kind,
        state,
        title: row.get(4)?,
        payload_json: row.get(5)?,
        confidence: row.get::<_, f64>(6)? as f32,
        source,
        superseded_by: row.get(8)?,
        project_specific: project_specific_i != 0,
        created_at,
        valid_until,
        retrieval_text: row.get(14)?,
        schema_version,
        phase,
        tier,
        owner,
        session_date,
        source_refs,
        protected: protected != 0,
    })
}

fn escape_sqlite_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn normalize_relation_kind(kind: &str) -> Result<&'static str, TrError> {
    match kind.trim().to_ascii_uppercase().as_str() {
        "REQUIRES" => Ok("REQUIRES"),
        "WARNS_ABOUT" => Ok("WARNS_ABOUT"),
        "DEMONSTRATES" => Ok("DEMONSTRATES"),
        "SUPERSEDES" => Ok("SUPERSEDES"),
        other => Err(TrError::Storage {
            message: format!("unsupported relation kind: {other}"),
        }),
    }
}

fn sort_neighbors(neighbors: &mut [KnowledgeNeighbor]) {
    neighbors.sort_by(|a, b| {
        relation_priority(&a.relation_kind)
            .cmp(&relation_priority(&b.relation_kind))
            .then_with(|| {
                b.relation_confidence
                    .partial_cmp(&a.relation_confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn relation_priority(kind: &str) -> u8 {
    match kind {
        "WARNS_ABOUT" => 0,
        "REQUIRES" => 1,
        "DEMONSTRATES" => 2,
        "SUPERSEDES" => 3,
        _ => 9,
    }
}

/// CPM resolution for dream clustering. With dream edges in `[0.80, 0.92)`
/// (Gemini thresholds, `fieldcraft_core::embedding`),
/// `0.40` keeps the existing 3-path fixture merged (`gamma < 0.866 / 2`)
/// while rejecting a single 0.85 bridge into a K4 (`gamma > 0.85 / 4`).
const DREAM_CPM_RESOLUTION: f32 = 0.40;
const DUPLICATE_MIN_CUE_OVERLAP: f32 = 0.35;
const DUPLICATE_MERGE_THRESHOLD: f32 = 0.82;

fn duplicate_score(
    duplicate_min_cosine: f32,
    a: &KnowledgeRecord,
    b: &KnowledgeRecord,
    a_dense: Option<&[f32]>,
    b_dense: Option<&[f32]>,
    a_cue_bundle: Option<&CueBundle>,
    b_cue_bundle: Option<&CueBundle>,
) -> f32 {
    let title_score = token_jaccard(&a.title, &b.title);
    let body_score = token_jaccard(&a.retrieval_text, &b.retrieval_text);
    let text_score = title_score.max(body_score);
    let Some(dense_score) = a_dense
        .zip(b_dense)
        .and_then(|(a, b)| cosine_similarity(a, b))
    else {
        // No embeddings to judge with (e.g. records written over HTTP are not
        // embedded server-side). The ONLY merge allowed without vectors is a
        // byte-identical duplicate: same title AND same retrieval_text after
        // trimming, both non-empty, no contradiction. Anything less similar
        // stays unmerged until embeddings exist.
        let (at, bt) = (a.title.trim(), b.title.trim());
        let (ar, br) = (a.retrieval_text.trim(), b.retrieval_text.trim());
        if !at.is_empty()
            && !ar.is_empty()
            && at == bt
            && ar == br
            && !title_contradicts(&a.title, &b.title)
        {
            return 0.95;
        }
        return 0.0;
    };
    if dense_score < duplicate_min_cosine || title_contradicts(&a.title, &b.title) {
        return 0.0;
    }

    let cue_score = a_cue_bundle
        .zip(b_cue_bundle)
        .map(|(a_bundle, b_bundle)| cue_bundle_overlap(a_bundle, b_bundle, &a.project_id))
        .unwrap_or(0.0);
    if cue_score < DUPLICATE_MIN_CUE_OVERLAP {
        return 0.0;
    }

    let has_text_or_strong_cue_support = text_score >= 0.18 || cue_score >= 0.60;
    if !has_text_or_strong_cue_support {
        return 0.0;
    }

    (dense_score * 0.72) + (cue_score * 0.18) + (text_score * 0.10)
}

/// Raw little-endian f32 bytes, the stored form of every dense vector.
pub(crate) fn vector_to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub(crate) fn blob_to_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let mut dot = 0.0_f32;
    let mut a_norm = 0.0_f32;
    let mut b_norm = 0.0_f32;
    for (av, bv) in a.iter().zip(b.iter()) {
        dot += av * bv;
        a_norm += av * av;
        b_norm += bv * bv;
    }
    if a_norm == 0.0 || b_norm == 0.0 {
        return None;
    }
    Some(dot / (a_norm.sqrt() * b_norm.sqrt()))
}

fn cue_bundle_overlap(a: &CueBundle, b: &CueBundle, project_id: &str) -> f32 {
    let a_rows = cue_rows_without_project_identity(a, project_id);
    let b_rows = cue_rows_without_project_identity(b, project_id);
    if a_rows.is_empty() || b_rows.is_empty() {
        return 0.0;
    }
    let intersection = a_rows.intersection(&b_rows).count() as f32;
    let union = a_rows.union(&b_rows).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn cue_rows_without_project_identity(
    bundle: &CueBundle,
    project_id: &str,
) -> HashSet<(String, String)> {
    bundle
        .to_index_rows()
        .into_iter()
        .filter(|(dim, value)| !(*dim == "identity" && value == project_id))
        .map(|(dim, value)| (dim.to_string(), value))
        .collect()
}

fn title_contradicts(a: &str, b: &str) -> bool {
    negation_count(a) != negation_count(b) && token_jaccard(a, b) >= 0.45
}

fn negation_count(input: &str) -> usize {
    normalized_tokens(input)
        .into_iter()
        .filter(|token| {
            matches!(
                token.as_str(),
                "not" | "never" | "avoid" | "without" | "disable" | "disabled"
            )
        })
        .count()
}

fn identity_similarity(cue: &str, candidate: &str) -> f64 {
    if cue.is_empty() || candidate.is_empty() {
        return 0.0;
    }
    if cue == candidate {
        return 3.0;
    }
    if cue.contains(candidate) || candidate.contains(cue) {
        return 2.0;
    }
    if levenshtein_at_most(cue, candidate, 2) {
        return 1.5;
    }
    if cue.len() >= 6 && candidate.len().abs_diff(cue.len()) <= 6 && is_subsequence(cue, candidate)
    {
        return 1.25;
    }
    0.0
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = needle.bytes();
    let mut current = chars.next();
    if current.is_none() {
        return true;
    }
    for candidate in haystack.bytes() {
        if Some(candidate) == current {
            current = chars.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

fn levenshtein_at_most(a: &str, b: &str, max_distance: usize) -> bool {
    if a.len().abs_diff(b.len()) > max_distance {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for (i, ac) in a.bytes().enumerate() {
        curr[0] = i + 1;
        let mut row_min = curr[0];
        for (j, bc) in b.bytes().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
            row_min = row_min.min(curr[j + 1]);
        }
        if row_min > max_distance {
            return false;
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()] <= max_distance
}

fn token_jaccard(a: &str, b: &str) -> f32 {
    let a_tokens = normalized_tokens(a);
    let b_tokens = normalized_tokens(b);
    if a_tokens.is_empty() || b_tokens.is_empty() {
        return 0.0;
    }
    let intersection = a_tokens.intersection(&b_tokens).count() as f32;
    let union = a_tokens.union(&b_tokens).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn normalized_tokens(input: &str) -> HashSet<String> {
    input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() >= 3 && !is_stopword(token))
        .collect()
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "the"
            | "and"
            | "for"
            | "with"
            | "that"
            | "this"
            | "when"
            | "from"
            | "into"
            | "should"
            | "because"
            | "while"
            | "lesson"
            | "fieldcraft"
    )
}

fn extract_affected_files(payload_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload_json).ok()?;
    // New types use "files", legacy types use "affected_files"
    let files = v
        .get("files")
        .or_else(|| v.get("affected_files"))?
        .as_array()?;
    if files.is_empty() {
        return None;
    }
    serde_json::to_string(files).ok()
}

fn knowledge_kind_str(kind: KnowledgeKind) -> &'static str {
    match kind {
        KnowledgeKind::Claim => "claim",
        KnowledgeKind::Procedure => "procedure",
        KnowledgeKind::Episode => "episode",
        KnowledgeKind::Directive => "directive",
        KnowledgeKind::Context => "context",
    }
}

fn knowledge_state_str(state: KnowledgeState) -> &'static str {
    match state {
        KnowledgeState::Active => "active",
        KnowledgeState::Provisional => "provisional",
        KnowledgeState::Superseded => "superseded",
        KnowledgeState::Retired => "retired",
    }
}

fn parse_knowledge_kind(s: &str) -> KnowledgeKind {
    match s {
        "claim" => KnowledgeKind::Claim,
        "procedure" => KnowledgeKind::Procedure,
        "episode" => KnowledgeKind::Episode,
        "directive" => KnowledgeKind::Directive,
        "context" => KnowledgeKind::Context,
        _ => KnowledgeKind::Claim,
    }
}

fn parse_knowledge_state(s: &str) -> KnowledgeState {
    match s {
        "active" => KnowledgeState::Active,
        "provisional" => KnowledgeState::Provisional,
        "superseded" => KnowledgeState::Superseded,
        "retired" => KnowledgeState::Retired,
        _ => KnowledgeState::Provisional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use fieldcraft_core::{AgentTool, KnowledgeKind, KnowledgeSource, KnowledgeState};
    use tempfile::tempdir;

    fn make_record(id: &str, project_id: &str, kind: KnowledgeKind) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.to_string(),
            project_id: project_id.to_string(),
            kind,
            state: KnowledgeState::Provisional,
            title: format!("Test {id}"),
            payload_json: r#"{"statement":"test"}"#.to_string(),
            confidence: 0.5,
            source: KnowledgeSource::Session {
                session_id: "s1".to_string(),
                tool: AgentTool::ClaudeCode,
            },
            superseded_by: None,
            project_specific: true,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: format!("retrieval text for {id}"),
            schema_version: 2,
            phase: None,
            tier: fieldcraft_core::KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
            protected: false,
        }
    }

    fn insert_active_claim_sources(repo: &KnowledgeRepository, ids: &[&str]) -> Vec<String> {
        let mut source_ids = Vec::new();
        for id in ids {
            let mut record = make_record(id, "proj", KnowledgeKind::Claim);
            record.state = KnowledgeState::Active;
            repo.insert_knowledge(&record).unwrap();
            source_ids.push((*id).to_string());
        }
        source_ids
    }

    fn source_demoted_count(repo: &KnowledgeRepository, derived_id: &str) -> i64 {
        repo.connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id = ?1 AND event_type = 'source_demoted'",
                rusqlite::params![derived_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut record = make_record("rec-1", "proj-a", KnowledgeKind::Claim);
        record.source_refs = vec![fieldcraft_core::SourceReference {
            source_id: "source-1".to_string(),
            chunk_index: Some(2),
        }];
        repo.insert_knowledge(&record).unwrap();
        let loaded = repo.get_knowledge("rec-1").unwrap().expect("should exist");
        assert_eq!(loaded.id, "rec-1");
        assert_eq!(loaded.project_id, "proj-a");
        assert_eq!(loaded.kind, KnowledgeKind::Claim);
        assert_eq!(loaded.state, KnowledgeState::Provisional);
        assert_eq!(loaded.retrieval_text, "retrieval text for rec-1");
        assert_eq!(loaded.source_refs, record.source_refs);
    }

    #[test]
    fn retrieval_marker_lookup_escapes_like_wildcards() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut exact = make_record("exact", "proj-a", KnowledgeKind::Context);
        exact.retrieval_text = "Uses [skill:my_skill] for design checks".to_string();
        let mut wildcard = make_record("wildcard", "proj-a", KnowledgeKind::Context);
        wildcard.retrieval_text = "Uses [skill:myXskill] for other checks".to_string();

        repo.insert_knowledge(&exact).unwrap();
        repo.insert_knowledge(&wildcard).unwrap();

        let ids = repo
            .list_knowledge_ids_by_retrieval_marker("proj-a", "[skill:my_skill]", "active")
            .unwrap();
        assert_eq!(ids, vec!["exact".to_string()]);
    }

    #[test]
    fn log_retrieval_entry_persists_result_and_raw_origins() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let result_ids = vec!["learned-1".to_string(), "external-1".to_string()];
        let result_scores = vec![0.9, 0.8];
        let result_origins = vec!["learned".to_string(), "external_derived".to_string()];
        let raw_ids = vec!["learned-1".to_string(), "external-1".to_string()];
        let raw_scores = vec![0.9, 0.8];
        let raw_titles = vec!["Learned".to_string(), "External".to_string()];
        let raw_origins = vec!["learned".to_string(), "external_derived".to_string()];

        repo.log_retrieval_entry(&RetrievalLogEntry {
            project_id: "proj",
            query_text: "routing sentinel",
            result_ids: &result_ids,
            result_scores: &result_scores,
            result_origins: &result_origins,
            raw_candidate_ids: &raw_ids,
            raw_candidate_scores: &raw_scores,
            raw_candidate_titles: &raw_titles,
            raw_candidate_origins: &raw_origins,
            ..Default::default()
        })
        .unwrap();

        let row: (String, String) = repo
            .conn
            .query_row(
                "SELECT result_origins, raw_candidate_origins FROM retrieval_log",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, r#"["learned","external_derived"]"#);
        assert_eq!(row.1, r#"["learned","external_derived"]"#);
    }

    #[test]
    fn log_retrieval_entry_builds_query_cue_bundle_when_missing() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let result_ids = vec!["learned-1".to_string()];
        let result_scores = vec![0.9];
        let query_files = vec![
            "crates/fieldcraft-storage/src/repository.rs".to_string(),
            "crates/fieldcraft-cli/src/main.rs".to_string(),
        ];

        repo.log_retrieval_entry(&RetrievalLogEntry {
            project_id: "proj",
            query_text: "Fix SQLite RetrievalLog telemetry",
            query_context: "Run cargo stats verification",
            query_files: &query_files,
            result_ids: &result_ids,
            result_scores: &result_scores,
            ..Default::default()
        })
        .unwrap();

        let bundle_json: String = repo
            .conn
            .query_row("SELECT query_cue_bundle FROM retrieval_log", [], |row| {
                row.get(0)
            })
            .unwrap();
        let bundle: CueBundle = serde_json::from_str(&bundle_json).unwrap();
        assert_eq!(
            bundle.active_files,
            vec!["repository.rs".to_string(), "main.rs".to_string()]
        );
        assert!(bundle.tools_involved.iter().any(|tool| tool == "cargo"));
        assert!(
            bundle
                .distinctive_terms
                .iter()
                .any(|term| term == "SQLite" || term == "RetrievalLog")
        );
        assert!(bundle.cues.iter().any(|cue| {
            cue.dimension == fieldcraft_core::CueDimension::Identity
                && cue.value.canonical == "proj"
        }));
    }

    #[test]
    fn merge_queue_is_idempotent_until_claimed() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        assert!(repo.enqueue_merge_candidate("lesson-1").unwrap());
        assert!(!repo.enqueue_merge_candidate("lesson-1").unwrap());

        let claimed = repo.claim_merge_candidates(10).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].lesson_id, "lesson-1");
        assert_eq!(claimed[0].attempts, 1);
    }

    #[test]
    fn obvious_duplicate_merge_archives_newer_lesson_and_keeps_provenance_event() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut canonical = make_record("canon", "proj-a", KnowledgeKind::Claim);
        canonical.title =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors".to_string();
        canonical.retrieval_text =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors in records body"
                .to_string();
        canonical.source_refs = vec![fieldcraft_core::SourceReference {
            source_id: "source-1".to_string(),
            chunk_index: Some(1),
        }];
        repo.insert_knowledge(&canonical).unwrap();

        let mut duplicate = make_record("dupe", "proj-a", KnowledgeKind::Claim);
        duplicate.title =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors".to_string();
        duplicate.retrieval_text =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors in records body"
                .to_string();
        duplicate.source_refs = vec![fieldcraft_core::SourceReference {
            source_id: "source-2".to_string(),
            chunk_index: Some(4),
        }];
        repo.insert_knowledge(&duplicate).unwrap();
        repo.store_dense_vector("canon", &[1.0, 0.0, 0.0]).unwrap();
        repo.store_dense_vector("dupe", &[0.995, 0.10, 0.0])
            .unwrap();

        let candidate = repo
            .obvious_merge_candidate("dupe")
            .unwrap()
            .expect("duplicate should be detected");
        assert_eq!(candidate.canonical_id, "canon");
        repo.merge_duplicate_into_canonical(
            &candidate.canonical_id,
            &candidate.duplicate_id,
            candidate.score,
        )
        .unwrap();

        let merged_canonical = repo.get_knowledge("canon").unwrap().unwrap();
        assert_eq!(merged_canonical.source_refs.len(), 2);
        let archived = repo.get_knowledge("dupe").unwrap().unwrap();
        assert_eq!(archived.state, KnowledgeState::Superseded);
        assert_eq!(archived.tier, fieldcraft_core::KnowledgeTier::Archive);
        assert_eq!(archived.superseded_by.as_deref(), Some("canon"));

        let events: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id = 'canon'
                   AND source_knowledge_id = 'dupe'
                   AND event_type = 'duplicate_merged'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
    }

    #[test]
    fn obvious_duplicate_rejects_low_cosine_distinct_lesson() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut canonical = make_record("canon", "proj-a", KnowledgeKind::Claim);
        canonical.title =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors".to_string();
        canonical.retrieval_text =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors in records body"
                .to_string();
        repo.insert_knowledge(&canonical).unwrap();
        repo.store_dense_vector("canon", &[1.0, 0.0, 0.0]).unwrap();

        let mut distinct = make_record("distinct", "proj-a", KnowledgeKind::Claim);
        distinct.title =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors".to_string();
        distinct.retrieval_text =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors in records body"
                .to_string();
        repo.insert_knowledge(&distinct).unwrap();
        repo.store_dense_vector("distinct", &[0.0, 1.0, 0.0])
            .unwrap();

        let candidate = repo.obvious_merge_candidate("distinct").unwrap();
        assert!(
            candidate.is_none(),
            "low embedding cosine must prevent a merge even with matching text and cues"
        );
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let result = repo.get_knowledge("no-such-id").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_with_kind_filter() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("c1", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.insert_knowledge(&make_record("p1", "proj", KnowledgeKind::Procedure))
            .unwrap();
        repo.insert_knowledge(&make_record("e1", "proj", KnowledgeKind::Episode))
            .unwrap();

        let claims = repo
            .list_knowledge("proj", Some(KnowledgeKind::Claim), None)
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].kind, KnowledgeKind::Claim);

        let all = repo.list_knowledge("proj", None, None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn list_with_state_filter() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("c1", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.update_state("c1", KnowledgeState::Active).unwrap();

        let provisional = repo
            .list_knowledge("proj", None, Some(KnowledgeState::Provisional))
            .unwrap();
        assert_eq!(provisional.len(), 0);

        let active = repo
            .list_knowledge("proj", None, Some(KnowledgeState::Active))
            .unwrap();
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn update_state_transition() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("rec-1", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.update_state("rec-1", KnowledgeState::Active).unwrap();
        let loaded = repo.get_knowledge("rec-1").unwrap().unwrap();
        assert_eq!(loaded.state, KnowledgeState::Active);

        repo.update_state("rec-1", KnowledgeState::Superseded)
            .unwrap();
        let loaded2 = repo.get_knowledge("rec-1").unwrap().unwrap();
        assert_eq!(loaded2.state, KnowledgeState::Superseded);
    }

    #[test]
    fn update_superseded_by() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("old", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.insert_knowledge(&make_record("new", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.update_superseded_by("old", "new").unwrap();
        let loaded = repo.get_knowledge("old").unwrap().unwrap();
        assert_eq!(loaded.superseded_by.as_deref(), Some("new"));
    }

    #[test]
    fn insert_relation_and_get() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_relation("rel-1", "from-1", "to-1", "supersedes")
            .unwrap();
        let rels = repo.get_relations_from("from-1").unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].0, "to-1");
        assert_eq!(rels[0].1, "supersedes");
    }

    #[test]
    fn active_neighbors_filter_inactive_targets_and_order_edge_types() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("from", "proj", KnowledgeKind::Procedure))
            .unwrap();
        repo.insert_knowledge(&make_record("warn", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.insert_knowledge(&make_record("requires", "proj", KnowledgeKind::Procedure))
            .unwrap();
        repo.insert_knowledge(&make_record("inactive", "proj", KnowledgeKind::Procedure))
            .unwrap();
        repo.update_state("from", KnowledgeState::Active).unwrap();
        repo.update_state("warn", KnowledgeState::Active).unwrap();
        repo.update_state("requires", KnowledgeState::Active)
            .unwrap();

        repo.insert_relation_with_metadata("r1", "from", "requires", "REQUIRES", 0.9, None)
            .unwrap();
        repo.insert_relation_with_metadata("r2", "from", "warn", "WARNS_ABOUT", 0.7, None)
            .unwrap();
        repo.insert_relation_with_metadata("r3", "from", "inactive", "DEMONSTRATES", 1.0, None)
            .unwrap();

        let neighbors = repo
            .get_active_neighbors("from", &["WARNS_ABOUT", "REQUIRES", "DEMONSTRATES"], 5)
            .unwrap();
        assert_eq!(neighbors.len(), 2);
        assert_eq!(neighbors[0].id, "warn");
        assert_eq!(neighbors[0].relation_kind, "WARNS_ABOUT");
        assert_eq!(neighbors[1].id, "requires");
    }

    #[test]
    fn count_by_project() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("r1", "proj-a", KnowledgeKind::Claim))
            .unwrap();
        repo.insert_knowledge(&make_record("r2", "proj-a", KnowledgeKind::Procedure))
            .unwrap();
        repo.insert_knowledge(&make_record("r3", "proj-b", KnowledgeKind::Episode))
            .unwrap();
        assert_eq!(repo.count_by_project("proj-a").unwrap(), 2);
        assert_eq!(repo.count_by_project("proj-b").unwrap(), 1);
        assert_eq!(repo.count_by_project("proj-c").unwrap(), 0);
    }

    #[test]
    fn delete_knowledge() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("del-me", "proj", KnowledgeKind::Claim))
            .unwrap();
        assert!(repo.get_knowledge("del-me").unwrap().is_some());
        repo.delete_knowledge("del-me").unwrap();
        assert!(repo.get_knowledge("del-me").unwrap().is_none());
    }

    #[test]
    fn store_and_load_dense_vector_roundtrip() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("vec-1", "proj", KnowledgeKind::Claim))
            .unwrap();

        let vector = vec![1.0_f32, 2.5, -3.0, 0.0, 42.42];
        repo.store_dense_vector("vec-1", &vector).unwrap();

        let loaded = repo
            .load_dense_vector("vec-1")
            .unwrap()
            .expect("should have vector");
        assert_eq!(loaded, vector);
    }

    #[test]
    fn load_dense_vector_returns_none_when_not_set() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("no-vec", "proj", KnowledgeKind::Claim))
            .unwrap();
        let loaded = repo.load_dense_vector("no-vec").unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn load_all_vectors_only_returns_records_with_vectors() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("v1", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.insert_knowledge(&make_record("v2", "proj", KnowledgeKind::Procedure))
            .unwrap();
        repo.insert_knowledge(&make_record("v3", "proj", KnowledgeKind::Episode))
            .unwrap();

        // Only store vectors for v1 and v3
        repo.store_dense_vector("v1", &[1.0, 2.0]).unwrap();
        repo.store_dense_vector("v3", &[3.0, 4.0]).unwrap();

        let results = repo.load_all_vectors("proj", "active").unwrap();
        assert_eq!(results.len(), 2);
        let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
        assert!(ids.contains(&"v1"));
        assert!(ids.contains(&"v3"));
    }

    #[test]
    fn load_all_vectors_filters_by_state() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        repo.insert_knowledge(&make_record("s1", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.insert_knowledge(&make_record("s2", "proj", KnowledgeKind::Claim))
            .unwrap();
        repo.store_dense_vector("s1", &[1.0]).unwrap();
        repo.store_dense_vector("s2", &[2.0]).unwrap();

        // Retire s2 — should be excluded
        repo.update_state("s2", KnowledgeState::Retired).unwrap();

        let results = repo.load_all_vectors("proj", "active").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "s1");
    }

    #[test]
    fn load_all_vectors_tier_filter() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut active_rec = make_record("tier-active", "proj", KnowledgeKind::Claim);
        active_rec.state = KnowledgeState::Active;
        active_rec.tier = fieldcraft_core::KnowledgeTier::Active;
        repo.insert_knowledge(&active_rec).unwrap();
        repo.store_dense_vector("tier-active", &[1.0, 0.0]).unwrap();

        let mut archive_rec = make_record("tier-archive", "proj", KnowledgeKind::Claim);
        archive_rec.state = KnowledgeState::Active;
        archive_rec.tier = fieldcraft_core::KnowledgeTier::Archive;
        repo.insert_knowledge(&archive_rec).unwrap();
        repo.store_dense_vector("tier-archive", &[0.0, 1.0])
            .unwrap();

        // Default 'active' query should return only the Active record
        let active_results = repo.load_all_vectors("proj", "active").unwrap();
        let active_ids: Vec<&str> = active_results.iter().map(|r| r.0.as_str()).collect();
        assert!(
            active_ids.contains(&"tier-active"),
            "active tier query should return active record"
        );
        assert!(
            !active_ids.contains(&"tier-archive"),
            "active tier query must NOT return archive record"
        );

        // Explicit 'archive' query should return only the Archive record
        let archive_results = repo.load_all_vectors("proj", "archive").unwrap();
        let archive_ids: Vec<&str> = archive_results.iter().map(|r| r.0.as_str()).collect();
        assert!(
            !archive_ids.contains(&"tier-active"),
            "archive tier query must NOT return active record"
        );
        assert!(
            archive_ids.contains(&"tier-archive"),
            "archive tier query should return archive record"
        );
    }

    #[test]
    fn fts_search_tier_filter() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut active_rec = make_record("fts-tier-active", "proj", KnowledgeKind::Claim);
        active_rec.retrieval_text = "tierfilterkeyword active record".to_string();
        active_rec.state = KnowledgeState::Active;
        active_rec.tier = fieldcraft_core::KnowledgeTier::Active;
        repo.insert_knowledge(&active_rec).unwrap();

        let mut archive_rec = make_record("fts-tier-archive", "proj", KnowledgeKind::Claim);
        archive_rec.retrieval_text = "tierfilterkeyword archive record".to_string();
        archive_rec.state = KnowledgeState::Active;
        archive_rec.tier = fieldcraft_core::KnowledgeTier::Archive;
        repo.insert_knowledge(&archive_rec).unwrap();

        // Default 'active' tier query must not return archive record
        let active_results = repo
            .fts_search("proj", "tierfilterkeyword", 10, "active")
            .unwrap();
        let active_ids: Vec<&str> = active_results.iter().map(|r| r.0.as_str()).collect();
        assert!(active_ids.contains(&"fts-tier-active"));
        assert!(
            !active_ids.contains(&"fts-tier-archive"),
            "active tier FTS must not return archive"
        );

        // 'archive' tier query should only return archive record
        let archive_results = repo
            .fts_search("proj", "tierfilterkeyword", 10, "archive")
            .unwrap();
        let archive_ids: Vec<&str> = archive_results.iter().map(|r| r.0.as_str()).collect();
        assert!(
            !archive_ids.contains(&"fts-tier-active"),
            "archive tier FTS must not return active"
        );
        assert!(archive_ids.contains(&"fts-tier-archive"));
    }

    #[test]
    fn fts_search_unscoped_filters_inactive_states() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut active_rec = make_record("fts-unscoped-active", "proj-a", KnowledgeKind::Claim);
        active_rec.retrieval_text = "unscopedstatekeyword active record".to_string();
        active_rec.state = KnowledgeState::Active;
        repo.insert_knowledge(&active_rec).unwrap();

        let mut retired_rec = make_record("fts-unscoped-retired", "proj-b", KnowledgeKind::Claim);
        retired_rec.retrieval_text = "unscopedstatekeyword retired record".to_string();
        retired_rec.state = KnowledgeState::Retired;
        repo.insert_knowledge(&retired_rec).unwrap();

        let results = repo
            .fts_search_unscoped("unscopedstatekeyword", 10, "active")
            .unwrap();
        let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();

        assert!(ids.contains(&"fts-unscoped-active"));
        assert!(!ids.contains(&"fts-unscoped-retired"));
    }

    #[test]
    fn fts_search_returns_matching_records() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut rec1 = make_record("fts-1", "proj", KnowledgeKind::Claim);
        rec1.retrieval_text = "rust programming language systems".to_string();
        repo.insert_knowledge(&rec1).unwrap();

        let mut rec2 = make_record("fts-2", "proj", KnowledgeKind::Procedure);
        rec2.retrieval_text = "python scripting automation".to_string();
        repo.insert_knowledge(&rec2).unwrap();

        let mut rec3 = make_record("fts-3", "proj", KnowledgeKind::Claim);
        rec3.retrieval_text = "rust ownership borrowing memory safety".to_string();
        repo.insert_knowledge(&rec3).unwrap();

        let results = repo.fts_search("proj", "rust", 10, "active").unwrap();
        assert_eq!(results.len(), 2);
        let ids: Vec<&str> = results.iter().map(|r| r.0.as_str()).collect();
        assert!(ids.contains(&"fts-1"));
        assert!(ids.contains(&"fts-3"));
        // BM25 scores should be negative (lower is better)
        for (_, score) in &results {
            assert!(*score < 0.0);
        }
    }

    #[test]
    fn fts_search_respects_project_filter() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut rec1 = make_record("fp-1", "proj-a", KnowledgeKind::Claim);
        rec1.retrieval_text = "database indexing strategies".to_string();
        repo.insert_knowledge(&rec1).unwrap();

        let mut rec2 = make_record("fp-2", "proj-b", KnowledgeKind::Claim);
        rec2.retrieval_text = "database query optimization".to_string();
        repo.insert_knowledge(&rec2).unwrap();

        let results_a = repo.fts_search("proj-a", "database", 10, "active").unwrap();
        assert_eq!(results_a.len(), 1);
        assert_eq!(results_a[0].0, "fp-1");

        let results_b = repo.fts_search("proj-b", "database", 10, "active").unwrap();
        assert_eq!(results_b.len(), 1);
        assert_eq!(results_b[0].0, "fp-2");
    }

    /// `record_haystack` of every record admissible in `tier` (production reads
    /// these from the record cache).
    fn admissible_haystacks(repo: &KnowledgeRepository, tier: &str) -> HashMap<String, String> {
        let mut stmt = repo
            .connection()
            .prepare(
                "SELECT id, title, retrieval_text FROM knowledge
                 WHERE state IN ('active', 'provisional') AND (tier = ?1 OR tier IS NULL)",
            )
            .unwrap();
        stmt.query_map([tier], |row| {
            Ok((
                row.get::<_, String>(0)?,
                crate::record_index::record_haystack(
                    &row.get::<_, String>(1)?,
                    &row.get::<_, String>(2)?,
                ),
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// The legacy identity candidates as the old full scan selected them
    /// (production gets them from the record cache).
    fn legacy_identity_candidates(repo: &KnowledgeRepository, tier: &str) -> Vec<(String, String)> {
        let mut stmt = repo
            .connection()
            .prepare(
                "SELECT k.id, k.project_id FROM knowledge k
                 WHERE k.state IN ('active', 'provisional')
                   AND (k.tier = ?1 OR k.tier IS NULL)
                   AND NOT EXISTS (
                     SELECT 1 FROM cue_index c
                     WHERE c.record_id = k.id AND c.dim_name = 'identity'
                   )
                 ORDER BY k.state, k.rowid",
            )
            .unwrap();
        stmt.query_map([tier], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn identity_cue_search_admits_legacy_rows_by_project_id() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        for (id, project) in [("legacy-a", "field_notes"), ("legacy-b", "elsewhere")] {
            let mut rec = make_record(id, project, KnowledgeKind::Claim);
            rec.state = KnowledgeState::Active;
            rec.retrieval_text = "meeting notes about budget".to_string();
            repo.insert_knowledge(&rec).unwrap();
        }
        repo.connection()
            .execute("DELETE FROM cue_index WHERE dim_name = 'identity'", [])
            .unwrap();
        let candidates = legacy_identity_candidates(&repo, "active");
        let haystacks = admissible_haystacks(&repo, "active");
        assert_eq!(candidates.len(), 2);
        let results = repo
            .identity_cue_search(
                &["field_notes".to_string()],
                &["budget".to_string()],
                10,
                &candidates,
                &|id| haystacks.get(id).map(String::as_str),
            )
            .unwrap();
        assert_eq!(results, vec![("legacy-a".to_string(), 31.0)]);
    }

    #[test]
    fn identity_cue_search_treats_project_names_as_fuzzy_cues() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut ingredient =
            make_record("ingredient-memory", "ohapotato_ios", KnowledgeKind::Claim);
        ingredient.state = KnowledgeState::Active;
        ingredient.retrieval_text = "Ingredient display strings must route recipe detail, edit, meal plan missing ingredients, shopping, and fridge surfaces through QuantityDisplayFormatter using canonicalName and extractedIngredient fallbacks.".to_string();
        repo.insert_knowledge(&ingredient).unwrap();

        let mut unrelated =
            make_record("same-project-other", "ohapotato_ios", KnowledgeKind::Claim);
        unrelated.state = KnowledgeState::Active;
        unrelated.retrieval_text =
            "Notification permission onboarding uses a delayed prompt.".to_string();
        repo.insert_knowledge(&unrelated).unwrap();

        let mut other_project = make_record("other-project", "fieldcraft", KnowledgeKind::Claim);
        other_project.state = KnowledgeState::Active;
        other_project.retrieval_text =
            "Ingredient display strings appear in a different application.".to_string();
        repo.insert_knowledge(&other_project).unwrap();

        let haystacks = admissible_haystacks(&repo, "active");
        let results = repo
            .identity_cue_search(
                &["opotato".to_string()],
                &[
                    "ingredient".to_string(),
                    "display".to_string(),
                    "strings".to_string(),
                    "surface".to_string(),
                ],
                10,
                &legacy_identity_candidates(&repo, "active"),
                &|id| haystacks.get(id).map(String::as_str),
            )
            .unwrap();
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();

        assert_eq!(ids.first().copied(), Some("ingredient-memory"));
        assert!(ids.contains(&"same-project-other"));
        assert!(!ids.contains(&"other-project"));
    }

    #[test]
    fn fts_sync_on_delete() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut rec = make_record("del-fts", "proj", KnowledgeKind::Claim);
        rec.retrieval_text = "unique searchable content here".to_string();
        repo.insert_knowledge(&rec).unwrap();

        // Verify it appears in FTS
        let results = repo
            .fts_search("proj", "unique searchable", 10, "active")
            .unwrap();
        assert_eq!(results.len(), 1);

        // Delete and verify it disappears from FTS
        repo.delete_knowledge("del-fts").unwrap();
        let results = repo
            .fts_search("proj", "unique searchable", 10, "active")
            .unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn fts_sync_on_update() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let mut rec = make_record("upd-fts", "proj", KnowledgeKind::Claim);
        rec.retrieval_text = "original content here".to_string();
        repo.insert_knowledge(&rec).unwrap();

        // Update via upsert with new retrieval_text
        rec.retrieval_text = "modified content now".to_string();
        repo.insert_knowledge(&rec).unwrap();

        // Old text should not match
        let old_results = repo.fts_search("proj", "original", 10, "active").unwrap();
        assert_eq!(old_results.len(), 0);

        // New text should match
        let new_results = repo.fts_search("proj", "modified", 10, "active").unwrap();
        assert_eq!(new_results.len(), 1);
        assert_eq!(new_results[0].0, "upd-fts");
    }

    // -------------------------------------------------------------------------
    // Write-time cue_bundle population (Phase 1 of cue-bundle retrieval)
    // -------------------------------------------------------------------------

    #[test]
    fn insert_populates_cue_bundle_column() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("cb-1", "proj", KnowledgeKind::Directive);
        rec.title = "Use cargo test before commit".to_string();
        rec.payload_json = r#"{"action":"run cargo test","reason":"catch regressions","files":["crates/fieldcraft-core/src/lib.rs"],"trigger":"pre-commit","avoid":[],"verify":null,"invalid_if":null,"retrieval_queries":[]}"#.to_string();
        rec.retrieval_text =
            "Use cargo test before commit | scope: crates/fieldcraft-core/src/lib.rs".to_string();

        repo.insert_knowledge(&rec).unwrap();

        let bundle_json: Option<String> = repo
            .conn
            .query_row(
                "SELECT cue_bundle FROM knowledge WHERE id = 'cb-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let bundle_json = bundle_json.expect("cue_bundle should be populated on insert");
        // Domain rewritten from `crates/fieldcraft-core/...` → `fieldcraft_core`.
        assert!(
            bundle_json.contains("fieldcraft_core"),
            "bundle should contain derived domain: {bundle_json}"
        );
        assert!(
            bundle_json.contains("cargo"),
            "bundle should detect cargo tool: {bundle_json}"
        );
        assert!(
            bundle_json.contains("\"dimension\":\"identity\""),
            "bundle should contain typed identity cue: {bundle_json}"
        );
    }

    #[test]
    fn insert_populates_cue_index_rows() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("ci-1", "proj", KnowledgeKind::Procedure);
        rec.title = "Run cargo test".to_string();
        rec.payload_json = r#"{"goal":"verify cargo build","preconditions":[],"steps":["cargo test"],"failure_modes":[],"affected_files":["crates/fieldcraft-retrieval/src/retrieval.rs"],"retrieval_questions":[],"trigger":null,"invalid_if":null}"#.to_string();
        rec.retrieval_text =
            "Run cargo test | scope: crates/fieldcraft-retrieval/src/retrieval.rs".to_string();

        repo.insert_knowledge(&rec).unwrap();

        let total: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index WHERE record_id = 'ci-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(total > 0, "cue_index should have rows for the new record");

        let domain_count: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index \
                 WHERE record_id = 'ci-1' AND dim_name = 'domain'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(domain_count, 1, "exactly one domain row expected");

        let identity_count: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index \
                 WHERE record_id = 'ci-1' AND dim_name = 'identity' AND dim_value = 'proj'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            identity_count, 1,
            "project_id should be indexed as identity cue"
        );
    }

    // -------------------------------------------------------------------------
    // Feedback / dynamic_confidence (Phase 6 of cue-bundle retrieval)
    // -------------------------------------------------------------------------

    #[test]
    fn compute_outcome_update_confirmed_raises_asymptotically() {
        // From 0.5, confirmed steps toward 1.0: 0.5 + 0.05*(1-0.5) = 0.525
        let after = compute_outcome_update(0.5, FeedbackOutcome::Confirmed);
        assert!((after - 0.525).abs() < 1e-5, "got {after}");
    }

    #[test]
    fn compute_outcome_update_ignored_decays_geometrically() {
        // From 0.5, ignored steps toward 0: 0.5 - 0.08*0.5 = 0.46
        let after = compute_outcome_update(0.5, FeedbackOutcome::Ignored);
        assert!((after - 0.46).abs() < 1e-5, "got {after}");
    }

    #[test]
    fn compute_outcome_update_contradicted_decays_harder() {
        let after = compute_outcome_update(0.5, FeedbackOutcome::Contradicted);
        assert!((after - 0.44).abs() < 1e-5, "got {after}");
    }

    #[test]
    fn compute_outcome_update_respects_floor_and_cap() {
        // Floor: many ignored events can't push below 0.05
        let mut c = 0.1_f32;
        for _ in 0..100 {
            c = compute_outcome_update(c, FeedbackOutcome::Ignored);
        }
        assert!((c - 0.05).abs() < 1e-5, "floor not respected: {c}");

        // Cap: many confirmed events can't push above 0.95
        let mut c = 0.5_f32;
        for _ in 0..1000 {
            c = compute_outcome_update(c, FeedbackOutcome::Confirmed);
        }
        assert!((c - 0.95).abs() < 1e-5, "cap not respected: {c}");
    }

    #[test]
    fn apply_feedback_outcome_writes_event_and_updates_dynamic_confidence() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("fb-1", "proj", KnowledgeKind::Claim);
        rec.confidence = 0.5;
        repo.insert_knowledge(&rec).unwrap();

        let (before, after) = repo
            .apply_feedback_outcome(
                "fb-1",
                FeedbackOutcome::Confirmed,
                Some("sess-1"),
                "test",
                None,
            )
            .unwrap();
        assert!((before - 0.5).abs() < 1e-5);
        assert!((after - 0.525).abs() < 1e-5);

        // dynamic_confidence persisted on the record
        let stored: f32 = repo
            .conn
            .query_row(
                "SELECT dynamic_confidence FROM knowledge WHERE id = 'fb-1'",
                [],
                |row| row.get::<_, f64>(0).map(|x| x as f32),
            )
            .unwrap();
        assert!((stored - after).abs() < 1e-5);

        // Event row appended
        let count: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM lesson_outcome_events WHERE knowledge_id = 'fb-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn closed_loop_stats_counts_feedback_and_query_cue_bundle_rows() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("stats-1", "proj", KnowledgeKind::Claim);
        rec.confidence = 0.5;
        repo.insert_knowledge(&rec).unwrap();
        repo.apply_feedback_outcome(
            "stats-1",
            FeedbackOutcome::Confirmed,
            Some("sess-stats"),
            "test",
            None,
        )
        .unwrap();
        repo.log_retrieval_entry(&RetrievalLogEntry {
            project_id: "proj",
            query_text: "with cue",
            result_ids: &["stats-1".to_string()],
            result_scores: &[1.0],
            latency_ms: 1,
            query_cue_bundle: r#"{"domains":["rust"]}"#,
            ..Default::default()
        })
        .unwrap();
        repo.conn
            .execute(
                "INSERT INTO retrieval_log
                 (project_id, query_text, result_ids, result_scores, result_count, latency_ms, created_at)
                 VALUES ('proj', 'legacy no cue', '[\"stats-1\"]', '[1.0]', 1, 1, ?1)",
                params![Utc::now().to_rfc3339()],
            )
            .unwrap();

        let stats = repo.closed_loop_stats(7).unwrap();
        assert_eq!(stats.confirmed_recent, 1);
        assert_eq!(stats.ignored_recent, 0);
        assert_eq!(stats.contradicted_recent, 0);
        assert_eq!(stats.dynamic_confidence_non_null, 1);
        assert_eq!(stats.retrieval_log_with_query_cue_bundle, 1);
        assert_eq!(stats.retrieval_log_without_query_cue_bundle, 1);
    }

    #[test]
    fn affected_files_for_knowledge_ids_reads_payload_derived_files() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("files-1", "proj", KnowledgeKind::Claim);
        rec.payload_json =
            r#"{"statement":"touches storage","affected_files":["crates/a/src/lib.rs"]}"#
                .to_string();
        repo.insert_knowledge(&rec).unwrap();

        let files = repo
            .affected_files_for_knowledge_ids(&["files-1".to_string()])
            .unwrap();
        assert_eq!(
            files.get("files-1").unwrap(),
            &vec!["crates/a/src/lib.rs".to_string()]
        );
    }

    #[test]
    fn apply_feedback_outcome_chains_off_dynamic_confidence() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("fb-2", "proj", KnowledgeKind::Claim);
        rec.confidence = 0.5;
        repo.insert_knowledge(&rec).unwrap();

        let (_, after1) = repo
            .apply_feedback_outcome("fb-2", FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();
        let (before2, _) = repo
            .apply_feedback_outcome("fb-2", FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();
        // Second confirm reads dynamic from first, not static.
        assert!(
            (before2 - after1).abs() < 1e-5,
            "second call should chain off dynamic={after1}, got before={before2}"
        );
    }

    #[test]
    fn apply_feedback_outcome_missing_record_errors() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let result =
            repo.apply_feedback_outcome("no-such", FeedbackOutcome::Confirmed, None, "test", None);
        assert!(result.is_err(), "expected NotFound, got {result:?}");
    }

    #[test]
    fn load_retrieval_signals_returns_cue_bundle_and_dynamic_confidence() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("sig-1", "proj", KnowledgeKind::Directive);
        rec.confidence = 0.5;
        repo.insert_knowledge(&rec).unwrap();
        repo.apply_feedback_outcome("sig-1", FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();

        let signals = repo
            .load_retrieval_signals_for(&["sig-1".to_string()])
            .unwrap();
        // cue_bundle was written by insert_knowledge.
        assert!(signals.cue_bundles.contains_key("sig-1"));
        // effective confidence = dynamic (0.525), not static (0.5).
        let conf = signals.effective_confidence.get("sig-1").copied().unwrap();
        assert!(
            (conf - 0.525).abs() < 1e-5,
            "expected dynamic 0.525, got {conf}"
        );
    }

    #[test]
    fn re_insert_refreshes_cue_index() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut rec = make_record("ref-1", "proj", KnowledgeKind::Claim);
        rec.payload_json = r#"{"statement":"first","affected_files":["a/b.rs"]}"#.to_string();
        rec.retrieval_text = "first | scope: a/b.rs".to_string();
        repo.insert_knowledge(&rec).unwrap();

        let count_first: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index WHERE record_id = 'ref-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Re-insert with different content (ON CONFLICT UPDATE).
        rec.payload_json =
            r#"{"statement":"second","affected_files":["x/y.rs","x/z.rs"]}"#.to_string();
        rec.retrieval_text = "second | scope: x/y.rs, x/z.rs".to_string();
        repo.insert_knowledge(&rec).unwrap();

        // No stale rows from the first insert should remain.
        let stale_a: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index \
                 WHERE record_id = 'ref-1' AND dim_value = 'a/b.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stale_a, 0,
            "stale active_files row from first insert must be gone"
        );

        let count_second: i64 = repo
            .conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index WHERE record_id = 'ref-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            count_second > 0,
            "re-insert should leave the record indexed (first={count_first}, second={count_second})"
        );
    }

    // -----------------------------------------------------------------
    // Memory Lifecycle L1 tests.
    // -----------------------------------------------------------------

    #[test]
    fn protected_column_migration_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        // open_catalog runs the migration once on creation; run it again
        // explicitly to assert a second run is a no-op, not an error.
        let conn = crate::schema::open_catalog(&path).expect("first open");
        let result = crate::schema::run_migration(
            &conn,
            "ALTER TABLE knowledge ADD COLUMN protected INTEGER NOT NULL DEFAULT 0",
        );
        assert!(
            result.is_ok(),
            "re-running the protected migration must succeed: {:?}",
            result
        );
        drop(conn);
        // And opening the catalog a second time (full migration sequence
        // re-run) must also succeed.
        let reopened = KnowledgeRepository::open(&path);
        assert!(reopened.is_ok(), "second open_catalog must succeed");
    }

    #[test]
    fn is_protected_true_for_explicit_pin() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut record = make_record("pin-1", "proj-a", KnowledgeKind::Claim);
        record.confidence = 0.5; // below the static-confidence threshold
        repo.insert_knowledge(&record).unwrap();

        assert!(!repo.is_protected("pin-1").unwrap(), "not protected yet");
        repo.set_protected("pin-1", true, "manually pinned by user")
            .unwrap();
        assert!(
            repo.is_protected("pin-1").unwrap(),
            "explicit pin must protect"
        );

        let event_type: String = repo
            .conn
            .query_row(
                "SELECT event_type FROM knowledge_events WHERE knowledge_id = 'pin-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_type, "protected_set");
    }

    #[test]
    fn is_protected_true_for_directive_and_context_kinds() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut directive = make_record("dir-1", "proj-a", KnowledgeKind::Directive);
        directive.confidence = 0.3;
        repo.insert_knowledge(&directive).unwrap();
        let mut context = make_record("ctx-1", "proj-a", KnowledgeKind::Context);
        context.confidence = 0.3;
        repo.insert_knowledge(&context).unwrap();

        assert!(
            repo.is_protected("dir-1").unwrap(),
            "directive is rare/protected"
        );
        assert!(
            repo.is_protected("ctx-1").unwrap(),
            "context is rare/protected"
        );
    }

    #[test]
    fn is_protected_true_for_high_static_confidence() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut record = make_record("hi-conf-1", "proj-a", KnowledgeKind::Claim);
        record.confidence = 0.9;
        repo.insert_knowledge(&record).unwrap();

        assert!(
            repo.is_protected("hi-conf-1").unwrap(),
            "confidence >= 0.9 must protect"
        );
    }

    #[test]
    fn is_protected_false_for_ordinary_record() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut record = make_record("ordinary-1", "proj-a", KnowledgeKind::Procedure);
        record.confidence = 0.6;
        repo.insert_knowledge(&record).unwrap();

        assert!(
            !repo.is_protected("ordinary-1").unwrap(),
            "ordinary claim/procedure below 0.9 confidence, unpinned, must not be protected"
        );
    }

    #[test]
    fn served_activation_none_for_never_served_id() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let record = make_record("never-served", "proj-a", KnowledgeKind::Claim);
        repo.insert_knowledge(&record).unwrap();

        // A retrieval happened, but it never served this id.
        repo.log_retrieval_entry(&RetrievalLogEntry {
            project_id: "proj-a",
            query_text: "unrelated query",
            result_ids: &["some-other-id".to_string()],
            result_scores: &[0.9],
            ..Default::default()
        })
        .unwrap();

        let activation = repo.served_activation("never-served").unwrap();
        assert_eq!(
            activation, None,
            "an id that never appears in any retrieval_log.result_ids must be the eviction sentinel None"
        );
    }

    #[test]
    fn served_activation_finite_for_served_id() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let record = make_record("served-1", "proj-a", KnowledgeKind::Claim);
        repo.insert_knowledge(&record).unwrap();

        repo.log_retrieval_entry(&RetrievalLogEntry {
            project_id: "proj-a",
            query_text: "a relevant query",
            result_ids: &["served-1".to_string()],
            result_scores: &[0.9],
            ..Default::default()
        })
        .unwrap();

        let activation = repo
            .served_activation("served-1")
            .unwrap()
            .expect("served id must yield Some");
        assert!(activation.is_finite(), "served_activation must be finite");
        // A single very-recent appearance: days_since clamps to >= 1.0, so
        // the inner term is <= 1.0, and ln(<=1.0) <= 0. Just assert it's a
        // real, bounded number rather than pin an exact value to avoid
        // coupling the test to wall-clock timing.
        assert!(
            activation <= 0.0001,
            "single near-zero-day-old appearance should give ln(~1.0) ~= 0, got {activation}"
        );
    }

    #[test]
    fn served_activation_sums_multiple_appearances() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let record = make_record("served-multi", "proj-a", KnowledgeKind::Claim);
        repo.insert_knowledge(&record).unwrap();

        // Two separate retrievals both serve this id.
        for _ in 0..2 {
            repo.log_retrieval_entry(&RetrievalLogEntry {
                project_id: "proj-a",
                query_text: "repeated query",
                result_ids: &["served-multi".to_string()],
                result_scores: &[0.9],
                ..Default::default()
            })
            .unwrap();
        }

        let single = {
            let dir2 = tempdir().unwrap();
            let repo2 = KnowledgeRepository::open(&dir2.path().join("memory.sqlite3")).unwrap();
            let record2 = make_record("served-once", "proj-a", KnowledgeKind::Claim);
            repo2.insert_knowledge(&record2).unwrap();
            repo2
                .log_retrieval_entry(&RetrievalLogEntry {
                    project_id: "proj-a",
                    query_text: "repeated query",
                    result_ids: &["served-once".to_string()],
                    result_scores: &[0.9],
                    ..Default::default()
                })
                .unwrap();
            repo2.served_activation("served-once").unwrap().unwrap()
        };

        let multi = repo.served_activation("served-multi").unwrap().unwrap();
        assert!(
            multi > single,
            "two appearances must sum to a larger activation than one: multi={multi} single={single}"
        );
    }

    #[test]
    fn archive_then_restore_round_trips_losslessly() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let record = make_record("arch-1", "proj-a", KnowledgeKind::Episode);
        repo.insert_knowledge(&record).unwrap();

        let before = repo.get_knowledge("arch-1").unwrap().expect("exists");
        assert_eq!(before.tier, fieldcraft_core::KnowledgeTier::Active);

        repo.archive_record("arch-1", "never served, age > 60d")
            .unwrap();
        let archived = repo.get_knowledge("arch-1").unwrap().expect("still exists");
        assert_eq!(archived.tier, fieldcraft_core::KnowledgeTier::Archive);
        // Non-destructive: every other field is untouched.
        assert_eq!(archived.title, before.title);
        assert_eq!(archived.payload_json, before.payload_json);
        assert_eq!(archived.state, before.state);
        assert_eq!(archived.confidence, before.confidence);

        repo.restore_record("arch-1", "resurrection: matched live query >= 0.93")
            .unwrap();
        let restored = repo.get_knowledge("arch-1").unwrap().expect("still exists");
        assert_eq!(restored.tier, fieldcraft_core::KnowledgeTier::Active);
        assert_eq!(restored.title, before.title);
        assert_eq!(restored.payload_json, before.payload_json);
        assert_eq!(restored.state, before.state);
        assert_eq!(restored.confidence, before.confidence);

        let event_types: Vec<String> = {
            let mut stmt = repo
                .conn
                .prepare(
                    "SELECT event_type FROM knowledge_events \
                     WHERE knowledge_id = 'arch-1' ORDER BY id ASC",
                )
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(
            event_types,
            vec!["archived".to_string(), "restored".to_string()],
            "both archive and restore must leave an audit row, in order"
        );
    }

    #[test]
    fn archive_and_restore_error_on_unknown_id() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        assert!(repo.archive_record("nope", "test").is_err());
        assert!(repo.restore_record("nope", "test").is_err());
        assert!(repo.is_protected("nope").is_err());
        assert!(repo.set_protected("nope", true, "test").is_err());
    }

    // Memory Lifecycle L2 tests.

    #[test]
    fn archive_candidates_respects_conjunctive_predicate() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();

        let insert =
            |id: &str, kind: KnowledgeKind, age_days: i64, confidence: f32, protected: bool| {
                let mut r = make_record(id, "proj", kind);
                r.created_at = Utc::now() - chrono::Duration::days(age_days);
                r.confidence = confidence;
                repo.insert_knowledge(&r).unwrap();
                if protected {
                    repo.set_protected(id, true, "test pin").unwrap();
                }
            };
        let outcome = |id: &str, outcome: &str, n: usize| {
            for _ in 0..n {
                repo.connection()
                    .execute(
                        "INSERT INTO lesson_outcome_events
                         (knowledge_id, session_id, outcome, source, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![id, "sess", outcome, "test", Utc::now().to_rfc3339()],
                    )
                    .unwrap();
            }
        };
        let serve = |id: &str| {
            repo.log_retrieval_entry(&RetrievalLogEntry {
                project_id: "proj",
                query_text: "q",
                result_ids: &[id.to_string()],
                result_scores: &[0.9],
                ..Default::default()
            })
            .unwrap();
        };

        insert("ghost-1", KnowledgeKind::Claim, 90, 0.5, false); // the one true ghost
        insert("pin-old", KnowledgeKind::Claim, 90, 0.5, true); // protected: explicit pin
        insert("dir-old", KnowledgeKind::Directive, 90, 0.5, false); // protected: kind
        insert("hiconf-old", KnowledgeKind::Claim, 90, 0.95, false); // protected: confidence
        insert("confirmed-old", KnowledgeKind::Claim, 90, 0.5, false);
        outcome("confirmed-old", "confirmed", 1); // confirmed -> not a ghost
        insert("served-old", KnowledgeKind::Claim, 90, 0.5, false);
        serve("served-old"); // served -> not a ghost
        insert("young-1", KnowledgeKind::Claim, 5, 0.5, false); // too young
        insert("contra-1", KnowledgeKind::Claim, 5, 0.5, false);
        outcome("contra-1", "contradicted", 3);
        outcome("contra-1", "confirmed", 1); // 3 contradicted > 1 confirmed -> loser
        insert("contra-dir", KnowledgeKind::Directive, 5, 0.5, false);
        outcome("contra-dir", "contradicted", 4); // contradicted but protected -> excluded

        let candidates = repo.archive_candidates(60).unwrap();
        let mut got: Vec<(String, &str)> = candidates
            .iter()
            .map(|c| (c.id.clone(), c.category))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("contra-1".to_string(), "contradicted_loser"),
                ("ghost-1".to_string(), "ghost"),
            ],
            "only the unprotected ghost and contradicted-loser are candidates"
        );
    }

    #[test]
    fn archive_candidates_apply_is_nondestructive_and_reversible() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut ghost = make_record("ghost-x", "proj", KnowledgeKind::Claim);
        ghost.created_at = Utc::now() - chrono::Duration::days(120);
        repo.insert_knowledge(&ghost).unwrap();
        let keep = make_record("keep-x", "proj", KnowledgeKind::Claim); // young -> not a candidate
        repo.insert_knowledge(&keep).unwrap();

        let before: i64 = repo
            .connection()
            .query_row("SELECT COUNT(*) FROM knowledge", [], |r| r.get(0))
            .unwrap();

        let candidates = repo.archive_candidates(60).unwrap();
        assert_eq!(candidates.len(), 1);
        for c in &candidates {
            repo.archive_record(&c.id, &c.reason).unwrap();
        }

        let after: i64 = repo
            .connection()
            .query_row("SELECT COUNT(*) FROM knowledge", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "archival must never delete rows");
        assert_eq!(
            repo.get_knowledge("ghost-x").unwrap().unwrap().tier,
            fieldcraft_core::KnowledgeTier::Archive
        );
        assert_eq!(
            repo.get_knowledge("keep-x").unwrap().unwrap().tier,
            fieldcraft_core::KnowledgeTier::Active
        );
        repo.restore_record("ghost-x", "test restore").unwrap();
        assert_eq!(
            repo.get_knowledge("ghost-x").unwrap().unwrap().tier,
            fieldcraft_core::KnowledgeTier::Active
        );
        let n_events: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id='ghost-x' AND event_type IN ('archived','restored')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_events, 2);
    }

    // Memory Lifecycle L3 (resurrection) + L4 (retrieval-induced forgetting) tests.

    #[test]
    fn note_archive_match_restores_after_threshold() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let r = make_record("res-1", "proj", KnowledgeKind::Claim);
        repo.insert_knowledge(&r).unwrap();
        repo.archive_record("res-1", "test archive").unwrap();

        assert!(!repo.note_archive_match("res-1", 3).unwrap());
        assert!(!repo.note_archive_match("res-1", 3).unwrap());
        assert_eq!(
            repo.get_knowledge("res-1").unwrap().unwrap().tier,
            fieldcraft_core::KnowledgeTier::Archive,
            "must stay archived below threshold"
        );
        assert!(
            repo.note_archive_match("res-1", 3).unwrap(),
            "third match restores"
        );
        assert_eq!(
            repo.get_knowledge("res-1").unwrap().unwrap().tier,
            fieldcraft_core::KnowledgeTier::Active
        );
        // Idempotent: further matches on the now-active record do not error or re-restore.
        assert!(!repo.note_archive_match("res-1", 3).unwrap());
    }

    #[test]
    fn superseded_archive_never_restores_from_resurrection_or_manual_restore() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let canonical = make_record("current-1", "proj", KnowledgeKind::Claim);
        repo.insert_knowledge(&canonical).unwrap();
        let old = make_record("old-1", "proj", KnowledgeKind::Claim);
        repo.insert_knowledge(&old).unwrap();
        repo.connection()
            .execute(
                "UPDATE knowledge
                 SET state = 'superseded', tier = 'archive', superseded_by = 'current-1'
                 WHERE id = 'old-1'",
                [],
            )
            .unwrap();

        assert!(!repo.note_archive_match("old-1", 1).unwrap());
        let err = repo
            .restore_record("old-1", "manual restore of superseded record")
            .unwrap_err();
        assert!(matches!(err, TrError::Conflict { .. }));

        let old = repo.get_knowledge("old-1").unwrap().unwrap();
        assert_eq!(old.state, KnowledgeState::Superseded);
        assert_eq!(old.tier, fieldcraft_core::KnowledgeTier::Archive);
        assert_eq!(old.superseded_by.as_deref(), Some("current-1"));
        let archive_matches: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id = 'old-1' AND event_type = 'archive_match'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archive_matches, 0);
    }

    #[test]
    fn rif_demotion_picks_the_confirmed_loser_of_a_near_twin_pair() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let a = make_record("twin-a", "proj", KnowledgeKind::Claim);
        let b = make_record("twin-b", "proj", KnowledgeKind::Claim);
        repo.insert_knowledge(&a).unwrap();
        repo.insert_knowledge(&b).unwrap();
        // cosine([1,0],[0.88,0.475]) ~= 0.88, inside [0.85, 0.92): near-twin, not a merge dup.
        repo.store_dense_vector("twin-a", &[1.0, 0.0]).unwrap();
        repo.store_dense_vector("twin-b", &[0.88, 0.475]).unwrap();

        for _ in 0..3 {
            repo.log_retrieval_entry(&RetrievalLogEntry {
                project_id: "proj",
                query_text: "q",
                result_ids: &["twin-a".to_string(), "twin-b".to_string()],
                result_scores: &[0.9, 0.8],
                ..Default::default()
            })
            .unwrap();
        }
        // twin-a wins the competition: 2 confirmations vs twin-b's 0.
        for _ in 0..2 {
            repo.connection()
                .execute(
                    "INSERT INTO lesson_outcome_events
                     (knowledge_id, session_id, outcome, source, created_at)
                     VALUES ('twin-a', 's', 'confirmed', 'test', ?1)",
                    rusqlite::params![Utc::now().to_rfc3339()],
                )
                .unwrap();
        }

        let cands = repo.rif_demotion_candidates(3).unwrap();
        assert_eq!(cands.len(), 1, "exactly the loser is demoted");
        assert_eq!(cands[0].id, "twin-b");
        assert_eq!(cands[0].category, "rif_loser");
    }

    // Memory Lifecycle L5 (dream tier) tests.

    #[test]
    fn dream_clusters_finds_confirmed_related_group() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        // Three related records (pairwise cosine ~0.85-0.90, same project/kind)
        // plus one unrelated one.
        // Angles 0deg/30deg/60deg: adjacent pairs land in the dream band
        // (cos 30deg ~= 0.866, in [0.80, 0.92)) and union-find chains all
        // three into one cluster; dr-far points the other way entirely.
        let vectors: [(&str, [f32; 2]); 4] = [
            ("dr-1", [1.0, 0.0]),
            ("dr-2", [0.866, 0.5]),
            ("dr-3", [0.5, 0.866]),
            ("dr-far", [-1.0, 0.0]),
        ];
        for (id, v) in &vectors {
            let mut r = make_record(id, "proj", KnowledgeKind::Claim);
            r.state = KnowledgeState::Active;
            repo.insert_knowledge(&r).unwrap();
            repo.store_dense_vector(id, v).unwrap();
        }
        for _ in 0..2 {
            repo.connection()
                .execute(
                    "INSERT INTO lesson_outcome_events
                     (knowledge_id, session_id, outcome, source, created_at)
                     VALUES ('dr-1', 's', 'confirmed', 'test', ?1)",
                    rusqlite::params![Utc::now().to_rfc3339()],
                )
                .unwrap();
        }
        let clusters = repo.dream_clusters(3, 2, 5).unwrap();
        assert_eq!(clusters.len(), 1, "one qualifying cluster");
        let mut ids = clusters[0].clone();
        ids.sort();
        assert_eq!(ids, vec!["dr-1", "dr-2", "dr-3"]);
    }

    fn insert_active_dream_vector(repo: &KnowledgeRepository, id: &str, vector: &[f32]) {
        let mut record = make_record(id, "proj", KnowledgeKind::Claim);
        record.state = KnowledgeState::Active;
        repo.insert_knowledge(&record).unwrap();
        repo.store_dense_vector(id, vector).unwrap();
    }

    fn seed_confirmed_outcomes(repo: &KnowledgeRepository, id: &str, count: usize) {
        for _ in 0..count {
            repo.connection()
                .execute(
                    "INSERT INTO lesson_outcome_events
                     (knowledge_id, session_id, outcome, source, created_at)
                     VALUES (?1, 's', 'confirmed', 'test', ?2)",
                    rusqlite::params![id, Utc::now().to_rfc3339()],
                )
                .unwrap();
        }
    }

    fn insert_bridge_tail_fixture(repo: &KnowledgeRepository) {
        let alpha = 0.85_f32;
        let a = alpha.sqrt();
        let b = (1.0 - alpha).sqrt();
        let tail = (1.0 - 0.7225_f32).sqrt();

        let mut v1 = [0.0_f32; 7];
        v1[0] = a;
        v1[1] = b;
        let mut v2 = [0.0_f32; 7];
        v2[0] = a;
        v2[2] = b;
        let mut v3 = [0.0_f32; 7];
        v3[0] = a;
        v3[3] = b;
        let mut v4 = [0.0_f32; 7];
        v4[0] = a;
        v4[4] = b;

        let mut w1 = [0.0_f32; 7];
        for i in 0..7 {
            w1[i] = alpha * v1[i];
        }
        w1[5] = tail;

        let mut w2 = [0.0_f32; 7];
        for i in 0..7 {
            w2[i] = alpha * w1[i];
        }
        w2[6] = tail;

        for (id, vector) in [
            ("v1", v1),
            ("v2", v2),
            ("v3", v3),
            ("v4", v4),
            ("w1", w1),
            ("w2", w2),
        ] {
            insert_active_dream_vector(repo, id, &vector);
        }
        seed_confirmed_outcomes(repo, "v1", 2);
    }

    #[test]
    fn dream_clusters_excludes_bridge_tail_from_clique() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        insert_bridge_tail_fixture(&repo);

        let clusters = repo.dream_clusters(3, 2, 5).unwrap();
        assert_eq!(clusters.len(), 1, "one qualifying dense clique");
        let mut ids = clusters[0].clone();
        ids.sort();
        assert_eq!(ids, vec!["v1", "v2", "v3", "v4"]);
        assert!(!ids.contains(&"w1".to_string()));
        assert!(!ids.contains(&"w2".to_string()));
    }

    #[test]
    fn dream_clusters_does_not_chain_across_path() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        for (id, degrees) in [
            ("path-0", 0.0_f32),
            ("path-32", 32.0),
            ("path-64", 64.0),
            ("path-96", 96.0),
            ("path-128", 128.0),
        ] {
            let radians = degrees.to_radians();
            insert_active_dream_vector(&repo, id, &[radians.cos(), radians.sin()]);
        }
        seed_confirmed_outcomes(&repo, "path-0", 2);

        let clusters = repo.dream_clusters(3, 2, 5).unwrap();
        assert!(clusters.iter().all(|cluster| cluster.len() < 4));
        assert!(clusters.iter().all(|cluster| {
            !(cluster.contains(&"path-0".to_string()) && cluster.contains(&"path-128".to_string()))
        }));
    }

    #[test]
    fn dream_clusters_is_deterministic_for_bridge_tail_fixture() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        insert_bridge_tail_fixture(&repo);

        let first = repo.dream_clusters(3, 2, 5).unwrap();
        let second = repo.dream_clusters(3, 2, 5).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn derived_claim_requires_provenance_and_lands_provisional() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        for id in ["s-1", "s-2", "s-3"] {
            let r = make_record(id, "proj", KnowledgeKind::Claim);
            repo.insert_knowledge(&r).unwrap();
        }
        // Guardrail: fewer than 3 sources is rejected.
        assert!(
            repo.insert_derived_claim(&["s-1".to_string(), "s-2".to_string()], "proj", "t", "s")
                .is_err()
        );
        let sources: Vec<String> = ["s-1", "s-2", "s-3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let id = repo
            .insert_derived_claim(
                &sources,
                "proj",
                "General claim",
                "The abstracted statement.",
            )
            .unwrap();
        let rec = repo.get_knowledge(&id).unwrap().unwrap();
        assert_eq!(rec.state, KnowledgeState::Provisional, "lands provisional");
        assert!(rec.payload_json.contains("derived_from"));
        let prov: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id = ?1 AND event_type = 'derived_from'",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(prov, 3, "one provenance audit row per source");
        // Originals retained untouched.
        assert_eq!(
            repo.get_knowledge("s-1").unwrap().unwrap().tier,
            fieldcraft_core::KnowledgeTier::Active
        );
    }

    #[test]
    fn derived_claim_rejects_pathologically_large_source_sets() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let accepted_sources: Vec<String> = (0..DERIVED_CLAIM_MAX_SOURCES)
            .map(|i| format!("accepted-{i}"))
            .collect();
        let accepted = repo.insert_derived_claim(
            &accepted_sources,
            "proj",
            "Accepted source cap",
            "Twenty-four sources is the allowed upper bound.",
        );
        assert!(accepted.is_ok(), "24 sources must remain valid");

        let rejected_sources: Vec<String> = (0..=DERIVED_CLAIM_MAX_SOURCES)
            .map(|i| format!("rejected-{i}"))
            .collect();
        let err = repo
            .insert_derived_claim(
                &rejected_sources,
                "proj",
                "Rejected source cap",
                "Twenty-five sources is pathological.",
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("expected 3..=24 source lessons"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn dream_cluster_already_dreamed_ignores_archived_derived_claim() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let sources = insert_active_claim_sources(&repo, &["shadow-1", "shadow-2", "shadow-3"]);
        let derived_id = repo
            .insert_derived_claim(
                &sources,
                "proj",
                "Archived dream",
                "Archived derived claims must not shadow new dreams.",
            )
            .unwrap();

        assert!(repo.dream_cluster_already_dreamed(&sources).unwrap());
        repo.archive_record(&derived_id, "manual archive of pathological blob")
            .unwrap();
        assert!(
            !repo.dream_cluster_already_dreamed(&sources).unwrap(),
            "archived derived claims must not shadow clusters"
        );
    }

    #[test]
    fn dream_cluster_already_dreamed_does_not_pool_minority_overlaps() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let cluster = insert_active_claim_sources(
            &repo,
            &[
                "minor-1", "minor-2", "minor-3", "minor-4", "minor-5", "minor-6", "minor-7",
            ],
        );
        let first: Vec<String> = cluster[0..3].to_vec();
        let second: Vec<String> = cluster[3..6].to_vec();

        repo.insert_derived_claim(
            &first,
            "proj",
            "First minority dream",
            "Three out of seven is not a majority.",
        )
        .unwrap();
        repo.insert_derived_claim(
            &second,
            "proj",
            "Second minority dream",
            "Another three out of seven must not be pooled.",
        )
        .unwrap();

        assert!(
            !repo.dream_cluster_already_dreamed(&cluster).unwrap(),
            "two unrelated minority overlaps must not sum into a skip"
        );
    }

    #[test]
    fn dream_cluster_already_dreamed_requires_one_live_majority_overlap() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let cluster = insert_active_claim_sources(
            &repo,
            &["major-1", "major-2", "major-3", "major-4", "major-5"],
        );
        let majority: Vec<String> = cluster[0..3].to_vec();

        repo.insert_derived_claim(
            &majority,
            "proj",
            "Majority dream",
            "Three out of five is a majority.",
        )
        .unwrap();

        assert!(
            repo.dream_cluster_already_dreamed(&cluster).unwrap(),
            "one live derived claim covering a majority must shadow the cluster"
        );
    }

    #[test]
    fn confirmed_outcome_promotes_provisional_to_active() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let r = make_record("prov-1", "proj", KnowledgeKind::Claim); // make_record is Provisional
        repo.insert_knowledge(&r).unwrap();
        repo.apply_feedback_outcome("prov-1", FeedbackOutcome::Ignored, None, "test", None)
            .unwrap();
        assert_eq!(
            repo.get_knowledge("prov-1").unwrap().unwrap().state,
            KnowledgeState::Provisional,
            "non-confirmed outcomes never promote"
        );
        repo.apply_feedback_outcome("prov-1", FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();
        assert_eq!(
            repo.get_knowledge("prov-1").unwrap().unwrap().state,
            KnowledgeState::Active,
            "a real confirm promotes provisional to active"
        );
    }

    #[test]
    fn confirming_provisional_derived_claim_demotes_sources() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let sources = insert_active_claim_sources(&repo, &["src-1", "src-2", "src-3"]);
        let derived_id = repo
            .insert_derived_claim(
                &sources,
                "proj",
                "General claim",
                "The abstracted statement.",
            )
            .unwrap();

        repo.apply_feedback_outcome(
            &derived_id,
            FeedbackOutcome::Confirmed,
            Some("sess-derived"),
            "test",
            None,
        )
        .unwrap();

        assert_eq!(
            repo.get_knowledge(&derived_id).unwrap().unwrap().state,
            KnowledgeState::Active
        );
        for source_id in &sources {
            let source = repo.get_knowledge(source_id).unwrap().unwrap();
            assert_eq!(source.state, KnowledgeState::Superseded);
            assert_eq!(source.tier, fieldcraft_core::KnowledgeTier::Archive);
            assert_eq!(source.superseded_by.as_deref(), Some(derived_id.as_str()));
        }
        assert_eq!(source_demoted_count(&repo, &derived_id), 3);
        let audit_rows: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id = ?1
                   AND event_type = 'source_demoted'
                   AND source_knowledge_id IN ('src-1', 'src-2', 'src-3')
                   AND source_refs_json = '[]'
                   AND note LIKE '%demote-on-promotion%'",
                rusqlite::params![derived_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(audit_rows, 3);
    }

    #[test]
    fn confirming_derived_claim_skips_protected_sources() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let sources =
            insert_active_claim_sources(&repo, &["prot-src-1", "prot-src-2", "prot-src-3"]);
        repo.set_protected("prot-src-2", true, "explicit retention pin")
            .unwrap();
        let derived_id = repo
            .insert_derived_claim(
                &sources,
                "proj",
                "Protected source claim",
                "Protected source test.",
            )
            .unwrap();

        repo.apply_feedback_outcome(&derived_id, FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();

        let protected = repo.get_knowledge("prot-src-2").unwrap().unwrap();
        assert_eq!(protected.state, KnowledgeState::Active);
        assert_eq!(protected.tier, fieldcraft_core::KnowledgeTier::Active);
        assert_eq!(protected.superseded_by, None);
        for source_id in ["prot-src-1", "prot-src-3"] {
            let source = repo.get_knowledge(source_id).unwrap().unwrap();
            assert_eq!(source.state, KnowledgeState::Superseded);
            assert_eq!(source.tier, fieldcraft_core::KnowledgeTier::Archive);
            assert_eq!(source.superseded_by.as_deref(), Some(derived_id.as_str()));
        }
        assert_eq!(source_demoted_count(&repo, &derived_id), 2);
    }

    #[test]
    fn confirming_derived_claim_skips_already_superseded_or_archived_sources() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let sources =
            insert_active_claim_sources(&repo, &["skip-src-1", "skip-src-2", "skip-src-3"]);
        repo.connection()
            .execute(
                "UPDATE knowledge SET state = 'superseded' WHERE id = 'skip-src-1'",
                [],
            )
            .unwrap();
        repo.archive_record("skip-src-2", "already archived before promotion")
            .unwrap();
        let derived_id = repo
            .insert_derived_claim(
                &sources,
                "proj",
                "Skip source claim",
                "Already skipped source test.",
            )
            .unwrap();

        repo.apply_feedback_outcome(&derived_id, FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();

        let superseded = repo.get_knowledge("skip-src-1").unwrap().unwrap();
        assert_eq!(superseded.state, KnowledgeState::Superseded);
        assert_eq!(superseded.tier, fieldcraft_core::KnowledgeTier::Active);
        assert_eq!(superseded.superseded_by, None);

        let archived = repo.get_knowledge("skip-src-2").unwrap().unwrap();
        assert_eq!(archived.state, KnowledgeState::Active);
        assert_eq!(archived.tier, fieldcraft_core::KnowledgeTier::Archive);
        assert_eq!(archived.superseded_by, None);

        let demoted = repo.get_knowledge("skip-src-3").unwrap().unwrap();
        assert_eq!(demoted.state, KnowledgeState::Superseded);
        assert_eq!(demoted.tier, fieldcraft_core::KnowledgeTier::Archive);
        assert_eq!(demoted.superseded_by.as_deref(), Some(derived_id.as_str()));
        assert_eq!(source_demoted_count(&repo, &derived_id), 1);
    }

    #[test]
    fn second_confirm_on_promoted_derived_claim_does_not_duplicate_source_demotions() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let sources =
            insert_active_claim_sources(&repo, &["once-src-1", "once-src-2", "once-src-3"]);
        let derived_id = repo
            .insert_derived_claim(
                &sources,
                "proj",
                "One-shot claim",
                "One-shot demotion test.",
            )
            .unwrap();

        repo.apply_feedback_outcome(&derived_id, FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();
        repo.apply_feedback_outcome(&derived_id, FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();

        assert_eq!(source_demoted_count(&repo, &derived_id), 3);
    }

    #[test]
    fn confirming_non_derived_provisional_record_demotes_nothing() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let sources =
            insert_active_claim_sources(&repo, &["plain-src-1", "plain-src-2", "plain-src-3"]);
        let plain = make_record("plain-prov", "proj", KnowledgeKind::Claim);
        repo.insert_knowledge(&plain).unwrap();

        repo.apply_feedback_outcome("plain-prov", FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();

        let source_demotions: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events WHERE event_type = 'source_demoted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(source_demotions, 0);
        for source_id in &sources {
            let source = repo.get_knowledge(source_id).unwrap().unwrap();
            assert_eq!(source.state, KnowledgeState::Active);
            assert_eq!(source.tier, fieldcraft_core::KnowledgeTier::Active);
            assert_eq!(source.superseded_by, None);
        }
    }

    #[test]
    fn demoted_source_cannot_restore_to_active_service() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let sources = insert_active_claim_sources(
            &repo,
            &["restore-src-1", "restore-src-2", "restore-src-3"],
        );
        let derived_id = repo
            .insert_derived_claim(
                &sources,
                "proj",
                "Restorable claim",
                "Restorable demotion test.",
            )
            .unwrap();
        repo.apply_feedback_outcome(&derived_id, FeedbackOutcome::Confirmed, None, "test", None)
            .unwrap();

        let err = repo
            .restore_record("restore-src-1", "manual restore after demote-on-promotion")
            .unwrap_err();
        assert!(matches!(err, TrError::Conflict { .. }));

        let restored = repo.get_knowledge("restore-src-1").unwrap().unwrap();
        assert_eq!(restored.state, KnowledgeState::Superseded);
        assert_eq!(restored.tier, fieldcraft_core::KnowledgeTier::Archive);
        assert_eq!(restored.superseded_by.as_deref(), Some(derived_id.as_str()));
    }
}
