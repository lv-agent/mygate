//! cr-301: 契约测试公共辅助
//!
//! 提供 MockBackend（mock HTTP 后端）和 StreamEventSequence（SSE 事件序列断言）
//! 让集成测试能：
//! - 启动一个 axum mock 后端，记录收到的请求，按剧本返回响应
//! - 把 MyGate 的 router 接到这个 mock 后端
//! - 验证整个调用链：客户端 → MyGate router → mock 后端 → 响应转换 → 客户端响应

use axum::{
    body::Bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, post},
    Router,
};
use futures::stream::{self, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;

/// 一条请求记录
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

/// 响应剧本
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// 立即返回 JSON 响应
    Json { status: u16, body: Value },
    /// 流式返回 SSE 事件（每个 event 是 event + data 字符串）
    StreamSse { events: Vec<SseEvent> },
}

#[derive(Debug, Clone)]
pub struct SseEvent {
    /// SSE event 字段（"message_start" / "content_block_delta" 等）。None 表示不带 event 字段
    pub event: Option<String>,
    pub data: String,
}

/// Mock HTTP 后端 axum app
#[derive(Clone)]
pub struct MockBackend {
    pub state: Arc<Mutex<MockState>>,
}

pub struct MockState {
    pub received: Vec<RecordedRequest>,
    pub scripts: Vec<MockResponse>,
    pub next_index: usize,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                received: Vec::new(),
                scripts: Vec::new(),
                next_index: 0,
            })),
        }
    }

    /// 追加一个响应剧本（按顺序消费）
    pub fn push_script(&self, resp: MockResponse) {
        self.state.lock().scripts.push(resp);
    }

    /// 拿到所有收到的请求
    pub fn received(&self) -> Vec<RecordedRequest> {
        self.state.lock().received.clone()
    }

    /// 拿到 axum Router
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/chat/completions", any(handle_request))
            .route("/v1/messages", any(handle_request))
            .with_state(self.clone())
    }

    /// 启动一个测试 server，返回 base_url
    pub async fn start(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock backend");
        let addr = listener.local_addr().expect("local_addr");
        let app = self.router();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // 等服务就绪
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        format!("http://{}", addr)
    }
}

async fn handle_request(
    State(backend): State<MockBackend>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .unwrap_or_default();
    let body_json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    let recorded = RecordedRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        headers: parts.headers.clone(),
        body: body_json,
    };

    let mut state = backend.state.lock();
    state.received.push(recorded);

    // 按顺序消费剧本
    let resp = if state.next_index < state.scripts.len() {
        let r = state.scripts[state.next_index].clone();
        state.next_index += 1;
        r
    } else {
        // 默认 200 + 空对象
        MockResponse::Json {
            status: 200,
            body: serde_json::json!({}),
        }
    };

    match resp {
        MockResponse::Json { status, body } => {
            let s = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            (s, axum::Json(body)).into_response()
        }
        MockResponse::StreamSse { events } => {
            // P1-2: 拼成单 chunk body (避免 stream 多 chunk 问题)
            let mut body_str = String::new();
            for e in events {
                if let Some(ev) = &e.event {
                    body_str.push_str(&format!("event: {}\n", ev));
                }
                body_str.push_str(&format!("data: {}\n\n", e.data));
            }
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                body_str,
            )
                .into_response()
        }
    }
}

/// 流式响应事件序列断言器
pub struct StreamEventSequence {
    pub events: Vec<RecordedSseEvent>,
}

#[derive(Debug, Clone)]
pub struct RecordedSseEvent {
    pub event: Option<String>,
    pub data: Value,
}

impl StreamEventSequence {
    pub fn parse(raw: &str) -> Self {
        let mut events = Vec::new();
        let mut current_event: Option<String> = None;
        let mut current_data: Option<String> = None;
        for line in raw.lines() {
            if let Some(ev) = line.strip_prefix("event: ") {
                current_event = Some(ev.to_string());
            } else if let Some(d) = line.strip_prefix("data: ") {
                current_data = Some(d.to_string());
            } else if line.is_empty() && current_data.is_some() {
                let data = current_data
                    .as_ref()
                    .and_then(|d| serde_json::from_str(d).ok())
                    .unwrap_or(Value::Null);
                events.push(RecordedSseEvent {
                    event: current_event.take(),
                    data,
                });
                current_data = None;
            }
        }
        Self { events }
    }

    pub fn assert_block_indices_paired(&self) {
        // 检查 content_block_start/stop index 配对
        use std::collections::HashMap;
        let mut open: HashMap<usize, ()> = HashMap::new();
        for e in &self.events {
            if e.event.as_deref() == Some("content_block_start") {
                if let Some(idx) = e.data.get("index").and_then(|i| i.as_u64()) {
                    open.insert(idx as usize, ());
                }
            } else if e.event.as_deref() == Some("content_block_stop") {
                if let Some(idx) = e.data.get("index").and_then(|i| i.as_u64()) {
                    open.remove(&(idx as usize));
                }
            }
        }
        assert!(open.is_empty(), "unclosed content_block indices: {:?}", open.keys());
    }

    pub fn assert_lifecycle_invariants(&self) {
        assert!(!self.events.is_empty(), "stream has no events");
        let first = &self.events[0];
        let last = self.events.last().unwrap();
        // 第一个事件必须是 message_start
        assert_eq!(
            first.event.as_deref(),
            Some("message_start"),
            "first event must be message_start, got {:?}",
            first.event
        );
        // 最后一个必须是 message_stop 或 error
        let last_ok = last.event.as_deref() == Some("message_stop")
            || last.event.as_deref() == Some("error");
        assert!(last_ok, "last event must be message_stop or error, got {:?}", last.event);
    }
}
