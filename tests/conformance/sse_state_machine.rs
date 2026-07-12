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
use mygate::state::AppState;
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

/// L3: Anthropic SSE tool_use 生命周期 — 验证 tool_use 块的完整事件序列
/// content_block_start(tool_use) → content_block_delta(input_json_delta ×2) → content_block_stop
#[tokio::test]
async fn anthropic_sse_tool_use_lifecycle() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            common::SseEvent {
                event: Some("message_start".to_string()),
                data: r#"{"type":"message_start","message":{"id":"msg_t","type":"message","role":"assistant","model":"c","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#.to_string(),
            },
            // tool_use block start (index 0)
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_001","name":"read_file","input":{}}}"#.to_string(),
            },
            // input_json_delta (分片发送 JSON 参数)
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"_path\":\"/etc/hosts\"}"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            },
            common::SseEvent {
                event: Some("message_delta".to_string()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":15}}"#.to_string(),
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

[aliases.T]
[[aliases.T.chain]]
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
                "model":"T","max_tokens":100,"stream":true,
                "messages":[{"role":"user","content":"read /etc/hosts"}],
                "tools":[{"name":"read_file","description":"Read a file","input_schema":{"type":"object"}}]
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

    // 解析所有 event: 行和数据行
    let events: Vec<(String, String)> = {
        let mut pairs = Vec::new();
        let mut current_event = String::new();
        for line in text.lines() {
            if let Some(ev) = line.strip_prefix("event: ") {
                current_event = ev.to_string();
            } else if let Some(data) = line.strip_prefix("data: ") {
                pairs.push((current_event.clone(), data.to_string()));
                current_event.clear();
            }
        }
        pairs
    };

    assert_eq!(events.len(), 7, "tool_use 流应 7 个 event+data 对, got {}: {:?}", events.len(), events);

    // 验证事件序列
    assert_eq!(events[0].0, "message_start");
    assert_eq!(events[1].0, "content_block_start");
    assert_eq!(events[2].0, "content_block_delta");
    assert_eq!(events[3].0, "content_block_delta");
    assert_eq!(events[4].0, "content_block_stop");
    assert_eq!(events[5].0, "message_delta");
    assert_eq!(events[6].0, "message_stop");

    // 验证 tool_use 数据完整性
    let start_data: serde_json::Value = serde_json::from_str(&events[1].1).unwrap();
    assert_eq!(start_data["content_block"]["type"], "tool_use");
    assert_eq!(start_data["content_block"]["name"], "read_file");

    // 验证 input_json 拼接后是完整的 JSON
    let json1: serde_json::Value = serde_json::from_str(&events[2].1).unwrap();
    let json2: serde_json::Value = serde_json::from_str(&events[3].1).unwrap();
    let partial1 = json1["delta"]["partial_json"].as_str().unwrap();
    let partial2 = json2["delta"]["partial_json"].as_str().unwrap();
    let combined = format!("{}{}", partial1, partial2);
    let parsed: serde_json::Value = serde_json::from_str(&combined).expect("input_json 拼接应有效");
    assert_eq!(parsed["file_path"], "/etc/hosts");

    // 验证 stop_reason 应为 tool_use
    let delta: serde_json::Value = serde_json::from_str(&events[5].1).unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
}

/// L3: Anthropic SSE thinking 生命周期 — 验证 thinking 块的完整事件序列
/// content_block_start(thinking) → thinking_delta → signature_delta → content_block_stop
#[tokio::test]
async fn anthropic_sse_thinking_lifecycle() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            common::SseEvent {
                event: Some("message_start".to_string()),
                data: r#"{"type":"message_start","message":{"id":"msg_th","type":"message","role":"assistant","model":"c","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":8,"output_tokens":0}}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#.to_string(),
            },
            // thinking_delta
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think about this"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" step by step."}}"#.to_string(),
            },
            // signature_delta
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_abc123"}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            },
            // 然后 text block
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"The answer is 42."}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":1}"#.to_string(),
            },
            common::SseEvent {
                event: Some("message_delta".to_string()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":25}}"#.to_string(),
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

[aliases.TH]
[[aliases.TH.chain]]
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
                "model":"TH","max_tokens":200,"stream":true,
                "messages":[{"role":"user","content":"what is the answer"}],
                "thinking":{"type":"enabled","budget_tokens":100}
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

    let event_names: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .collect();

    // 验证事件序列：message_start, content_block_start, delta×3, content_block_stop,
    //                 content_block_start, delta, content_block_stop, message_delta, message_stop
    assert_eq!(event_names.len(), 11, "thinking 流应为 11 个 event, got {}: {:?}", event_names.len(), event_names);
    assert_eq!(event_names[0], "message_start");
    assert_eq!(event_names[1], "content_block_start"); // thinking
    assert_eq!(event_names[2], "content_block_delta"); // thinking
    assert_eq!(event_names[3], "content_block_delta"); // thinking
    assert_eq!(event_names[4], "content_block_delta"); // signature
    assert_eq!(event_names[5], "content_block_stop");  // thinking end
    assert_eq!(event_names[6], "content_block_start"); // text
    assert_eq!(event_names[7], "content_block_delta"); // text
    assert_eq!(event_names[8], "content_block_stop");  // text end
    assert_eq!(event_names[9], "message_delta");
    assert_eq!(event_names[10], "message_stop");
}

/// L3: 多 block 并行 state machine — 验证 text (idx 0) + tool_use (idx 1) 交替发送的正确性
#[tokio::test]
async fn anthropic_sse_multiple_blocks_parallel() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            common::SseEvent {
                event: Some("message_start".to_string()),
                data: r#"{"type":"message_start","message":{"id":"msg_m","type":"message","role":"assistant","model":"c","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":0}}}"#.to_string(),
            },
            // Block 0: text
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I will"}}"#.to_string(),
            },
            // Block 1: tool_use starts while text is still going
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_002","name":"search","input":{}}}"#.to_string(),
            },
            // Interleaved: text delta
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" search for that."}}"#.to_string(),
            },
            // Interleaved: tool_use delta
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust\"}"}}"#.to_string(),
            },
            // Block 0: stop
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            },
            // Block 1: stop
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":1}"#.to_string(),
            },
            common::SseEvent {
                event: Some("message_delta".to_string()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":30}}"#.to_string(),
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

[aliases.M]
[[aliases.M.chain]]
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
                "model":"M","max_tokens":100,"stream":true,
                "messages":[{"role":"user","content":"search rust"}],
                "tools":[{"name":"search","description":"Search","input_schema":{"type":"object"}}]
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

    // 收集 (event, index) pair
    struct BlockEvent {
        event: String,
        index: u64,
        data: String,
    }
    let mut block_events: Vec<BlockEvent> = Vec::new();
    let mut current_event = String::new();
    for line in text.lines() {
        if let Some(ev) = line.strip_prefix("event: ") {
            current_event = ev.to_string();
        } else if let Some(data) = line.strip_prefix("data: ") {
            let v: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
            let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(999);
            block_events.push(BlockEvent {
                event: current_event.clone(),
                index: idx,
                data: data.to_string(),
            });
            current_event.clear();
        }
    }

    // 验证两个 block 的顺序
    let block0_events: Vec<_> = block_events.iter().filter(|e| e.index == 0).collect();
    let block1_events: Vec<_> = block_events.iter().filter(|e| e.index == 1).collect();

    // Block 0 (text): start → delta×2 → stop
    assert_eq!(block0_events[0].event, "content_block_start");
    assert_eq!(block0_events[1].event, "content_block_delta");
    assert_eq!(block0_events[2].event, "content_block_delta");
    assert_eq!(block0_events[3].event, "content_block_stop");

    // Block 1 (tool_use): start → delta → stop
    assert_eq!(block1_events[0].event, "content_block_start");
    assert_eq!(block1_events[1].event, "content_block_delta");
    assert_eq!(block1_events[2].event, "content_block_stop");

    // 验证 tool_use block 的 delta 是 input_json_delta
    let tool_delta: serde_json::Value = serde_json::from_str(&block1_events[1].data).unwrap();
    assert_eq!(tool_delta["delta"]["type"], "input_json_delta");
    assert!(tool_delta["delta"]["partial_json"].as_str().unwrap().contains("rust"));

    // 最后一个事件必须是 message_stop
    let last_event = text.lines().filter_map(|l| l.strip_prefix("event: ")).last();
    assert_eq!(last_event, Some("message_stop"));
}

/// L3: 流被截断 — 后端发完 message_delta 后直接关闭连接（不发 message_stop）
/// 验证 MyGate 不会 panic，客户端收到的事件数据完整
#[tokio::test]
async fn anthropic_sse_truncated_no_message_stop() {
    let mock = MockBackend::new();
    // 故意不发 message_stop 事件
    mock.push_script(common::MockResponse::StreamSse {
        events: vec![
            common::SseEvent {
                event: Some("message_start".to_string()),
                data: r#"{"type":"message_start","message":{"id":"msg_x","type":"message","role":"assistant","model":"c","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":0}}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_start".to_string()),
                data: r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_delta".to_string()),
                data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial response..."}}"#.to_string(),
            },
            common::SseEvent {
                event: Some("content_block_stop".to_string()),
                data: r#"{"type":"content_block_stop","index":0}"#.to_string(),
            },
            common::SseEvent {
                event: Some("message_delta".to_string()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#.to_string(),
            },
            // 没有 message_stop — 模拟后端异常断连
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

[aliases.TR]
[[aliases.TR.chain]]
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
                "model":"TR","max_tokens":50,"stream":true,
                "messages":[{"role":"user","content":"hi"}]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // HTTP 仍返回 200（流式响应 header 已发）
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();

    // 收集收到的事件
    let events: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .collect();

    // 应收到 5 个事件（没有 message_stop）
    assert_eq!(events.len(), 5, "截断流应收到 5 个 event, got {}: {:?}", events.len(), events);
    assert_eq!(events[0], "message_start");
    assert_eq!(events[4], "message_delta");
    // 关键：没有 message_stop，但 MyGate 不应 panic
    assert!(!events.contains(&"message_stop"), "截断流不应有 message_stop");
}
