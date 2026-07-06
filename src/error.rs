use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::store::StoreError;

/// API-level errors; every variant maps to a status code and a JSON body of
/// the shape `{"error": "..."}`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Gone(String),
    #[error("{0}")]
    Unprocessable(String),
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Gone(_) => StatusCode::GONE,
            ApiError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(ref err) = self {
            tracing::error!(error = %err, "internal error");
        }
        let status = self.status();
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Conflict(msg) => ApiError::Conflict(msg),
            // Stale is retried inside the posting engine; if one leaks the
            // command genuinely lost a concurrency race — 409 lets the
            // client retry.
            StoreError::Stale(msg) => ApiError::Conflict(msg),
            other => ApiError::Internal(other.into()),
        }
    }
}

impl From<crate::posting::engine::PostingError> for ApiError {
    fn from(err: crate::posting::engine::PostingError) -> Self {
        use crate::posting::engine::PostingError;
        match err {
            PostingError::Validation(msg) => ApiError::Unprocessable(msg),
            PostingError::NotFound => ApiError::NotFound,
            PostingError::Conflict(msg) => ApiError::Conflict(msg),
            PostingError::Store(err) => err.into(),
        }
    }
}
