#![recursion_limit = "256"]

use chrono::Utc;
use fieldcraft_core::{
    AgentTool, ExtractionBatchRecord, ImportStatusSummary, LearningSourceKind,
    LearningSourceRecord, LearningSourceSeed, LearningSourceStatus, LearningTraversalPolicy,
    SourceSessionRecord, SourceSessionStatus, SourceTranscriptEntry, TrError,
};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use walkdir::WalkDir;

pub mod cue_backfill;
pub mod import;
pub mod leiden;
pub mod maintenance;
pub mod memories;
pub mod promote;
pub mod record_index;
pub mod repository;
pub mod schema;
pub mod skills;

pub use cue_backfill::{BackfillStats, backfill_all};
pub use import::{ImportRecord, ImportStore};
pub use maintenance::{
    AnalysisReport, LessonFinding, LifecycleCandidate, LifecycleReport, ProjectStats,
    ReportSummary, SuggestedAction, analyze_report, lifecycle_report,
    render_analysis_report_stdout, render_lifecycle_report_stdout,
};
pub use memories::{
    ActionFilter, ActionRow, Memory, MemoryError, MemoryInput, MemoryPatch, MemoryRepository,
    Person, Project, RebuildSummary, Shape,
};
pub use promote::{
    DEFAULT_MAX_CONTRADICTED, DEFAULT_MIN_CONFIRM_RATE, DEFAULT_MIN_CONFIRMED,
    DEFAULT_PROMOTE_KINDS, OutcomeCounts, PROMOTE_EVENT_TYPE, PROMOTED_SKILLS_DIR,
    PromoteCandidate, PromoteProvenance, PromoteReport, PromoteRequest, PromoteTarget,
    PromoteThresholds, eligibility_reason, list_promote_candidates, load_promote_candidate,
    render_promote_report, run_promote,
};
pub use record_index::{DenseHitRow, RecordIndexRow, record_haystack};
pub use repository::{
    ArchiveCandidate, ClosedLoopStats, DERIVED_CLAIM_MAX_SOURCES, DERIVED_CLAIM_MIN_SOURCES,
    FeedbackOutcome, KnowledgeNeighbor, KnowledgeRepository, RetrievalLogEntry, RetrievalSignals,
};
pub use schema::{
    SCHEMA_VERSION, canonical_project_id_from_existing, fieldcraft_db_path, open_catalog,
    project_slug_match_key,
};
pub use skills::{
    SkillCardRecord, SkillFindingRecord, SkillPackageRecord, SkillRepository, SkillScanReport,
};

pub const SOURCE_SESSIONS_DB_FILE: &str = ".tr-source-sessions.sqlite3";
pub const SESSION_EXTRACTOR_VERSION: &str = "codex-session-extractor-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverSessionsReport {
    pub scanned: usize,
    pub discovered: usize,
    pub queued: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

pub fn compute_checksum(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

static SLUGIFY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());

pub fn slugify(value: &str) -> String {
    let lower = value.to_lowercase();
    let replaced = SLUGIFY_RE.replace_all(&lower, "-");
    let trimmed = replaced.trim_matches('-');
    if trimmed.is_empty() {
        "lesson".into()
    } else {
        trimmed.to_string()
    }
}

pub fn source_sessions_catalog_path(shared_root: &Path) -> PathBuf {
    shared_root.join(SOURCE_SESSIONS_DB_FILE)
}

pub fn open_source_sessions_catalog(shared_root: &Path) -> Result<Connection, TrError> {
    fs::create_dir_all(shared_root).map_err(|e| TrError::Storage {
        message: e.to_string(),
    })?;
    let conn = Connection::open(source_sessions_catalog_path(shared_root)).map_err(sqlite_error)?;
    // WAL mode + busy_timeout for concurrent access resilience.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .map_err(sqlite_error)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_sessions (
            source_id TEXT PRIMARY KEY,
            source_tool TEXT NOT NULL,
            source_path TEXT NOT NULL,
            session_id TEXT NOT NULL,
            project TEXT NOT NULL,
            machine TEXT NOT NULL,
            cwd TEXT,
            title TEXT NOT NULL,
            started_at TEXT,
            updated_at TEXT,
            discovered_at TEXT NOT NULL,
            source_fingerprint TEXT NOT NULL,
            transcript_json TEXT NOT NULL,
            status TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            extractor_version TEXT NOT NULL,
            last_error TEXT,
            last_extracted_at TEXT
        );
        CREATE INDEX IF NOT EXISTS source_sessions_project_idx ON source_sessions(project);
        CREATE INDEX IF NOT EXISTS source_sessions_status_idx ON source_sessions(status);
        CREATE TABLE IF NOT EXISTS learning_sources (
            source_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            uri TEXT NOT NULL,
            project TEXT NOT NULL,
            title TEXT NOT NULL,
            scope_hint TEXT,
            traversal_policy_json TEXT NOT NULL,
            status TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            claimed_at TEXT,
            finished_at TEXT,
            source_hash TEXT,
            derived_record_count INTEGER NOT NULL DEFAULT 0,
            worker_id TEXT,
            last_error TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS learning_sources_project_uri_idx ON learning_sources(project, uri);
        CREATE INDEX IF NOT EXISTS learning_sources_status_idx ON learning_sources(status);
        CREATE INDEX IF NOT EXISTS learning_sources_project_idx ON learning_sources(project);
        CREATE TABLE IF NOT EXISTS extraction_batches (
            batch_id           TEXT PRIMARY KEY,
            created_at         TEXT NOT NULL,
            status             TEXT NOT NULL,
            session_ids_json   TEXT NOT NULL,
            input_file_id      TEXT,
            output_file_id     TEXT,
            error_file_id      TEXT,
            total_requests     INTEGER NOT NULL,
            reconciled_at      TEXT,
            extractor_version  TEXT NOT NULL,
            last_polled_at     TEXT,
            poll_error         TEXT
        );
        CREATE INDEX IF NOT EXISTS extraction_batches_status_idx ON extraction_batches(status);",
    )
    .map_err(sqlite_error)?;
    // Migration: add extraction_batch_id column to source_sessions (idempotent).
    use crate::schema::run_migration;
    run_migration(
        &conn,
        "ALTER TABLE source_sessions ADD COLUMN extraction_batch_id TEXT",
    )?;
    merge_known_source_project_slug_split(&conn)?;
    // Promote source_fingerprint index to UNIQUE (with safe pre-dedup).
    promote_fingerprint_unique_index(&conn)?;
    Ok(conn)
}

fn merge_known_source_project_slug_split(conn: &Connection) -> Result<(), TrError> {
    use crate::schema::run_migration;
    run_migration(
        conn,
        "UPDATE source_sessions SET project = 'field_notes' WHERE project = 'field-notes';
         DELETE FROM learning_sources
          WHERE project = 'field-notes'
            AND EXISTS (
              SELECT 1 FROM learning_sources target
              WHERE target.project = 'field_notes'
                AND target.uri = learning_sources.uri
            );
         UPDATE learning_sources SET project = 'field_notes' WHERE project = 'field-notes';",
    )
}

/// Promotes `source_fingerprint` to a UNIQUE index.
/// Before creating the index, deletes higher-rowid duplicates keeping the lowest rowid.
/// Idempotent: if the unique index already exists, returns Ok immediately.
pub(crate) fn promote_fingerprint_unique_index(conn: &Connection) -> Result<(), TrError> {
    // Check if unique index already exists — if so, nothing to do.
    let already_unique: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='source_sessions_fingerprint_idx'
               AND sql LIKE '%UNIQUE%'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if already_unique > 0 {
        return Ok(());
    }

    // Drop the old non-unique index if it exists.
    conn.execute_batch("DROP INDEX IF EXISTS source_sessions_fingerprint_idx;")
        .map_err(sqlite_error)?;

    // Remove higher-rowid duplicates, keeping the lowest rowid per fingerprint.
    let removed: usize = conn
        .execute(
            "DELETE FROM source_sessions
             WHERE rowid NOT IN (
                 SELECT MIN(rowid) FROM source_sessions GROUP BY source_fingerprint
             )",
            [],
        )
        .map_err(sqlite_error)?;
    if removed > 0 {
        tracing::info!(
            "promote_fingerprint_unique_index: removed {} duplicate source_sessions rows",
            removed
        );
    }

    // Now create the unique index.
    conn.execute_batch(
        "CREATE UNIQUE INDEX source_sessions_fingerprint_idx ON source_sessions(source_fingerprint);",
    )
    .map_err(sqlite_error)?;

    Ok(())
}

pub fn list_source_session_ids(
    shared_root: &Path,
    project: Option<&str>,
) -> Result<Vec<String>, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let base_query = "SELECT source_id FROM source_sessions";
    let query = if project.is_some() {
        format!("{base_query} WHERE project = ?1 ORDER BY discovered_at DESC")
    } else {
        format!("{base_query} ORDER BY discovered_at DESC")
    };
    let mut stmt = conn.prepare(&query).map_err(sqlite_error)?;
    if let Some(project) = project {
        let rows = stmt
            .query_map([project], |row| row.get(0))
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(sqlite_error)
    } else {
        let rows = stmt.query_map([], |row| row.get(0)).map_err(sqlite_error)?;
        rows.collect::<Result<Vec<String>, _>>()
            .map_err(sqlite_error)
    }
}

pub fn get_source_session(
    shared_root: &Path,
    source_id: &str,
) -> Result<Option<SourceSessionRecord>, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let mut stmt = conn
        .prepare(
            "SELECT source_id, source_tool, source_path, session_id, project, machine, cwd, title, started_at, updated_at, discovered_at, source_fingerprint, transcript_json, status, attempts, extractor_version, last_error, last_extracted_at, extraction_batch_id
             FROM source_sessions WHERE source_id = ?1",
        )
        .map_err(sqlite_error)?;
    stmt.query_row([source_id], source_session_from_row)
        .optional()
        .map_err(sqlite_error)
}

pub fn list_source_sessions(
    shared_root: &Path,
    project: Option<&str>,
) -> Result<Vec<SourceSessionRecord>, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let base_query = "SELECT source_id, source_tool, source_path, session_id, project, machine, cwd, title, started_at, updated_at, discovered_at, source_fingerprint, transcript_json, status, attempts, extractor_version, last_error, last_extracted_at, extraction_batch_id
         FROM source_sessions";
    let query = if project.is_some() {
        format!("{base_query} WHERE project = ?1 ORDER BY discovered_at DESC")
    } else {
        format!("{base_query} ORDER BY discovered_at DESC")
    };
    let mut stmt = conn.prepare(&query).map_err(sqlite_error)?;
    if let Some(project) = project {
        let rows = stmt
            .query_map([project], source_session_from_row)
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    } else {
        let rows = stmt
            .query_map([], source_session_from_row)
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }
}

fn source_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceSessionRecord> {
    let transcript_json: String = row.get(12)?;
    let transcript =
        serde_json::from_str::<Vec<SourceTranscriptEntry>>(&transcript_json).unwrap_or_default();
    Ok(SourceSessionRecord {
        source_id: row.get(0)?,
        source_tool: parse_agent_tool(&row.get::<_, String>(1)?),
        source_path: row.get(2)?,
        session_id: row.get(3)?,
        project: row.get(4)?,
        machine: row.get(5)?,
        cwd: row.get(6)?,
        title: row.get(7)?,
        started_at: row.get(8)?,
        updated_at: row.get(9)?,
        discovered_at: row.get(10)?,
        source_fingerprint: row.get(11)?,
        transcript,
        status: parse_source_session_status(&row.get::<_, String>(13)?),
        attempts: row.get::<_, i64>(14)?.max(0) as u32,
        extractor_version: row.get(15)?,
        last_error: row.get(16)?,
        last_extracted_at: row.get(17)?,
        extraction_batch_id: row.get(18)?,
    })
}

pub fn source_session_status_summary(
    shared_root: &Path,
    project: Option<&str>,
) -> Result<ImportStatusSummary, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let mut summary = ImportStatusSummary {
        pending: 0,
        running: 0,
        succeeded: 0,
        failed: 0,
        stale: 0,
    };
    let query = if project.is_some() {
        "SELECT status, COUNT(*) FROM source_sessions WHERE project = ?1 GROUP BY status"
    } else {
        "SELECT status, COUNT(*) FROM source_sessions GROUP BY status"
    };
    let mut stmt = conn.prepare(query).map_err(sqlite_error)?;
    let pairs: Vec<(String, i64)> = if let Some(project) = project {
        let rows = stmt
            .query_map([project], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    } else {
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    for (status, count) in pairs {
        let count = count as usize;
        match parse_source_session_status(&status) {
            SourceSessionStatus::Pending => summary.pending = count,
            SourceSessionStatus::Running => summary.running = count,
            SourceSessionStatus::Succeeded => summary.succeeded = count,
            SourceSessionStatus::Failed => summary.failed = count,
            SourceSessionStatus::Stale => summary.stale = count,
        }
    }
    Ok(summary)
}

pub fn discover_local_sessions(
    shared_root: &Path,
    project: Option<&str>,
    codex_home: &Path,
    claude_home: &Path,
    extra_sources: &[PathBuf],
) -> Result<DiscoverSessionsReport, TrError> {
    let mut report = DiscoverSessionsReport {
        scanned: 0,
        discovered: 0,
        queued: 0,
        unchanged: 0,
        skipped: 0,
    };
    for base in [
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ] {
        if !base.exists() {
            continue;
        }
        for entry in WalkDir::new(&base) {
            let entry = entry.map_err(|e| TrError::Storage {
                message: e.to_string(),
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            report.scanned += 1;
            match codex_source_record(entry.path(), project)? {
                Some(record) => {
                    report.discovered += 1;
                    if upsert_source_session(shared_root, &record)? {
                        report.queued += 1;
                    } else {
                        report.unchanged += 1;
                    }
                }
                None => report.skipped += 1,
            }
        }
    }
    for record in claude_source_records(claude_home, project)? {
        report.scanned += 1;
        report.discovered += 1;
        if upsert_source_session(shared_root, &record)? {
            report.queued += 1;
        } else {
            report.unchanged += 1;
        }
    }

    // Discover from extra source paths (user-specified via --source).
    // Skip ~/.cursor — it has no JSONL sessions and is 1.4GB+.
    for dir in extra_sources {
        if !dir.exists() {
            continue;
        }
        for entry in WalkDir::new(dir).max_depth(5) {
            let entry = entry.map_err(|e| TrError::Storage {
                message: e.to_string(),
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let ext = entry.path().extension().and_then(|v| v.to_str());
            if ext != Some("jsonl") && ext != Some("json") {
                continue;
            }
            report.scanned += 1;
            // Try Codex-style parsing (JSONL with session metadata)
            if let Ok(Some(record)) = codex_source_record(entry.path(), project) {
                report.discovered += 1;
                // Override tool based on path
                let mut record = record;
                let path_str = entry.path().to_string_lossy();
                record.source_tool =
                    if path_str.contains("/.cursor/") || path_str.contains("/cursor/") {
                        AgentTool::Cursor
                    } else if path_str.contains("/.codex/") || path_str.contains("/codex/") {
                        AgentTool::Codex
                    } else {
                        AgentTool::Unknown
                    };
                if upsert_source_session(shared_root, &record)? {
                    report.queued += 1;
                } else {
                    report.unchanged += 1;
                }
            } else {
                report.skipped += 1;
            }
        }
    }

    Ok(report)
}

fn upsert_source_session(
    shared_root: &Path,
    record: &SourceSessionRecord,
) -> Result<bool, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let existing: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT source_fingerprint, status, attempts FROM source_sessions WHERE source_id = ?1",
            [&record.source_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let (status, attempts, queued) = match existing {
        None => (SourceSessionStatus::Pending, 0_i64, true),
        Some((fingerprint, existing_status, attempts)) => {
            if fingerprint != record.source_fingerprint {
                (SourceSessionStatus::Stale, 0_i64, true)
            } else if parse_source_session_status(&existing_status) == SourceSessionStatus::Failed {
                (SourceSessionStatus::Pending, attempts, true)
            } else {
                (
                    parse_source_session_status(&existing_status),
                    attempts,
                    false,
                )
            }
        }
    };
    let transcript_json =
        serde_json::to_string(&record.transcript).map_err(|e| TrError::Storage {
            message: e.to_string(),
        })?;
    conn.execute(
        "INSERT INTO source_sessions
         (source_id, source_tool, source_path, session_id, project, machine, cwd, title, started_at, updated_at, discovered_at, source_fingerprint, transcript_json, status, attempts, extractor_version, last_error, last_extracted_at, extraction_batch_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(source_id) DO UPDATE SET
           source_tool=excluded.source_tool,
           source_path=excluded.source_path,
           session_id=excluded.session_id,
           project=excluded.project,
           machine=excluded.machine,
           cwd=excluded.cwd,
           title=excluded.title,
           started_at=excluded.started_at,
           updated_at=excluded.updated_at,
           discovered_at=excluded.discovered_at,
           source_fingerprint=excluded.source_fingerprint,
           transcript_json=excluded.transcript_json,
           status=excluded.status,
           attempts=excluded.attempts,
           extractor_version=excluded.extractor_version,
           extraction_batch_id=excluded.extraction_batch_id",
        params![
            record.source_id,
            agent_tool_str(record.source_tool),
            record.source_path,
            record.session_id,
            record.project,
            record.machine,
            record.cwd,
            record.title,
            record.started_at,
            record.updated_at,
            record.discovered_at,
            record.source_fingerprint,
            transcript_json,
            source_session_status_str(status),
            attempts,
            record.extractor_version,
            record.last_error,
            record.last_extracted_at,
            record.extraction_batch_id,
        ],
    )
    .map_err(sqlite_error)?;
    Ok(queued)
}

pub fn enqueue_learning_source(
    conn: &Connection,
    seed: &LearningSourceSeed,
) -> Result<(LearningSourceRecord, bool), TrError> {
    let uri = seed.uri.trim();
    if uri.is_empty() {
        return Err(TrError::Storage {
            message: "learning source uri is empty".to_string(),
        });
    }
    let project = seed.project.as_deref().unwrap_or("global").trim();
    let project = if project.is_empty() {
        "global"
    } else {
        project
    };
    let kind = seed.kind.unwrap_or_else(|| infer_learning_source_kind(uri));
    let title = seed
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::trim)
        .unwrap_or(uri);
    let source_id = learning_source_id(project, uri);
    let now = Utc::now().to_rfc3339();
    let policy_json =
        serde_json::to_string(&seed.traversal_policy).map_err(|e| TrError::Storage {
            message: e.to_string(),
        })?;

    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT source_id, status FROM learning_sources WHERE project = ?1 AND uri = ?2",
            params![project, uri],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let status = existing
        .as_ref()
        .map(|(_, status)| LearningSourceStatus::parse(status))
        .unwrap_or(LearningSourceStatus::Pending);
    let queued = match status {
        LearningSourceStatus::Failed => true,
        LearningSourceStatus::Succeeded => false,
        _ => existing.is_none(),
    };
    let next_status = if status == LearningSourceStatus::Failed {
        LearningSourceStatus::Pending
    } else {
        status
    };

    conn.execute(
        "INSERT INTO learning_sources
         (source_id, kind, uri, project, title, scope_hint, traversal_policy_json, status,
          attempts, created_at, updated_at, claimed_at, finished_at, source_hash,
          derived_record_count, worker_id, last_error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?9, NULL, NULL, NULL, 0, NULL, NULL)
         ON CONFLICT(project, uri) DO UPDATE SET
           kind=excluded.kind,
           title=excluded.title,
           scope_hint=excluded.scope_hint,
           traversal_policy_json=excluded.traversal_policy_json,
           status=CASE WHEN learning_sources.status = 'failed' THEN 'pending' ELSE learning_sources.status END,
           updated_at=excluded.updated_at,
           last_error=CASE WHEN learning_sources.status = 'failed' THEN NULL ELSE learning_sources.last_error END",
        params![
            source_id,
            kind.as_str(),
            uri,
            project,
            title,
            seed.scope_hint.as_deref(),
            policy_json,
            next_status.as_str(),
            now,
        ],
    )
    .map_err(sqlite_error)?;

    let record = get_learning_source(
        conn,
        existing
            .as_ref()
            .map(|(id, _)| id.as_str())
            .unwrap_or(&source_id),
    )?
    .ok_or_else(|| TrError::Storage {
        message: "learning source upsert did not return a row".to_string(),
    })?;
    Ok((record, queued))
}

pub fn claim_learning_sources(
    conn: &Connection,
    worker_id: &str,
    limit: usize,
    project_filter: Option<&str>,
) -> Result<Vec<LearningSourceRecord>, TrError> {
    if limit == 0 {
        return Ok(vec![]);
    }
    let now = Utc::now().to_rfc3339();
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(sqlite_error)?;
    let result: Result<Vec<LearningSourceRecord>, TrError> = (|| {
        let select_filter = if project_filter.is_some() {
            " AND project = ?3"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE learning_sources
             SET status = 'running',
                 attempts = attempts + 1,
                 claimed_at = ?1,
                 updated_at = ?1,
                 worker_id = ?2,
                 last_error = NULL
             WHERE source_id IN (
               SELECT source_id FROM learning_sources
               WHERE status IN ('pending', 'stale'){select_filter}
               ORDER BY created_at ASC
               LIMIT {limit}
             )
             RETURNING source_id, kind, uri, project, title, scope_hint,
                       traversal_policy_json, status, attempts, created_at,
                       updated_at, claimed_at, finished_at, source_hash,
                       derived_record_count, worker_id, last_error"
        );
        let mut stmt = conn.prepare(&sql).map_err(sqlite_error)?;
        if let Some(project) = project_filter {
            let rows = stmt
                .query_map(params![now, worker_id, project], learning_source_from_row)
                .map_err(sqlite_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
        } else {
            let rows = stmt
                .query_map(params![now, worker_id], learning_source_from_row)
                .map_err(sqlite_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
        }
    })();
    match result {
        Ok(records) => {
            conn.execute_batch("COMMIT").map_err(sqlite_error)?;
            Ok(records)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

pub fn get_learning_source(
    conn: &Connection,
    source_id: &str,
) -> Result<Option<LearningSourceRecord>, TrError> {
    let mut stmt = conn
        .prepare(
            "SELECT source_id, kind, uri, project, title, scope_hint,
                    traversal_policy_json, status, attempts, created_at,
                    updated_at, claimed_at, finished_at, source_hash,
                    derived_record_count, worker_id, last_error
             FROM learning_sources WHERE source_id = ?1",
        )
        .map_err(sqlite_error)?;
    stmt.query_row([source_id], learning_source_from_row)
        .optional()
        .map_err(sqlite_error)
}

pub fn complete_learning_source(
    conn: &Connection,
    source_id: &str,
    source_hash: Option<&str>,
    derived_record_count: u32,
) -> Result<(), TrError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE learning_sources
         SET status = 'succeeded',
             updated_at = ?1,
             finished_at = ?1,
             source_hash = COALESCE(?2, source_hash),
             derived_record_count = ?3,
             worker_id = NULL,
             last_error = NULL
         WHERE source_id = ?4",
        params![now, source_hash, derived_record_count as i64, source_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub fn fail_learning_source(
    conn: &Connection,
    source_id: &str,
    error: &str,
) -> Result<(), TrError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE learning_sources
         SET status = 'failed',
             updated_at = ?1,
             finished_at = ?1,
             worker_id = NULL,
             last_error = ?2
         WHERE source_id = ?3",
        params![now, error, source_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

fn learning_source_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LearningSourceRecord> {
    let policy_json: String = row.get(6)?;
    let traversal_policy =
        serde_json::from_str::<LearningTraversalPolicy>(&policy_json).unwrap_or_default();
    Ok(LearningSourceRecord {
        source_id: row.get(0)?,
        kind: LearningSourceKind::parse(&row.get::<_, String>(1)?),
        uri: row.get(2)?,
        project: row.get(3)?,
        title: row.get(4)?,
        scope_hint: row.get(5)?,
        traversal_policy,
        status: LearningSourceStatus::parse(&row.get::<_, String>(7)?),
        attempts: row.get::<_, i64>(8)?.max(0) as u32,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        claimed_at: row.get(11)?,
        finished_at: row.get(12)?,
        source_hash: row.get(13)?,
        derived_record_count: row.get::<_, i64>(14)?.max(0) as u32,
        worker_id: row.get(15)?,
        last_error: row.get(16)?,
    })
}

fn infer_learning_source_kind(uri: &str) -> LearningSourceKind {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        LearningSourceKind::Url
    } else {
        let path = Path::new(uri);
        if path.is_dir() {
            LearningSourceKind::Directory
        } else if path.exists() {
            LearningSourceKind::File
        } else {
            LearningSourceKind::Url
        }
    }
}

fn learning_source_id(project: &str, uri: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"learning-source|");
    hasher.update(project.as_bytes());
    hasher.update(b"|");
    hasher.update(uri.as_bytes());
    format!("{:x}", hasher.finalize())[..24].to_string()
}

// ---------------------------------------------------------------------------
// US-004: extraction_batches CRUD helpers
// ---------------------------------------------------------------------------

fn extraction_batch_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExtractionBatchRecord> {
    let session_ids_json: String = row.get(3)?;
    let session_ids: Vec<String> = serde_json::from_str(&session_ids_json).unwrap_or_default();
    Ok(ExtractionBatchRecord {
        batch_id: row.get(0)?,
        created_at: row.get(1)?,
        status: row.get(2)?,
        session_ids,
        input_file_id: row.get(4)?,
        output_file_id: row.get(5)?,
        error_file_id: row.get(6)?,
        total_requests: row.get::<_, i64>(7)?.max(0) as u32,
        reconciled_at: row.get(8)?,
        extractor_version: row.get(9)?,
        last_polled_at: row.get(10)?,
        poll_error: row.get(11)?,
    })
}

pub fn insert_extraction_batch(
    shared_root: &Path,
    record: &ExtractionBatchRecord,
) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let session_ids_json =
        serde_json::to_string(&record.session_ids).map_err(|e| TrError::Storage {
            message: e.to_string(),
        })?;
    conn.execute(
        "INSERT INTO extraction_batches
         (batch_id, created_at, status, session_ids_json, input_file_id, output_file_id,
          error_file_id, total_requests, reconciled_at, extractor_version, last_polled_at, poll_error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            record.batch_id,
            record.created_at,
            record.status,
            session_ids_json,
            record.input_file_id,
            record.output_file_id,
            record.error_file_id,
            record.total_requests as i64,
            record.reconciled_at,
            record.extractor_version,
            record.last_polled_at,
            record.poll_error,
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub fn list_extraction_batches_by_status(
    shared_root: &Path,
    statuses: &[&str],
) -> Result<Vec<ExtractionBatchRecord>, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    if statuses.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = statuses
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "SELECT batch_id, created_at, status, session_ids_json, input_file_id, output_file_id,
                error_file_id, total_requests, reconciled_at, extractor_version, last_polled_at, poll_error
         FROM extraction_batches WHERE status IN ({placeholders}) ORDER BY created_at DESC"
    );
    let mut stmt = conn.prepare(&query).map_err(sqlite_error)?;
    let params_vec: Vec<&dyn rusqlite::ToSql> =
        statuses.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(params_vec.as_slice(), extraction_batch_from_row)
        .map_err(sqlite_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
}

pub fn update_extraction_batch_status(
    shared_root: &Path,
    batch_id: &str,
    new_status: &str,
    last_polled_at: Option<&str>,
    poll_error: Option<&str>,
) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    conn.execute(
        "UPDATE extraction_batches SET status = ?1, last_polled_at = ?2, poll_error = ?3
         WHERE batch_id = ?4",
        params![new_status, last_polled_at, poll_error, batch_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub fn mark_extraction_batch_reconciled(
    shared_root: &Path,
    batch_id: &str,
    reconciled_at: &str,
) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    conn.execute(
        "UPDATE extraction_batches SET status = 'reconciled', reconciled_at = ?1 WHERE batch_id = ?2",
        params![reconciled_at, batch_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub fn update_extraction_batch_file_ids(
    shared_root: &Path,
    batch_id: &str,
    input_file_id: Option<&str>,
    output_file_id: Option<&str>,
    error_file_id: Option<&str>,
) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    conn.execute(
        "UPDATE extraction_batches SET input_file_id = ?1, output_file_id = ?2, error_file_id = ?3
         WHERE batch_id = ?4",
        params![input_file_id, output_file_id, error_file_id, batch_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Atomically claim pending/stale sessions for a batch.
/// Uses BEGIN IMMEDIATE + UPDATE...RETURNING to avoid any SELECT-then-UPDATE race.
/// Returns the source_ids that were claimed.
pub fn atomic_claim_pending_sessions(
    shared_root: &Path,
    batch_id_placeholder: &str,
    limit: usize,
    project_filter: Option<&str>,
) -> Result<Vec<String>, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(sqlite_error)?;
    let result: Result<Vec<String>, TrError> = (|| {
        if let Some(project) = project_filter {
            let sql = format!(
                "UPDATE source_sessions
                   SET status = 'running', extraction_batch_id = ?1
                   WHERE source_id IN (
                     SELECT source_id FROM source_sessions
                   WHERE status IN ('pending', 'stale')
                       AND extraction_batch_id IS NULL
                       AND project = ?2
                      ORDER BY started_at DESC NULLS LAST
                      LIMIT {limit}
                   )
                   RETURNING source_id"
            );
            let mut stmt = conn.prepare(&sql).map_err(sqlite_error)?;
            let rows = stmt
                .query_map(params![batch_id_placeholder, project], |row| row.get(0))
                .map_err(sqlite_error)?;
            rows.collect::<Result<Vec<String>, _>>()
                .map_err(sqlite_error)
        } else {
            let sql = format!(
                "UPDATE source_sessions
                   SET status = 'running', extraction_batch_id = ?1
                   WHERE source_id IN (
                     SELECT source_id FROM source_sessions
                   WHERE status IN ('pending', 'stale')
                       AND extraction_batch_id IS NULL
                      ORDER BY started_at DESC NULLS LAST
                      LIMIT {limit}
                   )
                   RETURNING source_id"
            );
            let mut stmt = conn.prepare(&sql).map_err(sqlite_error)?;
            let rows = stmt
                .query_map(params![batch_id_placeholder], |row| row.get(0))
                .map_err(sqlite_error)?;
            rows.collect::<Result<Vec<String>, _>>()
                .map_err(sqlite_error)
        }
    })();
    match result {
        Ok(ids) => {
            conn.execute_batch("COMMIT").map_err(sqlite_error)?;
            Ok(ids)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

pub fn backfill_session_dates(
    knowledge_conn: &Connection,
    sessions_db_path: &Path,
) -> Result<usize, TrError> {
    knowledge_conn
        .execute_batch(&format!(
            "ATTACH DATABASE '{}' AS sessions_db;",
            sessions_db_path.to_string_lossy()
        ))
        .map_err(sqlite_error)?;
    let updated = knowledge_conn
        .execute(
            "UPDATE knowledge
         SET session_date = (
             SELECT ss.started_at
             FROM sessions_db.source_sessions ss
             WHERE json_extract(knowledge.source_json, '$.session_id') = ss.source_id
               AND ss.started_at IS NOT NULL
         )
         WHERE json_extract(knowledge.source_json, '$.session_id') IS NOT NULL
           AND (session_date IS NULL OR session_date = '')",
            [],
        )
        .map_err(sqlite_error)?;
    knowledge_conn
        .execute_batch("DETACH sessions_db;")
        .map_err(sqlite_error)?;
    Ok(updated)
}

/// Rename a batch: update BOTH source_sessions.extraction_batch_id AND
/// extraction_batches.batch_id from old_batch_id to new_batch_id in a single
/// transaction.
pub fn rename_extraction_batch_id(
    shared_root: &Path,
    old_batch_id: &str,
    new_batch_id: &str,
) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(sqlite_error)?;
    let result: Result<(), TrError> = (|| {
        conn.execute(
            "UPDATE source_sessions SET extraction_batch_id = ?1
             WHERE extraction_batch_id = ?2",
            params![new_batch_id, old_batch_id],
        )
        .map_err(sqlite_error)?;
        conn.execute(
            "UPDATE extraction_batches SET batch_id = ?1 WHERE batch_id = ?2",
            params![new_batch_id, old_batch_id],
        )
        .map_err(sqlite_error)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT").map_err(sqlite_error)?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// List the most recent `limit` extraction_batches records, ordered by created_at DESC.
/// Used by `extract-batch-status`.
pub fn list_extraction_batches_recent(
    shared_root: &Path,
    limit: usize,
) -> Result<Vec<ExtractionBatchRecord>, TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let query = format!(
        "SELECT batch_id, created_at, status, session_ids_json, input_file_id, output_file_id,
                error_file_id, total_requests, reconciled_at, extractor_version, last_polled_at, poll_error
         FROM extraction_batches ORDER BY created_at DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&query).map_err(sqlite_error)?;
    let rows = stmt
        .query_map([], extraction_batch_from_row)
        .map_err(sqlite_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
}

/// Revert a set of source_sessions to Pending and clear their extraction_batch_id.
/// Used when a batch fails, expires, or is stuck.
pub fn revert_sessions_to_pending(
    shared_root: &Path,
    session_ids: &[String],
) -> Result<(), TrError> {
    if session_ids.is_empty() {
        return Ok(());
    }
    let conn = open_source_sessions_catalog(shared_root)?;
    for session_id in session_ids {
        conn.execute(
            "UPDATE source_sessions SET status = 'pending', extraction_batch_id = NULL, last_error = NULL
             WHERE source_id = ?1",
            params![session_id],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

/// Mark a source session as Succeeded after batch reconciliation.
pub fn mark_source_session_succeeded(
    shared_root: &Path,
    source_id: &str,
    extractor_version: &str,
) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE source_sessions
         SET status = 'succeeded', extraction_batch_id = NULL,
             extractor_version = ?1, last_extracted_at = ?2, last_error = NULL
         WHERE source_id = ?3",
        params![extractor_version, now, source_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Mark a source session as Failed after terminal batch errors.
pub fn mark_source_session_failed(
    shared_root: &Path,
    source_id: &str,
    last_error: &str,
) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    conn.execute(
        "UPDATE source_sessions
         SET status = 'failed', extraction_batch_id = NULL, last_error = ?1
         WHERE source_id = ?2",
        params![last_error, source_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

fn sqlite_error(err: rusqlite::Error) -> TrError {
    TrError::Storage {
        message: err.to_string(),
    }
}

fn parse_agent_tool(value: &str) -> AgentTool {
    match value {
        "claude_code" | "claude code" | "claude" => AgentTool::ClaudeCode,
        "codex" => AgentTool::Codex,
        "cursor" => AgentTool::Cursor,
        _ => AgentTool::Unknown,
    }
}

pub(crate) fn agent_tool_str(tool: AgentTool) -> &'static str {
    match tool {
        AgentTool::ClaudeCode => "claude_code",
        AgentTool::Codex => "codex",
        AgentTool::Cursor => "cursor",
        AgentTool::Unknown => "unknown",
    }
}

/// Reset a source session's status to Pending so it can be re-extracted.
pub fn reset_source_session_to_pending(shared_root: &Path, source_id: &str) -> Result<(), TrError> {
    let conn = open_source_sessions_catalog(shared_root)?;
    conn.execute(
        "UPDATE source_sessions SET status = 'pending', last_error = NULL, attempts = 0 WHERE source_id = ?1",
        params![source_id],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

fn source_session_status_str(status: SourceSessionStatus) -> &'static str {
    match status {
        SourceSessionStatus::Pending => "pending",
        SourceSessionStatus::Running => "running",
        SourceSessionStatus::Succeeded => "succeeded",
        SourceSessionStatus::Failed => "failed",
        SourceSessionStatus::Stale => "stale",
    }
}

fn parse_source_session_status(value: &str) -> SourceSessionStatus {
    match value {
        "pending" => SourceSessionStatus::Pending,
        "running" => SourceSessionStatus::Running,
        "succeeded" => SourceSessionStatus::Succeeded,
        "failed" => SourceSessionStatus::Failed,
        "stale" => SourceSessionStatus::Stale,
        _ => SourceSessionStatus::Pending,
    }
}

fn machine_id() -> String {
    if let Ok(output) = Command::new("scutil")
        .arg("--get")
        .arg("ComputerName")
        .output()
    {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn project_slug_from_path(path: &str) -> String {
    if let Some((_, rest)) = path.split_once("/.symphony/workspaces/")
        && let Some(workspace) = rest.split('/').next()
    {
        let workspace = workspace.trim();
        if !workspace.is_empty() {
            return slugify(workspace);
        }
    }
    let path_buf = PathBuf::from(path);
    if path_buf.exists() {
        if let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(&path_buf)
            .arg("remote")
            .arg("get-url")
            .arg("origin")
            .output()
        {
            let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !remote.is_empty() {
                let repo = remote
                    .rsplit('/')
                    .next()
                    .unwrap_or(&remote)
                    .trim_end_matches(".git");
                return slugify(repo);
            }
        }
        if let Some(name) = path_buf.file_name().and_then(|value| value.to_str()) {
            return slugify(name);
        }
    }
    slugify(path)
}

fn clean_transcript_text(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let skip_prefixes = [
        "<environment_context>",
        "<permissions instructions>",
        "<app-context>",
        "<collaboration_mode>",
        "<turn_aborted>",
    ];
    if skip_prefixes
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return String::new();
    }
    if trimmed.starts_with("# AGENTS.md instructions for ") {
        return String::new();
    }
    trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(6000)
        .collect()
}

fn extract_content_text(content: &Value) -> Vec<String> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("text")
                .or_else(|| item.get("value"))
                .and_then(Value::as_str)
        })
        .map(clean_transcript_text)
        .filter(|text| !text.is_empty())
        .collect()
}

fn extract_codex_transcript_entries(obj: &Value) -> Vec<SourceTranscriptEntry> {
    let timestamp = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut entries = Vec::new();
    match obj.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = obj
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            if role != "developer" && role != "system" {
                for text in extract_content_text(obj.get("content").unwrap_or(&Value::Null)) {
                    entries.push(SourceTranscriptEntry {
                        timestamp: timestamp.clone(),
                        role: role.clone(),
                        kind: "message".into(),
                        content: text,
                    });
                }
            }
        }
        Some("response_item") => {
            let payload = obj.get("payload").unwrap_or(&Value::Null);
            match payload.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let role = payload
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    if role != "developer" && role != "system" {
                        for text in
                            extract_content_text(payload.get("content").unwrap_or(&Value::Null))
                        {
                            entries.push(SourceTranscriptEntry {
                                timestamp: timestamp.clone(),
                                role: role.clone(),
                                kind: "message".into(),
                                content: text,
                            });
                        }
                    }
                }
                Some("function_call") => {
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let args = payload
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(clean_transcript_text)
                        .unwrap_or_default();
                    entries.push(SourceTranscriptEntry {
                        timestamp: timestamp.clone(),
                        role: "tool".into(),
                        kind: "function_call".into(),
                        content: format!("{name} {args}").trim().to_string(),
                    });
                }
                Some("function_call_output") => {
                    let output = payload
                        .get("output")
                        .and_then(Value::as_str)
                        .map(clean_transcript_text)
                        .unwrap_or_default();
                    if !output.is_empty() {
                        entries.push(SourceTranscriptEntry {
                            timestamp,
                            role: "tool".into(),
                            kind: "function_call_output".into(),
                            content: output,
                        });
                    }
                }
                Some("reasoning") => {
                    let summaries = payload
                        .get("summary")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .map(clean_transcript_text)
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>();
                    if !summaries.is_empty() {
                        entries.push(SourceTranscriptEntry {
                            timestamp,
                            role: "assistant".into(),
                            kind: "reasoning_summary".into(),
                            content: summaries.join(" "),
                        });
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    entries
}

fn codex_source_record(
    path: &Path,
    project_filter: Option<&str>,
) -> Result<Option<SourceSessionRecord>, TrError> {
    let raw = fs::read_to_string(path).map_err(|e| TrError::Storage {
        message: e.to_string(),
    })?;
    let mut session_id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("codex-session")
        .to_string();
    let mut cwd: Option<String> = None;
    let mut repository_url: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut updated_at: Option<String> = None;
    let mut entries = Vec::new();

    for line in raw.lines() {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if started_at.is_none() {
            started_at = obj
                .get("timestamp")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        updated_at = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or(updated_at);
        if let Some(id) = obj.get("id").and_then(Value::as_str) {
            session_id = id.to_string();
        }
        if obj.get("type").and_then(Value::as_str) == Some("session_meta") {
            let payload = obj.get("payload").unwrap_or(&Value::Null);
            if let Some(id) = payload.get("id").and_then(Value::as_str) {
                session_id = id.to_string();
            }
            cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or(cwd);
        }
        repository_url = obj
            .get("git")
            .and_then(|git| git.get("repository_url"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or(repository_url);
        entries.extend(extract_codex_transcript_entries(&obj));
    }

    if entries.is_empty() {
        return Ok(None);
    }
    let project = cwd
        .as_deref()
        .map(project_slug_from_path)
        .or_else(|| {
            repository_url.as_deref().map(|remote| {
                let repo = remote
                    .rsplit('/')
                    .next()
                    .unwrap_or(remote)
                    .trim_end_matches(".git");
                slugify(repo)
            })
        })
        .unwrap_or_else(|| "unknown".into());
    if project_filter.is_some_and(|filter| filter != project) {
        return Ok(None);
    }
    let title = entries
        .iter()
        .find(|entry| entry.role == "user")
        .map(|entry| entry.content.chars().take(120).collect::<String>())
        .unwrap_or_else(|| format!("Imported Codex session {session_id}"));
    let fingerprint =
        compute_checksum(
            &serde_json::to_string(&entries).map_err(|e| TrError::Storage {
                message: e.to_string(),
            })?,
        );
    let source_path = path.display().to_string();
    let source_id = compute_checksum(&format!("codex|{source_path}|{session_id}"));
    Ok(Some(SourceSessionRecord {
        source_id,
        source_tool: AgentTool::Unknown,
        source_path,
        session_id,
        project,
        machine: machine_id(),
        cwd,
        title,
        started_at,
        updated_at,
        discovered_at: Utc::now().to_rfc3339(),
        source_fingerprint: fingerprint,
        transcript: entries,
        status: SourceSessionStatus::Pending,
        attempts: 0,
        extractor_version: SESSION_EXTRACTOR_VERSION.into(),
        last_error: None,
        last_extracted_at: None,
        extraction_batch_id: None,
    }))
}

fn claude_source_records(
    claude_home: &Path,
    project_filter: Option<&str>,
) -> Result<Vec<SourceSessionRecord>, TrError> {
    let history_path = claude_home.join("history.jsonl");
    if !history_path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&history_path).map_err(|e| TrError::Storage {
        message: e.to_string(),
    })?;
    let machine = machine_id();
    let mut grouped = BTreeMap::<String, Vec<Value>>::new();
    for line in raw.lines() {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let session_id = obj
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("claude-session")
            .to_string();
        grouped.entry(session_id).or_default().push(obj);
    }

    let mut records = Vec::new();
    for (session_id, mut items) in grouped {
        items.sort_by_key(|item| {
            item.get("timestamp")
                .and_then(Value::as_i64)
                .unwrap_or_default()
        });
        let project_path = items
            .iter()
            .filter_map(|item| item.get("project").and_then(Value::as_str))
            .find(|value| !value.trim().is_empty())
            .unwrap_or("");
        let project = if project_path.is_empty() {
            "unknown".into()
        } else {
            project_slug_from_path(project_path)
        };
        if project_filter.is_some_and(|filter| filter != project) {
            continue;
        }
        let transcript = items
            .iter()
            .filter_map(|item| {
                let display = item
                    .get("display")
                    .and_then(Value::as_str)
                    .map(clean_transcript_text)
                    .unwrap_or_default();
                if display.is_empty() || (display.starts_with('/') && !display.contains(' ')) {
                    return None;
                }
                let timestamp = item.get("timestamp").and_then(Value::as_i64).map(|value| {
                    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(value)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_else(|| value.to_string())
                });
                Some(SourceTranscriptEntry {
                    timestamp,
                    role: "user".into(),
                    kind: "message".into(),
                    content: display,
                })
            })
            .collect::<Vec<_>>();
        if transcript.is_empty() {
            continue;
        }
        let title = transcript
            .first()
            .map(|entry| entry.content.chars().take(120).collect::<String>())
            .unwrap_or_else(|| format!("Imported Claude session {session_id}"));
        let fingerprint = compute_checksum(&serde_json::to_string(&transcript).map_err(|e| {
            TrError::Storage {
                message: e.to_string(),
            }
        })?);
        let started_at = items
            .first()
            .and_then(|item| item.get("timestamp").and_then(Value::as_i64))
            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
            .map(|dt| dt.to_rfc3339());
        let updated_at = items
            .last()
            .and_then(|item| item.get("timestamp").and_then(Value::as_i64))
            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
            .map(|dt| dt.to_rfc3339());
        let source_path = history_path.display().to_string();
        let source_id = compute_checksum(&format!("claude|{source_path}|{session_id}"));
        records.push(SourceSessionRecord {
            source_id,
            source_tool: AgentTool::ClaudeCode,
            source_path,
            session_id,
            project,
            machine: machine.clone(),
            cwd: if project_path.is_empty() {
                None
            } else {
                Some(project_path.to_string())
            },
            title,
            started_at,
            updated_at,
            discovered_at: Utc::now().to_rfc3339(),
            source_fingerprint: fingerprint,
            transcript,
            status: SourceSessionStatus::Pending,
            attempts: 0,
            extractor_version: SESSION_EXTRACTOR_VERSION.into(),
            last_error: None,
            last_extracted_at: None,
            extraction_batch_id: None,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discover_local_sessions_registers_codex_and_claude_sources() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let codex_home = dir.path().join("codex");
        let claude_home = dir.path().join("claude");
        fs::create_dir_all(codex_home.join("sessions/2026/03/15")).unwrap();
        fs::create_dir_all(&claude_home).unwrap();
        fs::write(
            codex_home.join("sessions/2026/03/15/session.jsonl"),
            r#"{"id":"session-1","timestamp":"2026-03-15T00:00:00Z","git":{"repository_url":"https://github.com/example/ios.git"}}
{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix the meal picker dismiss bug"}]}
{"timestamp":"2026-03-15T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Check RecipeMealPlanDayPickerPopup.swift"}]}}
{"timestamp":"2026-03-15T00:00:03Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"rg -n meal picker"}}"#,
        )
        .unwrap();
        fs::write(
            claude_home.join("history.jsonl"),
            r#"{"display":"Investigate sync backlog ordering bug","timestamp":1767604440384,"project":"/tmp/web","sessionId":"claude-1"}"#,
        )
        .unwrap();

        let report =
            discover_local_sessions(&shared_root, None, &codex_home, &claude_home, &[]).unwrap();
        assert_eq!(report.discovered, 2);
        assert_eq!(report.queued, 2);
        let sessions = list_source_sessions(&shared_root, None).unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .iter()
                .any(|session| session.source_tool == AgentTool::ClaudeCode)
        );
        assert!(sessions.iter().any(|session| {
            session
                .transcript
                .iter()
                .any(|entry| entry.kind == "function_call")
        }));
    }

    #[test]
    fn project_slug_from_symphony_workspace_uses_workspace_id() {
        assert_eq!(
            project_slug_from_path("/Users/alice/Code/.symphony/workspaces/OH-104"),
            "oh-104"
        );
    }

    // -------------------------------------------------------------------------
    // US-004: UNIQUE source_fingerprint index tests
    // -------------------------------------------------------------------------

    fn insert_raw_session(conn: &Connection, source_id: &str, fingerprint: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO source_sessions
             (source_id, source_tool, source_path, session_id, project, machine,
              title, discovered_at, source_fingerprint, transcript_json,
              status, attempts, extractor_version)
             VALUES (?, 'unknown', '/path', 'sess', 'proj', 'machine',
                     'title', '2026-01-01T00:00:00Z', ?, '[]', 'pending', 0, 'v1')",
            rusqlite::params![source_id, fingerprint],
        )
        .unwrap();
    }

    #[test]
    fn promote_fingerprint_unique_index_deduplicates_rows() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        fs::create_dir_all(&shared_root).unwrap();

        // Open catalog without the promotion (raw connection)
        let conn = rusqlite::Connection::open(shared_root.join(SOURCE_SESSIONS_DB_FILE)).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS source_sessions (
                source_id TEXT PRIMARY KEY, source_tool TEXT NOT NULL,
                source_path TEXT NOT NULL, session_id TEXT NOT NULL,
                project TEXT NOT NULL, machine TEXT NOT NULL, cwd TEXT,
                title TEXT NOT NULL, started_at TEXT, updated_at TEXT,
                discovered_at TEXT NOT NULL, source_fingerprint TEXT NOT NULL,
                transcript_json TEXT NOT NULL, status TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0, extractor_version TEXT NOT NULL,
                last_error TEXT, last_extracted_at TEXT
            );",
        )
        .unwrap();

        // Insert two rows with the same fingerprint
        insert_raw_session(&conn, "id-1", "fp-duplicate");
        insert_raw_session(&conn, "id-2", "fp-duplicate");
        insert_raw_session(&conn, "id-3", "fp-unique");

        let count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM source_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_before, 3);

        // Run promotion
        promote_fingerprint_unique_index(&conn).unwrap();

        // After dedup: only the lowest rowid for the duplicate fingerprint should remain
        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM source_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_after, 2, "one duplicate should have been removed");

        // Verify unique index exists
        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index'
                 AND name='source_sessions_fingerprint_idx' AND sql LIKE '%UNIQUE%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 1, "unique index should exist");
    }

    #[test]
    fn promote_fingerprint_unique_index_is_idempotent() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        // First open creates the unique index
        let conn = open_source_sessions_catalog(&shared_root).unwrap();
        drop(conn);
        // Second open should not fail
        let _conn2 = open_source_sessions_catalog(&shared_root).unwrap();
    }

    // -------------------------------------------------------------------------
    // US-003: extraction_batch_id column on source_sessions
    // -------------------------------------------------------------------------

    fn make_test_session(source_id: &str) -> SourceSessionRecord {
        SourceSessionRecord {
            source_id: source_id.to_string(),
            source_tool: AgentTool::Codex,
            source_path: "/path/to/session.jsonl".to_string(),
            session_id: format!("sess-{source_id}"),
            project: "test-project".to_string(),
            machine: "test-machine".to_string(),
            cwd: None,
            title: "Test session".to_string(),
            started_at: None,
            updated_at: None,
            discovered_at: "2026-01-01T00:00:00Z".to_string(),
            source_fingerprint: format!("fp-{source_id}"),
            transcript: vec![],
            status: SourceSessionStatus::Pending,
            attempts: 0,
            extractor_version: "v1".to_string(),
            last_error: None,
            last_extracted_at: None,
            extraction_batch_id: None,
        }
    }

    #[test]
    fn extraction_batch_id_round_trips_through_db() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let mut session = make_test_session("s1");
        session.extraction_batch_id = Some("test-batch-1".to_string());
        upsert_source_session(&shared_root, &session).unwrap();
        let retrieved = get_source_session(&shared_root, "s1").unwrap().unwrap();
        assert_eq!(
            retrieved.extraction_batch_id,
            Some("test-batch-1".to_string())
        );
    }

    #[test]
    fn extraction_batch_id_defaults_to_none() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let session = make_test_session("s2");
        upsert_source_session(&shared_root, &session).unwrap();
        let retrieved = get_source_session(&shared_root, "s2").unwrap().unwrap();
        assert_eq!(retrieved.extraction_batch_id, None);
    }

    #[test]
    fn extraction_batch_migration_is_idempotent() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        // First open creates the catalog and runs the migration
        let conn1 = open_source_sessions_catalog(&shared_root).unwrap();
        drop(conn1);
        // Second open runs the same migration again — must not error
        let _conn2 = open_source_sessions_catalog(&shared_root).unwrap();
    }

    #[test]
    fn known_field_notes_split_migration_merges_source_catalog_projects() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let conn = open_source_sessions_catalog(&shared_root).unwrap();
        conn.execute_batch(
            "INSERT INTO source_sessions
             (source_id, source_tool, source_path, session_id, project, machine,
              title, discovered_at, source_fingerprint, transcript_json,
              status, attempts, extractor_version)
             VALUES ('s-from', 'unknown', '/path/from', 'sess-from', 'field-notes', 'machine',
                     'from', '2026-01-01T00:00:00Z', 'fp-from', '[]', 'pending', 0, 'v1'),
                    ('s-to', 'unknown', '/path/to', 'sess-to', 'field_notes', 'machine',
                     'to', '2026-01-01T00:00:00Z', 'fp-to', '[]', 'pending', 0, 'v1');
             INSERT INTO learning_sources
             (source_id, kind, uri, project, title, traversal_policy_json, status,
              attempts, created_at, updated_at)
             VALUES ('l-from', 'url', 'https://example.com/from', 'field-notes', 'from', '{}',
                     'pending', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
                    ('l-to', 'url', 'https://example.com/to', 'field_notes', 'to', '{}',
                     'pending', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
        )
        .expect("seed source split rows");
        drop(conn);

        let conn = open_source_sessions_catalog(&shared_root).unwrap();
        open_source_sessions_catalog(&shared_root).expect("idempotent reopen");
        for table in ["source_sessions", "learning_sources"] {
            let from_count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE project = 'field-notes'"),
                    [],
                    |row| row.get(0),
                )
                .expect("count old project");
            let to_count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE project = 'field_notes'"),
                    [],
                    |row| row.get(0),
                )
                .expect("count merged project");
            assert_eq!(from_count, 0, "{table} retained field-notes rows");
            assert_eq!(to_count, 2, "{table} did not merge both rows");
        }
    }

    // -------------------------------------------------------------------------
    // US-004: extraction_batches table + CRUD helpers + atomic claim
    // -------------------------------------------------------------------------

    fn make_test_batch(batch_id: &str, session_ids: Vec<String>) -> ExtractionBatchRecord {
        ExtractionBatchRecord {
            batch_id: batch_id.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            status: "submitted".to_string(),
            session_ids,
            input_file_id: None,
            output_file_id: None,
            error_file_id: None,
            total_requests: 5,
            reconciled_at: None,
            extractor_version: "v1".to_string(),
            last_polled_at: None,
            poll_error: None,
        }
    }

    #[test]
    fn insert_and_list_extraction_batch() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let batch = make_test_batch("batch-1", vec!["s1".to_string(), "s2".to_string()]);
        insert_extraction_batch(&shared_root, &batch).unwrap();
        let results = list_extraction_batches_by_status(&shared_root, &["submitted"]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].batch_id, "batch-1");
        assert_eq!(
            results[0].session_ids,
            vec!["s1".to_string(), "s2".to_string()]
        );
        assert_eq!(results[0].total_requests, 5);
        assert_eq!(results[0].status, "submitted");
    }

    #[test]
    fn update_extraction_batch_status_changes_row() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let batch = make_test_batch("batch-2", vec!["s1".to_string()]);
        insert_extraction_batch(&shared_root, &batch).unwrap();
        update_extraction_batch_status(
            &shared_root,
            "batch-2",
            "in_progress",
            Some("2026-01-01T01:00:00Z"),
            None,
        )
        .unwrap();
        let results = list_extraction_batches_by_status(&shared_root, &["in_progress"]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "in_progress");
        assert_eq!(
            results[0].last_polled_at,
            Some("2026-01-01T01:00:00Z".to_string())
        );
    }

    #[test]
    fn mark_extraction_batch_reconciled_sets_timestamp() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let batch = make_test_batch("batch-3", vec!["s1".to_string()]);
        insert_extraction_batch(&shared_root, &batch).unwrap();
        mark_extraction_batch_reconciled(&shared_root, "batch-3", "2026-01-02T00:00:00Z").unwrap();
        let results = list_extraction_batches_by_status(&shared_root, &["reconciled"]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].reconciled_at,
            Some("2026-01-02T00:00:00Z".to_string())
        );
    }

    #[test]
    fn atomic_claim_returns_claimed_ids() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        for i in 1..=3 {
            let session = make_test_session(&format!("s{i}"));
            upsert_source_session(&shared_root, &session).unwrap();
        }
        let claimed =
            atomic_claim_pending_sessions(&shared_root, "placeholder-batch", 2, None).unwrap();
        assert_eq!(claimed.len(), 2);
        // Verify they are now running with the placeholder batch id
        for id in &claimed {
            let s = get_source_session(&shared_root, id).unwrap().unwrap();
            assert_eq!(s.status, SourceSessionStatus::Running);
            assert_eq!(s.extraction_batch_id, Some("placeholder-batch".to_string()));
        }
        // The third session remains pending
        let all = list_source_sessions(&shared_root, None).unwrap();
        let pending: Vec<_> = all
            .iter()
            .filter(|s| s.status == SourceSessionStatus::Pending)
            .collect();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn learning_sources_enqueue_claim_and_complete() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        let conn = open_source_sessions_catalog(&shared_root).unwrap();
        let seed = LearningSourceSeed {
            uri: "https://example.com/article".to_string(),
            kind: None,
            project: Some("fieldcraft".to_string()),
            title: Some("Example Article".to_string()),
            scope_hint: Some("derive source-learning guidance".to_string()),
            traversal_policy: LearningTraversalPolicy::default(),
        };

        let (record, queued) = enqueue_learning_source(&conn, &seed).unwrap();
        assert!(queued);
        assert_eq!(record.status, LearningSourceStatus::Pending);
        assert_eq!(record.kind, LearningSourceKind::Url);

        let (_, queued_again) = enqueue_learning_source(&conn, &seed).unwrap();
        assert!(!queued_again, "unchanged source should be idempotent");

        let claimed = claim_learning_sources(&conn, "worker-1", 1, Some("fieldcraft")).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].source_id, record.source_id);
        assert_eq!(claimed[0].status, LearningSourceStatus::Running);
        assert_eq!(claimed[0].worker_id.as_deref(), Some("worker-1"));

        complete_learning_source(&conn, &record.source_id, Some("hash-1"), 3).unwrap();
        let completed = get_learning_source(&conn, &record.source_id)
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, LearningSourceStatus::Succeeded);
        assert_eq!(completed.source_hash.as_deref(), Some("hash-1"));
        assert_eq!(completed.derived_record_count, 3);
        assert_eq!(completed.worker_id, None);
    }

    #[test]
    fn atomic_claim_no_double_claim_under_concurrency() {
        use std::sync::Arc;
        let dir = tempdir().unwrap();
        let shared_root = Arc::new(dir.path().join("shared"));
        // Insert 10 pending sessions
        for i in 1..=10 {
            let session = make_test_session(&format!("conc-{i}"));
            upsert_source_session(&shared_root, &session).unwrap();
        }
        let root1 = Arc::clone(&shared_root);
        let root2 = Arc::clone(&shared_root);
        let t1 = std::thread::spawn(move || {
            atomic_claim_pending_sessions(&root1, "placeholder-A", 10, None).unwrap()
        });
        let t2 = std::thread::spawn(move || {
            atomic_claim_pending_sessions(&root2, "placeholder-B", 10, None).unwrap()
        });
        let ids_a = t1.join().unwrap();
        let ids_b = t2.join().unwrap();
        // Union must be exactly 10
        let mut all_ids: Vec<String> = ids_a.iter().chain(ids_b.iter()).cloned().collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), 10, "no overlap and no missed sessions");
        // No id should appear in both
        for id in &ids_a {
            assert!(!ids_b.contains(id), "double-claimed: {id}");
        }
    }

    #[test]
    fn atomic_claim_includes_stale_sessions() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        // Insert 1 pending, 1 stale, 1 succeeded
        let mut pending_session = make_test_session("stale-test-pending");
        pending_session.status = SourceSessionStatus::Pending;
        upsert_source_session(&shared_root, &pending_session).unwrap();

        let mut stale_session = make_test_session("stale-test-stale");
        stale_session.status = SourceSessionStatus::Pending;
        upsert_source_session(&shared_root, &stale_session).unwrap();
        // Manually set to stale
        let conn = open_source_sessions_catalog(&shared_root).unwrap();
        conn.execute(
            "UPDATE source_sessions SET status = 'stale' WHERE source_id = 'stale-test-stale'",
            [],
        )
        .unwrap();
        drop(conn);

        let mut succeeded_session = make_test_session("stale-test-succeeded");
        succeeded_session.status = SourceSessionStatus::Pending;
        upsert_source_session(&shared_root, &succeeded_session).unwrap();
        let conn = open_source_sessions_catalog(&shared_root).unwrap();
        conn.execute(
            "UPDATE source_sessions SET status = 'succeeded' WHERE source_id = 'stale-test-succeeded'",
            [],
        ).unwrap();
        drop(conn);

        let claimed = atomic_claim_pending_sessions(&shared_root, "test-batch", 10, None).unwrap();
        assert_eq!(
            claimed.len(),
            2,
            "should claim pending and stale, not succeeded"
        );
        assert!(claimed.contains(&"stale-test-pending".to_string()));
        assert!(claimed.contains(&"stale-test-stale".to_string()));
        assert!(!claimed.contains(&"stale-test-succeeded".to_string()));
    }

    #[test]
    fn rename_extraction_batch_id_updates_both_tables() {
        let dir = tempdir().unwrap();
        let shared_root = dir.path().join("shared");
        // Insert 2 sessions with placeholder batch id
        for id in ["rename-s1", "rename-s2"] {
            let mut session = make_test_session(id);
            session.extraction_batch_id = Some("placeholder-xyz".to_string());
            session.status = SourceSessionStatus::Running;
            upsert_source_session(&shared_root, &session).unwrap();
        }
        // Insert extraction_batches row with placeholder id
        let batch = make_test_batch(
            "placeholder-xyz",
            vec!["rename-s1".to_string(), "rename-s2".to_string()],
        );
        insert_extraction_batch(&shared_root, &batch).unwrap();

        // Rename to real OpenAI batch id
        rename_extraction_batch_id(&shared_root, "placeholder-xyz", "batch_real_openai_123")
            .unwrap();

        // Both source_sessions should now have the real batch id
        for id in ["rename-s1", "rename-s2"] {
            let s = get_source_session(&shared_root, id).unwrap().unwrap();
            assert_eq!(
                s.extraction_batch_id,
                Some("batch_real_openai_123".to_string())
            );
        }
        // extraction_batches table should have the real id
        let results = list_extraction_batches_by_status(&shared_root, &["submitted"]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].batch_id, "batch_real_openai_123");
    }
}
