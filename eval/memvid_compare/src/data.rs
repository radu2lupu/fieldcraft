//! Snapshot loading: the lesson corpus and the (query, cited lessons) label set.

use crate::guard::parse_ts;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const EVAL_DIR: &str = "/Volumes/Development SSD/fieldcraft-memvid-eval";
pub const EMBED_DIM: usize = 768;

pub struct Corpus {
    /// Sorted by (created_at, id) ascending so time-visible lessons are a prefix.
    pub ids: Vec<String>,
    pub created: Vec<DateTime<Utc>>,
    pub texts: Vec<String>,
    /// Row-major `ids.len() * dim`; rows without a stored vector are zero and `has_vec` is false.
    pub vectors: Vec<f32>,
    pub has_vec: Vec<bool>,
    pub dim: usize,
}

impl Corpus {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn vector(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }

    pub fn index_of(&self) -> HashMap<&str, usize> {
        self.ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect()
    }
}

#[derive(Debug, Clone)]
pub struct Query {
    pub rid: i64,
    pub project_id: String,
    pub session: String,
    pub text: String,
    pub at: DateTime<Utc>,
    /// Production order, as logged.
    pub served: Vec<String>,
    /// Lessons the session cited inline for which this is the earliest serving retrieval.
    pub cited: Vec<String>,
}

pub fn open_snapshot(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

/// Production decoding rule (fieldcraft-storage `blob_to_vector`): little-endian f32 chunks.
pub fn blob_to_vector(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

type LessonRow = (String, DateTime<Utc>, String, Option<Vec<u8>>);

/// `light` skips lesson text and vectors (ids and timestamps only): enough for the Memvid query
/// path, which keeps its own copy, so its RSS is not inflated by a second in-memory corpus.
pub fn load_corpus(conn: &Connection, light: bool) -> Result<Corpus, String> {
    let sql = if light {
        "SELECT id, created_at, '', NULL FROM knowledge"
    } else {
        "SELECT id, created_at, retrieval_text, dense_vector FROM knowledge"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| e.to_string())?;
    let mut rows: Vec<LessonRow> = Vec::new();
    let mut it = stmt.query([]).map_err(|e| e.to_string())?;
    while let Some(r) = it.next().map_err(|e| e.to_string())? {
        let created: String = r.get(1).map_err(|e| e.to_string())?;
        rows.push((
            r.get(0).map_err(|e| e.to_string())?,
            parse_ts(&created)?,
            r.get(2).map_err(|e| e.to_string())?,
            r.get(3).map_err(|e| e.to_string())?,
        ));
    }
    rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let dim = EMBED_DIM;
    let mut c = Corpus {
        ids: Vec::with_capacity(rows.len()),
        created: Vec::with_capacity(rows.len()),
        texts: Vec::with_capacity(rows.len()),
        vectors: vec![0.0; if light { 0 } else { rows.len() * dim }],
        has_vec: vec![false; rows.len()],
        dim,
    };
    for (i, (id, at, text, blob)) in rows.into_iter().enumerate() {
        if let Some(b) = blob {
            let v = blob_to_vector(&b);
            if v.len() != dim {
                return Err(format!("lesson {id}: vector has {} dims, expected {dim}", v.len()));
            }
            c.vectors[i * dim..(i + 1) * dim].copy_from_slice(&v);
            c.has_vec[i] = true;
        }
        c.ids.push(id);
        c.created.push(at);
        c.texts.push(text);
    }
    Ok(c)
}

/// Label-set window. `since`/`until` are RFC3339 bounds on the retrieval's `created_at`
/// (inclusive / exclusive); `None` leaves that side open.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

/// Cited-lesson flag: a `confirmed` outcome written by the trace analyzer whose note says the
/// session cited the lesson inline (`inline \`lesson:<id>\` citation`). `reason_code` is empty
/// in the snapshot, so `source` + `note` are the discriminator.
const CITED_SQL: &str = "SELECT DISTINCT session_id, knowledge_id FROM lesson_outcome_events \
     WHERE outcome = 'confirmed' AND source = 'trace_analyzer' AND note LIKE '%inline%' \
       AND session_id IS NOT NULL";

/// One row per retrieval that was the earliest in its session to serve a cited lesson; a row
/// carries every such lesson. Row count therefore sits between the number of (session, lesson)
/// pairs and the number of sessions.
pub fn load_queries(conn: &Connection, window: Window) -> Result<Vec<Query>, String> {
    let mut cited: HashMap<String, HashSet<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(CITED_SQL).map_err(|e| e.to_string())?;
        let mut it = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = it.next().map_err(|e| e.to_string())? {
            let (s, k): (String, String) = (r.get(0).map_err(|e| e.to_string())?, r.get(1).map_err(|e| e.to_string())?);
            cited.entry(s).or_default().insert(k);
        }
    }
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, query_text, created_at, result_ids, project_id FROM retrieval_log \
             WHERE session_id IS NOT NULL ORDER BY id",
        )
        .map_err(|e| e.to_string())?;
    let mut it = stmt.query([]).map_err(|e| e.to_string())?;
    let mut claimed: HashSet<(String, String)> = HashSet::new();
    let mut out: BTreeMap<i64, Query> = BTreeMap::new();
    while let Some(r) = it.next().map_err(|e| e.to_string())? {
        let session: String = r.get(1).map_err(|e| e.to_string())?;
        let at = parse_ts(&r.get::<_, String>(3).map_err(|e| e.to_string())?)?;
        // Window first: the Jev export is 60 days of retrieval_log, so "earliest in the session"
        // means earliest inside the window. Filtering after claiming would let pre-window
        // retrievals consume a (session, lesson) pair and shrink the set.
        if window.since.is_some_and(|s| at < s) || window.until.is_some_and(|u| at >= u) {
            continue;
        }
        let Some(want) = cited.get(&session) else { continue };
        let served: Vec<String> = serde_json::from_str(&r.get::<_, String>(4).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let mut mine = Vec::new();
        for id in &served {
            if want.contains(id) && claimed.insert((session.clone(), id.clone())) {
                mine.push(id.clone());
            }
        }
        if mine.is_empty() {
            continue;
        }
        let rid: i64 = r.get(0).map_err(|e| e.to_string())?;
        out.insert(
            rid,
            Query { rid, project_id: r.get(5).map_err(|e| e.to_string())?, session, text: r.get(2).map_err(|e| e.to_string())?, at, served, cited: mine },
        );
    }
    Ok(out.into_values().collect())
}

pub fn cited_pair_count(queries: &[Query]) -> usize {
    queries.iter().map(|q| q.cited.len()).sum()
}

/// Directory holding the snapshot and the cached query vectors; `MEMVID_EVAL_DIR` overrides it.
pub fn eval_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("MEMVID_EVAL_DIR").unwrap_or_else(|_| EVAL_DIR.to_string()))
}

pub fn snapshot_path() -> std::path::PathBuf {
    eval_dir().join("snapshot.sqlite3")
}

pub fn query_vector_cache() -> std::path::PathBuf {
    eval_dir().join("query_vectors.jsonl")
}

/// The Jev eval window. Its README (2026-09-27) reports 865 queries, 424 sessions and 2,370
/// distinct candidate lessons. On this snapshot exactly those counts are recovered with a
/// half-open `[2026-07-29, 2026-09-25)` bound; later end dates add rows the Jev export did not
/// have (e.g. until 2026-09-27 gives 870 rows/424 sessions, until 2026-09-28 gives 872/430).
pub const JEV_SINCE: &str = "2026-07-29T00:00:00+00:00";
pub const JEV_UNTIL: &str = "2026-09-25T00:00:00+00:00";

pub fn jev_window() -> Result<Window, String> {
    Ok(Window { since: Some(parse_ts(JEV_SINCE)?), until: Some(parse_ts(JEV_UNTIL)?) })
}
