//! Cue-bundle backfill — populates the deterministic dimensions of
//! [`fieldcraft_core::CueBundle`] for every record in the catalog and
//! synchronizes the `cue_index` inverted table.
//!
//! Run once after the schema migration that adds the `cue_bundle` column and
//! the `cue_index` table. Idempotent: by default skips records whose
//! `cue_bundle` is already non-null; `force=true` re-extracts every record.
//!
//! Phase 1 of cue-bundle retrieval — see `docs/plans/cue-bundle-retrieval.md`.

use fieldcraft_core::{TrError, cue_extract};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::schema::sqlite_error;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BackfillStats {
    /// Number of records selected for backfilling in this run.
    pub considered: usize,
    /// Number of records that received a freshly extracted bundle.
    pub updated: usize,
    /// Number of records already having a bundle and not re-extracted
    /// (only nonzero when `force=false`).
    pub skipped: usize,
}

/// Backfill `cue_bundle` and `cue_index` for records in the `knowledge` table.
///
/// With `force=false` (default), skips records whose `cue_bundle` is already
/// non-null/non-empty. With `force=true`, re-extracts every record.
///
/// Returns counts. All writes happen inside a single transaction.
pub fn backfill_all(conn: &mut Connection, force: bool) -> Result<BackfillStats, TrError> {
    let mut stats = BackfillStats::default();

    // Count records that already have a bundle, for `skipped` accounting.
    let already_populated: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM knowledge \
             WHERE cue_bundle IS NOT NULL AND cue_bundle <> ''",
            [],
            |row| row.get::<_, i64>(0).map(|n| n as usize),
        )
        .map_err(sqlite_error)?;

    let where_clause = if force {
        ""
    } else {
        "WHERE cue_bundle IS NULL OR cue_bundle = ''"
    };
    let select_sql = format!(
        "SELECT id, project_id, title, retrieval_text, COALESCE(affected_files_json, '[]') \
         FROM knowledge {where_clause}"
    );

    // Pull all candidate records out first so the read statement is closed
    // before we open the write transaction.
    let mut records: Vec<(String, String, String, String, String)> = Vec::new();
    {
        let mut stmt = conn.prepare(&select_sql).map_err(sqlite_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(sqlite_error)?;
        for row in rows {
            records.push(row.map_err(sqlite_error)?);
        }
    }
    stats.considered = records.len();
    stats.skipped = if force { 0 } else { already_populated };

    if records.is_empty() {
        return Ok(stats);
    }

    let tx = conn.transaction().map_err(sqlite_error)?;
    for (id, project_id, title, retrieval_text, affected_files_json) in records {
        let files: Vec<String> = serde_json::from_str(&affected_files_json).unwrap_or_default();
        let combined = format!("{title} {retrieval_text}");
        // Backfilling pre-existing records: turn_count unknown, LLM cues skipped.
        let mut bundle = cue_extract::extract_deterministic(&combined, &files, None);
        cue_extract::add_identity_cue(&mut bundle, &project_id, "project_id");
        let bundle_json = serde_json::to_string(&bundle).map_err(|e| TrError::Parse {
            message: format!("bundle serialize: {e}"),
        })?;

        tx.execute(
            "UPDATE knowledge SET cue_bundle = ?1 WHERE id = ?2",
            params![bundle_json, id],
        )
        .map_err(sqlite_error)?;

        // Idempotent index rebuild: drop prior rows for this record, then re-insert.
        tx.execute("DELETE FROM cue_index WHERE record_id = ?1", params![id])
            .map_err(sqlite_error)?;
        for (dim_name, dim_value) in bundle.to_index_rows() {
            tx.execute(
                "INSERT INTO cue_index (record_id, dim_name, dim_value) \
                 VALUES (?1, ?2, ?3)",
                params![id, dim_name, dim_value],
            )
            .map_err(sqlite_error)?;
        }

        stats.updated += 1;
    }
    tx.commit().map_err(sqlite_error)?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::open_catalog;
    use rusqlite::params;
    use tempfile::tempdir;

    fn insert_record(
        conn: &Connection,
        id: &str,
        kind: &str,
        title: &str,
        retrieval_text: &str,
        affected_files_json: &str,
    ) {
        conn.execute(
            "INSERT INTO knowledge (
                id, project_id, kind, state, title, payload_json,
                confidence, source_json, project_specific, created_at, updated_at,
                affected_files_json, retrieval_text
             ) VALUES (?1, 'fieldcraft', ?2, 'active', ?3, '{}', 0.5,
                       '{\"type\":\"manual\"}', 1,
                       '2026-05-17T00:00:00Z', '2026-05-17T00:00:00Z', ?4, ?5)",
            params![id, kind, title, affected_files_json, retrieval_text],
        )
        .unwrap();
    }

    #[test]
    fn backfill_populates_cue_bundle_and_cue_index() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let mut conn = open_catalog(&path).unwrap();

        insert_record(
            &conn,
            "rec-1",
            "directive",
            "Use cargo test before commit",
            "Use cargo test before commit | scope: crates/fieldcraft-core/src/lib.rs",
            r#"["crates/fieldcraft-core/src/lib.rs"]"#,
        );
        insert_record(
            &conn,
            "rec-2",
            "claim",
            "P0_WEIGHTS controls scoring",
            "P0_WEIGHTS is a 12-feature weight vector",
            r#"[]"#,
        );

        let stats = backfill_all(&mut conn, false).unwrap();
        assert_eq!(stats.considered, 2);
        assert_eq!(stats.updated, 2);
        assert_eq!(stats.skipped, 0);

        let bundle1: Option<String> = conn
            .query_row(
                "SELECT cue_bundle FROM knowledge WHERE id = 'rec-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let bundle1 = bundle1.expect("rec-1 should have a bundle");
        assert!(
            bundle1.contains("fieldcraft_core"),
            "bundle1 should have domain: {bundle1}"
        );
        assert!(
            bundle1.contains("cargo"),
            "bundle1 should have cargo tool: {bundle1}"
        );

        let count1: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index WHERE record_id = 'rec-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count1 > 0, "rec-1 should have cue_index rows");

        let count2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cue_index WHERE record_id = 'rec-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count2 > 0, "rec-2 should have cue_index rows");
    }

    #[test]
    fn backfill_is_idempotent_without_force() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let mut conn = open_catalog(&path).unwrap();

        insert_record(
            &conn,
            "rec-1",
            "claim",
            "Test",
            "Test record retrieval_text",
            r#"[]"#,
        );

        // First run: updates 1.
        let stats1 = backfill_all(&mut conn, false).unwrap();
        assert_eq!(stats1.updated, 1);
        assert_eq!(stats1.skipped, 0);

        // Second run without force: skips that 1.
        let stats2 = backfill_all(&mut conn, false).unwrap();
        assert_eq!(stats2.considered, 0);
        assert_eq!(stats2.updated, 0);
        assert_eq!(stats2.skipped, 1);
    }

    #[test]
    fn backfill_force_re_extracts_all() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let mut conn = open_catalog(&path).unwrap();

        insert_record(&conn, "rec-1", "claim", "Test", "Test", r#"[]"#);
        backfill_all(&mut conn, false).unwrap();

        let stats = backfill_all(&mut conn, true).unwrap();
        assert_eq!(stats.considered, 1);
        assert_eq!(stats.updated, 1);
    }

    #[test]
    fn backfill_empty_catalog_is_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let mut conn = open_catalog(&path).unwrap();
        let stats = backfill_all(&mut conn, false).unwrap();
        assert_eq!(stats.considered, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.skipped, 0);
    }
}
