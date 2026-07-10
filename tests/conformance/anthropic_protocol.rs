//! cr-301 L2: Anthropic 协议端到端契约测试
//!
//! 启动 MockBackend（模拟 Anthropic 后端）+ 接到 MyGate Anthropic router，验证：
//! 1. 客户端发 Anthropic 请求 → MyGate 转发给 mock 后端
//! 2. mock 后端响应 → MyGate 正确转换回 Anthropic 协议
//! 3. 字段级一致性：model 字段、tool_use 块、stop_reason

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

fn build_app_with_anthro_mock(mock_url: String) -> axum::Router {
    // base_url 故意不带 /v1；Anthropic router 内部拼 /v1/messages
    let config: mygate::config::AppConfig = toml::from_str(&format!(
        r#"
[server]
host = "127.0.0.1"
port = 8080
timeout_seconds = 30
admin_token = ""

[providers.mock]
base_url = "{mock_url}"
api_key = "sk-ant-test"
provider_type = "anthropic"
auth_style = "anthropic"

[aliases.Plan]
[[aliases.Plan.chain]]
provider = "mock"
model = "claude-sonnet-4-5"
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

/// 端到端：Anthropic 简单文本对话
#[tokio::test]
async fn anthropic_simple_text_e2e() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "Hello from Anthropic mock"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "container": null,
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }),
    });
    let mock_url = mock.start().await;
    let app = build_app_with_anthro_mock(mock_url);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "Plan",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    if status != StatusCode::OK {
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        panic!("status: {} body: {}", status, String::from_utf8_lossy(&body));
    }
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap(),
    )
    .unwrap();

    // 验证：响应 model 字段是 alias（"Plan"）
    assert_eq!(body["model"], "Plan");
    // 验证：content[0] 是 text 块
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "Hello from Anthropic mock");
    // 验证：stop_reason / usage 透传
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["stop_sequence"], serde_json::Value::Null);
    assert_eq!(body["usage"]["input_tokens"], 10);
    assert_eq!(body["usage"]["output_tokens"], 5);

    // 验证：MyGate 把请求正确转发给后端
    let received = mock.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].path, "/v1/messages");
    assert_eq!(received[0].body["model"], "claude-sonnet-4-5");
    assert_eq!(received[0].body["max_tokens"], 1024);
    assert_eq!(received[0].body["messages"][0]["role"], "user");
    // 验证：鉴权头是 x-api-key（不是 Bearer）
    assert_eq!(
        received[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("sk-ant-test")
    );
    assert_eq!(
        received[0]
            .headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok()),
        Some("2023-06-01")
    );
}

/// 端到端：Anthropic tool_use
#[tokio::test]
async fn anthropic_tool_use_e2e() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "msg_tool",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "text", "text": "I'll read the file"},
                {"type": "tool_use", "id": "toolu_01A", "name": "Read", "input": {"file_path": "main.rs"}}
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "container": null,
            "usage": {"input_tokens": 20, "output_tokens": 25}
        }),
    });
    let mock_url = mock.start().await;
    let app = build_app_with_anthro_mock(mock_url);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "Plan",
                "max_tokens": 4096,
                "system": "You are a coding assistant.",
                "messages": [{"role": "user", "content": "Read main.rs"}],
                "tools": [{
                    "name": "Read",
                    "description": "Read a file",
                    "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}
                }]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap(),
    )
    .unwrap();

    // 验证：content 数组有 text + tool_use 两块
    assert_eq!(body["content"].as_array().unwrap().len(), 2);
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][1]["type"], "tool_use");
    assert_eq!(body["content"][1]["id"], "toolu_01A");
    assert_eq!(body["content"][1]["name"], "Read");
    assert_eq!(body["content"][1]["input"]["file_path"], "main.rs");
    assert_eq!(body["stop_reason"], "tool_use");

    // 验证后端收到的 system 是顶层字段（cr-001）
    let received = mock.received();
    let rb = &received[0].body;
    assert_eq!(rb["system"], "You are a coding assistant.");
    // 验证 messages 列表里没有 role=system 消息
    for msg in rb["messages"].as_array().unwrap() {
        assert_ne!(msg["role"], "system", "system 消息不应出现在 messages 数组里");
    }
}

/// 端到端：Anthropic 北向 stream=false（非流式，验简单路径）
#[tokio::test]
async fn anthropic_non_streaming_works() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "msg_nostream",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "non-stream response"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "container": null,
            "usage": {"input_tokens": 5, "output_tokens": 3}
        }),
    });
    let mock_url = mock.start().await;
    let app = build_app_with_anthro_mock(mock_url);

    // 不发 stream 字段（默认 false）
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "Plan",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}


