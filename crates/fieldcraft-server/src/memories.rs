use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use fieldcraft_storage::{ActionFilter, MemoryError, MemoryInput, MemoryPatch, RebuildSummary};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{AppError, AppState};

#[derive(Debug, Deserialize)]
pub struct ListMemoriesQuery {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub include_archived: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct TodayQuery {
    pub now: Option<String>,
}

#[derive(Debug, Serialize)]
struct RebuildResponse {
    summary: RebuildSummary,
}

pub async fn create_memory(
    State(state): State<AppState>,
    Json(input): Json<MemoryInput>,
) -> Result<Response, AppError> {
    let memory = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .insert(input)
        .map_err(app_error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "memory": memory
        })),
    )
        .into_response())
}

pub async fn list_memories(
    State(state): State<AppState>,
    Query(query): Query<ListMemoriesQuery>,
) -> Result<Response, AppError> {
    let (memories, next_cursor) = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .list(
            query.cursor.as_deref(),
            query.limit.unwrap_or(50),
            query.include_archived.unwrap_or(false),
        )
        .map_err(app_error)?;
    Ok(Json(json!({
        "memories": memories,
        "next_cursor": next_cursor,
    }))
    .into_response())
}

pub async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let memory = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .get(&id)
        .map_err(app_error)?;
    json_with_etag(StatusCode::OK, memory.revision, json!({ "memory": memory }))
}

pub async fn patch_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(patch): Json<MemoryPatch>,
) -> Result<Response, AppError> {
    let expected_revision = match match_revision(&headers) {
        Ok(revision) => revision,
        Err(response) => return Ok(response),
    };
    let result = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .patch(&id, expected_revision, patch);
    match result {
        Ok(memory) => json_with_etag(StatusCode::OK, memory.revision, json!({ "memory": memory })),
        Err(MemoryError::RevisionConflict { actual, .. }) => Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "revision_conflict",
                "current_revision": actual
            })),
        )
            .into_response()),
        Err(other) => Err(app_error(other)),
    }
}

pub async fn archive_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let expected_revision = match match_revision(&headers) {
        Ok(revision) => revision,
        Err(response) => return Ok(response),
    };
    let result = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .archive(&id, expected_revision);
    match result {
        Ok(memory) => json_with_etag(StatusCode::OK, memory.revision, json!({ "memory": memory })),
        Err(MemoryError::RevisionConflict { actual, .. }) => Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "revision_conflict",
                "current_revision": actual
            })),
        )
            .into_response()),
        Err(other) => Err(app_error(other)),
    }
}

/// Body for `PATCH /people/{id}` and `PATCH /projects/{id}`.
/// Applies a rename (`display_name`) and/or a merge (`merge_from`).
#[derive(Debug, Deserialize)]
pub struct PatchEntityBody {
    pub display_name: Option<String>,
    pub merge_from: Option<Vec<String>>,
}

pub async fn patch_person(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchEntityBody>,
) -> Result<Response, AppError> {
    if body.display_name.is_none() && body.merge_from.is_none() {
        return Err(AppError::BadRequest(
            "expected display_name and/or merge_from".into(),
        ));
    }
    let repo = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
    let mut person = None;
    if let Some(from) = body.merge_from.as_ref().filter(|f| !f.is_empty()) {
        person = Some(repo.merge_people(&id, from).map_err(entity_error)?);
    }
    if let Some(name) = body.display_name.as_deref() {
        person = Some(repo.rename_person(&id, name).map_err(entity_error)?);
    }
    let person = match person {
        Some(person) => person,
        // merge_from was present but empty: surface a clear error.
        None => repo.rename_person(&id, "").map_err(entity_error)?,
    };
    Ok(Json(json!({ "person": person })).into_response())
}

pub async fn patch_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchEntityBody>,
) -> Result<Response, AppError> {
    if body.display_name.is_none() && body.merge_from.is_none() {
        return Err(AppError::BadRequest(
            "expected display_name and/or merge_from".into(),
        ));
    }
    let repo = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?;
    let mut project = None;
    if let Some(from) = body.merge_from.as_ref().filter(|f| !f.is_empty()) {
        project = Some(repo.merge_projects(&id, from).map_err(entity_error)?);
    }
    if let Some(name) = body.display_name.as_deref() {
        project = Some(repo.rename_project(&id, name).map_err(entity_error)?);
    }
    let project = match project {
        Some(project) => project,
        None => repo.rename_project(&id, "").map_err(entity_error)?,
    };
    Ok(Json(json!({ "project": project })).into_response())
}

pub async fn list_people(State(state): State<AppState>) -> Result<Response, AppError> {
    let items = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .list_people()
        .map_err(app_error)?;
    Ok(Json(json!({ "items": items })).into_response())
}

pub async fn list_projects(State(state): State<AppState>) -> Result<Response, AppError> {
    let items = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .list_projects()
        .map_err(app_error)?;
    Ok(Json(json!({ "items": items })).into_response())
}

pub async fn list_shapes(State(state): State<AppState>) -> Result<Response, AppError> {
    let items = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .list_shapes()
        .map_err(app_error)?;
    Ok(Json(json!({ "items": items })).into_response())
}

pub async fn view_today(
    State(state): State<AppState>,
    Query(query): Query<TodayQuery>,
) -> Result<Response, AppError> {
    let now = query.now.unwrap_or_else(|| Utc::now().to_rfc3339());
    let actions = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .view_today(&now)
        .map_err(app_error)?;
    Ok(Json(json!({ "items": actions })).into_response())
}

pub async fn view_inbox(
    State(state): State<AppState>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, AppError> {
    let memories = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .view_inbox(query.limit.unwrap_or(50))
        .map_err(app_error)?;
    Ok(Json(json!({ "memories": memories })).into_response())
}

pub async fn view_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, AppError> {
    let memories = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .view_project(&id, query.limit.unwrap_or(50))
        .map_err(app_error)?;
    Ok(Json(json!({ "memories": memories })).into_response())
}

pub async fn view_person(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, AppError> {
    let memories = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .view_person(&id, query.limit.unwrap_or(50))
        .map_err(app_error)?;
    Ok(Json(json!({ "memories": memories })).into_response())
}

pub async fn view_actions(
    State(state): State<AppState>,
    Query(filter): Query<ActionFilter>,
) -> Result<Response, AppError> {
    let actions = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .view_actions(filter)
        .map_err(app_error)?;
    Ok(Json(json!({ "items": actions })).into_response())
}

pub async fn rebuild_views_handler(State(state): State<AppState>) -> Result<Response, AppError> {
    let summary = state
        .memories
        .lock()
        .map_err(|_| AppError::Internal("mutex poisoned".into()))?
        .rebuild_views()
        .map_err(app_error)?;
    Ok(Json(RebuildResponse { summary }).into_response())
}

// The Err variant is a fully-rendered axum `Response` (the established pattern
// for these handlers) — intentionally returned so callers can short-circuit
// without re-deriving the error body. clippy::result_large_err flags its size;
// that's accepted here rather than boxing a single helper inconsistently.
#[allow(clippy::result_large_err)]
fn match_revision(headers: &HeaderMap) -> Result<i64, Response> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Err((
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({
                "error": "precondition_required",
                "message": "If-Match header is required"
            })),
        )
            .into_response());
    };
    let value = value.to_str().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_request",
                "message": "invalid If-Match header"
            })),
        )
            .into_response()
    })?;
    let trimmed = value.trim().trim_matches('"');
    trimmed.parse::<i64>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_request",
                "message": "invalid If-Match header"
            })),
        )
            .into_response()
    })
}

fn json_with_etag(
    status: StatusCode,
    revision: i64,
    body: serde_json::Value,
) -> Result<Response, AppError> {
    let mut response = (status, Json(body)).into_response();
    let etag = HeaderValue::from_str(&format!("\"{revision}\""))
        .map_err(|e| AppError::Internal(format!("invalid etag: {e}")))?;
    response.headers_mut().insert(header::ETAG, etag);
    Ok(response)
}

/// Map storage errors for the entity-hygiene endpoints. A self-merge is a
/// conflict (409); other invalid input is a 400; a missing entity is a 404.
/// We special-case 404 vs the memory endpoints (which map NotFound to 400)
/// because PATCH targets an entity addressed in the path.
fn entity_error(error: MemoryError) -> AppError {
    match error {
        MemoryError::NotFound => AppError::NotFound("entity not found".into()),
        MemoryError::Invalid(message) if message.contains("into itself") => {
            AppError::Conflict(message)
        }
        MemoryError::Invalid(message) => AppError::BadRequest(message),
        MemoryError::Storage(message) => AppError::Storage(message),
        MemoryError::RevisionConflict { .. } => AppError::Conflict(error.to_string()),
    }
}

fn app_error(error: MemoryError) -> AppError {
    match error {
        MemoryError::NotFound => AppError::BadRequest("memory not found".into()),
        MemoryError::Invalid(message) => AppError::BadRequest(message),
        MemoryError::Storage(message) => AppError::Storage(message),
        MemoryError::RevisionConflict { .. } => AppError::BadRequest(error.to_string()),
    }
}
