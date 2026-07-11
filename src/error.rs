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
    /// 403 with a caller-actionable message (the bare [`ApiError::Forbidden`]
    /// stays a plain "forbidden" — use this when the caller needs to know
    /// *why*, e.g. the bootstrap gate).
    #[error("{0}")]
    ForbiddenReason(String),
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
    /// 429 with a `Retry-After` header — a rate limit or intake cap tripped;
    /// the request is safe to retry after the given number of seconds.
    #[error("{message}")]
    TooManyRequests {
        message: String,
        retry_after_secs: u64,
    },
    /// 503 — a required piece of configuration is missing (e.g. the Stripe
    /// webhook secret); the request may succeed once the operator fixes it.
    #[error("{0}")]
    Unavailable(String),
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::ForbiddenReason(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Gone(_) => StatusCode::GONE,
            ApiError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
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
        let mut response = (status, Json(json!({ "error": self.to_string() }))).into_response();
        if let ApiError::TooManyRequests {
            retry_after_secs, ..
        } = self
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, retry_after_secs.into());
        }
        response
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
