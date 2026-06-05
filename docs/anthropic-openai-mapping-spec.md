# Anthropic ↔ OpenAI 协议映射规范

> 本文档详细描述 Anthropic Messages API 与 OpenAI Chat Completions API 之间的双向映射关系。
> 基于 MyGate (`src/router/anthropic.rs` + `src/backend/openai_compat.rs`) 实际转换逻辑整理。

---

## 1. 端点映射

| 方向 | Anthropic | OpenAI |
|---|---|---|
| 请求 | `POST /v1/messages` | `POST /v1/chat/completions` |
| 模型列表 | N/A（CC 不调用） | `GET /v1/models` |

MyGate 路由注册（`src/server.rs`）：
```
/v1/messages       → anthropic::messages    (Anthropic 协议入口)
/v1/chat/completions → openai::chat_completions (OpenAI 协议入口)
/v1/models         → openai::list_models    (模型列表)
```

---

## 2. 请求体映射

### 2.1 顶层字段

| Anthropic 字段 | OpenAI 字段 | 转换规则 |
|---|---|---|
| `model` | `model` | Anthropic 侧为 alias 名（如 `"Plan"`），OpenAI 侧为真实模型名（如 `"glm-5.1"`） |
| `stream` | `stream` | 直接传递 |
| `max_tokens` | `max_tokens` | 直接传递 |
| `temperature` | `temperature` | 直接传递 |
| `system` | → `messages[0]` (role=system) | 见 §2.2 |
| `messages` | `messages` | 见 §2.3 |
| `tools` | `tools` | 见 §2.5 |

### 2.2 system 字段转换

Anthropic 将 `system` 作为顶层字段，OpenAI 将其作为 `role=system` 的消息。

**Anthropic → OpenAI：**

```jsonc
// Anthropic 输入
{
  "system": "You are a helpful assistant.",
  "messages": [...]
}

// → OpenAI 输出
{
  "messages": [
    {"role": "system", "content": "You are a helpful assistant."},
    ...
  ]
}
```

数组形式的 system：
```jsonc
// Anthropic 输入
{
  "system": [
    {"type": "text", "text": "Part 1", "cache_control": {...}},
    {"type": "text", "text": "Part 2"}
  ]
}

// → OpenAI 输出（拼接为一条 system 消息）
{
  "messages": [
    {"role": "system", "content": "Part 1\nPart 2"},
    ...
  ]
}
```

`cache_control` 等 Anthropic 特有字段在转换中丢弃。

### 2.3 消息格式映射

#### 2.3.1 消息角色

| Anthropic role | OpenAI role | 备注 |
|---|---|---|
| `"user"` | `"user"` | 直接映射 |
| `"assistant"` | `"assistant"` | 直接映射 |
| _(system 顶层字段)_ | `"system"` | Anthropic 无 system role，通过顶层字段传递 |

**注意**：Anthropic **没有** `role=tool`。工具结果作为 `role=user` 消息中的 `tool_result` 内容块传递。

#### 2.3.2 消息内容结构

Anthropic 消息的 `content` 字段有三种形式：

| 形式 | 示例 | 处理 |
|---|---|---|
| 字符串 | `"content": "hello"` | 转为 `[{type: "text", text: "hello"}]` |
| 内容块数组 | `"content": [{type: "text", ...}]` | 逐块转换 |
| 其他 JSON | `"content": 42` | `.toString()` 后作为文本 |

---

## 3. 内容块映射（核心转换）

### 3.1 Anthropic → OpenAI（请求方向）

这是最复杂的部分。一条 Anthropic 消息可能包含多种类型的内容块，需要**拆分**为多条 OpenAI 消息。

#### 3.1.1 text 块

```jsonc
// Anthropic
{"type": "text", "text": "Hello"}

// → OpenAI message
{"role": "user", "content": "Hello"}
```

同一消息中的多个 text 块会合并（后者覆盖前者）。

#### 3.1.2 image 块

```jsonc
// Anthropic (base64)
{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "..."}}

// → OpenAI (data URL)
{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}]}
```

#### 3.1.3 tool_use 块（assistant 消息中）

```jsonc
// Anthropic (assistant message content block)
{"type": "tool_use", "id": "toolu_01ABC", "name": "Read", "input": {"file_path": "/home/user/main.rs"}}

// → OpenAI (assistant message tool_calls field)
{
  "role": "assistant",
  "content": null,
  "tool_calls": [{
    "id": "toolu_01ABC",
    "type": "function",
    "function": {
      "name": "Read",
      "arguments": "{\"file_path\":\"/home/user/main.rs\"}"
    }
  }]
}
```

**关键差异**：
- Anthropic：tool_use 是 content 数组中的一个块，`input` 是 **JSON 对象**
- OpenAI：tool_calls 是消息的**顶层字段**（不在 content 里），`arguments` 是 **JSON 字符串**

转换规则：`input`（JSON object）→ `.to_string()` → `arguments`（JSON string）

#### 3.1.4 tool_result 块（user 消息中）

```jsonc
// Anthropic (一条 user 消息中包含多个 tool_result)
{
  "role": "user",
  "content": [
    {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "result 1"},
    {"type": "tool_result", "tool_use_id": "toolu_01B", "content": "result 2"}
  ]
}

// → OpenAI (拆分为多条 tool 消息)
[
  {"role": "tool", "content": "result 1", "tool_call_id": "toolu_01A"},
  {"role": "tool", "content": "result 2", "tool_call_id": "toolu_01B"}
]
```

**⚠️ 这是最容易出错的地方！**

Anthropic 将多个 tool_result 放在同一条 user 消息中，但 OpenAI 要求：
1. 每个 tool_result 必须是独立的 `role: "tool"` 消息
2. 必须有 `tool_call_id` 与之前 assistant 消息中的 `tool_calls[].id` 对应
3. **顺序必须正确**：tool 消息必须紧跟在包含 tool_calls 的 assistant 消息之后

这就是为什么 MyGate 使用 `flat_map` 而不是 `map`——`map` 一对一，`flat_map` 允许一条消息拆成多条。

#### 3.1.5 完整转换示例

Anthropic 输入：
```jsonc
{
  "system": "Be helpful",
  "messages": [
    {"role": "user", "content": "Read foo.rs"},
    {
      "role": "assistant",
      "content": [
        {"type": "tool_use", "id": "toolu_01A", "name": "Read", "input": {"file_path": "foo.rs"}}
      ]
    },
    {
      "role": "user",
      "content": [
        {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "fn main() {}"}
      ]
    }
  ]
}
```

OpenAI 输出：
```jsonc
{
  "messages": [
    {"role": "system", "content": "Be helpful"},
    {"role": "user", "content": "Read foo.rs"},
    {
      "role": "assistant",
      "content": null,
      "tool_calls": [{
        "id": "toolu_01A",
        "type": "function",
        "function": {"name": "Read", "arguments": "{\"file_path\":\"foo.rs\"}"}
      }]
    },
    {"role": "tool", "content": "fn main() {}", "tool_call_id": "toolu_01A"}
  ]
}
```

---

## 4. 工具定义映射

### Anthropic → OpenAI

```jsonc
// Anthropic tool definition
{
  "name": "Read",
  "description": "Read a file",
  "input_schema": {
    "type": "object",
    "properties": {
      "file_path": {"type": "string", "description": "Path"}
    },
    "required": ["file_path"]
  }
}

// → OpenAI tool definition
{
  "type": "function",
  "function": {
    "name": "Read",
    "description": "Read a file",
    "parameters": {
      "type": "object",
      "properties": {
        "file_path": {"type": "string", "description": "Path"}
      },
      "required": ["file_path"]
    }
  }
}
```

| Anthropic 字段 | OpenAI 字段 | 备注 |
|---|---|---|
| `name` | `function.name` | 直接映射 |
| `description` | `function.description` | 直接映射 |
| `input_schema` | `function.parameters` | 直接映射（语义相同） |
| _(无)_ | `type` | 固定为 `"function"` |

---

## 5. 响应映射

### 5.1 非流式响应

#### OpenAI → Anthropic

```jsonc
// OpenAI response
{
  "id": "chatcmpl-123",
  "model": "glm-5.1",
  "choices": [{
    "message": {
      "content": "Hello!",
      "tool_calls": [{
        "id": "call_abc",
        "function": {"name": "Read", "arguments": "{\"file_path\":\"main.rs\"}"}
      }]
    },
    "finish_reason": "tool_calls"
  }],
  "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
}

// → Anthropic response
{
  "id": "chatcmpl-123",
  "type": "message",
  "role": "assistant",
  "model": "Plan",                    // alias 名
  "content": [
    {"type": "text", "text": "Hello!"},
    {
      "type": "tool_use",
      "id": "call_abc",
      "name": "Read",
      "input": {"file_path": "main.rs"}   // arguments string → JSON object
    }
  ],
  "usage": {"input_tokens": 10, "output_tokens": 5},
  "stop_reason": "tool_use"               // finish_reason 转换
}
```

#### finish_reason / stop_reason 映射

| OpenAI `finish_reason` | Anthropic `stop_reason` | 条件 |
|---|---|---|
| `"stop"` | `"end_turn"` | 正常文本结束 |
| `"tool_calls"` | `"tool_use"` | AI 请求调用工具 |
| `"length"` | `"max_tokens"` | 达到 token 限制 |
| `null` | _(不发)_ | 流式中未结束 |

#### usage 映射

| OpenAI | Anthropic |
|---|---|
| `prompt_tokens` | `input_tokens` |
| `completion_tokens` | `output_tokens` |
| `total_tokens` | _(Anthropic 无此字段)_ |

---

## 6. 流式 SSE 映射（最复杂部分）

### 6.1 SSE 格式对比

**OpenAI 后端返回的 SSE 格式：**
```
data: {"id":"chatcmpl-1","model":"glm-5.1","choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}
data: {"id":"chatcmpl-1","model":"glm-5.1","choices":[{"delta":{"content":"!"},"finish_reason":null}]}
data: {"id":"chatcmpl-1","model":"glm-5.1","choices":[{"delta":{},"finish_reason":"stop"}]}
data: [DONE]
```

**Anthropic 需要的 SSE 格式：**
```
event: message_start
data: {"type":"message_start","message":{...}}

event: content_block_start
data: {"type":"content_block_start",...}

event: content_block_delta
data: {"type":"content_block_delta",...}

event: content_block_stop
data: {"type":"content_block_stop",...}

event: message_delta
data: {"type":"message_delta",...}

event: message_stop
data: {"type":"message_stop"}
```

### 6.2 delta 字段映射

#### 普通文本

```jsonc
// OpenAI delta
{"delta": {"content": "Hello"}}

// → Anthropic events
// (先确保 text content_block_start 已发送)
{"type": "content_block_delta", "index": N, "delta": {"type": "text_delta", "text": "Hello"}}
```

#### 推理内容（reasoning_content）

某些后端模型（GLM-5.1、DeepSeek-R1）在 delta 中包含 `reasoning_content` 字段：

```jsonc
// OpenAI delta (GLM-5.1 特有)
{"delta": {"reasoning_content": "Let me think..."}}

// → Anthropic events
// content_block_start (thinking block)
{"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}
// content_block_delta
{"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me think..."}}
```

**注意**：`reasoning_content` 不是 OpenAI 标准字段，是中国 LLM 服务商（智谱、DeepSeek）的扩展。

#### 工具调用（增量）

OpenAI 的 tool_calls 在流式中是增量的：

```jsonc
// 第一个 chunk：新工具调用（含 id 和 name）
{"delta": {"tool_calls": [{"index": 0, "id": "call_abc", "function": {"name": "Read", "arguments": ""}}]}}
// 注意：arguments 可能是空字符串 ""

// 后续 chunks：只有参数增量
{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"file"}}]}}
{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "_path\":\"main.rs\"}"}}]}}

// 最后可能带 finish_reason
{"delta": {}, "finish_reason": "tool_calls"}
```

转换为 Anthropic 事件序列：

```jsonc
// 1. content_block_start (当 id 或 name 非空时)
{"type": "content_block_start", "index": N,
 "content_block": {"type": "tool_use", "id": "call_abc", "name": "Read", "input": {}}}

// 2. input_json_delta (当 arguments 非空时)
{"type": "content_block_delta", "index": N,
 "delta": {"type": "input_json_delta", "partial_json": "{\"file_path\":\"main.rs\"}"}}

// 3. content_block_stop (在新工具调用开始时，或 [DONE] 时)
{"type": "content_block_stop", "index": N}
```

### 6.3 完整状态机

MyGate 的 Anthropic 流式转换器是一个状态机，管理以下状态：

```
状态变量:
  block_index: usize      // 下一个 content block 的 index
  thinking_open: bool     // thinking 块是否打开
  text_open: bool         // text 块是否打开
  current_tc_block: Option<usize>  // 当前打开的 tool_use 块的 index
  had_blocks: bool        // 是否打开过任何块（防止空回复时漏发 text block）
  final_stop_reason: String  // 最终 stop_reason
```

转换逻辑（伪代码）：

```
for each SSE chunk from backend:
  delta = chunk.choices[0].delta

  if delta.reasoning_content:
    if current_tc_block open → close it
    if !thinking_open → open thinking block at index 0
    emit thinking_delta

  if delta.content:
    if current_tc_block open → close it, block_index = tc_block + 1
    if thinking_open → close thinking block, block_index = 1
    if !text_open → open text block at block_index
    emit text_delta

  if delta.tool_calls:
    for each tool_call:
      if id or name non-empty:        // 新工具调用
        if current_tc_block open → close it
        if thinking_open → close thinking, block_index = 1
        if text_open → close it, block_index += 1
        open tool_use block at block_index
        current_tc_block = block_index
        final_stop_reason = "tool_use"
      if arguments non-empty:         // 参数增量
        emit input_json_delta at current_tc_block index

  if chunk.finish_reason == "tool_calls":
    final_stop_reason = "tool_use"

on [DONE]:
  close any open blocks (tool, thinking, text)
  if !had_blocks → open+close empty text block (CC 需要至少一个 content block)
  emit message_delta with final_stop_reason
  emit message_stop
```

### 6.4 block_index 递增规则

这是最容易出错的细节。block_index 的递增时机：

| 事件 | block_index 变化 |
|---|---|
| 打开 thinking 块 | 使用 index 0（固定），block_index 不变 |
| 关闭 thinking 块，打开 text 块 | text block index = 1 |
| 关闭 text 块，打开 tool_use 块 | tool_use index = text_block_index + 1 |
| 关闭 tool_use 块，打开下一个 | block_index = closed_tc_index + 1 |
| 关闭 tool_use 块，打开 text 块 | block_index = closed_tc_index + 1 |

简化规则：**每个新块的 index = 上一个关闭的块的 index + 1**。

### 6.5 特殊边界情况

#### 空回复
后端返回的流中没有任何 content delta，只有 `[DONE]`。
- **必须**发送一个空的 text content block（open → close），否则 CC 会报错。
- 使用 `had_blocks` 标志判断：如果从未打开过任何块，在 [DONE] 时补发。

#### 只有 thinking，没有 text
推理模型可能只输出 reasoning_content，不输出普通 content。
- thinking 块正常打开/关闭
- `had_blocks = true`，所以 [DONE] 时不会补发空 text 块
- CC 能正确处理只有 thinking 的回复

#### 多个 tool_calls
后端可能一次返回多个工具调用（不同 index）。
- 每个 tool_call 需要独立的 `content_block_start` / `input_json_delta` / `content_block_stop`
- 关闭前一个 tool block 后再打开下一个

#### tool_call 中途切换到 text
后端可能在发送部分 tool_call arguments 后切换到普通 content。
- 先关闭当前 tool block
- 再打开/继续 text block
- `final_stop_reason` 设为最后出现的内容类型对应的 reason

---

## 7. 已知坑与经验总结

### 7.1 多 tool_result 必须拆分（最坑）

**问题**：Anthropic 将多个 `tool_result` 放在同一条 `user` 消息中。
**错误做法**：用 `map()` 一对一转换，所有 tool_result 被合并到一条 OpenAI 消息，`tool_call_id` 被覆盖。
**正确做法**：用 `flat_map()` 将每个 `tool_result` 拆分为独立的 `role: "tool"` 消息。

```rust
// ✗ 错误：map — 一条 Anthropic 消息 → 一条 OpenAI 消息
req.messages.iter().map(|msg| { ... }).collect()

// ✓ 正确：flat_map — 一条 Anthropic 消息 → 多条 OpenAI 消息
req.messages.iter().flat_map(|msg| { ... }).collect()
```

### 7.2 流式超时不能设全局

**问题**：`reqwest::RequestBuilder::timeout()` 限制的是整个请求的生命周期。
推理模型可能在开始输出前思考 30-60 秒，导致流被提前终止。

**正确做法**：
- 不对流式请求设 `.timeout()`
- 在 Client 上设 `connect_timeout`（仅限制 TCP 建连）
- 用 `tokio::time::timeout(duration, stream.next())` 做 per-chunk 超时

### 7.3 had_blocks 防止空 content

**问题**：[DONE] 时检查 `thinking_open || text_open || current_tc_block.is_some()` 判断是否需要补发空 text 块。
但如果 thinking 块已经打开又关闭了，这些标志会被重置，导致误判为"没有块被打开过"。

**正确做法**：用 `had_blocks` 标志记录"是否曾经打开过任何块"。

### 7.4 arguments vs input 的类型差异

- OpenAI `function.arguments`：**JSON 字符串**（`"{\"file_path\":\"main.rs\"}"`)
- Anthropic `tool_use.input`：**JSON 对象**（`{"file_path": "main.rs"}`)

转换时需要 `serde_json::from_str()` 或 `.to_string()`。

### 7.5 model 字段语义

- Anthropic 请求中 `model` 是 **alias 名**（如 `"Plan"`）
- 发给后端时替换为**真实模型名**（如 `"glm-5.1"`）
- 返回给 CC 时，`model` 字段填回 **alias 名**
- CC 用 `model` 字段匹配后续请求

### 7.6 tool_call_id 格式差异

- Anthropic 工具调用 ID 格式：`toolu_*`（如 `toolu_01ABC123`）
- OpenAI 工具调用 ID 格式：`call_*`（如 `call_abc123def`）
- 后端（GLM、DeepSeek）可能使用自己的格式
- MyGate **直接透传**后端返回的 ID，不做格式转换
- 只要 ID 在一次对话中保持唯一且能被 tool_result 正确引用即可

---

## 8. 快速参考表

### 请求方向映射速查

```
Anthropic                          OpenAI
─────────────────────────────────   ─────────────────────────────────
model: "Plan"                  →   model: "glm-5.1"
system: "..."                  →   messages[0]: {role: "system", ...}
messages[].role: "user"        →   messages[].role: "user"
messages[].role: "assistant"   →   messages[].role: "assistant"
content: "text"                →   content: "text"
content[].type: "text"         →   content: "text"
content[].type: "image"        →   content: [{type: "image_url", ...}]
content[].type: "tool_use"     →   tool_calls: [{type: "function", ...}]
content[].type: "tool_result"  →   {role: "tool", tool_call_id: "..."}
tools[].input_schema           →   tools[].function.parameters
stream: true                   →   stream: true
```

### 响应方向映射速查

```
OpenAI                             Anthropic
─────────────────────────────────   ─────────────────────────────────
choices[0].message.content     →   content[].type: "text"
choices[0].message.tool_calls  →   content[].type: "tool_use"
choices[0].finish_reason: "stop"      → stop_reason: "end_turn"
choices[0].finish_reason: "tool_calls"→ stop_reason: "tool_use"
choices[0].finish_reason: "length"    → stop_reason: "max_tokens"
usage.prompt_tokens            →   usage.input_tokens
usage.completion_tokens        →   usage.output_tokens
delta.content                  →   content_block_delta → text_delta
delta.reasoning_content        →   content_block_delta → thinking_delta
delta.tool_calls[].function    →   content_block_delta → input_json_delta
data: [DONE]                   →   message_delta + message_stop
```
