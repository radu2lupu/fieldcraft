//! fieldcraft-server: HTTP server for fieldcraft.
//!
//! Routes:
//!   POST /enroll          — no bearer; gated by `EnrollPolicy`, rate-limited 10/min per IP
//!   POST /lessons        — bearer required
//!   GET  /lessons        — bearer required
//!   POST /sessions/import — bearer required

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use tower_http::trace::TraceLayer;
use tracing::{debug, info, info_span, warn};

use fieldcraft_core::{
    KnowledgeKind, KnowledgeState, KnowledgeTier, LearningSourceRecord, LearningSourceSeed,
    SourceFilter, SourceSessionRecord,
};
use fieldcraft_retrieval::retrieval::{LessonPreference, RetrievalRequest};
use fieldcraft_storage::{FeedbackOutcome, KnowledgeRepository, MemoryRepository};

use crate::auth::{ENROLL_SECRET_HEADER, EnrollPolicy, TokenStore, extract_bearer};
use crate::error::AppError;

/// Carries the authenticated user's name (token label) through the request.
#[derive(Clone, Debug)]
pub struct TokenOwner(pub Option<String>);

pub mod auth;
pub mod error;
pub mod memories;
pub mod readers;

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// Shared state held by all handlers.
///
/// `KnowledgeRepository` wraps a `rusqlite::Connection` which is not `Send+Sync`
/// (it uses `RefCell` internally), so we wrap it in a `Mutex`.
///
/// # Security invariant
/// `tokens` is kept in its own sqlite3 file (`tokens.sqlite3`) with an
/// isolated lifecycle — revocation does not touch knowledge or session data.
#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<Mutex<KnowledgeRepository>>,
    /// Read-only connections for the query path, so queries never wait on
    /// the write mutex or on each other.
    pub readers: Arc<readers::ReaderPool>,
    pub memories: Arc<Mutex<MemoryRepository>>,
    pub source_sessions: Arc<Mutex<Connection>>,
    pub tokens: Arc<TokenStore>,
    /// Path to the knowledge SQLite catalog (`<data_dir>/memory.sqlite3`).
    /// Passed to `fieldcraft_retrieval::retrieve` so the read path uses the
    /// same catalog as `repo` (which handles writes). Without this, retrieve
    /// would resolve a DB path via `fieldcraft_db_path()` (env-var based)
    /// and could read from a different catalog than the server writes to.
    pub db_path: PathBuf,
    /// Who may mint tokens via `POST /enroll`.
    pub enroll_policy: EnrollPolicy,
}

#[derive(Clone)]
pub struct ProxyState {
    pub upstream_url: String,
    pub upstream_auth_token: Option<String>,
    pub client: reqwest::Client,
}

fn canonical_project_id_from_state(state: &AppState, project_id: &str) -> Result<String, AppError> {
    state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .canonical_project_id(project_id)
        .map_err(AppError::from)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    // Per-IP rate limit on /enroll: 10/min with a burst of 10 (1 token every 6s).
    // tower_governor 0.8: per_second(N) means "1 token every N seconds".
    let governor_config = std::sync::Arc::new(
        GovernorConfigBuilder::default()
            .per_second(6)
            .burst_size(10)
            .finish()
            .expect("valid governor config"),
    );

    // Routes that require a valid bearer token.
    let protected = Router::new()
        .route(
            "/lessons",
            post(write_lesson_handler).get(search_lessons_handler),
        )
        .route("/status", get(status_handler))
        .route("/stats", get(stats_handler))
        .route("/analyze", get(analyze_handler))
        .route("/lifecycle", get(lifecycle_handler))
        .route(
            "/dream",
            get(dream_handler).post(create_derived_claim_handler),
        )
        .route("/lessons/{id}/feedback", post(feedback_handler))
        .route("/sessions/import", post(import_sessions_handler))
        .route("/sessions/trace-analyze", post(trace_analyze_handler))
        .route("/learning/sources", post(import_learning_sources_handler))
        .route(
            "/learning/sources/claim",
            get(claim_learning_sources_handler),
        )
        .route(
            "/learning/sources/{id}/complete",
            post(complete_learning_source_handler),
        )
        .route(
            "/learning/sources/{id}/fail",
            post(fail_learning_source_handler),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_middleware,
        ));

    let memories = Router::new()
        .route(
            "/memories",
            post(memories::create_memory).get(memories::list_memories),
        )
        .route(
            "/memories/{id}",
            get(memories::get_memory).patch(memories::patch_memory),
        )
        .route("/memories/{id}/archive", post(memories::archive_memory))
        .route("/people", get(memories::list_people))
        .route("/people/{id}", patch(memories::patch_person))
        .route("/projects", get(memories::list_projects))
        .route("/projects/{id}", patch(memories::patch_project))
        .route("/shapes", get(memories::list_shapes))
        .route("/views/today", get(memories::view_today))
        .route("/views/inbox", get(memories::view_inbox))
        .route("/views/project/{id}", get(memories::view_project))
        .route("/views/person/{id}", get(memories::view_person))
        .route("/views/actions", get(memories::view_actions))
        .route(
            "/admin/rebuild-views",
            post(memories::rebuild_views_handler),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_middleware,
        ));

    let health = Router::new().route("/health", get(|| async { "ok" }));

    // /enroll needs no bearer: `EnrollPolicy` gates it, tower_governor rate-limits it per IP.
    let enroll = Router::new()
        .route("/enroll", post(enroll_handler))
        .layer(GovernorLayer::new(governor_config));

    // Per-request tracing span with a generated request_id for correlation.
    // The span carries method+uri+request_id; nested log lines inherit it.
    let trace_layer =
        TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<axum::body::Body>| {
            let request_id: u64 = rand::random();
            info_span!(
                "request",
                request_id = %format!("{request_id:016x}"),
                method = %req.method(),
                uri = %req.uri(),
            )
        });

    health
        .merge(enroll)
        .merge(protected)
        .merge(memories)
        // Bulk session imports can be large (~hundreds of MB for thousands of
        // sessions). Default axum body limit is 2 MB which is too low.
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .layer(trace_layer)
        .with_state(state)
}

pub fn proxy_router(state: ProxyState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .fallback(proxy_handler)
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

async fn proxy_handler(
    State(state): State<ProxyState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    if let Some(token) = state.upstream_auth_token.as_deref() {
        let auth_header = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if extract_bearer(auth_header) != Some(token) {
            return Err(AppError::Unauthorized);
        }
    }

    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!(
        "{}{}",
        state.upstream_url.trim_end_matches('/'),
        path_and_query
    );

    let mut request = state.client.request(method, url).body(body.to_vec());

    for (name, value) in headers.iter() {
        if should_skip_proxy_header(name) {
            continue;
        }
        if name == header::AUTHORIZATION && state.upstream_auth_token.is_some() {
            continue;
        }
        request = request.header(name, value);
    }
    if let Some(token) = state.upstream_auth_token.as_deref() {
        request = request.bearer_auth(token);
    }

    let upstream = request
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("upstream request failed: {e}")))?;
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| AppError::Internal(format!("upstream body read failed: {e}")))?;

    let mut response = Response::builder().status(status);
    for (name, value) in upstream_headers.iter() {
        if should_skip_proxy_header(name) {
            continue;
        }
        response = response.header(name, value);
    }
    response
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(format!("proxy response build failed: {e}")))
}

fn should_skip_proxy_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "te"
            | "upgrade"
            | "trailer"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-authenticate"
    )
}

// ---------------------------------------------------------------------------
// Bearer auth middleware
// ---------------------------------------------------------------------------

async fn bearer_auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: axum::extract::Request,
    next: Next,
) -> Result<impl IntoResponse, AppError> {
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let bearer = extract_bearer(auth_header).ok_or(AppError::Unauthorized)?;

    let label = state
        .tokens
        .verify_with_label(bearer)
        .map_err(|e| AppError::Storage(e.to_string()))?;

    match label {
        None => {
            debug!("bearer token rejected");
            return Err(AppError::Unauthorized);
        }
        Some(lbl) => {
            req.extensions_mut()
                .insert(TokenOwner(if lbl.is_empty() { None } else { Some(lbl) }));
        }
    }

    Ok(next.run(req).await)
}

// ---------------------------------------------------------------------------
// POST /enroll
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EnrollBody {
    pub label: Option<String>,
}

async fn enroll_handler(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<EnrollBody>,
) -> Result<impl IntoResponse, AppError> {
    let presented = headers
        .get(ENROLL_SECRET_HEADER)
        .and_then(|v| v.to_str().ok());
    if !state.enroll_policy.allows(peer.ip(), presented) {
        warn!(peer = %peer.ip(), "enroll rejected by enroll policy");
        return Err(AppError::Forbidden(
            "enrollment needs the server's FIELDCRAFT_ENROLL_SECRET in the \
             X-Fieldcraft-Enroll-Secret header (or a loopback caller when no secret is set)"
                .into(),
        ));
    }

    let token = state
        .tokens
        .mint(body.label.as_deref())
        .map_err(|e| AppError::Storage(e.to_string()))?;

    info!("new token enrolled");
    Ok((StatusCode::OK, axum::Json(json!({ "token": token }))))
}

// ---------------------------------------------------------------------------
// POST /lessons
// ---------------------------------------------------------------------------

async fn write_lesson_handler(
    State(state): State<AppState>,
    Extension(token_owner): Extension<TokenOwner>,
    axum::Json(mut record): axum::Json<fieldcraft_core::KnowledgeRecord>,
) -> Result<impl IntoResponse, AppError> {
    record.project_id = canonical_project_id_from_state(&state, &record.project_id)?;
    record.owner = token_owner.0;
    let id = record.id.clone();

    state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .insert_knowledge(&record)
        .map_err(AppError::from)?;

    index_record_best_effort(state.db_path.clone(), record, "lesson").await;

    if let Ok(repo) = state.repo.lock()
        && let Err(e) = repo.enqueue_merge_candidate(&id)
    {
        warn!(id = %id, "merge enqueue failed: {e}");
    }

    info!(id = %id, "lesson written");
    Ok((StatusCode::OK, axum::Json(json!({ "id": id }))))
}

async fn index_record_best_effort(
    db_path: PathBuf,
    record: fieldcraft_core::KnowledgeRecord,
    record_label: &'static str,
) {
    let id = record.id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let repo = KnowledgeRepository::open(&db_path).map_err(|e| format!("open repo: {e}"))?;
        fieldcraft_retrieval::index_knowledge_sqlite(&repo, &record)
            .map_err(|e| format!("index knowledge: {e}"))
    })
    .await;

    match result {
        Ok(Ok(())) => debug!(id = %id, "indexed {record_label} dense vector"),
        Ok(Err(e)) => warn!(id = %id, "{record_label} embedding/indexing failed after write: {e}"),
        Err(e) => warn!(id = %id, "{record_label} embedding/indexing task failed after write: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Background lesson consolidation
// ---------------------------------------------------------------------------

pub async fn merge_queue_loop(state: AppState) {
    use std::time::Duration;

    loop {
        // Keep the vector-cache change log bounded; a cache further behind
        // than this rebuilds from scratch.
        if let Ok(repo) = state.repo.lock()
            && let Err(e) = repo.prune_record_changes(100_000)
        {
            warn!("vector change log prune failed: {e}");
        }
        match process_merge_queue_once(&state, 16) {
            Ok(processed) if processed > 0 => {
                info!(processed, "processed lesson merge queue");
            }
            Ok(_) => {}
            Err(e) => warn!("lesson merge queue failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// Memory Lifecycle maintenance loop (L2/L4): periodically compute archive
/// candidates (ghosts, contradicted-losers, RIF near-twin losers) and, when
/// FIELDCRAFT_LIFECYCLE_APPLY=1, archive them reversibly. With the flag off it
/// only logs candidate counts (observability without mutation). Interval via
/// FIELDCRAFT_LIFECYCLE_INTERVAL_SECS (default 21600 = 6h). Sleeps first so a
/// crash-looping server never hammers archival passes.
pub async fn lifecycle_loop(state: AppState) {
    use std::time::Duration;

    let interval = std::env::var("FIELDCRAFT_LIFECYCLE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(21_600u64);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let apply = std::env::var("FIELDCRAFT_LIFECYCLE_APPLY").as_deref() == Ok("1");
        match process_lifecycle_once(&state, apply) {
            Ok((candidates, archived)) if apply => {
                info!(candidates, archived, "lifecycle maintenance pass applied");
            }
            Ok((candidates, _)) => {
                if candidates > 0 {
                    info!(
                        candidates,
                        "lifecycle maintenance pass (dry-run; set FIELDCRAFT_LIFECYCLE_APPLY=1 to archive)"
                    );
                }
            }
            Err(e) => warn!("lifecycle maintenance pass failed: {e}"),
        }
    }
}

/// One lifecycle pass: returns (candidate_count, archived_count). Candidates
/// are computed under one lock; each archive takes its own lock and a
/// per-record failure skips that record, never the batch.
pub fn process_lifecycle_once(state: &AppState, apply: bool) -> Result<(usize, usize), AppError> {
    let report = {
        let repo = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        fieldcraft_storage::lifecycle_report(&repo, 60).map_err(AppError::from)?
    };

    if !apply {
        return Ok((report.candidates.len(), 0));
    }
    let mut archived = 0usize;
    for c in &report.candidates {
        let result = {
            let repo = state
                .repo
                .lock()
                .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
            repo.archive_record(&c.id, &c.reason)
        };
        match result {
            Ok(()) => archived += 1,
            Err(e) => warn!(id = %c.id, "lifecycle archive skipped: {e}"),
        }
    }
    Ok((report.candidates.len(), archived))
}

pub fn process_merge_queue_once(state: &AppState, limit: usize) -> Result<usize, AppError> {
    let items = {
        let repo = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        repo.claim_merge_candidates(limit).map_err(AppError::from)?
    };

    let mut processed = 0usize;
    for item in items {
        let result = {
            let repo = state
                .repo
                .lock()
                .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
            match repo.obvious_merge_candidate(&item.lesson_id) {
                Ok(Some(candidate)) => repo.merge_duplicate_into_canonical(
                    &candidate.canonical_id,
                    &candidate.duplicate_id,
                    candidate.score,
                ),
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            }
        };

        let repo = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        match result {
            Ok(()) => {
                repo.complete_merge_candidate(item.id)
                    .map_err(AppError::from)?;
                processed += 1;
            }
            Err(e) => {
                repo.fail_merge_candidate(item.id, item.attempts, &e.to_string())
                    .map_err(AppError::from)?;
                warn!(queue_id = item.id, lesson_id = %item.lesson_id, "merge candidate failed: {e}");
            }
        }
    }
    Ok(processed)
}

// ---------------------------------------------------------------------------
// GET /status
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StatusParams {
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusRow {
    pub kind: String,
    pub state: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub project_id: String,
    pub total_records: usize,
    pub imports_queued: i64,
    pub rows: Vec<StatusRow>,
    /// Whether this server can embed (Gemini key in env or secrets file).
    /// `fieldcraft doctor` on a client reads it; older servers omit it.
    pub embedding_key_configured: bool,
}

async fn status_handler(
    State(state): State<AppState>,
    Query(params): Query<StatusParams>,
) -> Result<impl IntoResponse, AppError> {
    let project_id = canonical_project_id_from_state(
        &state,
        &params.project.unwrap_or_else(|| "global".to_string()),
    )?;
    let repo = state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
    let conn = repo.connection();

    let mut stmt = conn
        .prepare(
            "SELECT kind, state, COUNT(*) as n
             FROM knowledge
             WHERE project_id = ?1
             GROUP BY kind, state
             ORDER BY kind, state",
        )
        .map_err(|e| AppError::Internal(format!("status query prepare failed: {e}")))?;
    let rows_iter = stmt
        .query_map(rusqlite::params![&project_id], |row| {
            Ok(StatusRow {
                kind: row.get(0)?,
                state: row.get(1)?,
                count: row.get(2)?,
            })
        })
        .map_err(|e| AppError::Internal(format!("status query failed: {e}")))?;
    let rows: Vec<StatusRow> = rows_iter
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::Internal(format!("status row decode failed: {e}")))?;

    let imports_queued: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM imports WHERE project_id = ?1",
            rusqlite::params![&project_id],
            |row| row.get(0),
        )
        .map_err(|e| AppError::Internal(format!("imports count failed: {e}")))?;
    let total_records = repo.count_by_project(&project_id).map_err(AppError::from)?;

    Ok((
        StatusCode::OK,
        axum::Json(StatusResponse {
            project_id,
            total_records,
            imports_queued,
            rows,
            embedding_key_configured: fieldcraft_core::secrets::lookup_any(&[
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
            ])
            .is_some(),
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /stats
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StatsParams {
    pub days: Option<u32>,
}

async fn stats_handler(
    State(state): State<AppState>,
    Query(params): Query<StatsParams>,
) -> Result<impl IntoResponse, AppError> {
    let days = params.days.unwrap_or(7);
    let stats = state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .closed_loop_stats(days)
        .map_err(AppError::from)?;
    Ok((StatusCode::OK, axum::Json(stats)))
}

// ---------------------------------------------------------------------------
// GET /analyze and GET /lifecycle
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AnalyzeParams {
    pub days: Option<u32>,
    pub limit: Option<usize>,
}

async fn analyze_handler(
    State(state): State<AppState>,
    Query(params): Query<AnalyzeParams>,
) -> Result<impl IntoResponse, AppError> {
    let days = params.days.unwrap_or(14);
    let limit = params.limit.unwrap_or(20);
    let report = state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))
        .and_then(|repo| {
            fieldcraft_storage::analyze_report(&repo, days, limit).map_err(AppError::from)
        })?;
    Ok((StatusCode::OK, axum::Json(report)))
}

#[derive(Debug, Deserialize)]
pub struct LifecycleParams {
    pub min_age_days: Option<i64>,
}

async fn lifecycle_handler(
    State(state): State<AppState>,
    Query(params): Query<LifecycleParams>,
) -> Result<impl IntoResponse, AppError> {
    let min_age_days = params.min_age_days.unwrap_or(60);
    let report = state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))
        .and_then(|repo| {
            fieldcraft_storage::lifecycle_report(&repo, min_age_days).map_err(AppError::from)
        })?;
    Ok((StatusCode::OK, axum::Json(report)))
}

// ---------------------------------------------------------------------------
// GET/POST /dream
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DreamParams {
    pub max_clusters: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct DreamMember {
    pub id: String,
    pub title: String,
    pub retrieval_text: String,
}

#[derive(Debug, Serialize)]
pub struct DreamCluster {
    pub project_id: String,
    pub already_dreamed: bool,
    pub members: Vec<DreamMember>,
}

#[derive(Debug, Serialize)]
pub struct DreamResponse {
    pub clusters: Vec<DreamCluster>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDerivedClaimBody {
    pub source_ids: Vec<String>,
    pub project_id: String,
    pub title: String,
    pub statement: String,
}

async fn dream_handler(
    State(state): State<AppState>,
    Query(params): Query<DreamParams>,
) -> Result<impl IntoResponse, AppError> {
    let max_clusters = params.max_clusters.unwrap_or(3);
    let repo = state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
    let clusters = repo
        .dream_clusters(3, 2, max_clusters)
        .map_err(AppError::from)?;
    let mut response_clusters = Vec::new();

    for ids in clusters {
        let mut records = Vec::new();
        for id in ids {
            if let Some(record) = repo.get_knowledge(&id).map_err(AppError::from)? {
                records.push(record);
            }
        }
        if records.len() < 3 {
            continue;
        }
        let project_id = records[0].project_id.clone();
        let source_ids: Vec<String> = records.iter().map(|record| record.id.clone()).collect();
        let already_dreamed = repo
            .dream_cluster_already_dreamed(&source_ids)
            .map_err(AppError::from)?;
        let members = records
            .into_iter()
            .map(|record| DreamMember {
                id: record.id,
                title: record.title,
                retrieval_text: record.retrieval_text,
            })
            .collect();
        response_clusters.push(DreamCluster {
            project_id,
            already_dreamed,
            members,
        });
    }

    Ok((
        StatusCode::OK,
        axum::Json(DreamResponse {
            clusters: response_clusters,
        }),
    ))
}

async fn create_derived_claim_handler(
    State(state): State<AppState>,
    axum::Json(mut body): axum::Json<CreateDerivedClaimBody>,
) -> Result<impl IntoResponse, AppError> {
    body.project_id = canonical_project_id_from_state(&state, &body.project_id)?;
    let (id, record) = {
        let repo = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        let source_ids = validate_derived_claim_request(&repo, &body)?;
        let id = repo
            .insert_derived_claim(&source_ids, &body.project_id, &body.title, &body.statement)
            .map_err(AppError::from)?;
        let record = repo
            .get_knowledge(&id)
            .map_err(AppError::from)?
            .ok_or_else(|| {
                AppError::Internal(format!("derived claim missing after insert: {id}"))
            })?;
        (id, record)
    };
    index_record_best_effort(state.db_path.clone(), record, "derived claim").await;
    Ok((StatusCode::OK, axum::Json(json!({ "id": id }))))
}

fn validate_derived_claim_request(
    repo: &KnowledgeRepository,
    body: &CreateDerivedClaimBody,
) -> Result<Vec<String>, AppError> {
    if body.project_id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "derived claim requires a non-empty project_id".into(),
        ));
    }
    if body.title.trim().is_empty() || body.statement.trim().is_empty() {
        return Err(AppError::BadRequest(
            "derived claim requires a non-empty title and statement".into(),
        ));
    }

    let mut source_set = BTreeSet::new();
    for id in &body.source_ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(AppError::BadRequest(
                "derived claim source_ids must not contain empty ids".into(),
            ));
        }
        if !source_set.insert(id.to_string()) {
            return Err(AppError::BadRequest(format!(
                "derived claim source_id appears more than once: {id}"
            )));
        }
    }
    if source_set.len() < fieldcraft_storage::DERIVED_CLAIM_MIN_SOURCES {
        return Err(AppError::BadRequest(format!(
            "derived claim requires {}..={} unique source lessons, got {}",
            fieldcraft_storage::DERIVED_CLAIM_MIN_SOURCES,
            fieldcraft_storage::DERIVED_CLAIM_MAX_SOURCES,
            source_set.len()
        )));
    }
    if source_set.len() > fieldcraft_storage::DERIVED_CLAIM_MAX_SOURCES {
        return Err(AppError::BadRequest(format!(
            "derived claim refuses {} unique source lessons; expected {}..={}",
            source_set.len(),
            fieldcraft_storage::DERIVED_CLAIM_MIN_SOURCES,
            fieldcraft_storage::DERIVED_CLAIM_MAX_SOURCES
        )));
    }

    let mut expected_kind: Option<KnowledgeKind> = None;
    for id in &source_set {
        let record = repo
            .get_knowledge(id)
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::BadRequest(format!("dream source lesson not found: {id}")))?;
        if record.project_id != body.project_id {
            return Err(AppError::BadRequest(format!(
                "dream source {id} belongs to project {}, not {}",
                record.project_id, body.project_id
            )));
        }
        if record.state != KnowledgeState::Active || record.tier != KnowledgeTier::Active {
            return Err(AppError::BadRequest(format!(
                "dream source {id} is not an active-tier active lesson"
            )));
        }
        if let Some(kind) = expected_kind {
            if record.kind != kind {
                return Err(AppError::BadRequest(
                    "dream sources must all have the same kind".into(),
                ));
            }
        } else {
            expected_kind = Some(record.kind);
        }
    }

    let source_ids: Vec<String> = source_set.iter().cloned().collect();
    if repo
        .dream_cluster_already_dreamed(&source_ids)
        .map_err(AppError::from)?
    {
        return Err(AppError::Conflict(
            "dream source cluster already has a derived claim".into(),
        ));
    }

    let is_current_dream_cluster = repo
        .dream_clusters(3, 2, usize::MAX)
        .map_err(AppError::from)?
        .into_iter()
        .any(|cluster| cluster.into_iter().collect::<BTreeSet<_>>() == source_set);
    if !is_current_dream_cluster {
        return Err(AppError::BadRequest(
            "source_ids are not a current eligible dream cluster".into(),
        ));
    }

    Ok(source_ids)
}

// ---------------------------------------------------------------------------
// GET /lessons
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: String,
    pub project: Option<String>,
    pub limit: Option<usize>,
    pub tier: Option<String>,
    /// Eval/debug source-origin filter: any | learned_only | external_only.
    pub source_filter: Option<String>,
    /// Optional session id. When provided, retrievals get tagged with it so
    /// the trace analyzer can correlate exactly. Clients that don't know
    /// their session id mid-session can omit this; correlation falls back to
    /// time-window matching.
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub score: f32,
    pub confidence: f32,
    pub project_id: String,
    pub created_at: String,
    pub owner: Option<String>,
    pub source_origin: String,
}

async fn search_lessons_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<impl IntoResponse, AppError> {
    let limit = params.limit.unwrap_or(10);
    let tier_str = params.tier.as_deref().unwrap_or("active").to_lowercase();
    let tier = KnowledgeTier::parse(&tier_str);
    let source_filter = params
        .source_filter
        .as_deref()
        .map(SourceFilter::parse)
        .unwrap_or_default();
    let project_id = canonical_project_id_from_state(
        &state,
        &params.project.unwrap_or_else(|| "global".to_string()),
    )?;
    let query_text = params.q.clone();
    if params
        .session_id
        .as_ref()
        .is_some_and(|session_id| session_id.len() > 128)
    {
        return Err(AppError::BadRequest(
            "session_id must be 128 characters or fewer".into(),
        ));
    }

    let request = RetrievalRequest {
        project_id: project_id.clone(),
        context: params.q,
        files: vec![],
        task: None,
        preferred_types: vec![(LessonPreference::Any, 1.0)],
        tier,
        source_filter,
    };

    let start = std::time::Instant::now();
    let request_context = request.context.clone();
    let request_files = request.files.clone();
    // Embed first, holding no database handle or lock; then rank on a pooled
    // read-only connection. Both are blocking, so neither runs on an async
    // worker.
    let response = {
        let embed_request = request.clone();
        let query_vector = tokio::task::spawn_blocking(move || {
            fieldcraft_retrieval::retrieval::embed_query(&embed_request)
        })
        .await
        .map_err(|e| AppError::Internal(format!("embedding task failed: {e}")))?;
        let readers = state.readers.clone();
        tokio::task::spawn_blocking(move || {
            readers.with(|repo| {
                fieldcraft_retrieval::retrieval::retrieve_with_query_vector(
                    request,
                    repo,
                    query_vector,
                )
            })
        })
        .await
        .map_err(|e| AppError::Internal(format!("retrieval task failed: {e}")))?
        .map_err(AppError::from)?
        .map_err(AppError::from)?
    };
    let latency_ms = start.elapsed().as_millis() as u64;
    let query_cue_bundle_json = response.query_cue_bundle_json.clone();
    let raw_candidate_ids = response.raw_candidate_ids.clone();
    let raw_candidate_scores = response.raw_candidate_scores.clone();
    let raw_candidate_titles = response.raw_candidate_titles.clone();
    let raw_candidate_origins = response.raw_candidate_origins.clone();

    let lessons: Vec<_> = response.lessons.into_iter().take(limit).collect();

    // Closed-loop feedback: log this HTTP retrieval so subsequent feedback
    // calls can correlate outcomes with what was actually surfaced. Best-effort;
    // a logging failure must not fail the HTTP response.
    {
        let ids: Vec<String> = lessons.iter().map(|m| m.id.clone()).collect();
        let scores: Vec<f32> = lessons.iter().map(|m| m.score).collect();
        let origins: Vec<String> = lessons.iter().map(|m| m.source_origin.clone()).collect();
        let session_id = params.session_id.as_deref().unwrap_or("");
        if let Ok(repo) = state.repo.lock() {
            let _ = repo.log_retrieval_entry(&fieldcraft_storage::RetrievalLogEntry {
                project_id: &project_id,
                query_text: &query_text,
                query_context: &request_context,
                query_files: &request_files,
                result_ids: &ids,
                result_scores: &scores,
                latency_ms,
                raw_candidate_ids: &raw_candidate_ids,
                raw_candidate_scores: &raw_candidate_scores,
                raw_candidate_titles: &raw_candidate_titles,
                result_origins: &origins,
                raw_candidate_origins: &raw_candidate_origins,
                query_cue_bundle: &query_cue_bundle_json,
                session_id,
                ..Default::default()
            });
        }
    }

    // L3 resurrection: archived records that keep matching live queries restore
    // themselves. Runs in a background task (never on the response path) and
    // scans only tier='archive' rows. Disable with FIELDCRAFT_RESURRECTION=0.
    if std::env::var("FIELDCRAFT_RESURRECTION").as_deref() != Ok("0")
        && !response.query_dense.is_empty()
    {
        let res_state = state.clone();
        let query_dense = response.query_dense.clone();
        tokio::spawn(async move {
            // Default calibrated on the production store: record embeddings
            // are cue-enriched at index time, so even a query IDENTICAL to a
            // record's retrieval_text measures ~0.927 against it (live test,
            // 2026-07-01). 0.85 catches genuine re-asks; the 3-hit counter is
            // the guard against stray matches.
            let min_cosine: f32 = std::env::var("FIELDCRAFT_RESURRECTION_COSINE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.85);
            let threshold: i64 = std::env::var("FIELDCRAFT_RESURRECTION_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3);
            // Scan on a read connection (cached vectors, no write lock); only
            // the rare hits take the write mutex to be noted.
            let readers = res_state.readers.clone();
            let scan = tokio::task::spawn_blocking(move || {
                readers.with(|repo| {
                    fieldcraft_retrieval::archive_matches(repo, &query_dense, min_cosine)
                })
            })
            .await;
            let matches = match scan {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => Err(e),
                Err(e) => {
                    warn!("archive match scan task failed: {e}");
                    return;
                }
            };
            match matches {
                Ok(hits) if hits.is_empty() => {}
                Ok(hits) => {
                    let Ok(repo) = res_state.repo.lock() else {
                        return;
                    };
                    for (id, sim) in hits.into_iter().take(3) {
                        match repo.note_archive_match(&id, threshold) {
                            Ok(true) => info!(%id, sim, "resurrected archived record"),
                            Ok(false) => {}
                            Err(e) => warn!(%id, "resurrection note failed: {e}"),
                        }
                    }
                }
                Err(e) => warn!("archive match scan failed: {e}"),
            }
        });
    }

    let hits: Vec<SearchHit> = lessons
        .into_iter()
        .map(|m| SearchHit {
            id: m.id,
            kind: m.kind,
            title: m.title,
            content: m.content,
            score: m.score,
            confidence: m.confidence,
            project_id: m.project_id,
            created_at: m.created_at,
            owner: m.owner,
            source_origin: m.source_origin,
        })
        .collect();

    Ok((StatusCode::OK, axum::Json(hits)))
}

// ---------------------------------------------------------------------------
// POST /lessons/{id}/feedback
// ---------------------------------------------------------------------------
// Records a feedback outcome for a previously retrieved lesson. Drives
// closed-loop scoring via dynamic_confidence updates + an append-only event
// log. Phase 4 of cue-bundle retrieval — manual / HTTP-driven path; the
// trace analyzer (Phase 5) will emit the same outcomes automatically.

#[derive(Debug, Deserialize)]
pub struct FeedbackBody {
    pub outcome: String,
    pub session_id: Option<String>,
    pub note: Option<String>,
    /// Optional source tag. Defaults to "http". The trace analyzer passes
    /// "trace_analyzer" so we can distinguish auto-emitted events from
    /// manual `fieldcraft feedback` calls in stats and audits.
    pub source: Option<String>,
}

async fn feedback_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<FeedbackBody>,
) -> Result<impl IntoResponse, AppError> {
    let outcome = FeedbackOutcome::parse(&body.outcome).ok_or_else(|| {
        AppError::Internal(format!(
            "invalid outcome '{}': must be confirmed | ignored | contradicted",
            body.outcome
        ))
    })?;
    let source = body.source.as_deref().unwrap_or("http");

    // Idempotency: when the trace analyzer re-runs across a session it should
    // not stack duplicate events for the same (knowledge, session, source).
    // Manual `http` events are still recorded every time — they reflect explicit
    // user action and may legitimately repeat.
    if source != "http"
        && let Some(session_id) = body.session_id.as_deref()
    {
        let already = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?
            .outcome_event_exists(&id, session_id, source)
            .unwrap_or(false);
        if already {
            return Ok((
                StatusCode::OK,
                axum::Json(json!({
                    "lesson_id": id,
                    "outcome": body.outcome,
                    "idempotent": true,
                })),
            ));
        }
    }

    let (before, after) = state
        .repo
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .apply_feedback_outcome(
            &id,
            outcome,
            body.session_id.as_deref(),
            source,
            body.note.as_deref(),
        )
        .map_err(AppError::from)?;

    info!(id = %id, outcome = %body.outcome, before, after, "feedback recorded");
    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "lesson_id": id,
            "outcome": body.outcome,
            "confidence_before": before,
            "confidence_after": after,
        })),
    ))
}

// ---------------------------------------------------------------------------
// POST /sessions/trace-analyze
// ---------------------------------------------------------------------------
// Server-side trace analyzer. The client extracts citations and contradictions
// from a session transcript and POSTs them here in one shot; the server
// correlates against its own retrieval_log and emits the three event kinds:
//
//   - Confirmed   for every cited lesson id
//   - Contradicted for every contradicted entry (with explanatory note)
//   - Ignored     for every retrieved-but-not-cited lesson whose cue match
//                 factor against the relevant query was >= the threshold
//
// All emissions are idempotent on (knowledge_id, session_id, source).

#[derive(Debug, Deserialize)]
pub struct ContradictedEntry {
    pub id: String,
    /// The phrase / context that triggered the contradicted classification.
    pub context: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TraceAnalyzeBody {
    pub session_id: String,
    pub project_id: String,
    /// RFC3339; lower bound (inclusive) for retrieval_log correlation.
    pub since: String,
    /// RFC3339; upper bound (exclusive).
    pub until: String,
    /// Lesson IDs the agent cited inline as `lesson:<id>`.
    pub cited_ids: Vec<String>,
    /// Lessons the agent explicitly contradicted with surrounding context.
    #[serde(default)]
    pub contradicted: Vec<ContradictedEntry>,
    /// Cue-match-factor threshold above which an uncited retrieved lesson
    /// gets an Ignored event. Default 0.6.
    #[serde(default)]
    pub ignored_threshold: Option<f32>,
    /// Files changed in the session working tree. Basenames are matched
    /// against knowledge.affected_files_json as an observable used-signal.
    #[serde(default)]
    pub changed_files: Vec<String>,
}

const TRACE_ANALYZER_SOURCE: &str = "trace_analyzer";
// cue_match_factor renormalizes over dimensions populated in both bundles, so
// sparse cue data is no longer pinned near the all-floor baseline. A 0.60
// default now means "mostly matching on the evidence both sides actually have"
// instead of requiring phase/domain/user-state/tools to be populated first.
const DEFAULT_IGNORED_CUE_MATCH_THRESHOLD: f32 = 0.6;

async fn trace_analyze_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<TraceAnalyzeBody>,
) -> Result<impl IntoResponse, AppError> {
    use fieldcraft_core::CueBundle;
    use fieldcraft_retrieval::cue_match::cue_match_factor;

    let project_id = canonical_project_id_from_state(&state, &body.project_id)?;
    let threshold = body
        .ignored_threshold
        .unwrap_or(DEFAULT_IGNORED_CUE_MATCH_THRESHOLD);

    let mut confirmed_emitted = 0usize;
    let mut contradicted_emitted = 0usize;
    let mut ignored_emitted = 0usize;
    let mut idempotent = 0usize;

    // --- Confirmed --------------------------------------------------------
    for id in &body.cited_ids {
        let repo = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        if repo
            .outcome_event_exists(id, &body.session_id, TRACE_ANALYZER_SOURCE)
            .unwrap_or(false)
        {
            idempotent += 1;
            continue;
        }
        match repo.apply_feedback_outcome(
            id,
            FeedbackOutcome::Confirmed,
            Some(&body.session_id),
            TRACE_ANALYZER_SOURCE,
            Some("inline `lesson:<id>` citation"),
        ) {
            Ok(_) => confirmed_emitted += 1,
            Err(e) => warn!(lesson_id = %id, "confirmed apply failed: {e}"),
        }
    }

    // --- Contradicted -----------------------------------------------------
    for entry in &body.contradicted {
        let repo = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        if repo
            .outcome_event_exists(&entry.id, &body.session_id, TRACE_ANALYZER_SOURCE)
            .unwrap_or(false)
        {
            idempotent += 1;
            continue;
        }
        let note_text = entry
            .context
            .clone()
            .unwrap_or_else(|| "explicit contradiction in assistant turn".to_string());
        match repo.apply_feedback_outcome(
            &entry.id,
            FeedbackOutcome::Contradicted,
            Some(&body.session_id),
            TRACE_ANALYZER_SOURCE,
            Some(&note_text),
        ) {
            Ok(_) => contradicted_emitted += 1,
            Err(e) => warn!(lesson_id = %entry.id, "contradicted apply failed: {e}"),
        }
    }

    // --- Ignored (cue-gated) ---------------------------------------------
    // Fetch retrieval_log rows for this session/window.
    let retrievals = {
        let repo = state
            .repo
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        repo.retrievals_for_project_window(
            &project_id,
            &body.since,
            &body.until,
            Some(&body.session_id),
        )
        .map_err(AppError::from)?
    };

    // Collect candidate uncited ids surfaced in any of those retrievals.
    let cited: std::collections::HashSet<&String> = body.cited_ids.iter().collect();
    let contradicted_set: std::collections::HashSet<&String> =
        body.contradicted.iter().map(|c| &c.id).collect();
    let mut candidate_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &retrievals {
        for id in &r.result_ids {
            if !cited.contains(id) && !contradicted_set.contains(id) {
                candidate_ids.insert(id.clone());
            }
        }
    }

    // --- Confirmed (file overlap) ----------------------------------------
    if !body.changed_files.is_empty() {
        let changed_basenames: std::collections::HashSet<String> = body
            .changed_files
            .iter()
            .filter_map(|path| basename(path))
            .collect();
        if !changed_basenames.is_empty() {
            let served_ids: std::collections::HashSet<String> = retrievals
                .iter()
                .flat_map(|r| r.result_ids.iter().cloned())
                .collect();
            let served_ids_vec: Vec<String> = served_ids.iter().cloned().collect();
            let affected = {
                let repo = state
                    .repo
                    .lock()
                    .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
                repo.affected_files_for_knowledge_ids(&served_ids_vec)
                    .map_err(AppError::from)?
            };

            for lesson_id in served_ids {
                let Some(files) = affected.get(&lesson_id) else {
                    continue;
                };
                let overlaps = files
                    .iter()
                    .filter_map(|path| basename(path))
                    .any(|base| changed_basenames.contains(&base));
                if !overlaps {
                    continue;
                }

                let repo = state
                    .repo
                    .lock()
                    .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
                if repo
                    .outcome_event_exists(&lesson_id, &body.session_id, TRACE_ANALYZER_SOURCE)
                    .unwrap_or(false)
                {
                    idempotent += 1;
                    continue;
                }
                match repo.apply_feedback_outcome(
                    &lesson_id,
                    FeedbackOutcome::Confirmed,
                    Some(&body.session_id),
                    TRACE_ANALYZER_SOURCE,
                    Some("file_overlap"),
                ) {
                    Ok(_) => confirmed_emitted += 1,
                    Err(e) => {
                        warn!(lesson_id = %lesson_id, "file-overlap confirmed apply failed: {e}")
                    }
                }
            }
        }
    }

    if !candidate_ids.is_empty() {
        let candidates: Vec<String> = candidate_ids.iter().cloned().collect();
        let signals = {
            let repo = state
                .repo
                .lock()
                .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
            repo.load_retrieval_signals_for(&candidates)
                .map_err(AppError::from)?
        };

        for lesson_id in &candidates {
            let lesson_bundle = match signals.cue_bundles.get(lesson_id) {
                Some(b) => b.clone(),
                None => continue, // legacy lesson with no cue bundle — skip
            };

            // Take the MAX cue_match_factor across the retrievals that
            // surfaced this lesson. Strongest plausible context wins; if even
            // the best match is below threshold, the lesson was a marginal
            // keyword hit and "not cited" isn't a negative signal.
            let mut best: f32 = 0.0;
            for r in &retrievals {
                if !r.result_ids.iter().any(|id| id == lesson_id) {
                    continue;
                }
                if let Some(qcb_str) = &r.query_cue_bundle
                    && let Ok(query_bundle) = serde_json::from_str::<CueBundle>(qcb_str)
                {
                    let f = cue_match_factor(&lesson_bundle, &query_bundle);
                    if f > best {
                        best = f;
                    }
                }
            }

            if best < threshold {
                continue;
            }

            let repo = state
                .repo
                .lock()
                .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
            if repo
                .outcome_event_exists(lesson_id, &body.session_id, TRACE_ANALYZER_SOURCE)
                .unwrap_or(false)
            {
                idempotent += 1;
                continue;
            }
            let note = format!("uncited despite cue match {best:.2}");
            match repo.apply_feedback_outcome(
                lesson_id,
                FeedbackOutcome::Ignored,
                Some(&body.session_id),
                TRACE_ANALYZER_SOURCE,
                Some(&note),
            ) {
                Ok(_) => ignored_emitted += 1,
                Err(e) => warn!(lesson_id = %lesson_id, "ignored apply failed: {e}"),
            }
        }
    }

    info!(
        session_id = %body.session_id,
        confirmed = confirmed_emitted,
        contradicted = contradicted_emitted,
        ignored = ignored_emitted,
        idempotent,
        "trace-analyze complete"
    );

    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "session_id": body.session_id,
            "confirmed_emitted": confirmed_emitted,
            "contradicted_emitted": contradicted_emitted,
            "ignored_emitted": ignored_emitted,
            "idempotent": idempotent,
        })),
    ))
}

fn basename(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

// ---------------------------------------------------------------------------
// Learning source orchestration
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct LearningSourceImportResult {
    index: usize,
    status: String,
    source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct LearningSourceImportSummary {
    imported: usize,
    skipped_existing: usize,
    failed: usize,
}

#[derive(Debug, Serialize)]
struct LearningSourceImportResponse {
    results: Vec<LearningSourceImportResult>,
    summary: LearningSourceImportSummary,
}

#[derive(Debug, Deserialize)]
struct ClaimLearningSourcesParams {
    limit: Option<usize>,
    project: Option<String>,
    worker_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompleteLearningSourceBody {
    source_hash: Option<String>,
    #[serde(default)]
    derived_record_count: u32,
}

#[derive(Debug, Deserialize)]
struct FailLearningSourceBody {
    error: String,
}

async fn import_learning_sources_handler(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    let seeds = parse_learning_source_body(&body)?;
    let mut results = Vec::with_capacity(seeds.len());
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for (index, mut seed) in seeds.into_iter().enumerate() {
        if seed.uri.trim().is_empty() {
            failed += 1;
            results.push(LearningSourceImportResult {
                index,
                status: "failed".to_string(),
                source_id: String::new(),
                error: Some("missing uri".to_string()),
            });
            continue;
        }
        if let Some(project) = seed.project.as_deref() {
            seed.project = Some(canonical_project_id_from_state(&state, project)?);
        }

        let outcome = {
            let conn = state
                .source_sessions
                .lock()
                .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
            fieldcraft_storage::enqueue_learning_source(&conn, &seed)
        };
        match outcome {
            Ok((record, queued)) => {
                if queued {
                    imported += 1;
                    results.push(LearningSourceImportResult {
                        index,
                        status: "imported".to_string(),
                        source_id: record.source_id,
                        error: None,
                    });
                } else {
                    skipped += 1;
                    results.push(LearningSourceImportResult {
                        index,
                        status: "skipped_existing".to_string(),
                        source_id: record.source_id,
                        error: None,
                    });
                }
            }
            Err(e) => {
                warn!(index, error = %e, "learning source import failed");
                failed += 1;
                results.push(LearningSourceImportResult {
                    index,
                    status: "failed".to_string(),
                    source_id: String::new(),
                    error: Some(e.to_string()),
                });
            }
        }
    }

    info!(imported, skipped, failed, "learning source import complete");
    Ok((
        StatusCode::OK,
        axum::Json(LearningSourceImportResponse {
            results,
            summary: LearningSourceImportSummary {
                imported,
                skipped_existing: skipped,
                failed,
            },
        }),
    ))
}

async fn claim_learning_sources_handler(
    State(state): State<AppState>,
    Query(params): Query<ClaimLearningSourcesParams>,
) -> Result<impl IntoResponse, AppError> {
    let limit = params.limit.unwrap_or(10).clamp(1, 100);
    let project = match params.project.as_deref() {
        Some(project) => Some(canonical_project_id_from_state(&state, project)?),
        None => None,
    };
    let worker_id = params
        .worker_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("fieldcraft-worker");
    let records: Vec<LearningSourceRecord> = {
        let conn = state
            .source_sessions
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        fieldcraft_storage::claim_learning_sources(&conn, worker_id, limit, project.as_deref())
            .map_err(AppError::from)?
    };

    Ok((StatusCode::OK, axum::Json(records)))
}

async fn complete_learning_source_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<CompleteLearningSourceBody>,
) -> Result<impl IntoResponse, AppError> {
    {
        let conn = state
            .source_sessions
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        fieldcraft_storage::complete_learning_source(
            &conn,
            &id,
            body.source_hash.as_deref(),
            body.derived_record_count,
        )
        .map_err(AppError::from)?;
    }
    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "source_id": id,
            "status": "succeeded",
        })),
    ))
}

async fn fail_learning_source_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<FailLearningSourceBody>,
) -> Result<impl IntoResponse, AppError> {
    if body.error.trim().is_empty() {
        return Err(AppError::BadRequest("error must not be empty".into()));
    }
    {
        let conn = state
            .source_sessions
            .lock()
            .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
        fieldcraft_storage::fail_learning_source(&conn, &id, &body.error)
            .map_err(AppError::from)?;
    }
    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "source_id": id,
            "status": "failed",
        })),
    ))
}

fn parse_learning_source_body(body: &[u8]) -> Result<Vec<LearningSourceSeed>, AppError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| AppError::BadRequest("request body is not valid UTF-8".into()))?;
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON array: {e}")));
    }

    let mut records = Vec::new();
    for (line_no, line) in trimmed.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: LearningSourceSeed = serde_json::from_str(line)
            .map_err(|e| AppError::BadRequest(format!("invalid NDJSON at line {line_no}: {e}")))?;
        records.push(rec);
    }

    if records.is_empty() {
        return Err(AppError::BadRequest(
            "request body is empty or not valid JSON/NDJSON".into(),
        ));
    }

    Ok(records)
}

// ---------------------------------------------------------------------------
// POST /sessions/import
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ImportResult {
    index: usize,
    status: String,
    source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImportSummary {
    imported: usize,
    skipped_existing: usize,
    failed: usize,
}

#[derive(Debug, Serialize)]
struct ImportResponse {
    results: Vec<ImportResult>,
    summary: ImportSummary,
}

/// Accept either a JSON array or NDJSON body.
async fn import_sessions_handler(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Parse body: try JSON array first, then NDJSON.
    let records: Vec<SourceSessionRecord> = parse_session_body(&body)?;

    let mut results = Vec::with_capacity(records.len());
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for (index, mut record) in records.into_iter().enumerate() {
        // Validate required fields.
        if record.source_fingerprint.is_empty() {
            results.push(ImportResult {
                index,
                status: "failed".into(),
                source_id: record.source_id.clone(),
                error: Some("missing source_fingerprint".into()),
            });
            failed += 1;
            continue;
        }
        record.project = canonical_project_id_from_state(&state, &record.project)?;

        let transcript_json = match serde_json::to_string(&record.transcript) {
            Ok(j) => j,
            Err(e) => {
                results.push(ImportResult {
                    index,
                    status: "failed".into(),
                    source_id: record.source_id.clone(),
                    error: Some(format!("transcript serialization error: {e}")),
                });
                failed += 1;
                continue;
            }
        };

        // Per-record transaction: BEGIN IMMEDIATE; INSERT OR IGNORE; COMMIT.
        // This guarantees atomicity while remaining idempotent via UNIQUE INDEX
        // on source_fingerprint.
        let outcome = {
            let conn = state
                .source_sessions
                .lock()
                .map_err(|_| AppError::Internal("mutex poisoned".into()))?;

            import_session_record(&conn, &record, &transcript_json)
        };

        match outcome {
            Ok(was_inserted) => {
                if was_inserted {
                    imported += 1;
                    results.push(ImportResult {
                        index,
                        status: "imported".into(),
                        source_id: record.source_id,
                        error: None,
                    });
                } else {
                    skipped += 1;
                    results.push(ImportResult {
                        index,
                        status: "skipped_existing".into(),
                        source_id: record.source_id,
                        error: None,
                    });
                }
            }
            Err(e) => {
                warn!(index, error = %e, "session import record failed");
                failed += 1;
                results.push(ImportResult {
                    index,
                    status: "failed".into(),
                    source_id: record.source_id,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    info!(imported, skipped, failed, "session import complete");

    Ok((
        StatusCode::OK,
        axum::Json(ImportResponse {
            results,
            summary: ImportSummary {
                imported,
                skipped_existing: skipped,
                failed,
            },
        }),
    ))
}

/// Parse request body as JSON array or NDJSON.
/// Returns HTTP 400 only if the top-level body is malformed.
fn parse_session_body(body: &[u8]) -> Result<Vec<SourceSessionRecord>, AppError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| AppError::BadRequest("request body is not valid UTF-8".into()))?;

    let trimmed = text.trim();

    // Try JSON array first.
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed)
            .map_err(|e| AppError::BadRequest(format!("invalid JSON array: {e}")));
    }

    // Try NDJSON.
    let mut records = Vec::new();
    for (line_no, line) in trimmed.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: SourceSessionRecord = serde_json::from_str(line)
            .map_err(|e| AppError::BadRequest(format!("invalid NDJSON at line {line_no}: {e}")))?;
        records.push(rec);
    }

    if records.is_empty() {
        return Err(AppError::BadRequest(
            "request body is empty or not valid JSON/NDJSON".into(),
        ));
    }

    Ok(records)
}

/// Insert a single session record inside its own transaction.
/// Returns `true` if the row was newly inserted, `false` if it already existed.
fn import_session_record(
    conn: &Connection,
    record: &SourceSessionRecord,
    transcript_json: &str,
) -> Result<bool, rusqlite::Error> {
    conn.execute_batch("BEGIN IMMEDIATE")?;

    let tool_str = match record.source_tool {
        fieldcraft_core::AgentTool::Codex => "codex",
        fieldcraft_core::AgentTool::ClaudeCode => "claude",
        fieldcraft_core::AgentTool::Cursor => "cursor",
        fieldcraft_core::AgentTool::Unknown => "unknown",
    };

    let status_str = match record.status {
        fieldcraft_core::SourceSessionStatus::Pending => "pending",
        fieldcraft_core::SourceSessionStatus::Running => "running",
        fieldcraft_core::SourceSessionStatus::Succeeded => "succeeded",
        fieldcraft_core::SourceSessionStatus::Failed => "failed",
        fieldcraft_core::SourceSessionStatus::Stale => "stale",
    };

    let rows = conn.execute(
        "INSERT OR IGNORE INTO source_sessions
         (source_id, source_tool, source_path, session_id, project, machine, cwd,
          title, started_at, updated_at, discovered_at, source_fingerprint,
          transcript_json, status, attempts, extractor_version, last_error, last_extracted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        rusqlite::params![
            record.source_id,
            tool_str,
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
            status_str,
            record.attempts,
            record.extractor_version,
            record.last_error,
            record.last_extracted_at,
        ],
    )?;

    conn.execute_batch("COMMIT")?;
    Ok(rows > 0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
    };
    use chrono::Utc;
    use fieldcraft_core::{
        AgentTool, KnowledgeKind, KnowledgeRecord, KnowledgeSource, KnowledgeState, KnowledgeTier,
    };
    use std::net::SocketAddr;
    use std::path::Path;
    use tempfile::tempdir;
    use tower::ServiceExt;

    static LESSON_SEARCH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn make_state(dir: &Path) -> AppState {
        let knowledge_path = dir.join("knowledge.sqlite3");
        let repo = Arc::new(Mutex::new(
            KnowledgeRepository::open(&knowledge_path).unwrap(),
        ));
        let memories = Arc::new(Mutex::new(MemoryRepository::open(&knowledge_path).unwrap()));
        let conn = fieldcraft_storage::open_source_sessions_catalog(dir).unwrap();
        let tokens = Arc::new(TokenStore::open(&dir.join("tokens.sqlite3")).unwrap());
        AppState {
            repo,
            readers: Arc::new(readers::ReaderPool::new(knowledge_path.clone())),
            memories,
            source_sessions: Arc::new(Mutex::new(conn)),
            tokens,
            db_path: knowledge_path,
            enroll_policy: EnrollPolicy::default(),
        }
    }

    /// Build a request with a fake `ConnectInfo<SocketAddr>` extension so
    /// `tower_governor`'s peer-IP extractor has something to key on. In
    /// production this is supplied by
    /// `axum::serve(..).into_make_service_with_connect_info::<SocketAddr>()`.
    fn req_with_peer(method: &str, uri: &str, body: Body) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
        req
    }

    fn req_with_peer_json(method: &str, uri: &str, json: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
        req
    }

    /// `POST /enroll` from an arbitrary peer, optionally carrying the enroll secret.
    fn enroll_req(peer: [u8; 4], secret: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/enroll")
            .header("content-type", "application/json");
        if let Some(secret) = secret {
            builder = builder.header(crate::auth::ENROLL_SECRET_HEADER, secret);
        }
        let mut req = builder.body(Body::from(r#"{"label":"t"}"#)).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((peer, 0))));
        req
    }

    #[tokio::test]
    async fn enroll_rejects_remote_peer_without_secret_by_default() {
        let dir = tempdir().unwrap();
        let app = router(make_state(dir.path()));
        let resp = app
            .oneshot(enroll_req([100, 64, 0, 7], None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn enroll_with_secret_requires_it_from_every_peer() {
        let dir = tempdir().unwrap();
        let mut state = make_state(dir.path());
        state.enroll_policy = EnrollPolicy {
            secret: Some("s3cret".into()),
            open: false,
        };
        let app = router(state);
        let remote_ok = app
            .clone()
            .oneshot(enroll_req([100, 64, 0, 7], Some("s3cret")))
            .await
            .unwrap();
        assert_eq!(remote_ok.status(), StatusCode::OK);
        let remote_wrong = app
            .clone()
            .oneshot(enroll_req([100, 64, 0, 7], Some("nope")))
            .await
            .unwrap();
        assert_eq!(remote_wrong.status(), StatusCode::FORBIDDEN);
        // Loopback is not exempt once a secret is set: a reverse proxy looks local.
        let loopback_missing = app.oneshot(enroll_req([127, 0, 0, 1], None)).await.unwrap();
        assert_eq!(loopback_missing.status(), StatusCode::FORBIDDEN);
    }

    fn authed_req(method: &str, uri: &str, token: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
        req
    }

    fn authed_json_req(method: &str, uri: &str, token: &str, json: String) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(json))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
        req
    }

    fn mint_token(state: &AppState) -> String {
        state.tokens.mint(Some("test")).unwrap()
    }

    async fn spawn_test_server(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    fn memory_body(text: &str, extracted: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "text": text,
            "audio_ref": null,
            "source": "typed",
            "extracted": extracted
        })
    }

    fn make_record(id: &str, project_id: &str, retrieval_text: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            protected: false,
            id: id.to_string(),
            project_id: project_id.to_string(),
            kind: KnowledgeKind::Claim,
            title: format!("Test {id}"),
            payload_json: r#"{"statement":"feedback loop sentinel"}"#.to_string(),
            state: KnowledgeState::Active,
            confidence: 0.8,
            source: KnowledgeSource::Session {
                session_id: "source-session".to_string(),
                tool: AgentTool::Codex,
            },
            superseded_by: None,
            project_specific: true,
            created_at: Utc::now(),
            valid_until: None,
            retrieval_text: retrieval_text.to_string(),
            schema_version: 2,
            phase: None,
            tier: KnowledgeTier::Active,
            owner: None,
            session_date: None,
            source_refs: vec![],
        }
    }

    #[tokio::test]
    async fn enroll_returns_token() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let app = router(state);

        let resp = app
            .oneshot(req_with_peer_json("POST", "/enroll", r#"{"label":"test"}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["token"].as_str().is_some(),
            "token field must be present"
        );
        // Token is 64-char hex (32 bytes)
        let token = json["token"].as_str().unwrap();
        assert_eq!(token.len(), 64, "token should be 64-char hex");
    }

    #[tokio::test]
    async fn auth_rejects_missing_bearer() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let app = router(state);

        let resp = app
            .oneshot(req_with_peer("GET", "/lessons?q=test", Body::empty()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_rejects_bad_bearer() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let app = router(state);

        let mut req = Request::builder()
            .method("GET")
            .uri("/lessons?q=test")
            .header("authorization", "Bearer notarealtoken")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stats_endpoint_returns_authoritative_counts() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        {
            let repo = state.repo.lock().unwrap();
            let mut record = make_record("stats-endpoint", "proj", "stats endpoint sentinel");
            record.confidence = 0.5;
            repo.insert_knowledge(&record).unwrap();
            repo.apply_feedback_outcome(
                "stats-endpoint",
                fieldcraft_storage::FeedbackOutcome::Confirmed,
                Some("sess-stats"),
                "test",
                None,
            )
            .unwrap();
        }
        let app = router(state);

        let resp = app
            .oneshot(authed_req("GET", "/stats?days=7", &token))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["confirmed_recent"], 1);
        assert_eq!(json["dynamic_confidence_non_null"], 1);
    }

    #[tokio::test]
    async fn maintenance_endpoints_return_read_only_json() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        {
            let repo = state.repo.lock().unwrap();
            repo.insert_knowledge(&make_record(
                "analyze-endpoint",
                "proj",
                "analyze endpoint sentinel",
            ))
            .unwrap();
        }
        let app = router(state);

        let unauthenticated = app
            .clone()
            .oneshot(req_with_peer("GET", "/analyze?days=14", Body::empty()))
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let analyze = app
            .clone()
            .oneshot(authed_req("GET", "/analyze?days=14&limit=5", &token))
            .await
            .unwrap();
        assert_eq!(analyze.status(), StatusCode::OK);
        let body = axum::body::to_bytes(analyze.into_body(), 8192)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["window_days"], 14);
        assert!(json["summary"]["total_lessons"].as_i64().unwrap() >= 1);

        let lifecycle = app
            .oneshot(authed_req("GET", "/lifecycle?min_age_days=60", &token))
            .await
            .unwrap();
        assert_eq!(lifecycle.status(), StatusCode::OK);
        let body = axum::body::to_bytes(lifecycle.into_body(), 8192)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["min_age_days"], 60);
        assert!(json["candidates"].is_array());
    }

    #[tokio::test]
    async fn write_lesson_indexes_dense_vector_best_effort() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let app = router(state.clone());
        let record = make_record(
            "server-write-index",
            "proj",
            "server write path should attempt dense indexing",
        );

        let resp = app
            .oneshot(authed_json_req(
                "POST",
                "/lessons",
                &token,
                serde_json::to_string(&record).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let repo = state.repo.lock().unwrap();
        assert!(repo.get_knowledge("server-write-index").unwrap().is_some());
        if let Some(vector) = repo.load_dense_vector("server-write-index").unwrap() {
            assert!(!vector.is_empty(), "stored dense vector must be non-empty");
        }
    }

    #[tokio::test]
    async fn dream_endpoint_returns_clusters_and_creates_derived_claim() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        {
            let repo = state.repo.lock().unwrap();
            for (id, vector) in [
                ("dream-http-1", [1.0_f32, 0.0_f32]),
                ("dream-http-2", [0.866_f32, 0.5_f32]),
                ("dream-http-3", [0.5_f32, 0.866_f32]),
            ] {
                repo.insert_knowledge(&make_record(id, "proj", "dream http sentinel"))
                    .unwrap();
                repo.store_dense_vector(id, &vector).unwrap();
            }
            for _ in 0..2 {
                repo.connection()
                    .execute(
                        "INSERT INTO lesson_outcome_events
                         (knowledge_id, session_id, outcome, source, created_at)
                         VALUES ('dream-http-1', 's', 'confirmed', 'test', ?1)",
                        rusqlite::params![Utc::now().to_rfc3339()],
                    )
                    .unwrap();
            }
        }
        let app = router(state.clone());

        let resp = app
            .clone()
            .oneshot(authed_req("GET", "/dream?max_clusters=1", &token))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let clusters = json["clusters"].as_array().unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0]["project_id"], "proj");
        assert_eq!(clusters[0]["already_dreamed"], false);
        assert_eq!(clusters[0]["members"].as_array().unwrap().len(), 3);

        let source_ids: Vec<String> = clusters[0]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| member["id"].as_str().unwrap().to_string())
            .collect();
        let invalid_create = app
            .clone()
            .oneshot(authed_json_req(
                "POST",
                "/dream",
                &token,
                serde_json::json!({
                    "source_ids": ["dream-http-1", "dream-http-2", "missing-source"],
                    "project_id": "proj",
                    "title": "Invalid dream claim",
                    "statement": "This should not be accepted."
                })
                .to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_create.status(), StatusCode::BAD_REQUEST);

        let create_body = serde_json::json!({
            "source_ids": source_ids,
            "project_id": "proj",
            "title": "Dream HTTP claim",
            "statement": "The server-backed dream endpoint creates provisional derived claims."
        });
        let create = app
            .clone()
            .oneshot(authed_json_req(
                "POST",
                "/dream",
                &token,
                create_body.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created_body = axum::body::to_bytes(create.into_body(), 4096)
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_slice(&created_body).unwrap();
        let id = created["id"].as_str().unwrap();

        let repeat = app
            .oneshot(authed_json_req(
                "POST",
                "/dream",
                &token,
                create_body.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(repeat.status(), StatusCode::CONFLICT);

        let repo = state.repo.lock().unwrap();
        let record = repo.get_knowledge(id).unwrap().unwrap();
        assert_eq!(record.state, KnowledgeState::Provisional);
        let provenance_rows: i64 = repo
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_events
                 WHERE knowledge_id = ?1 AND event_type = 'derived_from'",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provenance_rows, 3);
    }

    #[tokio::test]
    async fn dream_create_rejects_more_than_twenty_four_sources() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let app = router(state);
        let source_ids: Vec<String> = (0..25).map(|i| format!("source-{i}")).collect();

        let resp = app
            .oneshot(authed_json_req(
                "POST",
                "/dream",
                &token,
                serde_json::json!({
                    "source_ids": source_ids,
                    "project_id": "proj",
                    "title": "Too large",
                    "statement": "This derived claim cites too many sources."
                })
                .to_string(),
            ))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .contains("expected 3..=24")
        );
    }

    #[tokio::test]
    async fn enroll_rate_limit_returns_429_after_burst() {
        // Burst is 10, so the 11th request from the same peer in the same
        // window must be 429.
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let app = router(state);

        let mut last_status = StatusCode::OK;
        for _ in 0..11 {
            let req = req_with_peer_json("POST", "/enroll", r#"{"label":"flood"}"#);
            last_status = app.clone().oneshot(req).await.unwrap().status();
        }
        assert_eq!(
            last_status,
            StatusCode::TOO_MANY_REQUESTS,
            "11th rapid /enroll request must be rate-limited"
        );
    }

    #[tokio::test]
    async fn status_reports_remote_project_counts() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        state
            .repo
            .lock()
            .unwrap()
            .insert_knowledge(&make_record("status-1", "proj", "status one"))
            .unwrap();
        state
            .repo
            .lock()
            .unwrap()
            .insert_knowledge(&make_record("status-2", "other", "status two"))
            .unwrap();
        let app = router(state);

        let unauthenticated = app
            .clone()
            .oneshot(req_with_peer("GET", "/status?project=proj", Body::empty()))
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .oneshot(authed_req("GET", "/status?project=proj", &token))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["project_id"], "proj");
        assert_eq!(json["total_records"], 1);
        assert_eq!(json["rows"][0]["kind"], "claim");
        assert_eq!(json["rows"][0]["state"], "active");
        assert_eq!(json["rows"][0]["count"], 1);
        assert_eq!(json["rows"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn lessons_and_status_canonicalize_project_slug_variants() {
        let _guard = LESSON_SEARCH_TEST_LOCK.lock().await;
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        state
            .repo
            .lock()
            .unwrap()
            .insert_knowledge(&make_record(
                "slug-canonical",
                "field_notes",
                "slug canonical sentinel",
            ))
            .unwrap();
        let repo = Arc::clone(&state.repo);
        let app = router(state);

        let status = app
            .clone()
            .oneshot(authed_req("GET", "/status?project=field-notes", &token))
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let status_body = axum::body::to_bytes(status.into_body(), 4096)
            .await
            .unwrap();
        let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
        assert_eq!(status_json["project_id"], "field_notes");
        assert_eq!(status_json["total_records"], 1);

        let search = app
            .clone()
            .oneshot(authed_req(
                "GET",
                "/lessons?q=slug%20canonical&project=Field-Notes&session_id=slug-session",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(search.status(), StatusCode::OK);

        let mut new_record = make_record("slug-written", "Field-Notes", "new slug lesson");
        new_record.title = "New slug lesson".to_string();
        let write = app
            .oneshot(authed_json_req(
                "POST",
                "/lessons",
                &token,
                serde_json::to_string(&new_record).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(write.status(), StatusCode::OK);

        let repo = repo.lock().unwrap();
        let stored = repo.get_knowledge("slug-written").unwrap().unwrap();
        assert_eq!(stored.project_id, "field_notes");
        let retrieval_project: String = repo
            .connection()
            .query_row(
                "SELECT project_id FROM retrieval_log WHERE session_id = 'slug-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retrieval_project, "field_notes");
    }

    #[tokio::test]
    async fn learning_source_queue_and_claim_are_server_orchestrated() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let app = router(state);
        let body = r#"[{
            "uri": "https://example.com/source-learning",
            "project": "proj",
            "title": "Source Learning",
            "scope_hint": "derive retrieval lessons"
        }]"#;

        let queue_resp = app
            .clone()
            .oneshot(authed_json_req(
                "POST",
                "/learning/sources",
                &token,
                body.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(queue_resp.status(), StatusCode::OK);
        let queue_body = axum::body::to_bytes(queue_resp.into_body(), 4096)
            .await
            .unwrap();
        let queue_json: serde_json::Value = serde_json::from_slice(&queue_body).unwrap();
        assert_eq!(queue_json["summary"]["imported"], 1);

        let claim_resp = app
            .oneshot(authed_req(
                "GET",
                "/learning/sources/claim?project=proj&limit=1&worker_id=test-worker",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(claim_resp.status(), StatusCode::OK);
        let claim_body = axum::body::to_bytes(claim_resp.into_body(), 8192)
            .await
            .unwrap();
        let claimed: Vec<LearningSourceRecord> = serde_json::from_slice(&claim_body).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].project, "proj");
        assert_eq!(claimed[0].status.as_str(), "running");
        assert_eq!(claimed[0].worker_id.as_deref(), Some("test-worker"));
    }

    #[tokio::test]
    async fn write_lesson_enqueues_server_side_merge_candidate() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let repo = Arc::clone(&state.repo);
        let app = router(state);
        let record = make_record("write-queue-1", "proj", "queue me for merge");

        let resp = app
            .oneshot(authed_json_req(
                "POST",
                "/lessons",
                &token,
                serde_json::to_string(&record).unwrap(),
            ))
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "body: {}",
            String::from_utf8_lossy(&body)
        );

        let queued: i64 = repo
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM knowledge_merge_queue
                 WHERE lesson_id = 'write-queue-1' AND status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, 1);
    }

    #[tokio::test]
    async fn merge_queue_worker_archives_obvious_duplicate_written_over_http() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let mut canonical = make_record(
            "merge-canon",
            "proj",
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors",
        );
        canonical.title =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors".to_string();
        state
            .repo
            .lock()
            .unwrap()
            .insert_knowledge(&canonical)
            .unwrap();

        let mut duplicate = make_record(
            "merge-dupe",
            "proj",
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors",
        );
        duplicate.title =
            "CloudKit JS saveRecords returns HTTP 200 with per-record errors".to_string();
        let app = router(state.clone());
        let resp = app
            .oneshot(authed_json_req(
                "POST",
                "/lessons",
                &token,
                serde_json::to_string(&duplicate).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let processed = process_merge_queue_once(&state, 10).unwrap();
        assert_eq!(processed, 1);
        let archived = state
            .repo
            .lock()
            .unwrap()
            .get_knowledge("merge-dupe")
            .unwrap()
            .unwrap();
        assert_eq!(archived.state, KnowledgeState::Superseded);
        assert_eq!(archived.tier, KnowledgeTier::Archive);
        assert_eq!(archived.superseded_by.as_deref(), Some("merge-canon"));
    }

    #[tokio::test]
    async fn lesson_search_logs_session_and_query_cue_bundle() {
        let _guard = LESSON_SEARCH_TEST_LOCK.lock().await;
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        state
            .repo
            .lock()
            .unwrap()
            .insert_knowledge(&make_record(
                "search-1",
                "proj",
                "feedback loop sentinel retrieval text",
            ))
            .unwrap();
        let repo = Arc::clone(&state.repo);
        let app = router(state);

        let resp = app
            .oneshot(authed_req(
                "GET",
                "/lessons?q=feedback%20loop%20sentinel&project=proj&limit=5&session_id=sess-test",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let retrievals = repo
            .lock()
            .unwrap()
            .retrievals_for_project_window(
                "proj",
                "1970-01-01T00:00:00Z",
                "2999-01-01T00:00:00Z",
                Some("sess-test"),
            )
            .unwrap();
        assert_eq!(retrievals.len(), 1);
        assert!(
            retrievals[0]
                .query_cue_bundle
                .as_ref()
                .is_some_and(|bundle| !bundle.is_empty()),
            "remote retrieval logs need query cues for feedback attribution"
        );
        assert!(
            retrievals[0].result_ids.iter().any(|id| id == "search-1"),
            "session-tagged retrieval should contain the surfaced lesson id"
        );
    }

    #[tokio::test]
    async fn lesson_search_rejects_overlong_session_id() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let app = router(state);
        let long_session = "s".repeat(129);
        let uri = format!("/lessons?q=test&project=proj&session_id={long_session}");

        let resp = app.oneshot(authed_req("GET", &uri, &token)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_memories_201_returns_memory_with_id_and_revision() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        let resp = client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body("Remember Ada", serde_json::json!({})))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["memory"]["id"].as_str().is_some());
        assert_eq!(body["memory"]["revision"], 1);
        handle.abort();
    }

    #[tokio::test]
    async fn get_memory_sets_etag_header() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let created: serde_json::Value = client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body("Remember Ada", serde_json::json!({})))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["memory"]["id"].as_str().unwrap();

        let resp = client
            .get(format!("{base}/memories/{id}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.headers().get("etag").unwrap(), "\"1\"");
        handle.abort();
    }

    #[tokio::test]
    async fn patch_without_if_match_returns_412() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let created: serde_json::Value = client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body("Remember Ada", serde_json::json!({})))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["memory"]["id"].as_str().unwrap();

        let resp = client
            .patch(format!("{base}/memories/{id}"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"text": "Updated"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::PRECONDITION_REQUIRED);
        handle.abort();
    }

    #[tokio::test]
    async fn patch_with_stale_if_match_returns_409_with_current_revision() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let created: serde_json::Value = client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body("Remember Ada", serde_json::json!({})))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["memory"]["id"].as_str().unwrap();

        let resp = client
            .patch(format!("{base}/memories/{id}"))
            .bearer_auth(&token)
            .header("if-match", "\"99\"")
            .json(&serde_json::json!({"text": "Updated"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "revision_conflict");
        assert_eq!(body["current_revision"], 1);
        handle.abort();
    }

    #[tokio::test]
    async fn patch_with_matching_if_match_returns_200_and_new_etag() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let created: serde_json::Value = client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body("Remember Ada", serde_json::json!({})))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["memory"]["id"].as_str().unwrap();

        let resp = client
            .patch(format!("{base}/memories/{id}"))
            .bearer_auth(&token)
            .header("if-match", "\"1\"")
            .json(&serde_json::json!({"text": "Updated"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.headers().get("etag").unwrap(), "\"2\"");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["memory"]["text"], "Updated");
        handle.abort();
    }

    #[tokio::test]
    async fn patch_person_merge_from_reattributes_and_returns_survivor() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        // Create the canonical person and a misheard split.
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Talk to Dalma",
                serde_json::json!({"people": ["Dalma"]}),
            ))
            .send()
            .await
            .unwrap();
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Talk to Delma",
                serde_json::json!({"people": ["Delma"]}),
            ))
            .send()
            .await
            .unwrap();

        let resp = client
            .patch(format!("{base}/people/dalma"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"merge_from": ["delma"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["person"]["canonical_id"], "dalma");
        assert_eq!(body["person"]["mention_count"], 2);

        // Only the survivor remains.
        let people: serde_json::Value = client
            .get(format!("{base}/people"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(people["items"].as_array().unwrap().len(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn patch_person_rename_updates_display_name() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Talk to Dalma",
                serde_json::json!({"people": ["Dalma"]}),
            ))
            .send()
            .await
            .unwrap();

        let resp = client
            .patch(format!("{base}/people/dalma"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"display_name": "Dalma Kovacs"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["person"]["display_name"], "Dalma Kovacs");
        handle.abort();
    }

    #[tokio::test]
    async fn patch_person_self_merge_returns_409() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Talk to Dalma",
                serde_json::json!({"people": ["Dalma"]}),
            ))
            .send()
            .await
            .unwrap();

        let resp = client
            .patch(format!("{base}/people/dalma"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"merge_from": ["dalma"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
        handle.abort();
    }

    #[tokio::test]
    async fn patch_person_empty_body_returns_400() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Talk to Dalma",
                serde_json::json!({"people": ["Dalma"]}),
            ))
            .send()
            .await
            .unwrap();

        let resp = client
            .patch(format!("{base}/people/dalma"))
            .bearer_auth(&token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
        handle.abort();
    }

    #[tokio::test]
    async fn patch_project_merge_from_reattributes_and_returns_survivor() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Ship Fieldcraft",
                serde_json::json!({"project": {"canonical_id": "fieldcraft", "slug": "fieldcraft", "raw_text": "Fieldcraft"}}),
            ))
            .send()
            .await
            .unwrap();
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Ship Feeldcraft",
                serde_json::json!({"project": {"canonical_id": "feeldcraft", "slug": "feeldcraft", "raw_text": "Feeldcraft"}}),
            ))
            .send()
            .await
            .unwrap();

        let resp = client
            .patch(format!("{base}/projects/fieldcraft"))
            .bearer_auth(&token)
            .json(&serde_json::json!({"merge_from": ["feeldcraft"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["project"]["canonical_id"], "fieldcraft");
        assert_eq!(body["project"]["mention_count"], 2);

        let projects: serde_json::Value = client
            .get(format!("{base}/projects"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(projects["items"].as_array().unwrap().len(), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn health_endpoint_is_unauthenticated_and_returns_ok() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let (base, handle) = spawn_test_server(state).await;
        let body = reqwest::get(format!("{base}/health"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
        handle.abort();
    }

    #[tokio::test]
    async fn views_today_returns_open_actions_for_seeded_data() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        client
            .post(format!("{base}/memories"))
            .bearer_auth(&token)
            .json(&memory_body(
                "Do the demo",
                serde_json::json!({
                    "actions": [
                        {"text": "Ship demo", "phase": "open", "due_at": "2026-05-01T00:00:00Z"},
                        {"text": "Done task", "phase": "done"}
                    ]
                }),
            ))
            .send()
            .await
            .unwrap();

        let resp = client
            .get(format!("{base}/views/today?now=2026-05-28T00:00:00Z"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["text"], "Ship demo");
        handle.abort();
    }

    #[tokio::test]
    async fn lessons_endpoints_still_work() {
        let _guard = LESSON_SEARCH_TEST_LOCK.lock().await;
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        let (base, handle) = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let record = make_record("lesson-regression", "proj", "lesson regression sentinel");

        let resp = client
            .post(format!("{base}/lessons"))
            .bearer_auth(&token)
            .json(&record)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let search = client
            .get(format!(
                "{base}/lessons?q=lesson%20regression%20sentinel&project=proj"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let status = search.status();
        let body = search.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
        handle.abort();
    }

    #[tokio::test]
    async fn trace_analyze_confirms_served_lesson_when_changed_file_overlaps() {
        let dir = tempdir().unwrap();
        let state = make_state(dir.path());
        let token = mint_token(&state);
        {
            let repo = state.repo.lock().unwrap();
            let mut record = make_record("file-overlap-lesson", "proj", "file overlap sentinel");
            record.payload_json =
                r#"{"statement":"touches storage","affected_files":["crates/a/src/storage.rs"]}"#
                    .to_string();
            record.confidence = 0.5;
            repo.insert_knowledge(&record).unwrap();
            repo.log_retrieval_entry(&fieldcraft_storage::RetrievalLogEntry {
                project_id: "proj",
                query_text: "file overlap query",
                result_ids: &["file-overlap-lesson".to_string()],
                result_scores: &[1.0],
                latency_ms: 1,
                session_id: "sess-file-overlap",
                ..Default::default()
            })
            .unwrap();
        }

        let app = router(state.clone());
        let body = serde_json::json!({
            "session_id": "sess-file-overlap",
            "project_id": "proj",
            "since": "2000-01-01T00:00:00Z",
            "until": "2999-01-01T00:00:00Z",
            "cited_ids": [],
            "contradicted": [],
            "changed_files": ["workspace/other/storage.rs"]
        })
        .to_string();

        let resp = app
            .oneshot(authed_json_req(
                "POST",
                "/sessions/trace-analyze",
                &token,
                body.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["confirmed_emitted"], 1);
        assert_eq!(json["ignored_emitted"], 0);

        let event_count: i64 = state
            .repo
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM lesson_outcome_events \
                 WHERE knowledge_id = 'file-overlap-lesson' \
                   AND session_id = 'sess-file-overlap' \
                   AND source = 'trace_analyzer' \
                   AND outcome = 'confirmed' \
                   AND note = 'file_overlap'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 1);

        let app = router(state.clone());
        let resp = app
            .oneshot(authed_json_req(
                "POST",
                "/sessions/trace-analyze",
                &token,
                body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["confirmed_emitted"], 0);
        assert!(json["idempotent"].as_u64().unwrap_or(0) >= 1);

        let event_count_after_rerun: i64 = state
            .repo
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM lesson_outcome_events \
                 WHERE knowledge_id = 'file-overlap-lesson' \
                   AND session_id = 'sess-file-overlap' \
                   AND source = 'trace_analyzer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count_after_rerun, 1);
    }
}
