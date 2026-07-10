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

/// cr-301 L3: Anthropic 协议 SSE 事件序列
/// 验证 8 种事件类型 + 顺序：message_start → content_block_start → deltas → content_block_stop → message_delta → message_stop
#[tokio::test]
async fn anthropic_sse_event_sequence() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            // 1. message_start
            common::SseEvent {
                event: Some("message_start".to_string()),
                data: r#"{"type":"message_start","message":{"id":"msg_x","type":"message","role":"assistant","model":"c","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":0}}}"#.to_string(),
            },
            // 2. content_block_start (text)
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
            },
            // 3. content_block_delta (3 个 deltas)
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", "}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"World"}}"#.to_string(),
            },
            // 4. content_block_stop
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            },
            // 5. message_delta (带 stop_reason)
            common::SseEvent {
                event: Some("message_delta".to_string()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#.to_string(),
            },
            // 6. message_stop
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

[aliases.S]
[[aliases.S.chain]]
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

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(
            json!({
                "model":"S",
                "max_tokens":50,
                "stream":true,
                "messages":[{"role":"user","content":"hi"}]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 收集所有 SSE 事件
    let body = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();
    let events: Vec<&str> = text.lines().filter(|l| l.starts_with("event: ")).map(|l| &l[7..]).collect();

    // 验证事件序列：8 个事件（content_block_delta 有 3 个）
    assert_eq!(events.len(), 8, "expected 8 SSE events, got {}: {:?}", events.len(), events);
    assert_eq!(events[0], "message_start");
    assert_eq!(events[1], "content_block_start");
    assert_eq!(events[2], "content_block_delta");
    assert_eq!(events[3], "content_block_delta");
    assert_eq!(events[4], "content_block_delta");
    assert_eq!(events[5], "content_block_stop");
    assert_eq!(events[6], "message_delta");
    assert_eq!(events[7], "message_stop");
}

/// cr-301 L3: Anthropic SSE 事件计数验证（确保流末尾 message_delta + message_stop 完整）
#[tokio::test]
async fn anthropic_sse_event_count() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            common::SseEvent {
                event: Some("message_start".to_string()),
                data: r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"c","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            },
            common::SseEvent {
                event: Some("message_delta".to_string()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}"#.to_string(),
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

[aliases.E]
[[aliases.E.chain]]
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

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(
            json!({
                "model":"E","max_tokens":10,"stream":true,
                "messages":[{"role":"user","content":"x"}]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();
    let events: Vec<&str> = text.lines().filter(|l| l.starts_with("event: ")).map(|l| &l[7..]).collect();

    // Anthropic 流必须有 6 个事件：message_start, content_block_start, content_block_delta, content_block_stop, message_delta, message_stop
    let required = ["message_start", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"];
    for req_ev in required {
        assert!(events.contains(&req_ev), "流式响应缺事件 {}: got {:?}", req_ev, events);
    }
    // 最后一个事件必须是 message_stop
    assert_eq!(events.last(), Some(&"message_stop"), "最后一个事件应是 message_stop, got: {:?}", events.last());
}
