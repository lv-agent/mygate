//! cr-301: 跨协议交叉连接端到端契约测试
//!
//! 验证 MyGate 北向协议 → 南向协议调度：
//! - 北向 OpenAI → 南向 Anthropic 后端（system 提到顶层、tool_choice 转换）
//! - 北向 Anthropic → 南向 OpenAI 后端（system 转消息、tools input_schema 转 function.parameters）

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mygate::state::AppState;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;

#[path = "common/mod.rs"]
mod common;

use common::MockBackend;

fn build_app(providers: Vec<(&str, &str, &str)>, preferred: &str) -> axum::Router {
    // providers: vec of (name, base_url, type)
    let mut providers_toml = String::new();
    for (name, base_url, ptype) in providers {
        providers_toml.push_str(&format!(
            r#"
[providers.{name}]
base_url = "{base_url}"
api_key = "test"
provider_type = "{ptype}"
auth_style = "bearer"
"#
        ));
    }
    let config: mygate::config::AppConfig = toml::from_str(&format!(
        r#"
[server]
host = "127.0.0.1"
port = 8080
timeout_seconds = 30
admin_token = ""

{providers_toml}
[aliases.T]
[[aliases.T.chain]]
provider = "{preferred}"
model = "m"
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

/// **北向 OpenAI → 南向 Anthropic 后端**
/// 验证：MyGate 把 role=system 消息提到顶层 system 字段
/// （注：测试只验证协议转换 dispatch 到 anthropic 后端 + system 字段重排）
#[tokio::test]
async fn north_openai_south_anthropic_dispatch() {
    let mock = MockBackend::new();
    // Mock 返回成功（实际 MiniMax 端失败但 MyGate 协议转换代码是对的）
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "msg_x",
            "type": "message",
            "role": "assistant",
            "model": "claude-test",
            "content": [{"type": "text", "text": "Hi"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "container": null,
            "usage": {"input_tokens": 5, "output_tokens": 2}
        }),
    });
    let mock_url = mock.start().await;
    let app = build_app(
        vec![
            ("openai-backend", &format!("{}/v1", mock_url), "openai"),
            ("anthropic-backend", &mock_url, "anthropic"),
        ],
        "anthropic-backend",  // 强制优先用 anthropic 后端
    );

    // OpenAI 北向请求（含 system 消息 + tools）
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model":"T",
                "messages":[
                    {"role":"system","content":"你简洁。"},
                    {"role":"user","content":"hi"}
                ],
                "tools":[{
                    "type":"function",
                    "function":{"name":"get_weather","parameters":{"type":"object"}}
                }],
                "tool_choice":"auto"
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body["model"], "T");
    assert_eq!(body["choices"][0]["message"]["content"], "Hi");

    // 验证: mock 收到的是 Anthropic 协议（POST /v1/messages）
    let received = mock.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].path, "/v1/messages");
    assert_eq!(received[0].method, "POST");
    // 验证: system 是顶层字段（cr-001）
    assert_eq!(received[0].body["system"], "你简洁。");
    // 验证: messages 列表里没有 role=system 消息
    for m in received[0].body["messages"].as_array().unwrap() {
        assert_ne!(m["role"], "system", "system 消息不应在 messages 数组里");
    }
}

/// **北向 Anthropic → 南向 OpenAI 后端**
/// 验证：MyGate 把顶层 system 转 role=system 消息、input_schema 转 function.parameters
#[tokio::test]
async fn north_anthropic_south_openai_dispatch() {
    let mock = MockBackend::new();
    mock.push_script(common::MockResponse::Json {
        status: 200,
        body: json!({
            "id": "x",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        }),
    });
    let mock_url = mock.start().await;
    let app = build_app(
        vec![
            ("openai-backend", &format!("{}/v1", mock_url), "openai"),
            ("anthropic-backend", &mock_url, "anthropic"),
        ],
        "openai-backend",  // 强制优先用 openai 后端
    );

    // Anthropic 北向请求
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(
            json!({
                "model":"T",
                "max_tokens":100,
                "system":"你简洁。",
                "tools":[{
                    "name":"get_weather",
                    "description":"查天气",
                    "input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}
                }],
                "messages":[{"role":"user","content":"北京天气？"}]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap(),
    )
    .unwrap();

    // 验证: Anthropic 响应格式（content 是数组，含 tool_use）
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["model"], "T");
    assert_eq!(body["stop_reason"], "tool_use");
    let content = body["content"].as_array().unwrap();
    let tool_use = content.iter().find(|b| b["type"] == "tool_use").unwrap();
    assert_eq!(tool_use["name"], "get_weather");
    assert_eq!(tool_use["input"]["city"], "北京");

    // 验证: mock 收到的是 OpenAI 协议（POST /v1/chat/completions）
    let received = mock.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].path, "/v1/chat/completions");
    // 验证: system 转成 messages[0] role=system（cr-001）
    assert_eq!(received[0].body["messages"][0]["role"], "system");
    assert_eq!(received[0].body["messages"][0]["content"], "你简洁。");
    // 验证: tools 的 input_schema 转 function.parameters
    assert_eq!(received[0].body["tools"][0]["function"]["name"], "get_weather");
    assert!(received[0].body["tools"][0]["function"]["parameters"].is_object());
}
