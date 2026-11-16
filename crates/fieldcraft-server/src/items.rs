//! Items API: things apps push, stored per source next to the main catalog
//! (`items-<source>.sqlite3`) and searched apart from lessons.
//!
//!   POST   /items:batch       upserts and deletes keyed by (source, external_id)
//!   DELETE /sources/{source}  every item, passage, vector and FTS row of a source
//!   GET    /search            BM25 + dense over passages, RRF (k = 60), recency
//!
//! All three need a bearer token (any enrolled token, for now; binding
//! tokens to sources is stage 2 of docs/plans/personal-index.md).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use fieldcraft_core::TrError;
use fieldcraft_retrieval::items::{ItemSearchHit, ItemSearchRequest, search_items};
use fieldcraft_storage::{ItemOp, ItemOpResult, ItemOpStatus, ItemStore, validate_source_id};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::AppState;
use crate::error::AppError;

/// Passages embedded per provider call while indexing.
const EMBED_BATCH: usize = 32;
const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 100;

/// One writer connection per source, opened on first use. A slot is emptied
/// (`None`) when its source is deleted, so a writer still holding it sees
/// that and never touches the removed file.
type Slot = Arc<Mutex<Option<ItemStore>>>;

pub struct ItemSources {
    data_dir: PathBuf,
    writers: Mutex<HashMap<String, Slot>>,
    /// Sources with an indexer running; `true` asks it for one more pass.
    indexing: Mutex<HashMap<String, bool>>,
}

impl ItemSources {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            writers: Mutex::new(HashMap::new()),
            indexing: Mutex::new(HashMap::new()),
        }
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    fn slot(&self, source: &str) -> Result<Slot, TrError> {
        let mut writers = self.writers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = writers.get(source) {
            return Ok(slot.clone());
        }
        let slot = Arc::new(Mutex::new(Some(ItemStore::open(&self.data_dir, source)?)));
        writers.insert(source.to_string(), slot.clone());
        Ok(slot)
    }

    /// Run `f` on the source's writer connection. Blocking.
    pub fn with_writer<R>(
        &self,
        source: &str,
        f: impl FnOnce(&mut ItemStore) -> Result<R, TrError>,
    ) -> Result<R, TrError> {
        validate_source_id(source)?;
        // A slot emptied by a concurrent delete is replaced on the next try.
        for _ in 0..2 {
            let slot = self.slot(source)?;
            let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(store) = guard.as_mut() {
                return f(store);
            }
            drop(guard);
            let mut writers = self.writers.lock().unwrap_or_else(|e| e.into_inner());
            if writers.get(source).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                writers.remove(source);
            }
        }
        Err(TrError::Conflict {
            message: format!("source {source} is being deleted"),
        })
    }

    /// Delete a source: its rows first (so nothing can read them), then the
    /// connection is closed and the file removed. Blocking.
    pub fn delete_source(&self, source: &str) -> Result<DeleteSourceResponse, TrError> {
        validate_source_id(source)?;
        let mut writers = self.writers.lock().unwrap_or_else(|e| e.into_inner());
        let mut deleted_items = 0;
        if let Some(slot) = writers.remove(source) {
            let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut store) = guard.take() {
                deleted_items = store.delete_all()?;
            }
        } else if fieldcraft_storage::items_db_path(&self.data_dir, source).exists() {
            let mut store = ItemStore::open(&self.data_dir, source)?;
            deleted_items = store.delete_all()?;
        }
        let removed_file = fieldcraft_storage::remove_item_source_files(&self.data_dir, source)?;
        Ok(DeleteSourceResponse {
            source: source.to_string(),
            deleted_items,
            removed_file,
        })
    }

    /// Read-only connections to every source file (or just `only`).
    pub fn readers(&self, only: Option<&str>) -> Result<Vec<ItemStore>, TrError> {
        let sources = match only {
            Some(source) => {
                validate_source_id(source)?;
                vec![source.to_string()]
            }
            None => fieldcraft_storage::list_item_sources(&self.data_dir),
        };
        Ok(sources
            .iter()
            .filter(|s| fieldcraft_storage::items_db_path(&self.data_dir, s).exists())
            .filter_map(|s| match ItemStore::open_reader(&self.data_dir, s) {
                Ok(store) => Some(store),
                Err(err) => {
                    warn!(source = %s, "items: cannot open reader: {err}");
                    None
                }
            })
            .collect())
    }

    /// Embed the source's passages that lack a vector of the configured
    /// model, in the background. At most one indexer per source; a request
    /// while one runs makes it take one more pass. No-op in BM25-only mode.
    pub fn spawn_indexer(self: &Arc<Self>, source: &str) {
        let Ok(spec) = fieldcraft_core::embedding::EmbeddingSpec::from_env() else {
            return;
        };
        if !spec.is_dense() {
            return;
        }
        {
            let mut indexing = self.indexing.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(again) = indexing.get_mut(source) {
                *again = true;
                return;
            }
            indexing.insert(source.to_string(), false);
        }
        let sources = self.clone();
        let source = source.to_string();
        tokio::task::spawn_blocking(move || {
            loop {
                let step = sources.with_writer(&source, |store| {
                    fieldcraft_retrieval::items::embed_pending_item_chunks_with_spec(
                        store,
                        &spec,
                        EMBED_BATCH,
                    )
                    .map_err(|message| TrError::Internal { message })
                });
                let done = match step {
                    Ok(0) => true,
                    Ok(n) => {
                        debug!(source = %source, n, "items: passages embedded");
                        false
                    }
                    Err(err) => {
                        // Left pending; the next batch or restart retries.
                        warn!(source = %source, "items: embedding stopped: {err}");
                        true
                    }
                };
                if done {
                    let mut indexing = sources.indexing.lock().unwrap_or_else(|e| e.into_inner());
                    if indexing.get(&source).copied() == Some(true) {
                        indexing.insert(source.clone(), false);
                        continue;
                    }
                    indexing.remove(&source);
                    return;
                }
            }
        });
    }

    /// Start an indexer for every source on disk (server start-up).
    pub fn spawn_all_indexers(self: &Arc<Self>) {
        for source in fieldcraft_storage::list_item_sources(&self.data_dir) {
            self.spawn_indexer(&source);
        }
    }
}

// ---------------------------------------------------------------------------
// POST /items:batch
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BatchBody {
    pub source: String,
    /// The connector's own position (opaque); stored when the batch commits.
    #[serde(default)]
    pub cursor: Option<String>,
    pub ops: Vec<ItemOp>,
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub source: String,
    pub results: Vec<ItemOpResult>,
    pub cursor: Option<String>,
    pub seq: i64,
}

pub async fn batch_handler(
    State(state): State<AppState>,
    Json(body): Json<BatchBody>,
) -> Result<impl IntoResponse, AppError> {
    validate_source_id(&body.source)?;
    let items = state.items.clone();
    let source = body.source.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        items.with_writer(&source, |store| {
            store.apply_batch(&body.ops, body.cursor.as_deref())
        })
    })
    .await
    .map_err(|e| AppError::Internal(format!("batch task failed: {e}")))?
    .map_err(item_error)?;
    if outcome
        .results
        .iter()
        .any(|r| matches!(r.status, ItemOpStatus::Created | ItemOpStatus::Updated))
    {
        state.items.spawn_indexer(&body.source);
    }
    Ok(Json(BatchResponse {
        source: body.source,
        results: outcome.results,
        cursor: outcome.cursor,
        seq: outcome.seq,
    }))
}

fn item_error(err: TrError) -> AppError {
    match err {
        TrError::Conflict { message } => AppError::Conflict(message),
        other => AppError::from(other),
    }
}

// ---------------------------------------------------------------------------
// DELETE /sources/{source}
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DeleteSourceResponse {
    pub source: String,
    pub deleted_items: usize,
    pub removed_file: bool,
}

pub async fn delete_source_handler(
    State(state): State<AppState>,
    Path(source): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let items = state.items.clone();
    let response = tokio::task::spawn_blocking(move || items.delete_source(&source))
        .await
        .map_err(|e| AppError::Internal(format!("delete task failed: {e}")))?
        .map_err(item_error)?;
    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// GET /search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ItemSearchParams {
    pub q: String,
    pub source: Option<String>,
    /// Comma-separated item kinds.
    pub kinds: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ItemSearchResponse {
    pub query: String,
    /// `hybrid` when the query had a vector, else `bm25`.
    pub mode: &'static str,
    pub results: Vec<ItemSearchHit>,
}

pub async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<ItemSearchParams>,
) -> Result<impl IntoResponse, AppError> {
    let query = params.q.trim().to_string();
    if query.is_empty() {
        return Err(AppError::BadRequest("q must not be empty".into()));
    }
    if query.chars().count() > 1000 {
        return Err(AppError::BadRequest(
            "q must be 1000 characters or fewer".into(),
        ));
    }
    let source = params
        .source
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(source) = &source {
        validate_source_id(source)?;
    }
    let kinds: Vec<String> = params
        .kinds
        .unwrap_or_default()
        .split(',')
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    let limit = params
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);

    // Same query-embedding path and deadline as GET /lessons: a slow
    // provider means BM25 only, never a slow answer.
    let (vector, outcome) = {
        use fieldcraft_retrieval::query_embed;
        query_embed::embed_within(
            query.clone(),
            query_embed::embed_budget(query_embed::RETRIEVAL_RESERVE),
        )
        .await
    };
    if outcome.is_fallback() {
        debug!(?outcome, "item search embedding fell back to BM25");
    }
    let mode = if vector.is_empty() { "bm25" } else { "hybrid" };
    let items = state.items.clone();
    let request = ItemSearchRequest {
        query: query.clone(),
        kinds,
        limit,
    };
    let results = tokio::task::spawn_blocking(move || {
        let stores = items.readers(source.as_deref())?;
        search_items(&stores, &request, &vector, chrono::Utc::now())
    })
    .await
    .map_err(|e| AppError::Internal(format!("search task failed: {e}")))?
    .map_err(item_error)?;
    Ok(Json(ItemSearchResponse {
        query,
        mode,
        results,
    }))
}
