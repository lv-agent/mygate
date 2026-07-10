//! Schema validation regression tests (cr-303 兼容性验证)
//!
//! 加载 `veps/contract/*.schema.json`，用 JSON Schema 2020-12 validator 校验
//! 22 个 sample 全部接受。sample 来源：
//! - 18 个手工构造（覆盖核心契约）
//! - 4 个从 Anthropic / OpenAI 官方 OpenAPI 抽出的字段集合（cr-303）
//!
//! 跑：`cargo test --test conformance_schema_validation`

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use jsonschema::Retrieve;

const CONTRACT_DIR: &str = "veps/contract";

/// 预加载所有 schema 文件，$ref 用相对路径（./foo.schema.json）解析
struct FileRetriever {
    base: PathBuf,
    cache: HashMap<String, serde_json::Value>,
}

impl FileRetriever {
    fn new() -> Self {
        let base = PathBuf::from(CONTRACT_DIR);
        let mut cache = HashMap::new();
        Self::load_dir(&base, &mut cache);
        Self { base, cache }
    }
    fn load_dir(dir: &std::path::Path, cache: &mut HashMap<String, serde_json::Value>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    Self::load_dir(&p, cache);
                } else if p.extension().and_then(|s| s.to_str()) == Some("json") {
                    if let Ok(body) = fs::read_to_string(&p) {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                            if let Some(rel) = p.strip_prefix(CONTRACT_DIR).ok().and_then(|p| p.to_str()) {
                                cache.insert(rel.to_string(), v);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Retrieve for FileRetriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let full = uri.as_str();
        // 去掉 query / fragment
        let path = full.split('?').next().unwrap_or(full).split('#').next().unwrap_or(full);
        // 去掉 file://mygate/ 前缀，转为相对 CONTRACT_DIR 的 key
        let key = path
            .strip_prefix("file://mygate/")
            .or_else(|| path.strip_prefix("file://"))
            .unwrap_or(path)
            .trim_start_matches("./")
            .trim_start_matches('/');
        self.cache
            .get(key)
            .cloned()
            .ok_or_else(|| format!("schema not found: {} (full={})", key, full).into())
    }
}

fn load_schema(name: &str) -> serde_json::Value {
    let path = PathBuf::from(CONTRACT_DIR).join(name);
    let body = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let mut v: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e));
    // 注入 $id 让相对 $ref 能解析
    if let Some(obj) = v.as_object_mut() {
        obj.entry("$id".to_string()).or_insert_with(|| {
            // 用 "file://mygate/{name}#" 作为虚拟 base
            // jsonschema 0.30 的 Uri<String> 接受 file:// scheme
            serde_json::Value::String(format!("file://mygate/{}", name))
        });
    }
    v
}

fn make_validator(schema_name: &str) -> jsonschema::Validator {
    let schema = load_schema(schema_name);
    let retriever = FileRetriever::new();
    jsonschema::options()
        .with_retriever(retriever)
        .build(&schema)
        .expect("schema compiles")
}

fn assert_valid(schema_name: &str, sample: serde_json::Value) {
    let v = make_validator(schema_name);
    if let Err(e) = v.validate(&sample) {
        eprintln!("❌ {} sample rejected: {}", schema_name, e);
        panic!("{} sample did not validate", schema_name);
    }
}

fn assert_invalid(schema_name: &str, sample: serde_json::Value) {
    let v = make_validator(schema_name);
    if v.is_valid(&sample) {
        panic!("{} sample wrongly accepted (should be rejected)", schema_name);
    }
}

#[test]
fn sample_openai_request_basic() {
    assert_valid("openai/chat-completions-request.schema.json", serde_json::json!({
        "model": "Plan",
        "messages": [{"role": "user", "content": "hi"}]
    }));
}

#[test]
fn sample_openai_request_full_tool_call() {
    assert_valid("openai/chat-completions-request.schema.json", serde_json::json!({
        "model": "Code",
        "messages": [
            {"role": "user", "content": "Read main.rs"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_abc", "type": "function",
                "function": {"name": "Read", "arguments": "{\"file_path\":\"main.rs\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_abc", "content": "fn main() {}"}
        ],
        "tools": [{"type": "function", "function": {
            "name": "Read", "description": "Read file",
            "parameters": {"type": "object", "properties": {"file_path": {"type": "string"}}}
        }}],
        "tool_choice": {"type": "function", "function": {"name": "Read"}},
        "response_format": {"type": "json_object"},
        "stream": true
    }));
}

#[test]
fn sample_openai_request_with_service_tier_audio_reasoning() {
    // cr-303 补的字段
    assert_valid("openai/chat-completions-request.schema.json", serde_json::json!({
        "model": "o3",
        "messages": [{"role": "user", "content": "hi"}],
        "service_tier": "auto",
        "audio": {"voice": "alloy", "format": "wav"},
        "reasoning_effort": "high",
        "logit_bias": {"50256": -100},
        "logprobs": true,
        "top_logprobs": 5,
        "store": false,
        "max_completion_tokens": 4096,
        "modalities": ["text", "audio"],
        "metadata": {"user_id": "u1"}
    }));
}

#[test]
fn sample_openai_request_rejects_unknown_field() {
    assert_invalid("openai/chat-completions-request.schema.json", serde_json::json!({
        "model": "Plan",
        "messages": [{"role": "user", "content": "hi"}],
        "unknown_field": "xyz"
    }));
}

#[test]
fn sample_openai_response_basic() {
    assert_valid("openai/chat-completions-response.schema.json", serde_json::json!({
        "id": "x", "object": "chat.completion", "created": 1, "model": "Plan",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }));
}

#[test]
fn sample_openai_response_with_service_tier() {
    assert_valid("openai/chat-completions-response.schema.json", serde_json::json!({
        "id": "x", "object": "chat.completion", "created": 1, "model": "Plan",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
        "service_tier": "auto",
        "system_fingerprint": "fp_x"
    }));
}

#[test]
fn sample_openai_chunk_basic() {
    assert_valid("openai/chat-completions-chunk.schema.json", serde_json::json!({
        "id": "x", "object": "chat.completion.chunk", "created": 1, "model": "Plan",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "hi"}, "finish_reason": null}]
    }));
}

#[test]
fn sample_openai_models_list() {
    assert_valid("openai/models-list.schema.json", serde_json::json!({
        "object": "list", "data": [{"id": "Plan", "object": "model", "owned_by": "mygate"}]
    }));
}

#[test]
fn sample_anthropic_request_basic() {
    assert_valid("anthropic/messages-request.schema.json", serde_json::json!({
        "model": "Plan", "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}]
    }));
}

#[test]
fn sample_anthropic_request_full_tool_use() {
    assert_valid("anthropic/messages-request.schema.json", serde_json::json!({
        "model": "Code", "max_tokens": 4096,
        "system": "You are helpful",
        "messages": [
            {"role": "user", "content": "Read main.rs"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_01A", "name": "Read", "input": {"file_path": "main.rs"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "fn main() {}"}
            ]}
        ],
        "tools": [{
            "name": "Read", "description": "Read file",
            "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}
        }],
        "tool_choice": {"type": "auto"}
    }));
}

#[test]
fn sample_anthropic_response_basic() {
    // cr-303 必填：stop_sequence, stop_details, container
    assert_valid("anthropic/messages-response.schema.json", serde_json::json!({
        "id": "msg_x", "type": "message", "role": "assistant", "model": "Plan",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "stop_details": null, "container": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }));
}

#[test]
fn sample_anthropic_response_with_thinking() {
    assert_valid("anthropic/messages-response.schema.json", serde_json::json!({
        "id": "msg_x", "type": "message", "role": "assistant", "model": "Plan",
        "content": [
            {"type": "thinking", "thinking": "...", "signature": "ed25519:abc"},
            {"type": "text", "text": "answer"}
        ],
        "stop_reason": "end_turn", "stop_sequence": null,
        "stop_details": null, "container": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }));
}

#[test]
fn sample_anthropic_response_rejects_missing_container() {
    // cr-303 发现：container 是官方必填
    assert_invalid("anthropic/messages-response.schema.json", serde_json::json!({
        "id": "msg_x", "type": "message", "role": "assistant", "model": "Plan",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn", "stop_sequence": null, "stop_details": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }));
}

// ===== SSE events =====

fn sse(sample: serde_json::Value) {
    assert_valid("anthropic/sse-events.schema.json", sample);
}

#[test]
fn sse_message_start() {
    sse(serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_x", "type": "message", "role": "assistant", "model": "P",
            "content": [{"type": "text", "text": "h"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "stop_details": null, "container": null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }
    }));
}

#[test]
fn sse_message_delta_with_usage() {
    // cr-303 验证：usage 必填
    sse(serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {"output_tokens": 5}
    }));
}

#[test]
fn sse_message_delta_rejects_missing_usage() {
    assert_invalid("anthropic/sse-events.schema.json", serde_json::json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"}
    }));
}

#[test]
fn sse_message_stop() { sse(serde_json::json!({"type": "message_stop"})); }

#[test]
fn sse_content_block_start_text() {
    sse(serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""}
    }));
}

#[test]
fn sse_content_block_start_thinking() {
    sse(serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "thinking", "thinking": ""}
    }));
}

#[test]
fn sse_content_block_start_redacted_thinking() {
    // cr-303 新增块类型
    sse(serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "redacted_thinking", "data": "encrypted"}
    }));
}

#[test]
fn sse_content_block_start_tool_use() {
    sse(serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "tool_use", "id": "toolu_01", "name": "Read", "input": {}}
    }));
}

#[test]
fn sse_content_block_start_server_tool_use() {
    // cr-303 新增块类型
    sse(serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "server_tool_use", "id": "srv_x", "name": "web_search", "input": {"q": "x"}}
    }));
}

#[test]
fn sse_content_block_start_web_search_result() {
    sse(serde_json::json!({
        "type": "content_block_start", "index": 0,
        "content_block": {"type": "web_search_tool_result", "tool_use_id": "srv_x", "content": []}
    }));
}

#[test]
fn sse_content_block_delta_text() {
    sse(serde_json::json!({
        "type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "h"}
    }));
}

#[test]
fn sse_content_block_delta_thinking() {
    sse(serde_json::json!({
        "type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "h"}
    }));
}

#[test]
fn sse_content_block_delta_signature() {
    sse(serde_json::json!({
        "type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "ed25519:x"}
    }));
}

#[test]
fn sse_content_block_delta_input_json() {
    sse(serde_json::json!({
        "type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{}"}
    }));
}

#[test]
fn sse_content_block_delta_citations() {
    sse(serde_json::json!({
        "type": "content_block_delta", "index": 0, "delta": {"type": "citations_delta", "citation": {}}
    }));
}

#[test]
fn sse_content_block_stop() {
    sse(serde_json::json!({"type": "content_block_stop", "index": 0}));
}

// ===== 错误响应 =====

#[test]
fn error_openai_style() {
    assert_valid("common/error-response.schema.json", serde_json::json!({
        "error": {"message": "x", "type": "invalid_request_error"}
    }));
}

#[test]
fn error_anthropic_style_with_request_id() {
    // cr-303 验证：request_id 必填
    assert_valid("common/error-response.schema.json", serde_json::json!({
        "type": "error", "error": {"type": "api_error", "message": "x"},
        "request_id": "req_abc"
    }));
}

#[test]
fn error_anthropic_rejects_missing_request_id() {
    assert_invalid("common/error-response.schema.json", serde_json::json!({
        "type": "error", "error": {"type": "api_error", "message": "x"}
    }));
}

#[test]
fn error_anthropic_timeout_error() {
    // cr-303 新增
    assert_valid("common/error-response.schema.json", serde_json::json!({
        "type": "error", "error": {"type": "timeout_error", "message": "x"},
        "request_id": "req_1"
    }));
}

#[test]
fn sample_openai_request_with_response_format_json() {
    // cr-102
    assert_valid("openai/chat-completions-request.schema.json", serde_json::json!({
        "model": "Plan",
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "json_object"}
    }));
}

#[test]
fn sample_openai_request_with_response_format_text() {
    // cr-102
    assert_valid("openai/chat-completions-request.schema.json", serde_json::json!({
        "model": "Plan",
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "text"}
    }));
}
