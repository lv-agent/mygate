//! cr-301 L2: OpenAI 协议端到端契约测试
//!
//! 启动 MockBackend + 接到 MyGate OpenAI router，验证：
//! 1. 客户端发 OpenAI 请求 → MyGate 转发给 mock 后端
//! 2. mock 后端响应 → MyGate 正确转换回 OpenAI 协议
//! 3. 字段级一致性（model 字段、tool_choice、response_format 等）

#[path = "common/mod.rs"]
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::MockBackend;
use mygate::core::types::*;
use mygate::router::openai::AppState;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;

/// 端到端测试：OpenAI 简单对话
#[tokio::test]
async fn openai_simple_text_e2e() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1720000000,
            "model": "glm-4-flash",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello from mock"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
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

[aliases.Test]
[[aliases.Test.chain]]
provider = "mock"
model = "glm-4-flash"
priority = 1
"#
    )).unwrap();
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client: reqwest::Client::new(),
    };
    let app = mygate::server::build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "Test",
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
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // 验证：响应里 model 字段是 alias（"Test"），不是后端真实模型
    assert_eq!(body["model"], "Test");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello from mock");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");

    // 验证：MyGate 把请求正确转发给了后端
    let received = mock.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].method, "POST");
    assert_eq!(received[0].path, "/v1/chat/completions");
    assert_eq!(received[0].body["model"], "glm-4-flash");
    assert_eq!(received[0].body["messages"][0]["role"], "user");
    assert_eq!(received[0].body["messages"][0]["content"], "hi");
}

/// 端到端：tool_choice 透传
#[tokio::test]
async fn openai_tool_choice_specific_e2e() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "x",
            "object": "chat.completion",
            "created": 1,
            "model": "m",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "calling Read",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "Read", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
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

[aliases.T]
[[aliases.T.chain]]
provider = "mock"
model = "m"
priority = 1
"#
    )).unwrap();
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client: reqwest::Client::new(),
    };
    let app = mygate::server::build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "T",
                "messages": [{"role": "user", "content": "use Read"}],
                "tools": [{"type": "function", "function": {"name": "Read"}}],
                "tool_choice": {"type": "function", "function": {"name": "Read"}}
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "Read");

    // 验证后端收到的 tool_choice
    let received = mock.received();
    assert_eq!(received.len(), 1);
    let rb = &received[0].body;
    if rb.get("tool_choice").is_none() || rb["tool_choice"].is_null() {
        panic!("mock did not receive tool_choice, full body: {}", rb);
    }
    assert_eq!(received[0].body["tool_choice"]["type"], "function");
    assert_eq!(received[0].body["tool_choice"]["function"]["name"], "Read");
}

/// cr-P0-2: OpenAI 北向流式 + Anthropic 后端
/// 验证：MyGate 自动检测后端 Anthropic SSE 格式，转换成 OpenAI SSE 给客户端
#[tokio::test]
async fn openai_to_anthropic_stream_conversion() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            // Anthropic 协议格式
            common::SseEvent {
                event: Some("message_start".to_string()),
                data: r#"{"type":"message_start","message":{"id":"msg_x","type":"message","role":"assistant","model":"claude-test","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":0}}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            },
            common::SseEvent {
                event: Some("message_delta".to_string()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("message_stop".to_string()),
                data: r#"{"type":"message_stop"}"#.to_string(),
            },
        ],
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
base_url = "{mock_url}"
api_key = "test"
provider_type = "anthropic"
auth_style = "bearer"

[aliases.XA]
[[aliases.XA.chain]]
provider = "mock"
model = "c"
priority = 1
"#
    ))
    .unwrap();
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client: reqwest::Client::new(),
    };
    let app = mygate::server::build_router(state);

    // OpenAI 客户端发流式请求
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model":"XA",
                "messages":[{"role":"user","content":"hi"}],
                "stream":true,
                "max_tokens":50
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();
    let chunks: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("data: ")).collect();

    // 验证：客户端应收到 OpenAI 格式 chunks（不是 Anthropic event: 格式）
    let has_openai_chunk = chunks.iter().any(|c| c.contains("chat.completion.chunk"));
    let has_anthropic_event = chunks.iter().any(|c| c.contains("\"type\":\"message_start\""));
    assert!(has_openai_chunk, "客户端未收到 OpenAI 格式 chunks. chunks: {:?}", chunks);
    assert!(!has_anthropic_event, "客户端不应收到 Anthropic 内部 event 格式");
    // 流末尾必须 [DONE]
    assert!(text.contains("data: [DONE]"), "缺 [DONE]");
}
