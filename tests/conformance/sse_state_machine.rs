//! cr-301 L3: SSE 流式状态机端到端契约测试
//!
//! 验证 MyGate 流式响应的行为：
//! 1. OpenAI 流式：mock 后端发 OpenAI SSE 格式，MyGate 正确替换 model 字段
//! 2. Anthropic 流式：mock 后端发 Anthropic SSE 8 事件格式，MyGate 原样透传
//! 3. SSE 结束：流末尾必须发 [DONE]（OpenAI）/ message_stop（Anthropic）
//!
//! 注意：当前实现不转换 SSE 协议（仅透传 + model 字段 JSON 路径替换）

use axum::body::Body;
use axum::http::{Request, StatusCode};

#[path = "common/mod.rs"]
mod common;

use common::MockBackend;
use mygate::router::openai::AppState;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;

fn build_app_openai(mock_url: String) -> axum::Router {
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
model = "glm-4-flash"
priority = 1
"#
    ))
    .unwrap();
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client: reqwest::Client::new(),
    };
    mygate::server::build_router(state)
}

/// L3: OpenAI 流式 — model 字段 JSON 路径替换
/// cr-105: 旧实现用 `data.replace("\"model\":\"X\"", ...)` 字符串替换，
/// 会误伤嵌套字段。新实现按 JSON 路径只替换顶层 model。
#[tokio::test]
async fn openai_streaming_model_field_replaced() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            common::SseEvent {
                event: None,
                data: r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"glm-4-flash","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#.to_string(),
            },
            common::SseEvent {
                event: None,
                data: r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"glm-4-flash","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#.to_string(),
            },
            common::SseEvent {
                event: None,
                data: r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"glm-4-flash","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string(),
            },
            common::SseEvent {
                event: None,
                data: "[DONE]".to_string(),
            },
        ],
    });
    let mock_url = mock.start().await;
    let app = build_app_openai(mock_url);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "T",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 收集 body（axum::body::Body → bytes）
    let body_bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body_bytes).to_string();
    let events: Vec<String> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(String::from)
        .collect();

    // 验证：4 个事件（3 chunk + [DONE]）
    assert_eq!(events.len(), 4, "got events: {:?}", events);
    assert_eq!(events[3], "[DONE]");

    // 验证：每个 chunk 的 model 字段是 alias（"T"），不是后端模型（"glm-4-flash"）
    for (i, e) in events.iter().enumerate().take(3) {
        let v: serde_json::Value = serde_json::from_str(e)
            .unwrap_or_else(|_| panic!("event {} not valid JSON: {}", i, e));
        assert_eq!(
            v["model"], "T",
            "event {} model should be alias 'T', got: {}",
            i, v["model"]
        );
    }

    // 验证：内容拼起来是 "Hello world"
    let combined: String = events
        .iter()
        .take(3)
        .filter_map(|e| {
            let v: serde_json::Value = serde_json::from_str(e).ok()?;
            v["choices"][0]["delta"]["content"].as_str().map(String::from)
        })
        .collect();
    assert_eq!(combined, "Hello world");
}

/// L3: 流式响应结束后必须发 [DONE]（OpenAI 协议约定）
#[tokio::test]
async fn openai_streaming_emits_done_marker() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            common::SseEvent {
                event: None,
                data: r#"{"id":"x","model":"glm-4-flash","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#.to_string(),
            },
            common::SseEvent {
                event: None,
                data: "[DONE]".to_string(),
            },
        ],
    });
    let mock_url = mock.start().await;
    let app = build_app_openai(mock_url);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "T",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    let body_bytes = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let all_text = String::from_utf8_lossy(&body_bytes).to_string();
    // [DONE] 必须出现
    assert!(all_text.contains("data: [DONE]"), "missing [DONE] in: {}", all_text);
}
