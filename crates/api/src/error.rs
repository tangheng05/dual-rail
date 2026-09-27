use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use dual_rail_core::{MoneyError, ParseCodeError};
use serde_json::json;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unprocessable(String),
    NotFound,
    Conflict(String),
    BadGateway,
    PayloadTooLarge,
    Internal,
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(%err, "database error");
        Self::Internal
    }
}

impl From<ParseCodeError> for ApiError {
    fn from(err: ParseCodeError) -> Self {
        Self::Unprocessable(err.to_string())
    }
}

impl From<MoneyError> for ApiError {
    fn from(err: MoneyError) -> Self {
        Self::Unprocessable(err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Unprocessable(message) => (StatusCode::UNPROCESSABLE_ENTITY, message),
            Self::NotFound => (StatusCode::NOT_FOUND, "payment not found".to_owned()),
            Self::Conflict(message) => (StatusCode::CONFLICT, message),
            Self::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body is too large".to_owned(),
            ),
            Self::BadGateway => (
                StatusCode::BAD_GATEWAY,
                "payment provider is unavailable, try again".to_owned(),
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            ),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
