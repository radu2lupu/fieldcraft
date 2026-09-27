//! Application error type for fieldcraft-server HTTP handlers.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", self.to_string()),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, "forbidden", msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg.clone()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "conflict", msg.clone()),
            AppError::Storage(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                msg.clone(),
            ),
            AppError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                msg.clone(),
            ),
        };
        let body = json!({ "error": code, "message": message });
        (status, axum::Json(body)).into_response()
    }
}

impl From<fieldcraft_core::TrError> for AppError {
    fn from(e: fieldcraft_core::TrError) -> Self {
        match e {
            fieldcraft_core::TrError::Unauthorized => AppError::Unauthorized,
            fieldcraft_core::TrError::NotFound { message } => AppError::BadRequest(message),
            fieldcraft_core::TrError::InvalidRequest { message } => AppError::BadRequest(message),
            other => AppError::Internal(other.to_string()),
        }
    }
}
