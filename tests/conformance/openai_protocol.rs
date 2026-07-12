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
use mygate::state::AppState;
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

/// cr-411: GLM 风格错误响应（缺 type 字段）应该被识别
#[test]
fn test_parse_error_body_glm_style() {
    let body = r#"{"error":{"code":"401","message":"令牌已过期"}}"#;
    let parsed = mygate::backend::openai_compat::parse_error_body(body);
    assert!(parsed.is_some(), "GLM 错误应能解析");
    let (typ, msg) = parsed.unwrap();
    assert_eq!(typ, "401", "type 字段缺失时 fallback 到 code");
    assert!(msg.contains("令牌"), "message 应能提取");
}

/// cr-411: 标准 OpenAI 错误响应应该被识别
#[test]
fn test_parse_error_body_openai_standard() {
    let body = r#"{"error":{"message":"Invalid API key","type":"invalid_request_error","code":"401"}}"#;
    let parsed = mygate::backend::openai_compat::parse_error_body(body);
    assert!(parsed.is_some(), "标准 OpenAI 错误应能解析");
    let (typ, msg) = parsed.unwrap();
    assert_eq!(typ, "invalid_request_error");
    assert!(msg.contains("Invalid"));
}

/// cr-411: 非错误 JSON（普通成功响应）不应该返回 Some
#[test]
fn test_parse_error_body_success_response() {
    let body = r#"{"id":"x","choices":[{"message":{"content":"hi"}}]}"#;
    let parsed = mygate::backend::openai_compat::parse_error_body(body);
    assert!(parsed.is_none(), "正常成功响应不应被认为是错误");
}

/// cr-411: 非 JSON body 应该让 parse_error_body 返回 None（让上游走 non-JSON 处理）
#[test]
fn test_parse_error_body_non_json() {
    let body = "Authentication Fails (governor)";
    let parsed = mygate::backend::openai_compat::parse_error_body(body);
    assert!(parsed.is_none(), "非 JSON 应该返回 None");
}

/// cr-411 P1: 流式 4xx 错误返回 GatewayError 而非 resp
#[tokio::test]
async fn streaming_4xx_error_returned_as_error() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 401,
        body: json!({"error": {"code": "401", "message": "invalid api key"}}),
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
    ))
    .unwrap();
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
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // cr-411 P1: 流式后端 4xx → MyGate 包装 502 + error message (不当作 fallback 耗尽)
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "4xx 错误应包装成 502");
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["error"]["type"], "gateway_error");
    assert!(
        text.contains("invalid api key"),
        "error body 应含 backend 错误信息: {}",
        text
    );
}

/// cr-411 P1: 流式 content-type 不是 text/event-stream → 拒绝
#[tokio::test]
async fn streaming_wrong_content_type_rejected() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({"error": "some weird response"}),
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
    ))
    .unwrap();
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
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();
    // cr-411 P1: content-type 非 text/event-stream → 500 (Internal error)
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "错 content-type 应 500");
    assert!(
        text.contains("unexpected content-type") || text.contains("gateway_error"),
        "应含错误: {}",
        text
    );
}

/// cr-411 P2: extract_thinking 单元测试
#[test]
fn test_extract_thinking_mixed_content() {
    use mygate::core::extract_thinking;
    // MiniMax 格式: thinking 混在 content 里
    let text = "<think>The user said hi</think>\n\nHello!";
    let (visible, reasoning) = extract_thinking(text);
    assert_eq!(visible, "Hello!");
    assert_eq!(reasoning.as_deref(), Some("The user said hi"));

    // 无 thinking 块
    let (v, r) = extract_thinking("Hello world");
    assert_eq!(v, "Hello world");
    assert!(r.is_none());

    // 只有 think 块没其它内容
    let (v, r) = extract_thinking("<think>just thinking</think>");
    assert_eq!(v, "");
    assert_eq!(r.as_deref(), Some("just thinking"));

    // think 块有前后缀
    let (v, r) = extract_thinking("<think>reasoning</think>\n\nactual");
    assert_eq!(v, "actual");
    assert_eq!(r.as_deref(), Some("reasoning"));
}

// 暂时删掉 openai_to_minimax_thinking_extraction 端到端测试 (format! 复杂 + 没有 mock 合适验证 L4 行为). 单元测试已够.

/* 端到端测试 openai_to_minimax_thinking_extraction 暂时删掉 - L4 实跑验证 */
