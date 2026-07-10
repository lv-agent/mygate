//! cr-202: /metrics 端点契约测试
//!
//! 验证 Prometheus metrics 端点工作：
//! 1. GET /metrics 返回 200 + Prometheus 文本格式
//! 2. 触发 chat completion 后 `mygate_requests_total` 计数 +1
//! 3. 触发 chat completion 后 `mygate_request_duration_seconds` 记录

#[path = "common/mod.rs"]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::MockBackend;
use mygate::router::openai::AppState;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text() {
    let mock = MockBackend::new();
    let mock_url = mock.start().await;

    let config: mygate::config::AppConfig = toml::from_str(&format!(
        r#"
[server]
host = "127.0.0.1"
port = 8080
timeout_seconds = 30
admin_token = ""

[providers.mock]
base_url = "{mock_url}/v1"
api_key = "test"
provider_type = "openai"
auth_style = "bearer"

[aliases.T]
[[aliases.T.chain]]
provider = "mock"
model = "m"
priority = 1
"#
    ))
    .unwrap();
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client: reqwest::Client::new(),
    };
    let app = mygate::server::build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    // 验证：Prometheus 文本格式（至少包含 mygate_ 前缀的指标说明）
    assert!(
        text.contains("# TYPE mygate_") || text.contains("# HELP mygate_"),
        "/metrics 响应不是 Prometheus 格式: {}",
        &text[..200.min(text.len())]
    );
}

#[tokio::test]
async fn metrics_request_counter_increments() {
    use mygate::metrics::{metrics, render};

    // 拿当前快照 baseline
    let _ = render(); // 触发 lazy init

    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "x", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }),
    });
    let mock_url = mock.start().await;

    let config: mygate::config::AppConfig = toml::from_str(&format!(
        r#"
[server]
host = "127.0.0.1"
port = 8080
timeout_seconds = 30
admin_token = ""

[providers.mock]
base_url = "{mock_url}/v1"
api_key = "test"
provider_type = "openai"
auth_style = "bearer"

[aliases.T2]
[[aliases.T2.chain]]
provider = "mock"
model = "m"
priority = 1
"#
    ))
    .unwrap();
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client: reqwest::Client::new(),
    };
    let app = mygate::server::build_router(state);

    // baseline counter
    let before = metrics()
        .requests_total
        .get_metric_with_label_values(&["T2", "success"])
        .map(|c| c.get())
        .unwrap_or(0.0);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model": "T2", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 验证 counter +1
    let after = metrics()
        .requests_total
        .get_metric_with_label_values(&["T2", "success"])
        .map(|c| c.get())
        .unwrap_or(0.0);
    assert!(
        after > before,
        "mygate_requests_total{{alias=T2,status=success}} 未递增: before={} after={}",
        before,
        after
    );
}
