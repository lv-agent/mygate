use crate::backend::BackendAdapter;
use crate::config::ProviderConfig;
use crate::core::types::*;
use crate::error::GatewayError;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    /// cr-101: 工具选择策略。序列化为 string 或 object。
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    /// cr-102: 响应格式。
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
    /// cr-103: 采样参数
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<u32>,
    /// cr-103: 停止序列（OpenAI 接受 string 或 array；MyGate 统一为 array）
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    /// cr-104: 流式选项
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
    /// cr-206: 用户标识
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: String,
    content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Debug, Serialize)]
struct OpenAITool {
    r#type: String,
    function: OpenAIToolFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct OpenAIToolCall {
    id: String,
    r#type: String,
    function: OpenAIToolFunctionCall,
}

#[derive(Debug, Serialize)]
struct OpenAIToolFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIResponse {
    pub id: Option<String>,
    pub model: Option<String>,
    pub choices: Vec<OpenAIChoice>,
    pub usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIChoice {
    pub message: Option<OpenAIResponseMessage>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIResponseMessage {
    pub content: Option<serde_json::Value>,
    pub tool_calls: Option<Vec<OpenAIToolCallResponse>>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIToolCallResponse {
    pub id: String,
    pub function: OpenAIToolFunctionCallResponse,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIToolFunctionCallResponse {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct OpenAIUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

fn to_openai_request(req: &InternalRequest, model: &str) -> OpenAIRequest {
    // cr-101: 把内部 ToolChoice 规范化转 OpenAI 协议格式
    let tool_choice = req.tool_choice.as_ref().map(|tc| match tc {
        ToolChoice::Auto => serde_json::Value::String("auto".to_string()),
        ToolChoice::None => serde_json::Value::String("none".to_string()),
        ToolChoice::Any => serde_json::Value::String("required".to_string()),
        ToolChoice::Specific(name) => serde_json::json!({
            "type": "function",
            "function": {"name": name}
        }),
    });
    // cr-102: 内部 ResponseFormat 序列化
    let response_format = req.response_format.as_ref().map(|rf| match rf {
        ResponseFormat::Text => serde_json::json!({"type": "text"}),
        ResponseFormat::JsonObject => serde_json::json!({"type": "json_object"}),
    });
    // cr-001: 如果有顶层 system 字段，预先追加为 messages[0] role=system
    let mut messages: Vec<OpenAIMessage> = Vec::new();
    if let Some(system) = &req.system {
        if !system.is_empty() {
            messages.push(OpenAIMessage {
                role: "system".to_string(),
                content: serde_json::Value::String(system.clone()),
                tool_call_id: None,
                tool_calls: None,
            });
        }
    }
    let msg_iter = req.messages.iter().flat_map(|msg| {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };

        // Collect tool_results separately — each needs its own OpenAI message
        let mut tool_results: Vec<OpenAIMessage> = Vec::new();
        let mut content = serde_json::Value::Null;
        let mut tool_calls_out: Option<Vec<OpenAIToolCall>> = None;

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    content = serde_json::Value::String(text.clone());
                }
                ContentBlock::ImageUrl { image_url } => {
                    content = serde_json::json!([{
                        "type": "image_url",
                        "image_url": { "url": image_url.url }
                    }]);
                }
                // cr-204: OpenAI 协议无 document 概念。降级为 text 描述。
                ContentBlock::Document { source: _ } => {
                    content = serde_json::Value::String(
                        "[document: OpenAI protocol does not support document blocks]".to_string()
                    );
                }
                ContentBlock::ToolResult { tool_use_id, content: result_content } => {
                    // Each tool_result must be its own message with role="tool"
                    tool_results.push(OpenAIMessage {
                        role: "tool".to_string(),
                        content: serde_json::Value::String(result_content.clone()),
                        tool_call_id: Some(tool_use_id.clone()),
                        tool_calls: None,
                    });
                }
                ContentBlock::ToolCall { id, function } => {
                    tool_calls_out.get_or_insert_with(Vec::new).push(OpenAIToolCall {
                        id: id.clone(),
                        r#type: "function".to_string(),
                        function: OpenAIToolFunctionCall {
                            name: function.name.clone(),
                            arguments: function.arguments.clone(),
                        },
                    });
                }
            }
        }

        let mut result = Vec::new();

        // If there were tool_results, emit them as separate messages
        if !tool_results.is_empty() {
            // If there's also text content alongside tool_results (rare but possible),
            // emit the original message first, then the tool result messages
            if !content.is_null() {
                result.push(OpenAIMessage {
                    role: role.to_string(),
                    content,
                    tool_call_id: None,
                    tool_calls: tool_calls_out,
                });
            }
            result.extend(tool_results);
        } else {
            result.push(OpenAIMessage { role: role.to_string(), content, tool_call_id: None, tool_calls: tool_calls_out });
        }

        result
    });

    let tools = req.tools.as_ref().map(|t| {
        t.iter().map(|tool| OpenAITool {
            r#type: "function".to_string(),
            function: OpenAIToolFunction {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            },
        }).collect()
    });

    // 把 flat_map 出来的 messages 追加到已预填的 system 消息后面
    messages.extend(msg_iter);

    OpenAIRequest {
        model: model.to_string(),
        messages,
        stream: if req.stream { Some(true) } else { None },
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        tools,
        tool_choice,
        response_format,
        top_p: req.top_p,
        frequency_penalty: req.frequency_penalty,
        presence_penalty: req.presence_penalty,
        seed: req.seed,
        n: req.n,
        stop: req.stop.clone(),
        stream_options: req.stream_options.as_ref().map(|o| {
            serde_json::json!({"include_usage": o.include_usage})
        }),
        // cr-206: user 标识
        user: req.user.clone(),
    }
}

fn from_openai_response(resp: OpenAIResponse, alias: &str) -> InternalResponse {
    let mut content = Vec::new();
    let mut finish_reason = None;
    if let Some(choice) = resp.choices.first() {
        finish_reason = choice.finish_reason.clone();
        if let Some(msg) = &choice.message {
            if let Some(c) = &msg.content {
                if let Some(text) = c.as_str() {
                    if !text.is_empty() {
                        content.push(ContentBlock::Text { text: text.to_string() });
                    }
                }
            }
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    content.push(ContentBlock::ToolCall {
                        id: tc.id.clone(),
                        function: FunctionCall {
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        },
                    });
                }
            }
        }
    }
    let usage = resp.usage.map(|u| Usage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    }).unwrap_or_default();
    InternalResponse {
        id: resp.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        model: resp.model.unwrap_or_default(),
        alias: alias.to_string(),
        content,
        usage,
        finish_reason,
    }
}

pub async fn send_non_streaming(
    client: &Client,
    provider: &ProviderConfig,
    request: &InternalRequest,
    model: &str,
    timeout: Duration,
) -> Result<InternalResponse, GatewayError> {
    let openai_req = to_openai_request(request, model);
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
    tracing::info!(model = model, url = %url, "Sending non-streaming request to backend");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .header("Content-Type", "application/json")
        .json(&openai_req)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| GatewayError::BackendRequestFailed(e.to_string()))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        tracing::warn!(status = status, body = %body, "Backend returned error");
        return Err(GatewayError::BackendError { status, body });
    }
    let openai_resp: OpenAIResponse = resp
        .json()
        .await
        .map_err(|e| GatewayError::Internal(format!("Failed to parse backend response: {}", e)))?;
    Ok(from_openai_response(openai_resp, &request.model_alias))
}

pub async fn send_streaming(
    client: &Client,
    provider: &ProviderConfig,
    request: &InternalRequest,
    model: &str,
    _timeout: Duration,  // 流式不设全局超时，用 per-chunk（spec §7.2）
) -> Result<reqwest::Response, GatewayError> {
    let mut openai_req = to_openai_request(request, model);
    openai_req.stream = Some(true);
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
    tracing::info!(model = model, url = %url, "Sending streaming request to backend");
    // Debug: log message structure for tool message diagnosis
    let tool_call_msgs: Vec<_> = openai_req.messages.iter().enumerate()
        .filter(|(_, m)| m.tool_calls.as_ref().map_or(false, |tc| !tc.is_empty()))
        .map(|(i, m)| {
            let ids: Vec<_> = m.tool_calls.as_ref().unwrap().iter().map(|tc| tc.id.as_str()).collect();
            (i, ids)
        })
        .collect();
    let tool_result_msgs: Vec<_> = openai_req.messages.iter().enumerate()
        .filter(|(_, m)| m.tool_call_id.is_some())
        .map(|(i, m)| (i, m.tool_call_id.as_deref().unwrap_or("?")))
        .collect();
    if !tool_call_msgs.is_empty() {
        tracing::info!(
            messages = openai_req.messages.len(),
            tool_call_at = ?tool_call_msgs,
            tool_results_at = ?tool_result_msgs,
            "Message structure for backend request"
        );
    }
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .header("Content-Type", "application/json")
        .json(&openai_req)
        // No .timeout() for streaming — the response is indefinite;
        // per-chunk timeout in the stream handler detects stalls.
        // connect_timeout is set on the Client itself.
        .send()
        .await
        .map_err(|e| GatewayError::BackendRequestFailed(e.to_string()))?;
    let status = resp.status().as_u16();
    let ct = resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    tracing::info!(status = status, content_type = %ct, "Backend streaming response headers received");
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        tracing::warn!(status = status, body = %body, "Backend returned error (streaming)");
        return Err(GatewayError::BackendError { status, body });
    }
    Ok(resp)
}

#[allow(dead_code)]
pub fn parse_sse_line(line: &str, alias: &str) -> Option<Vec<StreamChunk>> {
    let data = line.trim_start_matches("data: ").trim();
    if data == "[DONE]" || data.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    let id = parsed.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let model = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let choices = parsed.get("choices")?.as_array()?;
    let mut chunks = Vec::new();
    for choice in choices {
        let delta = choice.get("delta")?;
        let mut delta_content = None;
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                delta_content = Some(ContentBlock::Text { text: content.to_string() });
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let fn_obj = tc.get("function")?;
                let name = fn_obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let arguments = fn_obj.get("arguments").and_then(|v| v.as_str()).unwrap_or("").to_string();
                delta_content = Some(ContentBlock::ToolCall {
                    id,
                    function: FunctionCall { name, arguments },
                });
            }
        }
        let finish_reason = choice.get("finish_reason").and_then(|v| v.as_str()).map(|s| s.to_string());
        chunks.push(StreamChunk {
            id: id.clone(),
            model: model.clone(),
            alias: alias.to_string(),
            delta: delta_content,
            usage: None,
            finish_reason,
        });
    }
    Some(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_openai_request_basic() {
        let req = InternalRequest {
            model_alias: "Simple".to_string(),
            system: Some("You are helpful".to_string()),
            messages: vec![
                InternalMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: "Hello".to_string() }],
                },
            ],
            stream: false,
            temperature: Some(0.7),
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
            metadata: None,
            user: None,
        };
        let openai = to_openai_request(&req, "glm-4-flash");
        assert_eq!(openai.model, "glm-4-flash");
        assert_eq!(openai.messages.len(), 2);
        assert_eq!(openai.messages[0].role, "system");
        assert_eq!(openai.messages[0].content, serde_json::json!("You are helpful"));
        assert_eq!(openai.messages[1].role, "user");
        assert!(openai.stream.is_none());
        assert_eq!(openai.temperature, Some(0.7));
    }

    /// cr-001: 验证 system 字段转 role=system 消息
    #[test]
    fn test_to_openai_request_system_from_top_level() {
        let req = InternalRequest {
            model_alias: "Plan".to_string(),
            system: Some("Top-level system".to_string()),
            messages: vec![],
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
            metadata: None,
            user: None,
        };
        let openai = to_openai_request(&req, "glm-5.1");
        assert_eq!(openai.messages.len(), 1);
        assert_eq!(openai.messages[0].role, "system");
        assert_eq!(openai.messages[0].content, serde_json::json!("Top-level system"));
    }

    #[test]
    fn test_to_openai_request_streaming() {
        let req = InternalRequest {
            model_alias: "Simple".to_string(),
            system: None,
            messages: vec![],
            stream: true,
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
            metadata: None,
            user: None,
        };
        let openai = to_openai_request(&req, "test-model");
        assert_eq!(openai.stream, Some(true));
    }

    #[test]
    fn test_from_openai_response() {
        let resp = OpenAIResponse {
            id: Some("chatcmpl-123".to_string()),
            model: Some("glm-4-flash".to_string()),
            choices: vec![OpenAIChoice {
                message: Some(OpenAIResponseMessage {
                    content: Some(serde_json::Value::String("Hello!".to_string())),
                    tool_calls: None,
                }),
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
            }),
        };
        let internal = from_openai_response(resp, "Simple");
        assert_eq!(internal.alias, "Simple");
        assert_eq!(internal.model, "glm-4-flash");
        assert_eq!(internal.content.len(), 1);
        assert!(matches!(&internal.content[0], ContentBlock::Text { text } if text == "Hello!"));
    }

    #[test]
    fn test_parse_sse_line_text_delta() {
        let line = r#"data: {"id":"chatcmpl-1","model":"glm-4","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let chunks = parse_sse_line(line, "Simple").unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0].delta, Some(ContentBlock::Text { text }) if text == "Hi"));
    }

    #[test]
    fn test_parse_sse_line_done() {
        assert!(parse_sse_line("data: [DONE]", "Simple").is_none());
    }

    #[test]
    fn test_parse_sse_line_empty() {
        assert!(parse_sse_line("data: ", "Simple").is_none());
    }

    // ===== cr-101: tool_choice 序列化 =====

    fn req_with_tool_choice(tc: Option<ToolChoice>) -> InternalRequest {
        InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            metadata: None,
            user: None,
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: tc,
        }
    }

    /// cr-101: ToolChoice::Auto → "auto"
    #[test]
    fn test_tool_choice_auto() {
        let req = req_with_tool_choice(Some(ToolChoice::Auto));
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.tool_choice, Some(serde_json::json!("auto")));
    }

    /// cr-101: ToolChoice::None → "none"
    #[test]
    fn test_tool_choice_none() {
        let req = req_with_tool_choice(Some(ToolChoice::None));
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.tool_choice, Some(serde_json::json!("none")));
    }

    /// cr-101: ToolChoice::Any → "required"（OpenAI 协议特殊映射）
    #[test]
    fn test_tool_choice_any_to_required() {
        let req = req_with_tool_choice(Some(ToolChoice::Any));
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.tool_choice, Some(serde_json::json!("required")));
    }

    /// cr-101: ToolChoice::Specific("X") → {type:function, function:{name:"X"}}
    #[test]
    fn test_tool_choice_specific() {
        let req = req_with_tool_choice(Some(ToolChoice::Specific("Read".to_string())));
        let openai = to_openai_request(&req, "x");
        assert_eq!(
            openai.tool_choice,
            Some(serde_json::json!({"type":"function","function":{"name":"Read"}}))
        );
    }

    /// cr-101: None → 不输出 tool_choice 字段（skip_serializing_if）
    #[test]
    fn test_tool_choice_absent() {
        let req = req_with_tool_choice(None);
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.tool_choice, None);
    }

    // ===== cr-102: response_format 序列化 =====

    fn req_with_response_format(rf: Option<ResponseFormat>) -> InternalRequest {
        InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            response_format: rf,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
            metadata: None,
            user: None,
        }
    }

    /// cr-102: ResponseFormat::Text → {"type":"text"}
    #[test]
    fn test_response_format_text() {
        let req = req_with_response_format(Some(ResponseFormat::Text));
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.response_format, Some(serde_json::json!({"type":"text"})));
    }

    /// cr-102: ResponseFormat::JsonObject → {"type":"json_object"}
    #[test]
    fn test_response_format_json_object() {
        let req = req_with_response_format(Some(ResponseFormat::JsonObject));
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.response_format, Some(serde_json::json!({"type":"json_object"})));
    }

    /// cr-102: None → 不输出 response_format 字段
    #[test]
    fn test_response_format_absent() {
        let req = req_with_response_format(None);
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.response_format, None);
    }

    // ===== cr-103: 采样参数序列化 =====

    fn req_with_sampling(
        top_p: Option<f64>,
        freq: Option<f64>,
        pres: Option<f64>,
        stop: Option<Vec<String>>,
    ) -> InternalRequest {
        InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            top_p,
            top_k: None,
            frequency_penalty: freq,
            presence_penalty: pres,
            stop,
            seed: None,
            n: None,
            stream_options: None,
            metadata: None,
            user: None,
        }
    }

    /// cr-103: top_p 透传
    #[test]
    fn test_top_p_pass_through() {
        let req = req_with_sampling(Some(0.9), None, None, None);
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.top_p, Some(0.9));
    }

    /// cr-103: frequency_penalty / presence_penalty 透传
    #[test]
    fn test_penalties_pass_through() {
        let req = req_with_sampling(None, Some(0.5), Some(-0.5), None);
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.frequency_penalty, Some(0.5));
        assert_eq!(openai.presence_penalty, Some(-0.5));
    }

    /// cr-103: stop 序列透传（Vec）
    #[test]
    fn test_stop_pass_through() {
        let req = req_with_sampling(None, None, None, Some(vec!["END".to_string(), "STOP".to_string()]));
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.stop, Some(vec!["END".to_string(), "STOP".to_string()]));
    }

    /// cr-103: 全 None 时不输出字段（skip_serializing_if）
    #[test]
    fn test_sampling_absent() {
        let req = req_with_sampling(None, None, None, None);
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.top_p, None);
        assert_eq!(openai.frequency_penalty, None);
        assert_eq!(openai.presence_penalty, None);
        assert_eq!(openai.stop, None);
    }

    // ===== cr-104: stream_options 透传 =====

    /// cr-104: include_usage=true 序列化为 {"include_usage": true}
    #[test]
    fn test_stream_options_include_usage_true() {
        let req = InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![],
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
            stream_options: Some(StreamOptions { include_usage: true }),
            user: None,
            metadata: None,
        };
        let openai = to_openai_request(&req, "x");
        assert_eq!(
            openai.stream_options,
            Some(serde_json::json!({"include_usage": true}))
        );
    }

    /// cr-104: None → 不输出 stream_options
    #[test]
    fn test_stream_options_absent() {
        let req = req_with_sampling(None, None, None, None);
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.stream_options, None);
    }

    // ===== cr-206: user 透传 =====

    /// cr-206: user 标识透传
    #[test]
    fn test_user_pass_through() {
        let req = InternalRequest {
            model_alias: "P".to_string(),
            system: None,
            messages: vec![],
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
            metadata: None,
            user: Some("user-12345".to_string()),
        };
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.user, Some("user-12345".to_string()));
    }

    /// cr-206: None → 不输出 user 字段
    #[test]
    fn test_user_absent() {
        let req = req_with_sampling(None, None, None, None);
        let openai = to_openai_request(&req, "x");
        assert_eq!(openai.user, None);
    }
}

// =====================================================================
// cr-201: OpenAI 兼容后端作为 BackendAdapter 实现
// =====================================================================

/// OpenAI 兼容后端 adapter。
pub struct OpenAiCompatAdapter;

#[async_trait]
impl BackendAdapter for OpenAiCompatAdapter {
    fn name(&self) -> &'static str { "openai_compat" }

    async fn send(
        &self,
        client: &Client,
        provider: &ProviderConfig,
        request: &InternalRequest,
        model: &str,
        timeout: Duration,
    ) -> Result<InternalResponse, GatewayError> {
        send_non_streaming(client, provider, request, model, timeout).await
    }

    async fn send_streaming(
        &self,
        client: &Client,
        provider: &ProviderConfig,
        request: &InternalRequest,
        model: &str,
        timeout: Duration,
    ) -> Result<reqwest::Response, GatewayError> {
        send_streaming(client, provider, request, model, timeout).await
    }
}
