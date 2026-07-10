use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::pin::Pin;
use tokio_stream::StreamExt;

use crate::core::fallback;
use crate::core::types::*;
use crate::error::GatewayError;
use crate::router::openai::AppState;

#[derive(Debug, Deserialize)]
pub struct AnthropicMessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub stream: bool,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub system: Option<serde_json::Value>,
    pub tools: Option<Vec<AnthropicToolDef>>,
    /// cr-101: 工具选择策略（Anthropic 协议，必为 object）
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
    /// cr-103: 采样参数
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicToolDef {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicMessagesResponse {
    pub id: String,
    pub r#type: String,
    pub role: String,
    pub model: String,
    pub content: Vec<AnthropicContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<AnthropicUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Serialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

fn parse_anthropic_messages(req: &AnthropicMessagesRequest) -> (Option<String>, Vec<InternalMessage>) {
    // cr-001: system 提取为顶层字段（Anthropic 协议要求），不再塞到 Role::System 消息
    let system = req.system.as_ref().and_then(|sys| {
        let text = match sys {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(_) => sys
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        };
        if text.is_empty() { None } else { Some(text) }
    });

    let mut messages = Vec::new();
    for msg in &req.messages {
        let role = match msg.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => Role::User,
        };
        let mut content_blocks = Vec::new();
        match &msg.content {
            serde_json::Value::String(s) => {
                content_blocks.push(ContentBlock::Text { text: s.clone() });
            }
            serde_json::Value::Array(arr) => {
                for block in arr {
                    let block_type = block
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("text");
                    match block_type {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                content_blocks.push(ContentBlock::Text {
                                    text: text.to_string(),
                                });
                            }
                        }
                        "image" => {
                            if let Some(source) = block.get("source") {
                                let url =
                                    if let Some(base64) = source.get("data").and_then(|v| v.as_str())
                                    {
                                        let media_type = source
                                            .get("media_type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("image/png");
                                        format!("data:{};base64,{}", media_type, base64)
                                    } else {
                                        source
                                            .get("url")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string()
                                    };
                                content_blocks.push(ContentBlock::ImageUrl {
                                    image_url: ImageUrlContent { url, detail: None },
                                });
                            }
                        }
                        "tool_use" => {
                            let id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input = block
                                .get("input")
                                .cloned()
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                            content_blocks.push(ContentBlock::ToolCall {
                                id,
                                function: FunctionCall {
                                    name,
                                    arguments: input.to_string(),
                                },
                            });
                        }
                        "tool_result" => {
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let content = if let Some(c) = block.get("content") {
                                if let Some(s) = c.as_str() {
                                    s.to_string()
                                } else {
                                    c.to_string()
                                }
                            } else {
                                String::new()
                            };
                            content_blocks.push(ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                            });
                        }
                        // cr-204: document 块（PDF / text / url）
                        "document" => {
                            if let Some(source) = block.get("source") {
                                let s_type = source
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                match s_type {
                                    "base64" => {
                                        let mt = source
                                            .get("media_type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("application/pdf")
                                            .to_string();
                                        let data = source
                                            .get("data")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        content_blocks.push(ContentBlock::Document {
                                            source: DocumentSource::Base64 {
                                                media_type: mt,
                                                data,
                                            },
                                        });
                                    }
                                    "text" => {
                                        let mt = source
                                            .get("media_type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("text/plain")
                                            .to_string();
                                        let data = source
                                            .get("data")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        content_blocks.push(ContentBlock::Document {
                                            source: DocumentSource::Text {
                                                media_type: mt,
                                                data,
                                            },
                                        });
                                    }
                                    "url" => {
                                        if let Some(url) =
                                            source.get("url").and_then(|v| v.as_str())
                                        {
                                            content_blocks.push(ContentBlock::Document {
                                                source: DocumentSource::Url {
                                                    url: url.to_string(),
                                                },
                                            });
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            other => {
                content_blocks.push(ContentBlock::Text {
                    text: other.to_string(),
                });
            }
        }
        messages.push(InternalMessage {
            role,
            content: content_blocks,
        });
    }
    (system, messages)
}

/// cr-101: 解析 Anthropic tool_choice（必为 object）
fn parse_anthropic_tool_choice(v: &serde_json::Value) -> Option<ToolChoice> {
    let t = v.get("type").and_then(|x| x.as_str())?;
    match t {
        "auto" => Some(ToolChoice::Auto),
        "none" => Some(ToolChoice::None),
        "any" => Some(ToolChoice::Any),
        "tool" => {
            let name = v.get("name").and_then(|n| n.as_str())?;
            Some(ToolChoice::Specific(name.to_string()))
        }
        _ => None,
    }
}

fn to_anthropic_response(internal: InternalResponse) -> AnthropicMessagesResponse {
    let content: Vec<AnthropicContentBlock> = internal
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text { text } => AnthropicContentBlock::Text { text },
            ContentBlock::ToolCall { id, function } => AnthropicContentBlock::ToolUse {
                id,
                name: function.name,
                input: serde_json::from_str(&function.arguments)
                    .unwrap_or(serde_json::Value::Object(Default::default())),
            },
            _ => AnthropicContentBlock::Text {
                text: "[unsupported content type]".to_string(),
            },
        })
        .collect();
    // 跨协议兼容（cr-301 cross_protocol 测试）：如果内容里有 ToolUse，
    // 强制把 stop_reason 设为 Anthropic 词汇 "tool_use"，不管后端给的什么。
    let has_tool = content
        .iter()
        .any(|b| matches!(b, AnthropicContentBlock::ToolUse { .. }));
    let stop_reason = if has_tool {
        Some("tool_use".to_string())
    } else {
        // 非 tool_use 时按 OpenAI → Anthropic 词汇映射
        match internal.finish_reason.as_deref() {
            Some("stop") => Some("end_turn".to_string()),
            Some("length") => Some("max_tokens".to_string()),
            Some("tool_calls") => Some("tool_use".to_string()),
            Some(other) => Some(other.to_string()),
            None => None,
        }
    };

    AnthropicMessagesResponse {
        id: internal.id,
        r#type: "message".to_string(),
        role: "assistant".to_string(),
        model: internal.alias,
        content,
        usage: Some(AnthropicUsage {
            input_tokens: internal.usage.prompt_tokens.unwrap_or(0),
            output_tokens: internal.usage.completion_tokens.unwrap_or(0),
        }),
        stop_reason,
    }
}

pub async fn messages(
    State(state): State<AppState>,
    Json(req): Json<AnthropicMessagesRequest>,
) -> Result<impl IntoResponse, GatewayError> {
    let (system, internal_messages) = parse_anthropic_messages(&req);
    let tools = req.tools.map(|tools| {
        tools
            .into_iter()
            .map(|t| InternalTool {
                name: t.name,
                description: t.description,
                parameters: t.input_schema,
            })
            .collect()
    });
    let tool_choice = req.tool_choice.as_ref().and_then(parse_anthropic_tool_choice);
    let internal_req = InternalRequest {
        model_alias: req.model.clone(),
        system,
        response_format: None, // cr-102: Anthropic 协议无此字段，固定 None
        messages: internal_messages,
        stream: req.stream,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        tools,
        tool_choice,
        top_p: req.top_p,
        top_k: req.top_k,
        // cr-103: OpenAI 专属字段，Anthropic 客户端不发 → None
        frequency_penalty: None,
        presence_penalty: None,
        stop: req.stop_sequences.clone(),
        seed: None,
        n: None,
        stream_options: None, // cr-104: Anthropic 协议无此字段
        user: None, // cr-206: Anthropic 协议 metadata，OpenAI user 字段不直接通用
    };

    tracing::info!(
        "Anthropic request: {} messages, {} tools, stream={}",
        internal_req.messages.len(),
        internal_req.tools.as_ref().map(|t| t.len()).unwrap_or(0),
        req.stream
    );

    if req.stream {
        let stream = create_anthropic_stream(state, internal_req).await?;
        Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
    } else {
        let result =
            fallback::execute_with_fallback(&state.client, state.config.clone(), &internal_req).await?;
        let response = to_anthropic_response(result.response);
        Ok(Json(response).into_response())
    }
}

async fn create_anthropic_stream(
    state: AppState,
    internal_req: InternalRequest,
) -> Result<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>, GatewayError> {
    let (backend_resp, _target) =
        fallback::execute_streaming_fallback(&state.client, state.config.clone(), &internal_req).await?;
    let alias = internal_req.model_alias.clone();
    let msg_id = uuid::Uuid::new_v4().to_string();

    let stream = async_stream::stream! {
        // message_start
        let start_event = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": alias,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        yield Ok(Event::default().event("message_start").data(start_event.to_string()));

        let mut byte_stream = backend_resp.bytes_stream();
        let mut buffer = String::new();
        let mut output_tokens: u64 = 0;
        let chunk_timeout = std::time::Duration::from_secs(60);

        // State machine: track which content blocks are open
        // Block 0 = thinking (reasoning_content), Block 1 = text (content)
        let mut block_index: usize = 0;
        let mut thinking_open = false;
        let mut text_open = false;
        let mut final_stop_reason = "end_turn".to_string();
        let mut current_tc_block: Option<usize> = None;
        let mut had_blocks = false;

        loop {
            match tokio::time::timeout(chunk_timeout, byte_stream.next()).await {
                Ok(Some(chunk_result)) => match chunk_result {
                    Ok(bytes) => {
                        tracing::trace!(len = bytes.len(), "Received bytes from backend (anthropic)");
                        let text = match std::str::from_utf8(&bytes) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        buffer.push_str(text);
                        while let Some(pos) = buffer.find('\n') {
                            let line = buffer[..pos].to_string();
                            buffer = buffer[pos + 1..].to_string();
                            let trimmed = line.trim();
                            if trimmed.is_empty() || trimmed.starts_with(':') {
                                continue;
                            }
                            if trimmed == "data: [DONE]" {
                                // Close any open tool block
                                if let Some(bi) = current_tc_block.take() {
                                    yield Ok(Event::default()
                                        .event("content_block_stop")
                                        .data(serde_json::json!({"type":"content_block_stop","index":bi}).to_string()));
                                }
                                // Close thinking block if open
                                if thinking_open {
                                    yield Ok(Event::default()
                                        .event("content_block_stop")
                                        .data(serde_json::json!({"type":"content_block_stop","index":0}).to_string()));
                                }
                                // Close text block if open
                                if text_open {
                                    yield Ok(Event::default()
                                        .event("content_block_stop")
                                        .data(serde_json::json!({"type":"content_block_stop","index":block_index}).to_string()));
                                }
                                // If nothing was ever opened, open+close a text block
                                if !had_blocks {
                                    yield Ok(Event::default()
                                        .event("content_block_start")
                                        .data(serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string()));
                                    yield Ok(Event::default()
                                        .event("content_block_stop")
                                        .data(serde_json::json!({"type":"content_block_stop","index":0}).to_string()));
                                }
                                let msg_delta = serde_json::json!({
                                    "type": "message_delta",
                                    "delta": {"stop_reason": final_stop_reason, "stop_sequence": null},
                                    "usage": {"output_tokens": output_tokens}
                                });
                                yield Ok(Event::default()
                                    .event("message_delta")
                                    .data(msg_delta.to_string()));
                                yield Ok(Event::default()
                                    .event("message_stop")
                                    .data("{\"type\":\"message_stop\"}"));
                                return;
                            }
                            if let Some(data) = trimmed.strip_prefix("data: ") {
                                tracing::trace!("Raw SSE from backend (anthropic): {}", data);
                                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                                    if let Some(choices) =
                                        parsed.get("choices").and_then(|c| c.as_array())
                                    {
                                        for choice in choices {
                                            // Check finish_reason at choice level
                                            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                                                if fr == "tool_calls" {
                                                    final_stop_reason = "tool_use".to_string();
                                                    tracing::info!("Backend finish_reason=tool_calls → Anthropic stop_reason=tool_use");
                                                }
                                            }
                                            if let Some(delta) = choice.get("delta") {
                                                // Handle reasoning_content → thinking block
                                                if let Some(reasoning) = delta
                                                    .get("reasoning_content")
                                                    .and_then(|c| c.as_str())
                                                {
                                                    if !reasoning.is_empty() {
                                                        // Close any open tool block
                                                        if let Some(bi) = current_tc_block.take() {
                                                            yield Ok(Event::default()
                                                                .event("content_block_stop")
                                                                .data(serde_json::json!({"type":"content_block_stop","index":bi}).to_string()));
                                                        }
                                                        if !thinking_open {
                                                            thinking_open = true;
                                                            had_blocks = true;
                                                            yield Ok(Event::default()
                                                                .event("content_block_start")
                                                                .data(serde_json::json!({
                                                                    "type": "content_block_start",
                                                                    "index": 0,
                                                                    "content_block": {"type": "thinking", "thinking": ""}
                                                                }).to_string()));
                                                        }
                                                        output_tokens += 1;
                                                        yield Ok(Event::default()
                                                            .event("content_block_delta")
                                                            .data(serde_json::json!({
                                                                "type": "content_block_delta",
                                                                "index": 0,
                                                                "delta": {"type": "thinking_delta", "thinking": reasoning}
                                                            }).to_string()));
                                                    }
                                                }
                                                // Handle content → text block
                                                if let Some(content) =
                                                    delta.get("content").and_then(|c| c.as_str())
                                                {
                                                    if !content.is_empty() {
                                                        // Close any open tool block
                                                        if let Some(bi) = current_tc_block.take() {
                                                            yield Ok(Event::default()
                                                                .event("content_block_stop")
                                                                .data(serde_json::json!({"type":"content_block_stop","index":bi}).to_string()));
                                                            block_index = bi + 1;
                                                        }
                                                        // Close thinking block if still open
                                                        if thinking_open {
                                                            thinking_open = false;
                                                            yield Ok(Event::default()
                                                                .event("content_block_stop")
                                                                .data(serde_json::json!({"type":"content_block_stop","index":0}).to_string()));
                                                            block_index = 1;
                                                        }
                                                        if !text_open {
                                                            text_open = true;
                                                            had_blocks = true;
                                                            yield Ok(Event::default()
                                                                .event("content_block_start")
                                                                .data(serde_json::json!({
                                                                    "type": "content_block_start",
                                                                    "index": block_index,
                                                                    "content_block": {"type": "text", "text": ""}
                                                                }).to_string()));
                                                        }
                                                        output_tokens += 1;
                                                        yield Ok(Event::default()
                                                            .event("content_block_delta")
                                                            .data(serde_json::json!({
                                                                "type": "content_block_delta",
                                                                "index": block_index,
                                                                "delta": {"type": "text_delta", "text": content}
                                                            }).to_string()));
                                                    }
                                                }
                                                // Handle tool_calls → tool_use blocks
                                                if let Some(tool_calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                                                    for tc in tool_calls {
                                                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                                        let name = tc.get("function")
                                                            .and_then(|f| f.get("name"))
                                                            .and_then(|n| n.as_str())
                                                            .unwrap_or("");
                                                        let args = tc.get("function")
                                                            .and_then(|f| f.get("arguments"))
                                                            .and_then(|a| a.as_str())
                                                            .unwrap_or("");

                                                        // New tool call (has id or name)
                                                        if !id.is_empty() || !name.is_empty() {
                                                            // Close previous tool block if open
                                                            if let Some(bi) = current_tc_block.take() {
                                                                yield Ok(Event::default()
                                                                    .event("content_block_stop")
                                                                    .data(serde_json::json!({"type":"content_block_stop","index":bi}).to_string()));
                                                                block_index = bi + 1;
                                                            }
                                                            // Close thinking block if open
                                                            if thinking_open {
                                                                thinking_open = false;
                                                                yield Ok(Event::default()
                                                                    .event("content_block_stop")
                                                                    .data(serde_json::json!({"type":"content_block_stop","index":0}).to_string()));
                                                                block_index = 1;
                                                            }
                                                            // Close text block if open
                                                            if text_open {
                                                                text_open = false;
                                                                yield Ok(Event::default()
                                                                    .event("content_block_stop")
                                                                    .data(serde_json::json!({"type":"content_block_stop","index":block_index}).to_string()));
                                                                block_index += 1;
                                                            }

                                                            let tool_id = if id.is_empty() {
                                                                format!("toolu_{}", uuid::Uuid::new_v4())
                                                            } else {
                                                                id.to_string()
                                                            };
                                                            let tool_name = if name.is_empty() {
                                                                "unknown"
                                                            } else {
                                                                name
                                                            };

                                                            tracing::info!(
                                                                "Tool call detected: id={} name={} block_index={}",
                                                                tool_id, tool_name, block_index
                                                            );

                                                            // Start tool_use content block
                                                            had_blocks = true;
                                                            yield Ok(Event::default()
                                                                .event("content_block_start")
                                                                .data(serde_json::json!({
                                                                    "type": "content_block_start",
                                                                    "index": block_index,
                                                                    "content_block": {
                                                                        "type": "tool_use",
                                                                        "id": tool_id,
                                                                        "name": tool_name,
                                                                        "input": {}
                                                                    }
                                                                }).to_string()));

                                                            current_tc_block = Some(block_index);
                                                            final_stop_reason = "tool_use".to_string();
                                                        }

                                                        // Arguments delta (can be in same chunk or subsequent chunks)
                                                        if !args.is_empty() {
                                                            if let Some(bi) = current_tc_block {
                                                                yield Ok(Event::default()
                                                                    .event("content_block_delta")
                                                                    .data(serde_json::json!({
                                                                        "type": "content_block_delta",
                                                                        "index": bi,
                                                                        "delta": {"type": "input_json_delta", "partial_json": args}
                                                                    }).to_string()));
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Stream error in Anthropic proxy");
                        break;
                    }
                },
                Ok(None) => {
                    tracing::warn!("Backend stream ended without [DONE] (Anthropic)");
                    break;
                }
                Err(_) => {
                    tracing::error!(
                        "Stream chunk timeout — no data from backend for {:?} (Anthropic)",
                        chunk_timeout
                    );
                    break;
                }
            }
        }
    };
    Ok(Box::pin(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use std::collections::HashMap;

    /// cr-001 RED: 当前 parse_anthropic_messages 把 system 塞进 Role::System 消息，
    /// 这是协议错误。Anthropic /v1/messages 要求 system 是顶层字段，不是消息。
    /// cr-001 实施后：
    /// 1. 返回值改成 (Option<String>, Vec<InternalMessage>)
    /// 2. 系统文本放第一个元素
    /// 3. messages 列表里不出现 Role::System
    #[test]
    fn parse_anthropic_messages_extracts_system_to_top_level() {
        let req = AnthropicMessagesRequest {
            model: "Plan".to_string(),
            max_tokens: Some(100),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            system: Some(serde_json::json!("You are helpful")),
            stream: false,
            temperature: None,
            tools: None,
            tool_choice: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
        };
        // 现状 RED：调用方期望 (Option<String>, Vec<InternalMessage>) 但当前函数返回 Vec<InternalMessage>
        // 期望系统被抽到顶层，messages 不含 Role::System
        let (system, messages) = parse_anthropic_messages(&req);
        assert_eq!(system, Some("You are helpful".to_string()));
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0].role, Role::User));
        assert!(!messages.iter().any(|m| matches!(m.role, Role::System)));
    }

    #[test]
    fn parse_anthropic_messages_system_array_concatenates() {
        // 数组形式 system 块：拼接所有 text 字段
        let req = AnthropicMessagesRequest {
            model: "Plan".to_string(),
            max_tokens: Some(100),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            system: Some(serde_json::json!([
                {"type": "text", "text": "Part 1"},
                {"type": "text", "text": "Part 2"}
            ])),
            stream: false,
            temperature: None,
            tools: None,
            tool_choice: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
        };
        let (system, messages) = parse_anthropic_messages(&req);
        assert_eq!(system, Some("Part 1\nPart 2".to_string()));
        assert!(!messages.iter().any(|m| matches!(m.role, Role::System)));
    }

    #[test]
    fn parse_anthropic_messages_no_system_returns_none() {
        let req = AnthropicMessagesRequest {
            model: "Plan".to_string(),
            max_tokens: Some(100),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            system: None,
            stream: false,
            temperature: None,
            tools: None,
            tool_choice: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
        };
        let (system, messages) = parse_anthropic_messages(&req);
        assert_eq!(system, None);
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0].role, Role::User));
    }

    /// cr-205: 系统 array 形式带 cache_control → 当前实现只取 text，cache_control 被丢弃
    /// 短期方案 A（已实施）：cache_control 不透传
    /// 长期方案 B（待）：passthrough 到 Anthropic 后端需要 array 形式保留
    #[test]
    fn parse_anthropic_system_array_with_cache_control_drops_cache() {
        let req = AnthropicMessagesRequest {
            model: "Plan".to_string(),
            max_tokens: Some(100),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            system: Some(serde_json::json!([
                {"type": "text", "text": "Part 1", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "Part 2"}
            ])),
            stream: false,
            temperature: None,
            tools: None,
            tool_choice: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
        };
        let (system, _) = parse_anthropic_messages(&req);
        // 短期方案 A：cache_control 丢弃，system 文本拼接
        assert_eq!(system, Some("Part 1\nPart 2".to_string()));
    }
}
