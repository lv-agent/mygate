/// Anthropic passthrough backend — forwards Anthropic requests directly to the provider.
use crate::backend::BackendAdapter;
use crate::config::ProviderConfig;
use crate::core::types::{ContentBlock, FunctionCall, InternalRequest, InternalResponse, Role, Usage};
use crate::error::GatewayError;
use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;

/// 构造 Anthropic 协议请求 body（流式与非流式共用）
fn build_anthropic_body(internal_req: &InternalRequest, model: &str) -> serde_json::Value {
    // cr-001: system 不在 messages 里，而是顶层字段
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for msg in &internal_req.messages {
        // 防御性：跳过 Role::System（理论上 parse_anthropic_messages 已经提取过了）
        if matches!(msg.role, Role::System) { continue; }
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "user",
            Role::System => unreachable!(),
        };
        let content = msg.content.iter().map(|block| match block {
            ContentBlock::Text { text } => serde_json::json!({"type":"text","text":text}),
            ContentBlock::ImageUrl { image_url } => {
                // cr-207: OpenAI ImageUrl (data: URL) → Anthropic image 块
                let url = &image_url.url;
                if let Some(rest) = url.strip_prefix("data:").and_then(|s| s.split_once(";base64,")) {
                    let media_type = rest.0;
                    let data = rest.1;
                    serde_json::json!({
                        "type":"image",
                        "source":{"type":"base64","media_type":media_type,"data":data}
                    })
                } else {
                    tracing::warn!(url_len = url.len(), "ImageUrl 非 data: URL 格式，anthropic 不支持 HTTP 图片，丢弃");
                    serde_json::json!({"type":"text","text":"[image: HTTP URL not supported]"})
                }
            }
            // cr-204: document 块 → Anthropic document 块
            ContentBlock::Document { source } => {
                let src = match source {
                    crate::core::types::DocumentSource::Base64 { media_type, data } => serde_json::json!({
                        "type":"base64","media_type":media_type,"data":data
                    }),
                    crate::core::types::DocumentSource::Text { media_type, data } => serde_json::json!({
                        "type":"text","media_type":media_type,"data":data
                    }),
                    crate::core::types::DocumentSource::Url { url } => serde_json::json!({
                        "type":"url","url":url
                    }),
                };
                serde_json::json!({"type":"document","source":src})
            }
            ContentBlock::ToolCall { id, function } => serde_json::json!({
                "type":"tool_use","id":id,"name":function.name,"input":serde_json::from_str::<serde_json::Value>(&function.arguments).unwrap_or_default()
            }),
            ContentBlock::ToolResult { tool_use_id, content } => serde_json::json!({
                "type":"tool_result","tool_use_id":tool_use_id,"content":content
            }),
            _ => serde_json::json!({"type":"text","text":"[unsupported]"})
        }).collect::<Vec<_>>();
        messages.push(serde_json::json!({"role":role,"content":content}));
    }

    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "max_tokens": internal_req.max_tokens.unwrap_or(4096),
    });
    // cr-001: system 顶层字段
    if let Some(ref system) = internal_req.system {
        if !system.is_empty() {
            body["system"] = serde_json::Value::String(system.clone());
        }
    }
    if let Some(temp) = internal_req.temperature { body["temperature"] = temp.into(); }
    // cr-103: Anthropic 采样参数
    if let Some(top_p) = internal_req.top_p { body["top_p"] = top_p.into(); }
    if let Some(top_k) = internal_req.top_k { body["top_k"] = top_k.into(); }
    if let Some(ref stop) = internal_req.stop {
        if !stop.is_empty() {
            body["stop_sequences"] = serde_json::Value::Array(
                stop.iter().map(|s| serde_json::Value::String(s.clone())).collect()
            );
        }
    }
    if let Some(ref tools) = internal_req.tools {
        let anthropic_tools: Vec<serde_json::Value> = tools.iter().map(|t| serde_json::json!({
            "name": t.name, "description": t.description, "input_schema": t.parameters,
        })).collect();
        body["tools"] = serde_json::json!(anthropic_tools);
    }
    // cr-101: tool_choice 序列化为 Anthropic object 格式
    if let Some(ref tc) = internal_req.tool_choice {
        let v = match tc {
            crate::core::types::ToolChoice::Auto => serde_json::json!({"type":"auto"}),
            crate::core::types::ToolChoice::None => serde_json::json!({"type":"none"}),
            crate::core::types::ToolChoice::Any => serde_json::json!({"type":"any"}),
            crate::core::types::ToolChoice::Specific(name) => serde_json::json!({"type":"tool","name":name}),
        };
        body["tool_choice"] = v;
    }
    body
}

/// 构造带鉴权头的 reqwest RequestBuilder
fn apply_auth(
    builder: reqwest::RequestBuilder,
    provider: &ProviderConfig,
) -> reqwest::RequestBuilder {
    // cr-003: 按 auth_style 选择鉴权头
    match provider.auth_style.as_str() {
        "anthropic" => builder
            .header("x-api-key", &provider.api_key)
            .header("anthropic-version", "2023-06-01"),
        _ => builder.header("Authorization", format!("Bearer {}", provider.api_key)),
    }
}

pub async fn send_anthropic_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    internal_req: &InternalRequest,
    preferred_model: &str,
) -> Result<InternalResponse, GatewayError> {
    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{}/v1/messages", base);
    let model = preferred_model;
    let body = build_anthropic_body(internal_req, model);

    tracing::info!(model=%model, url=%url, tools=%internal_req.tools.as_ref().map(|t|t.len()).unwrap_or(0), "Anthropic passthrough");

    let req_builder = client.post(&url)
        .header("Content-Type", "application/json");
    let req_builder = apply_auth(req_builder, provider);
    let resp = req_builder
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| GatewayError::BackendError { status: 502, body: format!("request failed: {}", e) })?;

    let status = resp.status();
    // P0-1 修复: 容忍非 JSON 响应（如 HTML 404、文本错误页）。先看 content-type：
    // - application/json → 正常 JSON 解析
    // - 其他 → 读 body 为文本，返回明确的错误信息（之前会 panic 报 "parse: expected value"）
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let response_body: serde_json::Value = if content_type.contains("application/json") {
        resp.json()
            .await
            .map_err(|e| GatewayError::BackendError {
                status: 502,
                body: format!("json parse error: {}", e),
            })?
    } else {
        // 非 JSON 响应：读 body 为文本 + 返回明确错误
        let body_text = resp
            .text()
            .await
            .map_err(|e| GatewayError::BackendError {
                status: 502,
                body: format!("read body error: {}", e),
            })?;
        let preview: String = body_text.chars().take(200).collect();
        return Err(GatewayError::BackendError {
            status: status.as_u16(),
            body: format!(
                "non-JSON response (content-type={}): {}",
                content_type, preview
            ),
        });
    };

    if !status.is_success() {
        return Err(GatewayError::BackendError { status: 502, body: format!("{}: {}", status, response_body) });
    }

    let content: Vec<ContentBlock> = response_body.get("content").and_then(|c| c.as_array()).map(|arr| {
        arr.iter().map(|block| {
            let t = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            match t {
                "text" => ContentBlock::Text { text: block.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string() },
                "tool_use" => ContentBlock::ToolCall {
                    id: block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    function: FunctionCall {
                        name: block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        arguments: block.get("input").map(|v| v.to_string()).unwrap_or_default(),
                    },
                },
                _ => ContentBlock::Text { text: String::new() },
            }
        }).collect()
    }).unwrap_or_default();

    let has_tool = content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }));
    let finish = if has_tool { Some("tool_use".to_string()) } else { response_body.get("stop_reason").and_then(|v| v.as_str()).map(|s| s.to_string()) };
    let usage = response_body.get("usage");
    Ok(InternalResponse {
        id: response_body.get("id").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
        model: model.to_string(),
        alias: internal_req.model_alias.clone(),
        content,
        finish_reason: finish,
        usage: Usage {
            prompt_tokens: usage.and_then(|u| u.get("input_tokens")).and_then(|v| v.as_u64()),
            completion_tokens: usage.and_then(|u| u.get("output_tokens")).and_then(|v| v.as_u64()),
            total_tokens: None,
        },
    })
}

/// cr-004: 流式请求。返回 reqwest::Response，body 是 Anthropic SSE 事件流。
/// 调用方（router/anthropic.rs）作为 SSE 原样透传给北向客户端。
/// 注意：当前实现不转换为 OpenAI SSE — 限制是仅 Anthropic 北向才能用 provider_type=anthropic 后端。
pub async fn send_anthropic_streaming_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    internal_req: &InternalRequest,
    preferred_model: &str,
) -> Result<reqwest::Response, GatewayError> {
    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{}/v1/messages", base);
    let model = preferred_model;
    let mut body = build_anthropic_body(internal_req, model);
    // 流式标志
    body["stream"] = serde_json::Value::Bool(true);

    tracing::info!(model=%model, url=%url, tools=%internal_req.tools.as_ref().map(|t|t.len()).unwrap_or(0), "Anthropic passthrough (streaming)");

    let req_builder = client.post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");
    let req_builder = apply_auth(req_builder, provider);
    let resp = req_builder
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| GatewayError::BackendError { status: 502, body: format!("request failed: {}", e) })?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(GatewayError::BackendError { status: status.as_u16(), body });
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ContentBlock, FunctionCall, InternalRequest, InternalMessage, Role, ToolChoice};

    fn req_with_system_and_tool() -> InternalRequest {
        InternalRequest {
            model_alias: "Plan".to_string(),
            system: Some("You are helpful".to_string()),
            messages: vec![InternalMessage {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".to_string() }],
            }],
            stream: false,
            temperature: Some(0.5),
            max_tokens: Some(1024),
            tools: Some(vec![crate::core::types::InternalTool {
                name: "Read".to_string(),
                description: Some("Read file".to_string()),
                parameters: Some(serde_json::json!({"type": "object"})),
            }]),
            tool_choice: Some(ToolChoice::Specific("Read".to_string())),
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            user: None,
        }
    }

    /// cr-004: build_anthropic_body 包含 system 顶层、tools、tool_choice object
    #[test]
    fn test_build_anthropic_body_has_all_fields() {
        let body = build_anthropic_body(&req_with_system_and_tool(), "claude-sonnet-4-5");
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["system"], "You are helpful");
        assert!(body["messages"].is_array());
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], "Read");
    }

    /// cr-207: ImageUrl (data: URL) → Anthropic image 块
    #[test]
    fn test_image_data_url_to_anthropic_block() {
        use crate::core::types::{ContentBlock, ImageUrlContent, InternalMessage, Role};
        let req = InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![InternalMessage {
                role: Role::User,
                content: vec![ContentBlock::ImageUrl {
                    image_url: ImageUrlContent {
                        url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAUA".to_string(),
                        detail: None,
                    },
                }],
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            user: None,
        };
        let body = build_anthropic_body(&req, "m");
        let content = &body["messages"][0]["content"][0];
        assert_eq!(content["type"], "image");
        assert_eq!(content["source"]["type"], "base64");
        assert_eq!(content["source"]["media_type"], "image/png");
        assert_eq!(content["source"]["data"], "iVBORw0KGgoAAAANSUhEUgAAAAUA");
    }

    /// cr-204: document 块（PDF base64）→ Anthropic document 块
    #[test]
    fn test_document_base64_to_anthropic_block() {
        use crate::core::types::{
            ContentBlock, DocumentSource, InternalMessage, Role,
        };
        let req = InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![InternalMessage {
                role: Role::User,
                content: vec![ContentBlock::Document {
                    source: DocumentSource::Base64 {
                        media_type: "application/pdf".to_string(),
                        data: "JVBERi0xLjQKJeLjz9MK".to_string(),
                    },
                }],
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            user: None,
        };
        let body = build_anthropic_body(&req, "m");
        let content = &body["messages"][0]["content"][0];
        assert_eq!(content["type"], "document");
        assert_eq!(content["source"]["type"], "base64");
        assert_eq!(content["source"]["media_type"], "application/pdf");
        assert_eq!(content["source"]["data"], "JVBERi0xLjQKJeLjz9MK");
    }

    /// cr-204: document 块（url）→ Anthropic document 块（type=url）
    #[test]
    fn test_document_url_to_anthropic_block() {
        use crate::core::types::{
            ContentBlock, DocumentSource, InternalMessage, Role,
        };
        let req = InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![InternalMessage {
                role: Role::User,
                content: vec![ContentBlock::Document {
                    source: DocumentSource::Url {
                        url: "https://example.com/spec.pdf".to_string(),
                    },
                }],
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            user: None,
        };
        let body = build_anthropic_body(&req, "m");
        let content = &body["messages"][0]["content"][0];
        assert_eq!(content["type"], "document");
        assert_eq!(content["source"]["type"], "url");
        assert_eq!(content["source"]["url"], "https://example.com/spec.pdf");
    }

    /// cr-207: ImageUrl 非 data: 格式（HTTP URL）→ 降级为文本提示，不丢消息
    #[test]
    fn test_image_http_url_degrades_to_text() {
        use crate::core::types::{ContentBlock, ImageUrlContent, InternalMessage, Role};
        let req = InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![InternalMessage {
                role: Role::User,
                content: vec![ContentBlock::ImageUrl {
                    image_url: ImageUrlContent {
                        url: "https://example.com/cat.png".to_string(),
                        detail: None,
                    },
                }],
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            user: None,
        };
        let body = build_anthropic_body(&req, "m");
        let content = &body["messages"][0]["content"][0];
        // HTTP URL 降级为文本（不丢消息），让客户端能感知到
        assert_eq!(content["type"], "text");
        assert!(content["text"].as_str().unwrap().contains("HTTP URL not supported"));
    }

    #[test]
    fn test_build_anthropic_body_minimal() {
        let req = InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: Some(100),
            tools: None,
            tool_choice: None,
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            user: None,
        };
        let body = build_anthropic_body(&req, "m");
        assert_eq!(body["model"], "m");
        assert_eq!(body["max_tokens"], 100);
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }
}

// =====================================================================
// cr-201: Anthropic 直通后端作为 BackendAdapter 实现
// =====================================================================

/// Anthropic 直通后端 adapter。
pub struct AnthropicPassthroughAdapter;

#[async_trait]
impl BackendAdapter for AnthropicPassthroughAdapter {
    fn name(&self) -> &'static str { "anthropic_passthrough" }

    async fn send(
        &self,
        client: &Client,
        provider: &ProviderConfig,
        request: &InternalRequest,
        model: &str,
        _timeout: Duration,
    ) -> Result<InternalResponse, GatewayError> {
        send_anthropic_request(client, provider, request, model).await
    }

    async fn send_streaming(
        &self,
        client: &Client,
        provider: &ProviderConfig,
        request: &InternalRequest,
        model: &str,
        _timeout: Duration,
    ) -> Result<reqwest::Response, GatewayError> {
        send_anthropic_streaming_request(client, provider, request, model).await
    }
}
