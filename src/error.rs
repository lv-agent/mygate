use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("Unknown model alias: {0}")]
    UnknownAlias(String),

    #[error("All fallback attempts exhausted for alias: {0}")]
    AllFallbacksExhausted(String),

    #[error("Backend request failed: {0}")]
    BackendRequestFailed(String),

    #[error("Backend returned error {status}: {body}")]
    BackendError { status: u16, body: String },

    #[error("Config error: {0}")]
    #[allow(dead_code)]
    ConfigError(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

/// Whether a backend error should trigger fallback.
pub fn should_fallback(status: u16) -> bool {
    matches!(status, 429 | 500..=599)
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            GatewayError::UnknownAlias(_) => (StatusCode::NOT_FOUND, self.to_string()),
            GatewayError::AllFallbacksExhausted(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
            }
            GatewayError::BackendRequestFailed(_) => {
                (StatusCode::BAD_GATEWAY, self.to_string())
            }
            GatewayError::BackendError { .. } => (StatusCode::BAD_GATEWAY, self.to_string()),
            GatewayError::ConfigError(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            GatewayError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };

        let body = axum::Json(json!({
            "error": {
                "message": message,
                "type": "gateway_error",
            }
        }));

        (status, body).into_response()
    }
}
