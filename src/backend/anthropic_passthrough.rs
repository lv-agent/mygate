/// Anthropic passthrough backend — forwards Anthropic requests directly to the provider.
use crate::config::ProviderConfig;
use crate::core::types::{ContentBlock, FunctionCall, InternalRequest, InternalResponse, Role, Usage};
use crate::error::GatewayError;

pub async fn send_anthropic_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    internal_req: &InternalRequest,
    preferred_model: &str,
) -> Result<InternalResponse, GatewayError> {
    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{}/v1/messages", base);
    let model = preferred_model;

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

    tracing::info!(model=%model, url=%url, tools=%internal_req.tools.as_ref().map(|t|t.len()).unwrap_or(0), "Anthropic passthrough");

    // cr-003: 按 auth_style 选择鉴权头
    // - "bearer"（默认）：`Authorization: Bearer <api_key>`（MiniMax / 多数中国厂商）
    // - "anthropic"：用于真实 Anthropic API，需要 `x-api-key: <api_key>` + `anthropic-version: 2023-06-01`
    let mut req_builder = client.post(&url).header("Content-Type", "application/json");
    req_builder = match provider.auth_style.as_str() {
        "anthropic" => req_builder
            .header("x-api-key", &provider.api_key)
            .header("anthropic-version", "2023-06-01"),
        _ => req_builder.header("Authorization", format!("Bearer {}", provider.api_key)),
    };
    let resp = req_builder
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| GatewayError::BackendError { status: 502, body: format!("request failed: {}", e) })?;

    let status = resp.status();
    let response_body: serde_json::Value = resp.json().await
        .map_err(|e| GatewayError::BackendError { status: 502, body: format!("parse: {}", e) })?;

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
