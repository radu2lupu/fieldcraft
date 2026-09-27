use chrono::Utc;
use fieldcraft_core::TrError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::schema::sqlite_error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportRecord {
    pub import_id: String,
    pub project_id: String,
    pub source_kind: String,
    pub source_uri: Option<String>,
    pub source_hash: String,
    pub title: String,
    pub status: String,
    pub extracted_count: i32,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct ImportStore {
    conn: Connection,
}

impl ImportStore {
    pub fn open(path: &std::path::Path) -> Result<Self, TrError> {
        Ok(Self {
            conn: crate::schema::open_catalog(path)?,
        })
    }

    pub fn from_connection(conn: Connection) -> Self {
        Self { conn }
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn create_import(&self, import: &ImportRecord) -> Result<(), TrError> {
        self.conn
            .execute(
                "INSERT INTO imports
                 (import_id, project_id, source_kind, source_uri, source_hash, title,
                  status, extracted_count, last_error, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(import_id) DO UPDATE SET
                   project_id=excluded.project_id,
                   source_kind=excluded.source_kind,
                   source_uri=excluded.source_uri,
                   source_hash=excluded.source_hash,
                   title=excluded.title,
                   status=excluded.status,
                   extracted_count=excluded.extracted_count,
                   last_error=excluded.last_error,
                   updated_at=excluded.updated_at",
                params![
                    import.import_id,
                    import.project_id,
                    import.source_kind,
                    import.source_uri,
                    import.source_hash,
                    import.title,
                    import.status,
                    import.extracted_count,
                    import.last_error,
                    import.created_at,
                    import.updated_at,
                ],
            )
            .map_err(sqlite_error)?;
        Ok(())
    }

    pub fn get_import(&self, import_id: &str) -> Result<Option<ImportRecord>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT import_id, project_id, source_kind, source_uri, source_hash, title,
                        status, extracted_count, last_error, created_at, updated_at
                 FROM imports WHERE import_id = ?1",
            )
            .map_err(sqlite_error)?;
        stmt.query_row([import_id], row_to_import)
            .optional()
            .map_err(sqlite_error)
    }

    pub fn update_import_status(
        &self,
        import_id: &str,
        status: &str,
        extracted_count: Option<i32>,
        error: Option<&str>,
    ) -> Result<(), TrError> {
        let now = Utc::now().to_rfc3339();
        if let Some(count) = extracted_count {
            self.conn
                .execute(
                    "UPDATE imports SET status = ?1, extracted_count = ?2, last_error = ?3, updated_at = ?4 WHERE import_id = ?5",
                    params![status, count, error, now, import_id],
                )
                .map_err(sqlite_error)?;
        } else {
            self.conn
                .execute(
                    "UPDATE imports SET status = ?1, last_error = ?2, updated_at = ?3 WHERE import_id = ?4",
                    params![status, error, now, import_id],
                )
                .map_err(sqlite_error)?;
        }
        Ok(())
    }

    pub fn list_imports(&self, project_id: &str) -> Result<Vec<ImportRecord>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT import_id, project_id, source_kind, source_uri, source_hash, title,
                        status, extracted_count, last_error, created_at, updated_at
                 FROM imports WHERE project_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map([project_id], row_to_import)
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }
}

fn row_to_import(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRecord> {
    Ok(ImportRecord {
        import_id: row.get(0)?,
        project_id: row.get(1)?,
        source_kind: row.get(2)?,
        source_uri: row.get(3)?,
        source_hash: row.get(4)?,
        title: row.get(5)?,
        status: row.get(6)?,
        extracted_count: row.get::<_, i64>(7)? as i32,
        last_error: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    fn make_import(id: &str, project_id: &str) -> ImportRecord {
        let now = Utc::now().to_rfc3339();
        ImportRecord {
            import_id: id.to_string(),
            project_id: project_id.to_string(),
            source_kind: "session".to_string(),
            source_uri: Some("file:///path/to/session.jsonl".to_string()),
            source_hash: "abc123".to_string(),
            title: format!("Import {id}"),
            status: "pending".to_string(),
            extracted_count: 0,
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[test]
    fn create_and_get_import() {
        let dir = tempdir().unwrap();
        let store = ImportStore::open(&dir.path().join("memory.sqlite3")).unwrap();
        let import = make_import("imp-1", "proj-a");
        store.create_import(&import).unwrap();
        let loaded = store.get_import("imp-1").unwrap().expect("should exist");
        assert_eq!(loaded.import_id, "imp-1");
        assert_eq!(loaded.project_id, "proj-a");
        assert_eq!(loaded.status, "pending");
        assert_eq!(loaded.extracted_count, 0);
    }

    #[test]
    fn get_missing_import_returns_none() {
        let dir = tempdir().unwrap();
        let store = ImportStore::open(&dir.path().join("memory.sqlite3")).unwrap();
        assert!(store.get_import("no-such-id").unwrap().is_none());
    }

    #[test]
    fn update_import_status_with_count() {
        let dir = tempdir().unwrap();
        let store = ImportStore::open(&dir.path().join("memory.sqlite3")).unwrap();
        store.create_import(&make_import("imp-2", "proj")).unwrap();
        store
            .update_import_status("imp-2", "succeeded", Some(42), None)
            .unwrap();
        let loaded = store.get_import("imp-2").unwrap().unwrap();
        assert_eq!(loaded.status, "succeeded");
        assert_eq!(loaded.extracted_count, 42);
        assert!(loaded.last_error.is_none());
    }

    #[test]
    fn update_import_status_with_error() {
        let dir = tempdir().unwrap();
        let store = ImportStore::open(&dir.path().join("memory.sqlite3")).unwrap();
        store.create_import(&make_import("imp-3", "proj")).unwrap();
        store
            .update_import_status("imp-3", "failed", None, Some("parse error"))
            .unwrap();
        let loaded = store.get_import("imp-3").unwrap().unwrap();
        assert_eq!(loaded.status, "failed");
        assert_eq!(loaded.last_error.as_deref(), Some("parse error"));
    }

    #[test]
    fn list_imports_by_project() {
        let dir = tempdir().unwrap();
        let store = ImportStore::open(&dir.path().join("memory.sqlite3")).unwrap();
        store.create_import(&make_import("i1", "proj-a")).unwrap();
        store.create_import(&make_import("i2", "proj-a")).unwrap();
        store.create_import(&make_import("i3", "proj-b")).unwrap();
        let a = store.list_imports("proj-a").unwrap();
        assert_eq!(a.len(), 2);
        let b = store.list_imports("proj-b").unwrap();
        assert_eq!(b.len(), 1);
        let c = store.list_imports("proj-c").unwrap();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn upsert_on_conflict() {
        let dir = tempdir().unwrap();
        let store = ImportStore::open(&dir.path().join("memory.sqlite3")).unwrap();
        let mut imp = make_import("imp-upsert", "proj");
        store.create_import(&imp).unwrap();
        imp.status = "running".to_string();
        store.create_import(&imp).unwrap(); // should upsert, not error
        let loaded = store.get_import("imp-upsert").unwrap().unwrap();
        assert_eq!(loaded.status, "running");
    }
}
