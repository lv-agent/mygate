use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::extract::State;
use axum::Json;
use futures::stream::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;

use crate::config::AppConfig;
use crate::core::fallback;
use crate::core::types::*;
use crate::error::GatewayError;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub client: Client,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIChatRequest {
    pub model: String,
    pub messages: Vec<OpenAIChatMessage>,
    #[serde(default)]
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tools: Option<Vec<OpenAIToolDef>>,
    /// cr-101: 工具选择策略（OpenAI 协议）。可为 string ("auto"/"none"/"required") 或 object
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
    /// cr-102: 响应格式。MyGate 仅支持 text / json_object。
    #[serde(default)]
    pub response_format: Option<serde_json::Value>,
    /// cr-103: 采样参数
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub frequency_penalty: Option<f64>,
    #[serde(default)]
    pub presence_penalty: Option<f64>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub n: Option<u32>,
    /// cr-103: 停止序列。OpenAI 支持 string 或 array，统一为 Option<Vec<String>>
    #[serde(default)]
    pub stop: Option<StopField>,
    /// cr-104: 流式选项
    #[serde(default)]
    pub stream_options: Option<OpenAIStreamOptions>,
    /// cr-206: 用户标识（OpenAI 标准字段）
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OpenAIStreamOptions {
    pub include_usage: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum StopField {
    Single(String),
    Array(Vec<String>),
}

impl StopField {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            StopField::Single(s) => vec![s],
            StopField::Array(v) => v,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenAIChatMessage {
    pub role: String,
    pub content: serde_json::Value,
    #[serde(rename = "tool_call_id")]
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIToolDef {
    #[allow(dead_code)]
    pub r#type: Option<String>,
    pub function: OpenAIToolFnDef,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIToolFnDef {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct OpenAIChatResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAIChatChoice>,
    pub usage: Option<OpenAIUsageResponse>,
}

#[derive(Debug, Serialize)]
pub struct OpenAIChatChoice {
    pub index: u32,
    pub message: OpenAIChatResponseMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OpenAIChatResponseMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct OpenAIUsageResponse {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct OpenAIModelsResponse {
    pub object: String,
    pub data: Vec<OpenAIModelItem>,
}

#[derive(Debug, Serialize)]
pub struct OpenAIModelItem {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub owned_by: String,
}

/// cr-001: 从 role=system 消息提取 system 文本，剩余 messages 不含 system 角色。
/// 返回 (system, messages)。
fn parse_openai_messages(messages: Vec<OpenAIChatMessage>) -> (Option<String>, Vec<InternalMessage>) {
    let mut system: Option<String> = None;
    let mut internal_messages: Vec<InternalMessage> = Vec::new();

    for msg in messages {
        // 提取 system：第一条 role=system 消息
        if msg.role == "system" && system.is_none() {
            if let serde_json::Value::String(s) = &msg.content {
                if !s.is_empty() {
                    system = Some(s.clone());
                    continue; // 不当作普通消息保留
                }
            }
        }

        let role = match msg.role.as_str() {
            "system" => Role::System, // 多个 system 消息保留为 Role::System（罕见）
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => Role::User,
        };
        let mut content_blocks = Vec::new();
        if let Some(tool_calls) = msg.tool_calls {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if let Some(func) = tc.get("function").cloned() {
                    let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let args = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    content_blocks.push(ContentBlock::ToolCall {
                        id,
                        function: FunctionCall { name, arguments: args },
                    });
                }
            }
        }
        if let Some(tool_call_id) = msg.tool_call_id {
            let text = match &msg.content {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            content_blocks.push(ContentBlock::ToolResult { tool_use_id: tool_call_id, content: text });
        } else if let serde_json::Value::String(s) = &msg.content {
            if !s.is_empty() {
                content_blocks.push(ContentBlock::Text { text: s.clone() });
            }
        } else if let Some(arr) = msg.content.as_array() {
            for item in arr {
                if let Some(t) = item.get("type").and_then(|v| v.as_str()) {
                    match t {
                        "text" => {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                content_blocks.push(ContentBlock::Text { text: text.to_string() });
                            }
                        }
                        "image_url" => {
                            if let Some(url_obj) = item.get("image_url") {
                                let url = url_obj.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                content_blocks.push(ContentBlock::ImageUrl {
                                    image_url: ImageUrlContent { url, detail: None },
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        internal_messages.push(InternalMessage { role, content: content_blocks });
    }
    (system, internal_messages)
}

/// cr-101: 解析 OpenAI tool_choice（string 或 object）
fn parse_openai_tool_choice(v: &serde_json::Value) -> Option<ToolChoice> {
    match v {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Any),
            _ => None,
        },
        serde_json::Value::Object(_) => {
            let t = v.get("type").and_then(|x| x.as_str());
            let name = v
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str());
            match (t, name) {
                (Some("function"), Some(n)) => Some(ToolChoice::Specific(n.to_string())),
                _ => None,
            }
        }
        _ => None,
    }
}

/// cr-102: 解析 OpenAI response_format。本期仅支持 text / json_object。
fn parse_openai_response_format(v: &serde_json::Value) -> Option<ResponseFormat> {
    let t = v.get("type").and_then(|x| x.as_str())?;
    match t {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        _ => None, // 未知值（json_schema 等）返回 None，契约要求未知字段 400
    }
}

fn to_openai_response(internal: InternalResponse) -> OpenAIChatResponse {
    let mut content_str = None;
    let mut tool_calls_out = None;
    for block in &internal.content {
        match block {
            ContentBlock::Text { text } => content_str = Some(text.clone()),
            ContentBlock::ToolCall { id, function } => {
                tool_calls_out.get_or_insert_with(Vec::new).push(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": function.name, "arguments": function.arguments }
                }));
            }
            _ => {}
        }
    }
    let created = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    OpenAIChatResponse {
        id: internal.id,
        object: "chat.completion".to_string(),
        created,
        model: internal.alias,
        choices: vec![OpenAIChatChoice {
            index: 0,
            message: OpenAIChatResponseMessage { role: "assistant".to_string(), content: content_str, tool_calls: tool_calls_out },
            finish_reason: internal.finish_reason,
        }],
        usage: Some(OpenAIUsageResponse {
            prompt_tokens: internal.usage.prompt_tokens.unwrap_or(0),
            completion_tokens: internal.usage.completion_tokens.unwrap_or(0),
            total_tokens: internal.usage.total_tokens.unwrap_or(0),
        }),
    }
}

pub async fn list_models(State(state): State<AppState>) -> Json<OpenAIModelsResponse> {
    let created = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let config = state.config.read().await;
    let models: Vec<OpenAIModelItem> = config.aliases.keys().map(|name| OpenAIModelItem {
        id: name.clone(),
        object: "model".to_string(),
        created,
        owned_by: "mygate".to_string(),
    }).collect();
    Json(OpenAIModelsResponse { object: "list".to_string(), data: models })
}

pub async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<OpenAIChatRequest>,
) -> Result<impl IntoResponse, GatewayError> {
    let (system, internal_messages) = parse_openai_messages(req.messages);
    let tool_choice = req.tool_choice.as_ref().and_then(parse_openai_tool_choice);
    let internal_req = InternalRequest {
        model_alias: req.model.clone(),
        system,
        messages: internal_messages,
        stream: req.stream,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        tools: req.tools.map(|tools| tools.into_iter().map(|t| InternalTool {
            name: t.function.name,
            description: t.function.description,
            parameters: t.function.parameters,
        }).collect()),
        tool_choice,
        response_format: req.response_format.as_ref().and_then(parse_openai_response_format),
        top_p: req.top_p,
        // cr-103: Anthropic 专属字段，OpenAI 客户端不发 → None
        top_k: None,
        frequency_penalty: req.frequency_penalty,
        presence_penalty: req.presence_penalty,
        stop: req.stop.clone().map(|s| s.into_vec()),
        seed: req.seed,
        n: req.n,
        // cr-104: stream_options 透传
        stream_options: req.stream_options.as_ref().map(|o| StreamOptions {
            include_usage: o.include_usage.unwrap_or(false),
        }),
        // cr-206: user 标识
        user: req.user.clone(),
        // cr-206 补全: metadata 透传（OpenAI 客户端发的 metadata 字段）
        metadata: None, // cr-206 补全: OpenAI metadata → 暂不自动转 Anthropic
    };

    if req.stream {
        // cr-202: active_streams gauge inc
        crate::metrics::metrics().active_streams.inc();
        let stream = create_streaming_response(state, internal_req).await?;
        // 包装 stream：drop 时 dec
        let guarded = ActiveStreamsGuard::new(stream);
        Ok(Sse::new(guarded).keep_alive(KeepAlive::default()).into_response())
    } else {
        let result = fallback::execute_with_fallback(&state.client, state.config.clone(), &internal_req).await?;
        // cr-202: tokens_total counter
        let alias = internal_req.model_alias.clone();
        let u = &result.response.usage;
        if let Some(t) = u.prompt_tokens {
            crate::metrics::metrics().tokens_total.with_label_values(&[&alias, "prompt"]).inc_by(t as f64);
        }
        if let Some(t) = u.completion_tokens {
            crate::metrics::metrics().tokens_total.with_label_values(&[&alias, "completion"]).inc_by(t as f64);
        }
        let response = to_openai_response(result.response);
        Ok(Json(response).into_response())
    }
}

/// cr-202: 流式 stream 守卫，drop 时 dec `active_streams` gauge
pub struct ActiveStreamsGuard<S> {
    inner: S,
    dec_on_drop: bool,
}

impl<S> ActiveStreamsGuard<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, dec_on_drop: true }
    }
}

impl<S> Drop for ActiveStreamsGuard<S> {
    fn drop(&mut self) {
        if self.dec_on_drop {
            crate::metrics::metrics().active_streams.dec();
        }
    }
}

impl<S: futures::Stream + Unpin> futures::Stream for ActiveStreamsGuard<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

async fn create_streaming_response(
    state: AppState,
    internal_req: InternalRequest,
) -> Result<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>, GatewayError> {
    let (backend_resp, target) = fallback::execute_streaming_fallback(
        &state.client, state.config.clone(), &internal_req,
    ).await?;
    let alias = internal_req.model_alias.clone();
    let target_model = target.model.clone();

    let stream = async_stream::stream! {
        let mut byte_stream = backend_resp.bytes_stream();
        let mut buffer = String::new();
        // Per-chunk timeout: if backend stops sending for 60s, abort the stream
        let chunk_timeout = std::time::Duration::from_secs(60);
        loop {
            match tokio::time::timeout(chunk_timeout, byte_stream.next()).await {
                Ok(Some(chunk_result)) => match chunk_result {
                    Ok(bytes) => {
                        tracing::debug!(len = bytes.len(), "Received bytes from backend");
                        let text = match std::str::from_utf8(&bytes) { Ok(t) => t, Err(_) => continue };
                        buffer.push_str(text);
                        while let Some(pos) = buffer.find('\n') {
                            let line = buffer[..pos].to_string();
                            buffer = buffer[pos + 1..].to_string();
                            let trimmed = line.trim();
                            if trimmed.is_empty() || trimmed.starts_with(':') { continue; }
                            if trimmed == "data: [DONE]" {
                                tracing::info!("Stream complete, sending [DONE] to client");
                                yield Ok(Event::default().data("[DONE]"));
                                return;
                            }
                            if let Some(data) = trimmed.strip_prefix("data: ") {
                                // cr-105: JSON 路径感知的 model 替换（替代原字符串 replace）
                                let replaced = transform_model_in_chunk(data, &target_model, &alias)
                                    .unwrap_or_else(|| data.to_string());
                                yield Ok(Event::default().data(replaced));
                            } else {
                                tracing::debug!(line = %trimmed, "Skipping non-SSE line from backend");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Stream chunk error");
                        let error_data = serde_json::json!({"error": "stream interrupted", "detail": e.to_string()});
                        yield Ok(Event::default().data(error_data.to_string()));
                        yield Ok(Event::default().data("[DONE]"));
                        return;
                    }
                },
                Ok(None) => {
                    // Stream ended normally (backend closed connection)
                    tracing::warn!("Backend stream ended without [DONE]");
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
                Err(_) => {
                    // Per-chunk timeout — backend stopped sending data
                    tracing::error!("Stream chunk timeout — no data from backend for {:?}", chunk_timeout);
                    let error_data = serde_json::json!({"error": "stream timeout", "detail": "backend stopped sending data"});
                    yield Ok(Event::default().data(error_data.to_string()));
                    yield Ok(Event::default().data("[DONE]"));
                    return;
                }
            }
        }
    };
    Ok(Box::pin(stream))
}

pub async fn reload_config(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, axum::response::Response> {
    use axum::response::IntoResponse;
    // cr-203: admin_token 鉴权
    let config = state.config.read().await;
    match &config.server.admin_token {
        None => {
            // 未配置 admin_token → 端点禁用
            Ok((axum::http::StatusCode::NOT_FOUND, "admin endpoint disabled").into_response())
        }
        Some(expected) => {
            let provided = headers
                .get("x-admin-token")
                .and_then(|v| v.to_str().ok());
            match provided {
                Some(t) if t == expected => {
                    // 鉴权通过，继续
                    drop(config);
                    let config_path = std::env::var("MYGATE_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
                    let result = AppConfig::load(&config_path).map_err(|e| e.to_string());
                    match result {
                        Ok(new_config) => {
                            let alias_count = new_config.aliases.len();
                            let provider_count = new_config.providers.len();
                            *state.config.write().await = new_config;
                            // cr-202: config_reload_total counter (http trigger)
                            crate::metrics::metrics()
                                .config_reload_total
                                .with_label_values(&["http"])
                                .inc();
                            tracing::info!("Config reloaded: {} aliases, {} providers", alias_count, provider_count);
                            let body = serde_json::json!({"status": "ok", "aliases": alias_count, "providers": provider_count});
                            Ok(axum::response::Json(body).into_response())
                        }
                        Err(e) => {
                            tracing::error!("Config reload failed: {}", e);
                            Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("reload error: {}", e)).into_response())
                        }
                    }
                }
                _ => {
                    tracing::warn!("Admin endpoint auth failed");
                    Err((axum::http::StatusCode::UNAUTHORIZED, "missing or invalid X-Admin-Token").into_response())
                }
            }
        }
    }
}

// =====================================================================
// cr-105: OpenAI 流式状态机 — 把 model 字段替换抽成纯函数
// =====================================================================

/// cr-105: JSON 路径感知的 model 字段替换
/// - **无条件**把顶层 `model` 替换为 alias（不依赖 target_model 匹配）
/// - 因为后端可能规范化 model 名（如 deepseek-chat → deepseek-v4-flash），仅匹配会漏替换
/// - 解析失败时回退到原文（不丢数据）
/// - 不是合法 JSON 或不含 `model` 字段时返回 None（不修改）
pub(crate) fn transform_model_in_chunk(
    data: &str,
    _target_model: &str,
    alias: &str,
) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(data).ok()?;
    let obj = v.as_object_mut()?;
    if !obj.contains_key("model") {
        return None;
    }
    if let Some(m) = obj.get_mut("model") {
        // 只在 model 字段是 string 时替换（避免误伤 object/array）
        if m.is_string() {
            *m = serde_json::Value::String(alias.to_string());
        }
    }
    Some(serde_json::to_string(&v).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cr-105 RED: 旧实现用 `data.replace("\"model\":\"X\"", "\"model\":\"alias\"")`
    /// 字符串替换，会误伤嵌套字段（如 `metadata.model` 或 chunk 内的 "X-extra-model"）。
    /// 新实现按 JSON 路径只替换顶层 `model`。
    #[test]
    fn test_transform_model_basic() {
        let data = r#"{"id":"x","model":"glm-5.1","choices":[]}"#;
        let out = transform_model_in_chunk(data, "glm-5.1", "Plan").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["model"], "Plan");
    }

    /// cr-105: 嵌套字段含 "glm-5.1" 不应被替换
    #[test]
    fn test_transform_model_does_not_touch_nested() {
        let data = r#"{"id":"x","model":"glm-5.1","choices":[{"message":{"reasoning":"based on glm-5.1"}}]}"#;
        let out = transform_model_in_chunk(data, "glm-5.1", "Plan").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["model"], "Plan");
        // 嵌套的 "glm-5.1" 必须保持
        assert_eq!(parsed["choices"][0]["message"]["reasoning"], "based on glm-5.1");
    }

    /// cr-105: 后端 model 与配置 model 不匹配时也替换为 alias（深度规范化场景）
    /// 例：配置 deepseek-chat，后端实际返回 deepseek-v4-flash
    #[test]
    fn test_transform_model_replaces_even_when_backend_normalizes() {
        let data = r#"{"id":"x","model":"deepseek-v4-flash","choices":[]}"#;
        let out = transform_model_in_chunk(data, "deepseek-chat", "Simple").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["model"], "Simple");
    }

    /// cr-105: model 字段不是 string 类型（异常情况）不替换
    #[test]
    fn test_transform_model_non_string_model() {
        let data = r#"{"model":123,"choices":[]}"#;
        let out = transform_model_in_chunk(data, "glm-5.1", "Plan").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        // 不替换，保持原值
        assert_eq!(parsed["model"], 123);
    }

    /// cr-105: 非法 JSON 返回 None（保留原文，调用方应直接转发）
    #[test]
    fn test_transform_model_invalid_json() {
        let data = "not json";
        assert_eq!(transform_model_in_chunk(data, "glm-5.1", "Plan"), None);
    }

    /// cr-105: 不是 object（顶层是数组）返回 None
    #[test]
    fn test_transform_model_array_top_level() {
        let data = r#"[1,2,3]"#;
        assert_eq!(transform_model_in_chunk(data, "glm-5.1", "Plan"), None);
    }

    /// cr-105: 没有 model 字段返回 None
    #[test]
    fn test_transform_model_no_model_field() {
        let data = r#"{"id":"x","choices":[]}"#;
        assert_eq!(transform_model_in_chunk(data, "glm-5.1", "Plan"), None);
    }
}
