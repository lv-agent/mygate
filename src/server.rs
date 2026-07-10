use axum::routing::{get, post};
use axum::Router;

use crate::router::openai::{chat_completions, list_models, reload_config, AppState};
use crate::router::anthropic;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        // OpenAI-compatible endpoints
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        // Anthropic-compatible endpoints
        .route("/v1/messages", post(anthropic::messages))
        // Admin endpoints
        .route("/admin/reload", post(reload_config))
        .route("/health", get(health_check))
        .route("/metrics", get(crate::metrics::metrics_handler))
        .with_state(state)
}

async fn health_check() -> &'static str {
    "ok"
}
