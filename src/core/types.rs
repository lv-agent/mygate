use serde::{Deserialize, Serialize};

/// Role of a message participant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlContent },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        function: FunctionCall,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlContent {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalTool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<serde_json::Value>,
}

/// cr-102: 响应格式。OpenAI 标准字段，Anthropic 无对应（不支持）。
/// - `Text`       普通文本（默认）
/// - `JsonObject` 强制 JSON 输出
///
/// 本期仅支持这两种；OpenAI 后续 json_schema 模式留 P2。
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFormat {
    Text,
    JsonObject,
}

/// cr-101: 工具选择策略。规范化 OpenAI / Anthropic 两种协议的不同表示。
/// - `Auto`      模型自决定（默认；OpenAI "auto" / Anthropic `{type:"auto"}`）
/// - `None`      不调用工具（OpenAI "none" / Anthropic `{type:"none"}`）
/// - `Any`       必须调用任一工具（OpenAI "required" / Anthropic `{type:"any"}`）
/// - `Specific`  强制调用指定工具（OpenAI `{type:function,function:{name}}` / Anthropic `{type:"tool",name:"X"}`）
#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    None,
    Any,
    Specific(String), // 工具名
}

/// Unified internal request format, protocol-agnostic.
#[derive(Debug, Clone)]
pub struct InternalRequest {
    pub model_alias: String,
    /// 系统提示词（顶层字段）。cr-001: 从 Role::System 消息升级而来。
    /// - Anthropic 后端：作为 body 顶层 `system` 字段
    /// - OpenAI 兼容后端：作为 messages[0] role=system 消息
    pub system: Option<String>,
    pub messages: Vec<InternalMessage>,
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tools: Option<Vec<InternalTool>>,
    /// cr-101: 工具选择策略
    pub tool_choice: Option<ToolChoice>,
    /// cr-102: 响应格式（OpenAI 协议字段）
    pub response_format: Option<ResponseFormat>,
    /// cr-103: 采样参数
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    /// cr-103: 停止序列。OpenAI `stop`（string|array）和 Anthropic `stop_sequences`（array）统一为 Option<Vec<String>>
    pub stop: Option<Vec<String>>,
    /// cr-103: 随机种子（OpenAI only，Anthropic 不支持）
    pub seed: Option<u64>,
    /// cr-103: 生成 completions 数（OpenAI only，MyGate 仅保证 n=1）
    pub n: Option<u32>,
    /// cr-104: 流式选项（如 include_usage）
    pub stream_options: Option<StreamOptions>,
}

/// cr-104: 流式选项
#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone)]
pub struct InternalMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// Token usage info from backend response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// Unified internal response format.
#[derive(Debug, Clone)]
pub struct InternalResponse {
    pub id: String,
    #[allow(dead_code)]
    pub model: String,
    pub alias: String,
    pub content: Vec<ContentBlock>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// A single SSE chunk from a streaming response. (预留，未来流式重构成单 chunk 处理时启用)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StreamChunk {
    pub id: String,
    pub model: String,
    pub alias: String,
    pub delta: Option<ContentBlock>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}
