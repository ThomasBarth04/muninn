use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::api::ErrorBody;

/// Every error the API returns: a status and a contract code,
/// serialized as `{"error":"<code>"}` (spec 001 Contract).
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str) -> ApiError {
        ApiError { status, code }
    }
    pub fn bad_request(code: &'static str) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, code)
    }
    pub fn conflict(code: &'static str) -> ApiError {
        ApiError::new(StatusCode::CONFLICT, code)
    }
    pub fn not_found() -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "notFound")
    }
    pub fn owner_only() -> ApiError {
        ApiError::new(StatusCode::FORBIDDEN, "ownerOnly")
    }
    pub fn unauthenticated() -> ApiError {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated")
    }
    pub fn internal() -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal")
    }
    /// An upstream (Postmark, Stripe) failed while the user waited.
    pub fn upstream() -> ApiError {
        ApiError::new(StatusCode::BAD_GATEWAY, "upstream")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.code.to_string(),
            }),
        )
            .into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> ApiError {
        tracing::error!("database: {e}");
        ApiError::internal()
    }
}
