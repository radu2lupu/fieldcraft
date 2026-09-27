use fieldcraft_core::TrError;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: &str = "v4";
pub const DAM_DB_FILE: &str = "memory.sqlite3";

/// Returns the Fieldcraft SQLite DB path.
///
/// `FIELDCRAFT_HOME` is primarily for isolated benchmark/test runs. Normal
/// local usage falls back to `~/.fieldcraft/memory.sqlite3`.
pub fn fieldcraft_db_path() -> PathBuf {
    if let Ok(root) = std::env::var("FIELDCRAFT_HOME") {
        return PathBuf::from(root).join(DAM_DB_FILE);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".fieldcraft").join(DAM_DB_FILE)
}

pub fn project_slug_match_key(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| match ch {
            '-' => '_',
            _ => ch.to_ascii_lowercase(),
        })
        .collect()
}

pub fn canonical_project_id_from_existing<I, S>(project_id: &str, existing: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let requested = project_id.trim();
    let requested = if requested.is_empty() {
        "default"
    } else {
        requested
    };
    let key = project_slug_match_key(requested);
    let matches: BTreeSet<String> = existing
        .into_iter()
        .map(|candidate| candidate.as_ref().trim().to_string())
        .filter(|candidate| !candidate.is_empty())
        .filter(|candidate| project_slug_match_key(candidate) == key)
        .collect();

    if matches.len() == 1 {
        matches
            .into_iter()
            .next()
            .unwrap_or_else(|| requested.to_string())
    } else {
        requested.to_ascii_lowercase()
    }
}

/// Open (or create) the unified DAM catalog at the given path.
/// Creates the directory and all tables if they don't already exist.
/// Applies WAL mode and busy_timeout=5000 on every open.
pub fn open_catalog(path: &Path) -> Result<Connection, TrError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TrError::Storage {
            message: e.to_string(),
        })?;
    }
    let conn = Connection::open(path).map_err(sqlite_error)?;
    // WAL mode + busy_timeout for concurrent access resilience.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .map_err(sqlite_error)?;
    conn.execute_batch(CATALOG_SCHEMA_SQL)
        .map_err(sqlite_error)?;
    // Migrations for existing databases (idempotent, transactional).
    run_migration(&conn, "ALTER TABLE knowledge ADD COLUMN dense_vector BLOB")?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 2",
    )?;
    run_migration(&conn, "ALTER TABLE knowledge ADD COLUMN phase TEXT")?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN tier TEXT NOT NULL DEFAULT 'active'",
    )?;
    run_migration(&conn, "ALTER TABLE knowledge ADD COLUMN owner TEXT")?;
    run_migration(&conn, "ALTER TABLE knowledge ADD COLUMN session_date TEXT")?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN source_refs_json TEXT NOT NULL DEFAULT '[]'",
    )?;
    // Cue bundle (Phase 1 of cue-bundle retrieval — see docs/plans/cue-bundle-retrieval.md).
    // Stored as JSON string of the `CueBundle` struct. NULL for legacy records
    // until the backfill CLI populates them.
    run_migration(&conn, "ALTER TABLE knowledge ADD COLUMN cue_bundle TEXT")?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN query_cue_bundle TEXT",
    )?;
    // Per-session correlation for the trace analyzer. NULL for legacy rows;
    // populated forward when an agent passes session_id with its query.
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN session_id TEXT",
    )?;
    // Phase 6: dynamic_confidence — evolves from `confidence` via feedback
    // outcomes (testing-effect-style asymptotic Bayesian update). NULL = no
    // feedback yet; retrieval falls back to the static `confidence` column.
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN dynamic_confidence REAL",
    )?;
    run_migration(&conn, "ALTER TABLE knowledge ADD COLUMN text TEXT")?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN audio_ref_json TEXT",
    )?;
    run_migration(&conn, "ALTER TABLE knowledge ADD COLUMN source TEXT")?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN extracted_json TEXT",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
    )?;
    // Memory Lifecycle L1: explicit retention pin. A record with protected=1
    // is never auto-archived regardless of served_activation/outcome signals.
    // Distinct from the implicit protection rules (kind in {directive,
    // context}, static confidence >= 0.9) computed at read time by
    // `is_protected` — this column only carries the explicit override.
    run_migration(
        &conn,
        "ALTER TABLE knowledge ADD COLUMN protected INTEGER NOT NULL DEFAULT 0",
    )?;
    // Feedback log: append-only record of every outcome event for a lesson.
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS lesson_outcome_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            knowledge_id TEXT NOT NULL,
            session_id TEXT,
            outcome TEXT NOT NULL,
            source TEXT NOT NULL,
            confidence_before REAL,
            confidence_after REAL,
            note TEXT,
            created_at TEXT NOT NULL
        )",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_lesson_outcome_events_knowledge \
         ON lesson_outcome_events(knowledge_id)",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_lesson_outcome_events_created \
         ON lesson_outcome_events(created_at)",
    )?;
    // Server-side consolidation queue. `POST /lessons` only enqueues work; a
    // background server worker claims pending rows and consolidates obvious
    // duplicate lessons without adding latency to client writes.
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS knowledge_merge_queue (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            lesson_id TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            processed_at TEXT
        )",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_merge_queue_status \
         ON knowledge_merge_queue(status, id)",
    )?;
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS knowledge_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            knowledge_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            source_knowledge_id TEXT,
            source_refs_json TEXT NOT NULL DEFAULT '[]',
            note TEXT,
            created_at TEXT NOT NULL
        )",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_knowledge_events_knowledge \
         ON knowledge_events(knowledge_id)",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN raw_candidate_ids TEXT NOT NULL DEFAULT '[]'",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN raw_candidate_scores TEXT NOT NULL DEFAULT '[]'",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN raw_candidate_titles TEXT NOT NULL DEFAULT '[]'",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN result_origins TEXT NOT NULL DEFAULT '[]'",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN raw_candidate_origins TEXT NOT NULL DEFAULT '[]'",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN link_ids_surfaced TEXT NOT NULL DEFAULT '[]'",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE retrieval_log ADD COLUMN link_ids_revealed TEXT NOT NULL DEFAULT '[]'",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge_relations ADD COLUMN confidence REAL NOT NULL DEFAULT 1.0",
    )?;
    run_migration(
        &conn,
        "ALTER TABLE knowledge_relations ADD COLUMN valid_until TEXT",
    )?;
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS mentions (
          memory_id     TEXT NOT NULL,
          entity_kind   TEXT NOT NULL,
          canonical_id  TEXT NOT NULL,
          raw_text      TEXT NOT NULL,
          span_start    INTEGER,
          span_end      INTEGER,
          PRIMARY KEY (memory_id, entity_kind, canonical_id, raw_text)
        )",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_mentions_canonical ON mentions(entity_kind, canonical_id)",
    )?;
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS people (
          canonical_id     TEXT PRIMARY KEY,
          display_name     TEXT NOT NULL,
          aliases_json     TEXT NOT NULL DEFAULT '[]',
          mention_count    INTEGER NOT NULL DEFAULT 0,
          last_seen_at     TEXT,
          is_device_owner  INTEGER NOT NULL DEFAULT 0,
          created_at       TEXT NOT NULL,
          updated_at       TEXT NOT NULL,
          revision         INTEGER NOT NULL DEFAULT 1
        )",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_people_last_seen ON people(last_seen_at)",
    )?;
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS projects (
          canonical_id   TEXT PRIMARY KEY,
          slug           TEXT NOT NULL UNIQUE,
          display_name   TEXT NOT NULL,
          aliases_json   TEXT NOT NULL DEFAULT '[]',
          repo_root      TEXT,
          mention_count  INTEGER NOT NULL DEFAULT 0,
          last_seen_at   TEXT,
          created_at     TEXT NOT NULL,
          updated_at     TEXT NOT NULL,
          revision       INTEGER NOT NULL DEFAULT 1
        )",
    )?;
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS actions (
          memory_id              TEXT NOT NULL,
          action_index           INTEGER NOT NULL,
          text                   TEXT NOT NULL,
          assignee_canonical_id  TEXT,
          due_at                 TEXT,
          phase                  TEXT,
          waiting_on             TEXT,
          snoozed_until          TEXT,
          priority               TEXT,
          effort                 TEXT,
          tags_json              TEXT NOT NULL DEFAULT '[]',
          raw_status_json        TEXT NOT NULL DEFAULT '{}',
          shape_id               TEXT,
          PRIMARY KEY (memory_id, action_index)
        )",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_actions_phase ON actions(phase, due_at)",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_actions_assignee ON actions(assignee_canonical_id, phase)",
    )?;
    run_migration(
        &conn,
        "CREATE INDEX IF NOT EXISTS idx_actions_shape ON actions(shape_id)",
    )?;
    run_migration(
        &conn,
        "CREATE TABLE IF NOT EXISTS shapes (
          id                  TEXT PRIMARY KEY,
          name                TEXT NOT NULL UNIQUE,
          field_signature     TEXT NOT NULL,
          description         TEXT NOT NULL DEFAULT '',
          extraction_hints    TEXT NOT NULL DEFAULT '',
          query_synonyms      TEXT NOT NULL DEFAULT '[]',
          render_hint         TEXT,
          user_annotations    TEXT,
          occurrences         INTEGER NOT NULL DEFAULT 0,
          example_memory_ids  TEXT NOT NULL DEFAULT '[]',
          discovered_at       TEXT NOT NULL,
          updated_at          TEXT NOT NULL,
          revision            INTEGER NOT NULL DEFAULT 1
        )",
    )?;
    // Indexes on migrated columns must be created AFTER their ALTER succeeds —
    // CATALOG_SCHEMA_SQL is a no-op for these columns when the table already
    // exists from a previous schema version.
    let _ = conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_knowledge_phase ON knowledge(project_id, phase);
         CREATE INDEX IF NOT EXISTS idx_knowledge_tier ON knowledge(tier);
         CREATE INDEX IF NOT EXISTS idx_cue_dim_value ON cue_index(dim_name, dim_value);
         CREATE INDEX IF NOT EXISTS idx_cue_record    ON cue_index(record_id);",
    );
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS skill_cards_fts_insert;
         DROP TRIGGER IF EXISTS skill_cards_fts_delete;
         DROP TRIGGER IF EXISTS skill_cards_fts_update;
         DROP TABLE IF EXISTS skill_cards_fts;
         DROP TABLE IF EXISTS skill_retrieval_log;",
    )
    .map_err(sqlite_error)?;
    merge_known_project_slug_split(&conn)?;

    // ----- v5 convergence: everything is a memory -----
    // `knowledge` is the single substrate (it already carries the retrieval
    // brain: embeddings, FTS, confidence, supersede chains). A captured voice
    // memory, a coding lesson, a deconstructed skill, a decision — all become
    // `knowledge` rows distinguished only by `kind`. `extracted_json` carries the
    // supertag blob: emergent fields + the action `status` object
    // (phase/since/waiting_on/snoozed_until/…) the LLM reads and updates, AND
    // any memory↔memory references as a `links` array (e.g.
    // [{ "to": "mem_…", "type": "resolves" }]) — relationships live ON the memory,
    // not in a separate table. NULL for legacy rows; the writer fills it forward.

    // Record schema version.
    conn.execute_batch("INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('version', 'v5');")
        .map_err(sqlite_error)?;
    Ok(conn)
}

fn merge_known_project_slug_split(conn: &Connection) -> Result<(), TrError> {
    run_migration(
        conn,
        "UPDATE knowledge SET project_id = 'field_notes' WHERE project_id = 'field-notes';
         UPDATE imports SET project_id = 'field_notes' WHERE project_id = 'field-notes';
         UPDATE retrieval_log SET project_id = 'field_notes' WHERE project_id = 'field-notes';
         UPDATE lesson_feedback SET project_id = 'field_notes' WHERE project_id = 'field-notes';",
    )
}

const CATALOG_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS knowledge (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'provisional',
    title TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.5,
    source_json TEXT NOT NULL,
    superseded_by TEXT,
    project_specific INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    valid_until TEXT,
    updated_at TEXT NOT NULL,
    affected_files_json TEXT,
    retrieval_text TEXT NOT NULL,
    dense_vector BLOB,
    schema_version INTEGER NOT NULL DEFAULT 2,
    phase TEXT,
    tier TEXT NOT NULL DEFAULT 'active',
    session_date TEXT,
    source_refs_json TEXT NOT NULL DEFAULT '[]',
    cue_bundle TEXT
);
CREATE INDEX IF NOT EXISTS idx_knowledge_project ON knowledge(project_id);
CREATE INDEX IF NOT EXISTS idx_knowledge_kind ON knowledge(project_id, kind);
CREATE INDEX IF NOT EXISTS idx_knowledge_state ON knowledge(state);
-- idx_knowledge_tier and idx_knowledge_phase are created post-migration
-- (see open_catalog) so they only run AFTER their column's ALTER has applied.

CREATE VIRTUAL TABLE IF NOT EXISTS knowledge_fts USING fts5(
    id UNINDEXED,
    project_id UNINDEXED,
    retrieval_text,
    content='knowledge',
    content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS knowledge_fts_insert AFTER INSERT ON knowledge BEGIN
    INSERT INTO knowledge_fts(rowid, id, project_id, retrieval_text)
    VALUES (new.rowid, new.id, new.project_id, new.retrieval_text);
END;

CREATE TRIGGER IF NOT EXISTS knowledge_fts_delete AFTER DELETE ON knowledge BEGIN
    INSERT INTO knowledge_fts(knowledge_fts, rowid, id, project_id, retrieval_text)
    VALUES ('delete', old.rowid, old.id, old.project_id, old.retrieval_text);
END;

CREATE TRIGGER IF NOT EXISTS knowledge_fts_update AFTER UPDATE ON knowledge BEGIN
    INSERT INTO knowledge_fts(knowledge_fts, rowid, id, project_id, retrieval_text)
    VALUES ('delete', old.rowid, old.id, old.project_id, old.retrieval_text);
    INSERT INTO knowledge_fts(rowid, id, project_id, retrieval_text)
    VALUES (new.rowid, new.id, new.project_id, new.retrieval_text);
END;

CREATE TABLE IF NOT EXISTS knowledge_relations (
    id TEXT PRIMARY KEY,
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    valid_until TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_relations_from ON knowledge_relations(from_id);
CREATE INDEX IF NOT EXISTS idx_relations_to ON knowledge_relations(to_id);

CREATE TABLE IF NOT EXISTS imports (
    import_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_uri TEXT,
    source_hash TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    extracted_count INTEGER DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS feedback (
    id TEXT PRIMARY KEY,
    knowledge_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    signal TEXT NOT NULL,
    context TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_feedback_knowledge ON feedback(knowledge_id);

CREATE TABLE IF NOT EXISTS retrieval_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  TEXT NOT NULL,
    query_text  TEXT NOT NULL,
    result_ids  TEXT NOT NULL,
    result_scores TEXT NOT NULL,
    result_count INTEGER NOT NULL,
    latency_ms  INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    raw_candidate_ids TEXT NOT NULL DEFAULT '[]',
    raw_candidate_scores TEXT NOT NULL DEFAULT '[]',
    raw_candidate_titles TEXT NOT NULL DEFAULT '[]',
    result_origins TEXT NOT NULL DEFAULT '[]',
    raw_candidate_origins TEXT NOT NULL DEFAULT '[]',
    link_ids_surfaced TEXT NOT NULL DEFAULT '[]',
    link_ids_revealed TEXT NOT NULL DEFAULT '[]',
    query_cue_bundle TEXT
);
CREATE INDEX IF NOT EXISTS idx_retrieval_log_project ON retrieval_log(project_id);
CREATE INDEX IF NOT EXISTS idx_retrieval_log_created ON retrieval_log(created_at);

CREATE TABLE IF NOT EXISTS lesson_feedback (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    knowledge_id  TEXT NOT NULL,
    signal        TEXT NOT NULL,
    retrieval_id  INTEGER,
    project_id    TEXT NOT NULL,
    created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lesson_feedback_knowledge ON lesson_feedback(knowledge_id);

CREATE TABLE IF NOT EXISTS skill_packages (
    id TEXT PRIMARY KEY,
    source_uri TEXT NOT NULL,
    package_hash TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    trust_status TEXT NOT NULL,
    scan_status TEXT NOT NULL,
    file_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_packages_source_hash
    ON skill_packages(source_uri, package_hash);
CREATE INDEX IF NOT EXISTS idx_skill_packages_scan_status ON skill_packages(scan_status);

CREATE TABLE IF NOT EXISTS skill_findings (
    id TEXT PRIMARY KEY,
    package_id TEXT NOT NULL,
    severity TEXT NOT NULL,
    category TEXT NOT NULL,
    file_path TEXT NOT NULL,
    evidence TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skill_findings_package ON skill_findings(package_id);

CREATE TABLE IF NOT EXISTS skill_cards (
    id TEXT PRIMARY KEY,
    package_id TEXT NOT NULL,
    title TEXT NOT NULL,
    skill_name TEXT NOT NULL,
    description TEXT NOT NULL,
    activation_cues_json TEXT NOT NULL,
    negative_cues_json TEXT NOT NULL,
    procedure TEXT NOT NULL,
    constraints_json TEXT NOT NULL,
    required_capabilities_json TEXT NOT NULL,
    verification_steps_json TEXT NOT NULL,
    risk_status TEXT NOT NULL,
    risk_summary TEXT NOT NULL,
    retrieval_text TEXT NOT NULL,
    dense_vector BLOB,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_skill_cards_package ON skill_cards(package_id);
CREATE INDEX IF NOT EXISTS idx_skill_cards_risk_status ON skill_cards(risk_status);

CREATE TABLE IF NOT EXISTS cue_index (
    record_id TEXT NOT NULL,
    dim_name  TEXT NOT NULL,
    dim_value TEXT NOT NULL,
    FOREIGN KEY (record_id) REFERENCES knowledge(id) ON DELETE CASCADE
);
-- Indexes on cue_index are created post-CATALOG_SCHEMA_SQL (see open_catalog)
-- so they survive both fresh creation and ALTER-style migrations.

-- Append-only outcome log for the closed-loop feedback system. Each row is
-- one feedback event tagging a retrieval as confirmed / ignored / contradicted.
-- Powers `fieldcraft stats` analytics and feeds `knowledge.dynamic_confidence`.
CREATE TABLE IF NOT EXISTS lesson_outcome_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    knowledge_id TEXT NOT NULL,
    session_id TEXT,
    outcome TEXT NOT NULL,
    source TEXT NOT NULL,
    confidence_before REAL,
    confidence_after REAL,
    note TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS knowledge_merge_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    lesson_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    processed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_merge_queue_status ON knowledge_merge_queue(status, id);

CREATE TABLE IF NOT EXISTS knowledge_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    knowledge_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    source_knowledge_id TEXT,
    source_refs_json TEXT NOT NULL DEFAULT '[]',
    note TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_knowledge_events_knowledge ON knowledge_events(knowledge_id);

CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// Run a single DDL migration inside BEGIN IMMEDIATE / COMMIT.
/// On `duplicate column` SqliteFailure (idempotent re-run): ROLLBACK + return Ok.
/// All other errors are propagated after a best-effort ROLLBACK.
pub(crate) fn run_migration(conn: &Connection, sql: &str) -> Result<(), TrError> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(sqlite_error)?;
    match conn.execute_batch(sql) {
        Ok(()) => {
            conn.execute_batch("COMMIT").map_err(sqlite_error)?;
            Ok(())
        }
        Err(rusqlite::Error::SqliteFailure(e, Some(ref msg)))
            if e.code == rusqlite::ErrorCode::Unknown && msg.contains("duplicate column") =>
        {
            // Column already exists — idempotent re-run is fine.
            let _ = conn.execute_batch("ROLLBACK");
            Ok(())
        }
        Err(other) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(TrError::Storage {
                message: format!("migration failed: {other}"),
            })
        }
    }
}

pub(crate) fn sqlite_error(err: rusqlite::Error) -> TrError {
    TrError::Storage {
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_catalog_creates_all_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let conn = open_catalog(&path).expect("open_catalog");
        for table in &[
            "knowledge",
            "knowledge_relations",
            "imports",
            "feedback",
            "retrieval_log",
            "lesson_feedback",
            "cue_index",
            "mentions",
            "people",
            "projects",
            "actions",
            "shapes",
            "schema_meta",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query");
            assert_eq!(count, 1, "missing table {table}");
        }
        // Verify FTS5 virtual table exists
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='knowledge_fts'",
                [],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(fts_count, 1, "missing FTS5 virtual table knowledge_fts");
    }

    #[test]
    fn open_catalog_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        open_catalog(&path).expect("first open");
        open_catalog(&path).expect("second open (idempotent)");
    }

    #[test]
    fn open_catalog_drops_legacy_skill_runtime_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let conn = open_catalog(&path).expect("first open");
        conn.execute_batch(
            "CREATE VIRTUAL TABLE skill_cards_fts USING fts5(id UNINDEXED, retrieval_text);
             CREATE TABLE skill_retrieval_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                query_text TEXT NOT NULL,
                result_card_ids TEXT NOT NULL,
                result_scores TEXT NOT NULL,
                result_count INTEGER NOT NULL,
                created_at TEXT NOT NULL
             );",
        )
        .expect("create legacy skill runtime tables");
        drop(conn);

        let conn = open_catalog(&path).expect("reopen");
        for table in ["skill_cards_fts", "skill_retrieval_log"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query");
            assert_eq!(count, 0, "legacy runtime table still exists: {table}");
        }
    }

    #[test]
    fn schema_meta_records_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let conn = open_catalog(&path).expect("open_catalog");
        let version: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .expect("schema_meta query");
        assert_eq!(version, "v5");
    }

    #[test]
    fn run_migration_is_idempotent_on_duplicate_column() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let conn = open_catalog(&path).expect("open_catalog");
        // Running the tier migration again should not error (column already exists).
        let result = run_migration(
            &conn,
            "ALTER TABLE knowledge ADD COLUMN tier TEXT NOT NULL DEFAULT 'active'",
        );
        assert!(
            result.is_ok(),
            "re-running migration must succeed: {:?}",
            result
        );
    }

    #[test]
    fn run_migration_propagates_real_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let conn = open_catalog(&path).expect("open_catalog");
        // Invalid SQL — must return an error, not silently succeed.
        let result = run_migration(&conn, "ALTER TABLE nonexistent_table ADD COLUMN foo TEXT");
        assert!(result.is_err(), "real errors must be propagated");
    }

    #[test]
    fn project_slug_matching_folds_case_hyphen_and_underscore() {
        let existing = vec!["field_notes".to_string()];
        assert_eq!(
            canonical_project_id_from_existing("field-notes", &existing),
            "field_notes"
        );
        assert_eq!(
            canonical_project_id_from_existing("Field_Notes", &existing),
            "field_notes"
        );
        assert_eq!(project_slug_match_key("Field-Notes"), "field_notes");
        assert_eq!(
            canonical_project_id_from_existing("fieldnotes", &existing),
            "fieldnotes"
        );
    }

    #[test]
    fn project_slug_matching_uses_natural_lowercase_when_ambiguous_or_new() {
        assert_eq!(
            canonical_project_id_from_existing("Field_Notes", ["field-notes", "field_notes"]),
            "field_notes"
        );
        assert_eq!(
            canonical_project_id_from_existing("New-Project", std::iter::empty::<&str>()),
            "new-project"
        );
    }

    #[test]
    fn known_field_notes_split_migration_merges_memory_project_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let conn = open_catalog(&path).expect("open_catalog");
        conn.execute_batch(
            "INSERT INTO knowledge
             (id, project_id, kind, state, title, payload_json, confidence,
              source_json, project_specific, created_at, updated_at, retrieval_text)
             VALUES
             ('k-from', 'field-notes', 'claim', 'active', 'from', '{}', 0.8, '{}', 1, 't', 't', 'from text'),
             ('k-to', 'field_notes', 'claim', 'active', 'to', '{}', 0.8, '{}', 1, 't', 't', 'to text');
             INSERT INTO imports
             (import_id, project_id, source_kind, source_hash, title, status, created_at, updated_at)
             VALUES ('i-from', 'field-notes', 'markdown', 'h1', 'from', 'queued', 't', 't'),
                    ('i-to', 'field_notes', 'markdown', 'h2', 'to', 'queued', 't', 't');
             INSERT INTO retrieval_log
             (project_id, query_text, result_ids, result_scores, result_count, latency_ms, created_at)
             VALUES ('field-notes', 'q1', '[]', '[]', 0, 1, 't'),
                    ('field_notes', 'q2', '[]', '[]', 0, 1, 't');
             INSERT INTO lesson_feedback
             (knowledge_id, signal, retrieval_id, project_id, created_at)
             VALUES ('k-from', 'co_session', NULL, 'field-notes', 't'),
                    ('k-to', 'co_session', NULL, 'field_notes', 't');
             INSERT INTO lesson_outcome_events
             (knowledge_id, session_id, outcome, source, created_at)
             VALUES ('k-from', 's', 'confirmed', 'test', 't');",
        )
        .expect("seed split rows");
        drop(conn);

        let conn = open_catalog(&path).expect("first migration");
        open_catalog(&path).expect("second migration idempotent");
        for table in ["knowledge", "imports", "retrieval_log", "lesson_feedback"] {
            let from_count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE project_id = 'field-notes'"),
                    [],
                    |row| row.get(0),
                )
                .expect("count old project");
            let to_count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE project_id = 'field_notes'"),
                    [],
                    |row| row.get(0),
                )
                .expect("count merged project");
            assert_eq!(from_count, 0, "{table} retained field-notes rows");
            assert_eq!(to_count, 2, "{table} did not merge both rows");
        }

        let outcome_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM lesson_outcome_events WHERE knowledge_id = 'k-from'",
                [],
                |row| row.get(0),
            )
            .expect("outcome count");
        assert_eq!(outcome_count, 1);
    }
}
