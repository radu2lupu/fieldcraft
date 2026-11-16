//! Items: things apps push (saved pages, bookmarks, later mail and agent
//! documents), stored apart from lessons.
//!
//! Each source has its own SQLite file next to the main catalog
//! (`items-<source>.sqlite3`), so bulk ingest never queues behind lesson
//! writes, lesson maintenance (cruft, dream, forgetting) cannot reach items,
//! and forgetting a source is deleting its file.
//!
//! An item is keyed by `(source, external_id)`; writes are idempotent
//! upserts. Its title and body are cut into passages with I Want Things'
//! rule ([`item_passages`]) so vectors made here and in IWT are comparable.
//! Passages are re-cut, and their vectors dropped, only when the content hash
//! (title and body) changes. FTS5 rows and vectors follow the passages
//! through triggers.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fieldcraft_core::TrError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::compute_checksum;
use crate::repository::{blob_to_vector, vector_to_blob};
use crate::schema::{configure_sqlite, sqlite_error};

/// Passage length in characters (IWT `LocalSearchContent.passageCharacters`).
pub const ITEM_PASSAGE_CHARS: usize = 800;
/// Overlap between passages (IWT `LocalSearchContent.overlapCharacters`).
pub const ITEM_PASSAGE_OVERLAP: usize = 160;
/// Title characters prefixed to every passage (IWT `passages(title:body:)`).
pub const ITEM_TITLE_PREFIX_CHARS: usize = 160;
/// Body characters cut into passages (IWT `LocalSearchContent.maximumCharacters`).
pub const ITEM_MAX_PASSAGE_BODY_CHARS: usize = 16_000;
/// Largest body accepted per item; the rest of the body is still stored
/// only up to this size.
pub const ITEM_MAX_BODY_CHARS: usize = 1_000_000;
/// Operations accepted per batch.
pub const ITEM_MAX_BATCH_OPS: usize = 1_000;

const ITEMS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS items (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    external_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    url TEXT,
    author TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    source_updated_at TEXT,
    scope TEXT NOT NULL DEFAULT 'private',
    display_safe INTEGER NOT NULL DEFAULT 0,
    content_hash TEXT NOT NULL,
    seq INTEGER NOT NULL,
    UNIQUE (source, external_id)
);
CREATE INDEX IF NOT EXISTS idx_items_kind ON items(kind);

CREATE TABLE IF NOT EXISTS item_chunks (
    id INTEGER PRIMARY KEY,
    item_id TEXT NOT NULL,
    ord INTEGER NOT NULL,
    text TEXT NOT NULL,
    UNIQUE (item_id, ord)
);

CREATE TABLE IF NOT EXISTS item_chunk_vectors (
    chunk_id INTEGER NOT NULL,
    tag TEXT NOT NULL,
    dims INTEGER NOT NULL,
    vector BLOB NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (chunk_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_item_chunk_vectors_tag ON item_chunk_vectors(tag);

CREATE VIRTUAL TABLE IF NOT EXISTS item_chunks_fts USING fts5(
    title, text, url, tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TABLE IF NOT EXISTS item_source_meta (
    key TEXT PRIMARY KEY,
    value TEXT
);

CREATE TRIGGER IF NOT EXISTS item_chunks_fts_insert AFTER INSERT ON item_chunks BEGIN
    INSERT INTO item_chunks_fts (rowid, title, text, url)
    SELECT new.id, i.title, substr(new.text, instr(new.text, char(10)) + 1), COALESCE(i.url, '')
    FROM items i WHERE i.id = new.item_id;
END;
CREATE TRIGGER IF NOT EXISTS item_chunks_derived_delete AFTER DELETE ON item_chunks BEGIN
    DELETE FROM item_chunks_fts WHERE rowid = old.id;
    DELETE FROM item_chunk_vectors WHERE chunk_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS items_chunks_delete AFTER DELETE ON items BEGIN
    DELETE FROM item_chunks WHERE item_id = old.id;
END;
CREATE TRIGGER IF NOT EXISTS items_fts_title_url AFTER UPDATE OF title, url ON items BEGIN
    UPDATE item_chunks_fts SET title = new.title, url = COALESCE(new.url, '')
    WHERE rowid IN (SELECT id FROM item_chunks WHERE item_id = new.id);
END;
"#;

/// I Want Things' passage rule (`LocalSearchContent.passages(title:body:)`):
/// each passage is the first 160 title characters, a newline, then up to 800
/// body characters; passages overlap by 160; only the first 16,000 body
/// characters are cut; an empty body gives one title-only passage.
pub fn item_passages(title: &str, body: &str) -> Vec<String> {
    let prefix: String = title
        .chars()
        .take(ITEM_TITLE_PREFIX_CHARS)
        .collect::<String>()
        + "\n";
    let chars: Vec<char> = body.chars().take(ITEM_MAX_PASSAGE_BODY_CHARS).collect();
    if chars.is_empty() {
        return vec![prefix];
    }
    let mut passages = Vec::new();
    let mut offset = 0;
    while offset < chars.len() {
        let end = (offset + ITEM_PASSAGE_CHARS).min(chars.len());
        let mut passage = prefix.clone();
        passage.extend(&chars[offset..end]);
        passages.push(passage);
        if offset + ITEM_PASSAGE_CHARS >= chars.len() {
            break;
        }
        offset += ITEM_PASSAGE_CHARS - ITEM_PASSAGE_OVERLAP;
    }
    passages
}

/// A source id names a file, so it is restricted to `[a-z0-9][a-z0-9_-]{0,63}`.
pub fn validate_source_id(source: &str) -> Result<(), TrError> {
    let ok = !source.is_empty()
        && source.len() <= 64
        && source
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && source
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(TrError::InvalidRequest {
            message: format!(
                "source {source:?} must match [a-z0-9][a-z0-9_-]{{0,63}} (it names a file)"
            ),
        })
    }
}

/// `<data_dir>/items-<source>.sqlite3`.
pub fn items_db_path(data_dir: &Path, source: &str) -> PathBuf {
    data_dir.join(format!("items-{source}.sqlite3"))
}

/// Sources that have an item file in `data_dir`, sorted.
pub fn list_item_sources(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return Vec::new();
    };
    let mut sources: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let source = name.strip_prefix("items-")?.strip_suffix(".sqlite3")?;
            validate_source_id(source).ok()?;
            Some(source.to_string())
        })
        .collect();
    sources.sort();
    sources
}

/// Remove a source's item file (with its WAL and shared-memory files).
/// Returns whether the main file existed. Close every connection first.
pub fn remove_item_source_files(data_dir: &Path, source: &str) -> Result<bool, TrError> {
    validate_source_id(source)?;
    let path = items_db_path(data_dir, source);
    let existed = path.exists();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(TrError::Storage {
                    message: format!("remove {}: {e}", file.display()),
                });
            }
        }
    }
    Ok(existed)
}

/// One upsert as a connector sends it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemUpsert {
    pub external_id: String,
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    /// RFC 3339; defaults to the first time the server saw the item.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC 3339; the source's own modification time.
    #[serde(default)]
    pub source_updated_at: Option<String>,
    /// `private` (default) or `shared`.
    #[serde(default)]
    pub scope: Option<String>,
    /// Safe to show while the screen is shared (default false).
    #[serde(default)]
    pub display_safe: Option<bool>,
}

/// One operation of a batch, applied in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ItemOp {
    Upsert(ItemUpsert),
    Delete { external_id: String },
}

impl ItemOp {
    pub fn external_id(&self) -> &str {
        match self {
            Self::Upsert(item) => &item.external_id,
            Self::Delete { external_id } => external_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemOpStatus {
    Created,
    /// Title or body changed: passages re-cut, vectors to be re-made.
    Updated,
    /// Only metadata changed; passages and vectors kept.
    MetadataUpdated,
    Unchanged,
    Deleted,
    NotFound,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemOpResult {
    pub external_id: String,
    pub status: ItemOpStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchOutcome {
    pub results: Vec<ItemOpResult>,
    /// The connector's own position, as last stored (see [`ItemStore::apply_batch`]).
    pub cursor: Option<String>,
    /// Server change sequence of this source after the batch.
    pub seq: i64,
}

/// A stored item, as search shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemRow {
    pub id: String,
    pub source: String,
    pub external_id: String,
    pub kind: String,
    pub title: String,
    pub url: Option<String>,
    pub author: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub source_updated_at: Option<String>,
    pub scope: String,
    pub display_safe: bool,
}

/// Row counts of one source, for tests and status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ItemCounts {
    pub items: i64,
    pub chunks: i64,
    pub fts_rows: i64,
    pub vectors: i64,
}

/// One chunk's best BM25 match for an item.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemTextHit {
    pub item_id: String,
    pub chunk_id: i64,
    /// FTS5 `bm25()`: lower is better.
    pub bm25: f64,
    pub snippet: String,
}

/// A chunk vector of one tag.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemChunkVector {
    pub item_id: String,
    pub chunk_id: i64,
    pub vector: Vec<f32>,
}

/// Stable id of `(source, external_id)`.
pub fn item_id(source: &str, external_id: &str) -> String {
    compute_checksum(&format!("{source}\u{0}{external_id}"))[..32].to_string()
}

fn content_hash(title: &str, body: &str) -> String {
    compute_checksum(&format!("{title}\u{0}{body}"))
}

fn invalid(message: impl Into<String>) -> TrError {
    TrError::InvalidRequest {
        message: message.into(),
    }
}

fn normalize_time(field: &str, value: Option<&str>) -> Result<Option<String>, TrError> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.with_timezone(&Utc).to_rfc3339()))
            .map_err(|e| invalid(format!("{field} {v:?} is not RFC 3339: {e}"))),
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The row an upsert asks for, validated.
struct NewItem {
    external_id: String,
    kind: String,
    title: String,
    body: String,
    url: Option<String>,
    author: Option<String>,
    created_at: Option<String>,
    source_updated_at: Option<String>,
    scope: String,
    display_safe: bool,
    content_hash: String,
}

fn validate_upsert(item: &ItemUpsert) -> Result<NewItem, TrError> {
    let external_id = item.external_id.trim();
    if external_id.is_empty() || external_id.len() > 1024 {
        return Err(invalid("external_id must be 1 to 1024 bytes"));
    }
    let kind = item.kind.trim();
    if kind.is_empty() || kind.len() > 64 {
        return Err(invalid("kind must be 1 to 64 bytes"));
    }
    if item.title.chars().count() > 4096 {
        return Err(invalid("title is longer than 4096 characters"));
    }
    if item.body.chars().count() > ITEM_MAX_BODY_CHARS {
        return Err(invalid(format!(
            "body is longer than {ITEM_MAX_BODY_CHARS} characters"
        )));
    }
    let scope = non_empty(item.scope.as_deref()).unwrap_or_else(|| "private".to_string());
    if scope != "private" && scope != "shared" {
        return Err(invalid("scope must be private or shared"));
    }
    Ok(NewItem {
        external_id: external_id.to_string(),
        kind: kind.to_string(),
        title: item.title.clone(),
        body: item.body.clone(),
        url: non_empty(item.url.as_deref()),
        author: non_empty(item.author.as_deref()),
        created_at: normalize_time("created_at", item.created_at.as_deref())?,
        source_updated_at: normalize_time("source_updated_at", item.source_updated_at.as_deref())?,
        scope,
        display_safe: item.display_safe.unwrap_or(false),
        content_hash: content_hash(&item.title, &item.body),
    })
}

/// The items of one source, in that source's own SQLite file.
pub struct ItemStore {
    conn: Connection,
    source: String,
}

impl ItemStore {
    /// Open (or create) `<data_dir>/items-<source>.sqlite3`.
    pub fn open(data_dir: &Path, source: &str) -> Result<Self, TrError> {
        validate_source_id(source)?;
        std::fs::create_dir_all(data_dir).map_err(|e| TrError::Storage {
            message: e.to_string(),
        })?;
        Self::open_path(&items_db_path(data_dir, source), source)
    }

    pub fn open_path(path: &Path, source: &str) -> Result<Self, TrError> {
        validate_source_id(source)?;
        configure_sqlite();
        let conn = Connection::open(path).map_err(sqlite_error)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .map_err(sqlite_error)?;
        conn.execute_batch(ITEMS_SCHEMA_SQL).map_err(sqlite_error)?;
        Ok(Self {
            conn,
            source: source.to_string(),
        })
    }

    /// A read-only connection to an existing source file, for the query path.
    /// Never creates the file; never checkpoints or removes the WAL on close.
    pub fn open_reader(data_dir: &Path, source: &str) -> Result<Self, TrError> {
        validate_source_id(source)?;
        configure_sqlite();
        let conn = Connection::open_with_flags(
            items_db_path(data_dir, source),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sqlite_error)?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")
            .map_err(sqlite_error)?;
        Ok(Self {
            conn,
            source: source.to_string(),
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn meta(&self, key: &str) -> Result<Option<String>, TrError> {
        self.conn
            .query_row(
                "SELECT value FROM item_source_meta WHERE key = ?1",
                [key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(sqlite_error)
    }

    /// Server change sequence: grows by one for every item created, changed or deleted.
    pub fn seq(&self) -> Result<i64, TrError> {
        Ok(self.meta("seq")?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// The connector cursor stored by the last batch that sent one.
    pub fn cursor(&self) -> Result<Option<String>, TrError> {
        self.meta("cursor")
    }

    /// Apply `ops` in order in one transaction. A bad operation is reported in
    /// its result and does not stop the others; a storage failure rolls the
    /// whole batch back. `cursor`, when given, is stored with the batch so a
    /// connector can resume from it.
    pub fn apply_batch(
        &mut self,
        ops: &[ItemOp],
        cursor: Option<&str>,
    ) -> Result<BatchOutcome, TrError> {
        if ops.len() > ITEM_MAX_BATCH_OPS {
            return Err(invalid(format!(
                "a batch holds at most {ITEM_MAX_BATCH_OPS} operations"
            )));
        }
        let source = self.source.clone();
        let tx = self.conn.transaction().map_err(sqlite_error)?;
        let mut seq: i64 = tx
            .query_row(
                "SELECT value FROM item_source_meta WHERE key = 'seq'",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sqlite_error)?
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut results = Vec::with_capacity(ops.len());
        for op in ops {
            let outcome = match op {
                ItemOp::Upsert(item) => match validate_upsert(item) {
                    Ok(new) => upsert_in(&tx, &source, &new, seq + 1),
                    Err(err) => Err(err),
                },
                ItemOp::Delete { external_id } => delete_in(&tx, &source, external_id.trim()),
            };
            match outcome {
                Ok(status) => {
                    if !matches!(status, ItemOpStatus::Unchanged | ItemOpStatus::NotFound) {
                        seq += 1;
                    }
                    results.push(ItemOpResult {
                        external_id: op.external_id().trim().to_string(),
                        status,
                        error: None,
                    });
                }
                Err(TrError::InvalidRequest { message }) => results.push(ItemOpResult {
                    external_id: op.external_id().trim().to_string(),
                    status: ItemOpStatus::Error,
                    error: Some(message),
                }),
                Err(other) => return Err(other),
            }
        }
        tx.execute(
            "INSERT INTO item_source_meta (key, value) VALUES ('seq', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [seq.to_string()],
        )
        .map_err(sqlite_error)?;
        if let Some(cursor) = cursor {
            tx.execute(
                "INSERT INTO item_source_meta (key, value) VALUES ('cursor', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [cursor],
            )
            .map_err(sqlite_error)?;
        }
        tx.commit().map_err(sqlite_error)?;
        Ok(BatchOutcome {
            results,
            cursor: self.cursor()?,
            seq,
        })
    }

    /// Delete every item of this source with its chunks, FTS rows and vectors.
    pub fn delete_all(&mut self) -> Result<usize, TrError> {
        let tx = self.conn.transaction().map_err(sqlite_error)?;
        let n = tx.execute("DELETE FROM items", []).map_err(sqlite_error)?;
        tx.execute("DELETE FROM item_source_meta", [])
            .map_err(sqlite_error)?;
        tx.commit().map_err(sqlite_error)?;
        Ok(n)
    }

    pub fn counts(&self) -> Result<ItemCounts, TrError> {
        self.conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM items), (SELECT COUNT(*) FROM item_chunks),
                        (SELECT COUNT(*) FROM item_chunks_fts),
                        (SELECT COUNT(*) FROM item_chunk_vectors)",
                [],
                |r| {
                    Ok(ItemCounts {
                        items: r.get(0)?,
                        chunks: r.get(1)?,
                        fts_rows: r.get(2)?,
                        vectors: r.get(3)?,
                    })
                },
            )
            .map_err(sqlite_error)
    }

    /// Chunks without a vector under `tag`, oldest first: `(chunk_id, text)`.
    pub fn chunks_missing_vector(
        &self,
        tag: &str,
        limit: usize,
    ) -> Result<Vec<(i64, String)>, TrError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT c.id, c.text FROM item_chunks c
                 WHERE NOT EXISTS (SELECT 1 FROM item_chunk_vectors v
                                   WHERE v.chunk_id = c.id AND v.tag = ?1)
                 ORDER BY c.id LIMIT ?2",
            )
            .map_err(sqlite_error)?;
        stmt.query_map(params![tag, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)
    }

    /// Store vectors under `tag`. Chunks deleted meanwhile (the item changed
    /// while it was being embedded) are skipped.
    pub fn store_chunk_vectors(
        &mut self,
        tag: &str,
        vectors: &[(i64, Vec<f32>)],
    ) -> Result<usize, TrError> {
        let now = Utc::now().to_rfc3339();
        let tx = self.conn.transaction().map_err(sqlite_error)?;
        let mut stored = 0;
        for (chunk_id, vector) in vectors {
            let bytes = vector_to_blob(vector);
            stored += tx
                .execute(
                    "INSERT INTO item_chunk_vectors (chunk_id, tag, dims, vector, created_at)
                     SELECT ?1, ?2, ?3, ?4, ?5 WHERE EXISTS (SELECT 1 FROM item_chunks WHERE id = ?1)
                     ON CONFLICT (chunk_id, tag) DO UPDATE SET
                       dims = excluded.dims, vector = excluded.vector,
                       created_at = excluded.created_at",
                    params![chunk_id, tag, vector.len() as i64, bytes, now],
                )
                .map_err(sqlite_error)?;
        }
        tx.commit().map_err(sqlite_error)?;
        Ok(stored)
    }

    /// BM25 over passages (title weighted 4, passage text 1, URL 0.5) for an
    /// FTS5 `match_expr`; the best passage per item, best first. The FTS
    /// `text` column holds the passage without its title line (the title has
    /// its own column), so snippets show body text.
    pub fn text_search(
        &self,
        match_expr: &str,
        kinds: &[String],
        limit: usize,
    ) -> Result<Vec<ItemTextHit>, TrError> {
        let kind_filter = kind_clause(kinds, 3);
        let sql = format!(
            "SELECT c.item_id, c.id, bm25(item_chunks_fts, 4.0, 1.0, 0.5) AS score,
                    snippet(item_chunks_fts, 1, '', '', '…', 24)
             FROM item_chunks_fts
             JOIN item_chunks c ON c.id = item_chunks_fts.rowid
             JOIN items i ON i.id = c.item_id
             WHERE item_chunks_fts MATCH ?1 {kind_filter}
             ORDER BY score LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let mut values: Vec<rusqlite::types::Value> = vec![
            match_expr.to_string().into(),
            // Several passages of one item can match; fetch more, keep the best.
            ((limit * 4) as i64).into(),
        ];
        values.extend(kinds.iter().map(|k| k.clone().into()));
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values), |r| {
                Ok(ItemTextHit {
                    item_id: r.get(0)?,
                    chunk_id: r.get(1)?,
                    bm25: r.get(2)?,
                    snippet: r.get(3)?,
                })
            })
            .map_err(sqlite_error)?;
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for row in rows {
            let hit = row.map_err(sqlite_error)?;
            if seen.insert(hit.item_id.clone()) {
                hits.push(hit);
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    /// Every chunk vector under `tag` (items of `kinds`, all when empty).
    pub fn chunk_vectors(
        &self,
        tag: &str,
        kinds: &[String],
    ) -> Result<Vec<ItemChunkVector>, TrError> {
        let kind_filter = kind_clause(kinds, 2);
        let sql = format!(
            "SELECT c.item_id, c.id, v.vector FROM item_chunk_vectors v
             JOIN item_chunks c ON c.id = v.chunk_id
             JOIN items i ON i.id = c.item_id
             WHERE v.tag = ?1 {kind_filter}"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(sqlite_error)?;
        let mut values: Vec<rusqlite::types::Value> = vec![tag.to_string().into()];
        values.extend(kinds.iter().map(|k| k.clone().into()));
        stmt.query_map(rusqlite::params_from_iter(values), |r| {
            let blob: Vec<u8> = r.get(2)?;
            Ok(ItemChunkVector {
                item_id: r.get(0)?,
                chunk_id: r.get(1)?,
                vector: blob_to_vector(&blob),
            })
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)
    }

    pub fn chunk_text(&self, chunk_id: i64) -> Result<Option<String>, TrError> {
        self.conn
            .query_row(
                "SELECT text FROM item_chunks WHERE id = ?1",
                [chunk_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sqlite_error)
    }

    pub fn get(&self, external_id: &str) -> Result<Option<ItemRow>, TrError> {
        self.get_by_id(&item_id(&self.source, external_id))
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<ItemRow>, TrError> {
        self.conn
            .query_row(
                "SELECT id, source, external_id, kind, title, url, author, created_at,
                        updated_at, source_updated_at, scope, display_safe
                 FROM items WHERE id = ?1",
                [id],
                |r| {
                    Ok(ItemRow {
                        id: r.get(0)?,
                        source: r.get(1)?,
                        external_id: r.get(2)?,
                        kind: r.get(3)?,
                        title: r.get(4)?,
                        url: r.get(5)?,
                        author: r.get(6)?,
                        created_at: r.get(7)?,
                        updated_at: r.get(8)?,
                        source_updated_at: r.get(9)?,
                        scope: r.get(10)?,
                        display_safe: r.get::<_, i64>(11)? != 0,
                    })
                },
            )
            .optional()
            .map_err(sqlite_error)
    }

    /// Passage texts of an item, in order.
    pub fn chunk_texts(&self, external_id: &str) -> Result<Vec<String>, TrError> {
        let mut stmt = self
            .conn
            .prepare("SELECT text FROM item_chunks WHERE item_id = ?1 ORDER BY ord")
            .map_err(sqlite_error)?;
        stmt.query_map([item_id(&self.source, external_id)], |r| r.get(0))
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)
    }
}

/// `AND i.kind IN (?n, ...)` with positional parameters from `first`.
fn kind_clause(kinds: &[String], first: usize) -> String {
    if kinds.is_empty() {
        return String::new();
    }
    let marks: Vec<String> = (0..kinds.len())
        .map(|n| format!("?{}", first + n))
        .collect();
    format!("AND i.kind IN ({})", marks.join(", "))
}

fn upsert_in(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    new: &NewItem,
    seq: i64,
) -> Result<ItemOpStatus, TrError> {
    let id = item_id(source, &new.external_id);
    let now = Utc::now().to_rfc3339();
    type Existing = (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        String,
        i64,
    );
    let existing: Option<Existing> = tx
        .query_row(
            "SELECT content_hash, kind, url, author, created_at, source_updated_at, scope,
                    display_safe
             FROM items WHERE id = ?1",
            [&id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;

    let Some((hash, kind, url, author, created_at, source_updated_at, scope, display_safe)) =
        existing
    else {
        let created_at = new.created_at.clone().unwrap_or_else(|| now.clone());
        tx.execute(
            "INSERT INTO items (id, source, external_id, kind, title, body, url, author,
                                created_at, updated_at, source_updated_at, scope, display_safe,
                                content_hash, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                id,
                source,
                new.external_id,
                new.kind,
                new.title,
                new.body,
                new.url,
                new.author,
                created_at,
                now,
                new.source_updated_at,
                new.scope,
                new.display_safe as i64,
                new.content_hash,
                seq
            ],
        )
        .map_err(sqlite_error)?;
        insert_chunks(tx, &id, &new.title, &new.body)?;
        return Ok(ItemOpStatus::Created);
    };

    let created_at = new.created_at.clone().unwrap_or(created_at);
    let content_changed = hash != new.content_hash;
    let metadata_changed = kind != new.kind
        || url != new.url
        || author != new.author
        || new.created_at.as_ref().is_some_and(|c| *c != created_at)
        || source_updated_at != new.source_updated_at
        || scope != new.scope
        || (display_safe != 0) != new.display_safe;
    if !content_changed && !metadata_changed {
        return Ok(ItemOpStatus::Unchanged);
    }
    tx.execute(
        "UPDATE items SET kind = ?2, title = ?3, body = ?4, url = ?5, author = ?6,
                          created_at = ?7, updated_at = ?8, source_updated_at = ?9, scope = ?10,
                          display_safe = ?11, content_hash = ?12, seq = ?13
         WHERE id = ?1",
        params![
            id,
            new.kind,
            new.title,
            new.body,
            new.url,
            new.author,
            created_at,
            now,
            new.source_updated_at,
            new.scope,
            new.display_safe as i64,
            new.content_hash,
            seq
        ],
    )
    .map_err(sqlite_error)?;
    if !content_changed {
        return Ok(ItemOpStatus::MetadataUpdated);
    }
    tx.execute("DELETE FROM item_chunks WHERE item_id = ?1", [&id])
        .map_err(sqlite_error)?;
    insert_chunks(tx, &id, &new.title, &new.body)?;
    Ok(ItemOpStatus::Updated)
}

fn insert_chunks(
    tx: &rusqlite::Transaction<'_>,
    item_id: &str,
    title: &str,
    body: &str,
) -> Result<(), TrError> {
    let mut stmt = tx
        .prepare_cached("INSERT INTO item_chunks (item_id, ord, text) VALUES (?1, ?2, ?3)")
        .map_err(sqlite_error)?;
    for (ord, text) in item_passages(title, body).iter().enumerate() {
        stmt.execute(params![item_id, ord as i64, text])
            .map_err(sqlite_error)?;
    }
    Ok(())
}

fn delete_in(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    external_id: &str,
) -> Result<ItemOpStatus, TrError> {
    if external_id.is_empty() {
        return Err(invalid("external_id must not be empty"));
    }
    let n = tx
        .execute(
            "DELETE FROM items WHERE id = ?1",
            [item_id(source, external_id)],
        )
        .map_err(sqlite_error)?;
    Ok(if n > 0 {
        ItemOpStatus::Deleted
    } else {
        ItemOpStatus::NotFound
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn upsert(external_id: &str, title: &str, body: &str) -> ItemOp {
        ItemOp::Upsert(ItemUpsert {
            external_id: external_id.into(),
            kind: "save".into(),
            title: title.into(),
            body: body.into(),
            url: Some(format!("https://example.com/{external_id}")),
            ..Default::default()
        })
    }

    fn statuses(outcome: &BatchOutcome) -> Vec<ItemOpStatus> {
        outcome.results.iter().map(|r| r.status).collect()
    }

    fn dump(store: &ItemStore) -> Vec<(String, String, String, String)> {
        let mut stmt = store
            .connection()
            .prepare("SELECT id, title, body, content_hash FROM items ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn passages_follow_iwt_rule() {
        assert_eq!(item_passages("T", ""), vec!["T\n".to_string()]);
        let body: String = "a".repeat(800);
        assert_eq!(item_passages("T", &body).len(), 1);
        let body: String = (0..2000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let passages = item_passages("Title", &body);
        // Offsets 0, 640, 1280 (the third reaches the end).
        assert_eq!(passages.len(), 3);
        assert_eq!(passages[0].chars().count(), "Title\n".len() + 800);
        let chars: Vec<char> = body.chars().collect();
        let second: String = chars[640..1440].iter().collect();
        assert_eq!(passages[1], format!("Title\n{second}"));
        let third: String = chars[1280..].iter().collect();
        assert_eq!(passages[2], format!("Title\n{third}"));
        // Long titles are cut to 160 characters; bodies past 16,000 are not cut.
        let long_title = "t".repeat(500);
        assert!(item_passages(&long_title, "x")[0].starts_with(&"t".repeat(160)));
        assert!(!item_passages(&long_title, "x")[0].starts_with(&"t".repeat(161)));
        let huge = "é".repeat(40_000);
        let p = item_passages("", &huge);
        assert_eq!(p.len(), 25); // ceil((16000 - 800) / 640) + 1
    }

    #[test]
    fn source_ids_are_file_safe() {
        assert!(validate_source_id("iwt").is_ok());
        assert!(validate_source_id("mail-1_a").is_ok());
        for bad in ["", "../x", "IWT", "-x", "a/b", &"a".repeat(65)] {
            assert!(validate_source_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn same_batch_twice_gives_same_rows() {
        let dir = tempdir().unwrap();
        let mut store = ItemStore::open(dir.path(), "iwt").unwrap();
        let batch = vec![
            upsert("a", "Rust ownership", "Borrowing rules explained"),
            upsert("b", "Espresso grinder", &"burr ".repeat(400)),
        ];
        let first = store.apply_batch(&batch, Some("c1")).unwrap();
        assert_eq!(
            statuses(&first),
            vec![ItemOpStatus::Created, ItemOpStatus::Created]
        );
        let rows = dump(&store);
        let counts = store.counts().unwrap();
        let second = store.apply_batch(&batch, Some("c1")).unwrap();
        assert_eq!(
            statuses(&second),
            vec![ItemOpStatus::Unchanged, ItemOpStatus::Unchanged]
        );
        assert_eq!(dump(&store), rows);
        assert_eq!(store.counts().unwrap(), counts);
        assert_eq!(second.seq, first.seq);
        assert_eq!(second.cursor.as_deref(), Some("c1"));
        assert_eq!(counts.items, 2);
        assert_eq!(counts.chunks, counts.fts_rows);
        assert_eq!(
            counts.chunks,
            1 + item_passages("", &"burr ".repeat(400)).len() as i64
        );
    }

    #[test]
    fn content_update_recuts_chunks_and_drops_vectors_metadata_update_keeps_them() {
        let dir = tempdir().unwrap();
        let mut store = ItemStore::open(dir.path(), "iwt").unwrap();
        store
            .apply_batch(&[upsert("a", "Old title", "old body words")], None)
            .unwrap();
        let pending = store.chunks_missing_vector("t:m:2", 100).unwrap();
        store
            .store_chunk_vectors(
                "t:m:2",
                &pending
                    .iter()
                    .map(|(id, _)| (*id, vec![1.0, 0.0]))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert_eq!(store.counts().unwrap().vectors, 1);

        // Metadata only: vectors stay.
        let mut meta = ItemUpsert {
            external_id: "a".into(),
            kind: "bookmark".into(),
            title: "Old title".into(),
            body: "old body words".into(),
            url: Some("https://example.com/a".into()),
            ..Default::default()
        };
        let out = store
            .apply_batch(&[ItemOp::Upsert(meta.clone())], None)
            .unwrap();
        assert_eq!(statuses(&out), vec![ItemOpStatus::MetadataUpdated]);
        assert_eq!(store.counts().unwrap().vectors, 1);
        assert!(store.chunks_missing_vector("t:m:2", 10).unwrap().is_empty());

        // New content: new passages, old vectors and FTS rows gone.
        meta.title = "New title".into();
        meta.body = "fresh body text".into();
        let out = store.apply_batch(&[ItemOp::Upsert(meta)], None).unwrap();
        assert_eq!(statuses(&out), vec![ItemOpStatus::Updated]);
        let counts = store.counts().unwrap();
        assert_eq!(counts.vectors, 0);
        assert_eq!(counts.chunks, 1);
        assert_eq!(counts.fts_rows, 1);
        assert_eq!(
            store.chunk_texts("a").unwrap(),
            vec!["New title\nfresh body text"]
        );
        assert!(store.text_search("\"old\"*", &[], 10).unwrap().is_empty());
        assert_eq!(store.text_search("\"fresh\"*", &[], 10).unwrap().len(), 1);
        assert_eq!(store.get("a").unwrap().unwrap().kind, "bookmark");
        assert_eq!(store.chunks_missing_vector("t:m:2", 10).unwrap().len(), 1);
    }

    #[test]
    fn delete_removes_chunks_vectors_and_fts_rows() {
        let dir = tempdir().unwrap();
        let mut store = ItemStore::open(dir.path(), "iwt").unwrap();
        store
            .apply_batch(
                &[
                    upsert("a", "Keep me", "alpha"),
                    upsert("b", "Drop me", &"beta ".repeat(500)),
                ],
                None,
            )
            .unwrap();
        let pending = store.chunks_missing_vector("tag", 100).unwrap();
        let vectors: Vec<(i64, Vec<f32>)> = pending
            .iter()
            .map(|(id, _)| (*id, vec![0.5, 0.5]))
            .collect();
        store.store_chunk_vectors("tag", &vectors).unwrap();
        let out = store
            .apply_batch(
                &[
                    ItemOp::Delete {
                        external_id: "b".into(),
                    },
                    ItemOp::Delete {
                        external_id: "zzz".into(),
                    },
                ],
                None,
            )
            .unwrap();
        assert_eq!(
            statuses(&out),
            vec![ItemOpStatus::Deleted, ItemOpStatus::NotFound]
        );
        assert_eq!(
            store.counts().unwrap(),
            ItemCounts {
                items: 1,
                chunks: 1,
                fts_rows: 1,
                vectors: 1
            }
        );
        assert!(store.text_search("\"beta\"", &[], 10).unwrap().is_empty());
        assert!(store.get("b").unwrap().is_none());
    }

    #[test]
    fn bad_operations_report_errors_without_stopping_the_batch() {
        let dir = tempdir().unwrap();
        let mut store = ItemStore::open(dir.path(), "iwt").unwrap();
        let mut bad_time = ItemUpsert {
            external_id: "t".into(),
            kind: "save".into(),
            created_at: Some("yesterday".into()),
            ..Default::default()
        };
        let out = store
            .apply_batch(
                &[
                    upsert("", "x", "y"),
                    ItemOp::Upsert(bad_time.clone()),
                    upsert("ok", "fine", "body"),
                ],
                None,
            )
            .unwrap();
        assert_eq!(
            statuses(&out),
            vec![
                ItemOpStatus::Error,
                ItemOpStatus::Error,
                ItemOpStatus::Created
            ]
        );
        assert_eq!(out.seq, 1);
        bad_time.created_at = Some("2026-11-01T10:00:00+02:00".into());
        store
            .apply_batch(&[ItemOp::Upsert(bad_time)], None)
            .unwrap();
        assert_eq!(
            store.get("t").unwrap().unwrap().created_at,
            "2026-11-01T08:00:00+00:00"
        );
    }

    #[test]
    fn item_files_are_listed_and_removed_per_source() {
        let dir = tempdir().unwrap();
        {
            let mut a = ItemStore::open(dir.path(), "iwt").unwrap();
            a.apply_batch(&[upsert("a", "x", "y")], None).unwrap();
            ItemStore::open(dir.path(), "mail").unwrap();
        }
        std::fs::write(dir.path().join("items-BAD.sqlite3"), b"").unwrap();
        assert_eq!(list_item_sources(dir.path()), vec!["iwt", "mail"]);
        assert!(remove_item_source_files(dir.path(), "iwt").unwrap());
        assert!(!items_db_path(dir.path(), "iwt").exists());
        assert!(!dir.path().join("items-iwt.sqlite3-wal").exists());
        assert_eq!(list_item_sources(dir.path()), vec!["mail"]);
        assert!(!remove_item_source_files(dir.path(), "iwt").unwrap());
        assert!(remove_item_source_files(dir.path(), "../x").is_err());
    }
}
