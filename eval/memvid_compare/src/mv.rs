//! Adapter over the real `memvid-core` crate: build one `.mv2` holding every lesson, then run
//! Memvid's own lexical and vector retrieval over it (hybrid fusion lives in `arms::hybrid`).
//!
//! One lesson is one parent frame carrying URI `mv2://lesson/<corpus index>`. Memvid may add child
//! frames and `put` returns WAL sequence numbers, so frame ids are not the lesson order: `open`
//! maps every frame to its lesson through its URI or its parent frame.

use crate::arms::{OVERFETCH, fts_query};
use crate::data::Corpus;
use memvid_core::{Memvid, PutManyOpts, PutOptions, SearchRequest, Ticket};
use std::path::Path;
use std::time::{Duration, Instant};

const URI_PREFIX: &str = "mv2://lesson/";
/// `search_vec` runs HNSW with the crate hard-coded `ef_search = 50` and panics when the
/// requested limit exceeds the number of neighbours it returns, so the vector leg caps at 50.
const VEC_OVERFETCH: usize = 50;

pub struct BuildStats {
    pub put: Duration,
    pub commit: Duration,
    pub total: Duration,
    pub with_vectors: usize,
}

fn err<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Batch ingest: `begin_batch` + `put_with_embedding_and_options` + `end_batch` + `commit`.
/// Lessons with a stored vector go in through `put_with_embedding`; lessons without one are
/// text-only (lexically searchable but not vector-searchable, as in Fieldcraft).
/// `lift_cap` applies a local capacity ticket (`Memvid::apply_ticket`, the call the crate's own
/// tests use) because a fresh file is free tier: 50 MiB over WAL + payload, which this corpus
/// exceeds. It switches the batch to the documented fast mode (no auto-checkpoint, pre-sized WAL).
pub fn build(corpus: &Corpus, path: &Path, lift_cap: bool) -> Result<BuildStats, String> {
    if path.exists() {
        std::fs::remove_file(path).map_err(err("remove old .mv2"))?;
    }
    let t0 = Instant::now();
    let mut mem = Memvid::create(path).map_err(err("create"))?;
    mem.enable_lex().map_err(err("enable_lex"))?;
    mem.enable_vec().map_err(err("enable_vec"))?;
    let opts = if lift_cap {
        #[allow(deprecated)]
        mem.apply_ticket(Ticket::new("memvid-compare-local", 2).capacity_bytes(8 << 30)).map_err(err("apply_ticket"))?;
        let per_entry = 2048 + corpus.dim as u64 * 4;
        PutManyOpts { compression_level: 1, wal_pre_size_bytes: corpus.len() as u64 * per_entry, ..PutManyOpts::default() }
    } else {
        PutManyOpts { compression_level: 1, disable_auto_checkpoint: false, ..PutManyOpts::default() }
    };
    mem.begin_batch(opts).map_err(err("begin_batch"))?;
    let put_start = Instant::now();
    let mut with_vectors = 0;
    for i in 0..corpus.len() {
        let options = PutOptions {
            uri: Some(format!("{URI_PREFIX}{i}")),
            timestamp: Some(corpus.created[i].timestamp()),
            instant_index: false,
            no_raw: true,
            ..PutOptions::default()
        };
        let payload = corpus.texts[i].as_bytes();
        if corpus.has_vec[i] {
            with_vectors += 1;
            mem.put_with_embedding_and_options(payload, corpus.vector(i).to_vec(), options)
        } else {
            mem.put_bytes_with_options(payload, options)
        }
        .map_err(err(&format!("put lesson {i}")))?;
    }
    mem.end_batch().map_err(err("end_batch"))?;
    let put = put_start.elapsed();
    let commit_start = Instant::now();
    mem.commit().map_err(err("commit"))?;
    let commit = commit_start.elapsed();
    drop(mem);
    Ok(BuildStats { put, commit, total: t0.elapsed(), with_vectors })
}

pub struct MvIndex {
    mem: Memvid,
    /// Frame id -> corpus index of the lesson it belongs to (parent or child frame).
    lesson_of: Vec<Option<usize>>,
    pub frames: usize,
}

/// Open read-only (cold-open timing is the caller's) and map every frame to its lesson.
pub fn open(corpus: &Corpus, path: &Path) -> Result<MvIndex, String> {
    let mem = Memvid::open_read_only(path).map_err(err("open_read_only"))?;
    let frames = mem.frame_count();
    let mut lesson_of: Vec<Option<usize>> = vec![None; frames];
    let mut parents = 0;
    for f in 0..frames {
        let frame = mem.frame_by_id(f as u64).map_err(err("frame_by_id"))?;
        let own = frame
            .uri
            .as_deref()
            .and_then(|u| u.strip_prefix(URI_PREFIX))
            .and_then(|s| s.parse::<usize>().ok());
        lesson_of[f] = own.or_else(|| {
            frame
                .parent_id
                .and_then(|p| usize::try_from(p).ok())
                .and_then(|p| lesson_of.get(p).copied().flatten())
        });
        parents += usize::from(own.is_some());
    }
    if parents != corpus.len() {
        return Err(format!("{parents} lesson frames, expected {}", corpus.len()));
    }
    Ok(MvIndex { mem, lesson_of, frames })
}

fn dedup_in_order(frames: impl Iterator<Item = u64>, lesson_of: &[Option<usize>], limit: usize) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    frames
        .filter_map(|f| usize::try_from(f).ok().and_then(|f| lesson_of.get(f).copied().flatten()))
        .filter(|i| seen.insert(*i))
        .take(limit)
        .collect()
}

impl MvIndex {
    /// `mv-lex`: `Memvid::search` (Tantivy BM25), the lexical leg of Memvid's `ask` path.
    /// The query is the production OR-joined term string so it is comparable with `fc-bm25`.
    ///
    /// `Memvid::search_lex` (the legacy in-memory index) cannot serve this build: after a file
    /// written by `put_many` is reopened it returns `Lexical index is not enabled`, because only
    /// the Tantivy/segment index is persisted. `ask` itself calls `search`, so this is the same
    /// retrieval Memvid actually uses.
    pub fn lex(&mut self, query: &str) -> Result<Vec<usize>, String> {
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let req = SearchRequest {
            query: q,
            top_k: OVERFETCH,
            snippet_chars: 80,
            uri: None,
            scope: None,
            cursor: None,
            as_of_frame: None,
            as_of_ts: None,
            no_sketch: true,
            acl_context: None,
            acl_enforcement_mode: Default::default(),
        };
        let resp = self.mem.search(req).map_err(err("search"))?;
        Ok(dedup_in_order(resp.hits.iter().map(|h| h.frame_id), &self.lesson_of, OVERFETCH))
    }

    /// `mv-vec`: `Memvid::search_vec` (HNSW over the stored vectors, L2 distance; vectors are unit
    /// length so L2 order equals cosine order).
    pub fn vec(&mut self, qvec: &[f32]) -> Result<Vec<usize>, String> {
        let hits = self.mem.search_vec(qvec, VEC_OVERFETCH).map_err(err("search_vec"))?;
        Ok(dedup_in_order(hits.iter().map(|h| h.frame_id), &self.lesson_of, VEC_OVERFETCH))
    }
}
