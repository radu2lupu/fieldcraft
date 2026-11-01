//! Storage feeds for the in-memory record index
//! (`fieldcraft_retrieval::record_cache`).
//!
//! The query path used to read every candidate row from SQLite on each query:
//! all ~27k active vectors (with text and payload) for dense search, and all
//! ~9k records without an identity cue (with text) for legacy identity
//! admission. The record cache keeps the few fields those scans decide on,
//! per record, in memory.
//!
//! Freshness: every insert, delete, and update of a column the cache holds
//! (`dense_vector`, `tier`, `state`, `superseded_by`, `source_json`,
//! `project_id`), and every identity cue added or removed, appends the record
//! id to `knowledge_vector_changes` through triggers created by `open_catalog`.
//! The cache remembers the last sequence number it applied and reloads only
//! the ids logged after it, so writes from any connection or process (server,
//! CLI, reindex) invalidate exactly the records they touched.

use std::collections::HashMap;
use std::path::Path;

use fieldcraft_core::TrError;
use rusqlite::{Connection, params};

use crate::repository::{KnowledgeRepository, blob_to_vector};
use crate::schema::sqlite_error;

/// Schema for the change log and its triggers. Idempotent; run by
/// `open_catalog` on every open.
pub(crate) const RECORD_CHANGE_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS knowledge_vector_changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    knowledge_id TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS knowledge_vector_change_insert AFTER INSERT ON knowledge BEGIN
    INSERT INTO knowledge_vector_changes(knowledge_id) VALUES (new.id);
END;
CREATE TRIGGER IF NOT EXISTS knowledge_vector_change_delete AFTER DELETE ON knowledge BEGIN
    INSERT INTO knowledge_vector_changes(knowledge_id) VALUES (old.id);
END;
CREATE TRIGGER IF NOT EXISTS knowledge_vector_change_update
AFTER UPDATE OF id, dense_vector, tier, state, superseded_by, source_json, project_id
ON knowledge BEGIN
    INSERT INTO knowledge_vector_changes(knowledge_id) VALUES (old.id);
    INSERT INTO knowledge_vector_changes(knowledge_id)
        SELECT new.id WHERE new.id IS NOT old.id;
END;
CREATE TRIGGER IF NOT EXISTS cue_index_identity_change_insert AFTER INSERT ON cue_index
WHEN new.dim_name = 'identity' BEGIN
    INSERT INTO knowledge_vector_changes(knowledge_id) VALUES (new.record_id);
END;
CREATE TRIGGER IF NOT EXISTS cue_index_identity_change_delete AFTER DELETE ON cue_index
WHEN old.dim_name = 'identity' BEGIN
    INSERT INTO knowledge_vector_changes(knowledge_id) VALUES (old.record_id);
END;
"#;

/// One active or provisional record as the record cache holds it: what the
/// dense and identity scans decide on, nothing else. Full rows for the
/// winners are loaded afterwards ([`KnowledgeRepository::load_dense_hit_rows`]).
#[derive(Debug, Clone)]
pub struct RecordIndexRow {
    pub rowid: i64,
    pub id: String,
    pub state: String,
    pub tier: Option<String>,
    pub superseded_by_is_null: bool,
    pub source_json: String,
    pub project_id: String,
    pub has_identity_cue: bool,
    pub vector: Option<Vec<f32>>,
}

/// The display/scoring fields of a dense-search hit.
#[derive(Debug, Clone)]
pub struct DenseHitRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub retrieval_text: String,
    pub payload_json: String,
    pub confidence: f32,
    pub state: String,
    pub created_at: String,
    pub phase: Option<String>,
    pub owner: Option<String>,
    pub session_date: Option<String>,
    pub source_json: String,
    pub project_id: String,
}

const INDEX_ROW_COLUMNS: &str = "k.rowid, k.id, k.state, k.tier, k.superseded_by IS NULL, \
     k.source_json, k.project_id, \
     EXISTS (SELECT 1 FROM cue_index c WHERE c.record_id = k.id AND c.dim_name = 'identity'), \
     k.dense_vector";

fn index_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecordIndexRow> {
    let blob: Option<Vec<u8>> = row.get(8)?;
    Ok(RecordIndexRow {
        rowid: row.get(0)?,
        id: row.get(1)?,
        state: row.get(2)?,
        tier: row.get(3)?,
        superseded_by_is_null: row.get(4)?,
        source_json: row.get(5)?,
        project_id: row.get(6)?,
        has_identity_cue: row.get(7)?,
        vector: blob.map(|bytes| blob_to_vector(&bytes)),
    })
}

pub(crate) fn id_placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

impl KnowledgeRepository {
    /// Open a connection for reads only: no schema work, no migrations, so it
    /// never takes a write lock. The catalog must already have been opened
    /// once with [`KnowledgeRepository::open`] (the server does that at start).
    pub fn open_reader(path: &Path) -> Result<Self, TrError> {
        let conn = Connection::open(path).map_err(sqlite_error)?;
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA query_only=1;")
            .map_err(sqlite_error)?;
        Ok(Self::from_connection(conn))
    }

    /// Highest change sequence number, or 0 when nothing was logged yet.
    pub fn max_record_change_seq(&self) -> Result<i64, TrError> {
        self.connection()
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM knowledge_vector_changes",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)
    }

    /// Lowest change sequence number still in the log (0 when empty). A cache
    /// whose last applied sequence is below `min - 1` missed pruned entries
    /// and must rebuild.
    pub fn min_record_change_seq(&self) -> Result<i64, TrError> {
        self.connection()
            .query_row(
                "SELECT COALESCE(MIN(seq), 0) FROM knowledge_vector_changes",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)
    }

    /// Distinct record ids changed after `seq`, and the highest sequence seen.
    pub fn record_changes_since(&self, seq: i64) -> Result<(Vec<String>, i64), TrError> {
        let mut stmt = self
            .connection()
            .prepare_cached(
                "SELECT knowledge_id, seq FROM knowledge_vector_changes
                 WHERE seq > ?1 ORDER BY seq",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![seq], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(sqlite_error)?;
        let mut max_seq = seq;
        let mut seen = std::collections::HashSet::new();
        let mut ids = Vec::new();
        for row in rows {
            let (id, s) = row.map_err(sqlite_error)?;
            max_seq = max_seq.max(s);
            if seen.insert(id.clone()) {
                ids.push(id);
            }
        }
        Ok((ids, max_seq))
    }

    /// Drop change-log entries older than the newest `keep`. A cache that
    /// falls further behind than that rebuilds from scratch.
    pub fn prune_record_changes(&self, keep: i64) -> Result<usize, TrError> {
        self.connection()
            .execute(
                "DELETE FROM knowledge_vector_changes
                 WHERE seq <= (SELECT COALESCE(MAX(seq), 0) FROM knowledge_vector_changes) - ?1",
                params![keep],
            )
            .map_err(sqlite_error)
    }

    /// Every active or provisional record, in the order the old full-table
    /// scans visited them: by state, then rowid.
    pub fn load_record_index_rows(&self) -> Result<Vec<RecordIndexRow>, TrError> {
        let sql = format!(
            "SELECT {INDEX_ROW_COLUMNS} FROM knowledge k
             WHERE k.state IN ('active', 'provisional')
             ORDER BY k.state, k.rowid"
        );
        let mut stmt = self.connection().prepare(&sql).map_err(sqlite_error)?;
        let rows = stmt.query_map([], index_row).map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// The current index rows for `ids` (same filter as
    /// [`Self::load_record_index_rows`]). Ids that no longer qualify are absent.
    pub fn load_record_index_rows_for(
        &self,
        ids: &[String],
    ) -> Result<Vec<RecordIndexRow>, TrError> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let sql = format!(
                "SELECT {INDEX_ROW_COLUMNS} FROM knowledge k
                 WHERE k.id IN ({}) AND k.state IN ('active', 'provisional')",
                id_placeholders(chunk.len())
            );
            let mut stmt = self.connection().prepare(&sql).map_err(sqlite_error)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), index_row)
                .map_err(sqlite_error)?;
            for row in rows {
                out.push(row.map_err(sqlite_error)?);
            }
        }
        Ok(out)
    }

    /// `(title, retrieval_text)` for `ids`, keyed by id.
    pub fn load_titles_and_texts(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, (String, String)>, TrError> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let sql = format!(
                "SELECT id, title, retrieval_text FROM knowledge WHERE id IN ({})",
                id_placeholders(chunk.len())
            );
            let mut stmt = self.connection().prepare(&sql).map_err(sqlite_error)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (id, pair) = row.map_err(sqlite_error)?;
                out.insert(id, pair);
            }
        }
        Ok(out)
    }

    /// Display/scoring fields for dense-search winners, keyed by id.
    pub fn load_dense_hit_rows(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, DenseHitRow>, TrError> {
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let sql = format!(
                "SELECT id, kind, title, retrieval_text, payload_json, confidence, state,
                        created_at, phase, owner, session_date, source_json, project_id
                 FROM knowledge WHERE id IN ({})",
                id_placeholders(chunk.len())
            );
            let mut stmt = self.connection().prepare(&sql).map_err(sqlite_error)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok(DenseHitRow {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        title: row.get(2)?,
                        retrieval_text: row.get(3)?,
                        payload_json: row.get(4)?,
                        confidence: row.get::<_, f64>(5)? as f32,
                        state: row.get(6)?,
                        created_at: row.get(7)?,
                        phase: row.get(8)?,
                        owner: row.get(9)?,
                        session_date: row.get(10)?,
                        source_json: row.get(11)?,
                        project_id: row.get(12)?,
                    })
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let row = row.map_err(sqlite_error)?;
                out.insert(row.id.clone(), row);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fieldcraft_core::{
        KnowledgeKind, KnowledgeRecord, KnowledgeSource, KnowledgeState, KnowledgeTier,
    };
    use tempfile::tempdir;

    fn record(id: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.to_string(),
            project_id: "p".to_string(),
            kind: KnowledgeKind::Claim,
            title: format!("title {id}"),
            payload_json: "{}".to_string(),
            state: KnowledgeState::Active,
            confidence: 0.5,
            source: KnowledgeSource::Manual,
            superseded_by: None,
            project_specific: false,
            created_at: chrono::Utc::now(),
            valid_until: None,
            retrieval_text: format!("text {id}"),
            schema_version: 4,
            phase: None,
            tier: KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
            protected: false,
        }
    }

    #[test]
    fn writes_that_change_cached_fields_are_logged() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let start = repo.max_record_change_seq().unwrap();
        repo.insert_knowledge(&record("a")).unwrap();
        repo.store_dense_vector("a", &[1.0, 0.0]).unwrap();
        let (ids, seq) = repo.record_changes_since(start).unwrap();
        assert_eq!(ids, vec!["a".to_string()]);
        assert!(seq > start);

        // Confidence-only updates change nothing the cache holds.
        repo.connection()
            .execute(
                "UPDATE knowledge SET dynamic_confidence = 0.7 WHERE id = 'a'",
                [],
            )
            .unwrap();
        assert!(repo.record_changes_since(seq).unwrap().0.is_empty());

        repo.archive_record("a", "test").unwrap();
        let (ids, seq2) = repo.record_changes_since(seq).unwrap();
        assert_eq!(ids, vec!["a".to_string()]);

        repo.connection()
            .execute(
                "INSERT INTO cue_index (dim_name, dim_value, record_id) VALUES ('identity', 'p', 'a')",
                [],
            )
            .unwrap();
        let (ids, seq3) = repo.record_changes_since(seq2).unwrap();
        assert_eq!(ids, vec!["a".to_string()]);

        assert!(repo.prune_record_changes(1).unwrap() > 0);
        assert_eq!(repo.min_record_change_seq().unwrap(), seq3);
    }

    #[test]
    fn index_rows_follow_state_then_rowid_order() {
        let dir = tempdir().unwrap();
        let repo = KnowledgeRepository::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut provisional = record("first-provisional");
        provisional.state = KnowledgeState::Provisional;
        repo.insert_knowledge(&provisional).unwrap();
        repo.insert_knowledge(&record("second-active")).unwrap();
        repo.insert_knowledge(&record("no-vector")).unwrap();
        repo.store_dense_vector("first-provisional", &[1.0])
            .unwrap();
        repo.store_dense_vector("second-active", &[1.0]).unwrap();

        let rows = repo.load_record_index_rows().unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["second-active", "no-vector", "first-provisional"]);
        assert!(rows[1].vector.is_none());

        let reader = KnowledgeRepository::open_reader(&dir.path().join("memory.sqlite3")).unwrap();
        let some = reader
            .load_record_index_rows_for(&["no-vector".into(), "gone".into()])
            .unwrap();
        assert_eq!(some.len(), 1);
        let hits = reader
            .load_dense_hit_rows(&["second-active".into()])
            .unwrap();
        assert_eq!(hits["second-active"].title, "title second-active");
    }
}
