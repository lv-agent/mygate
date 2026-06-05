# Claude Code ↔ MyGate 接口规范

> 本文档描述 Claude Code (CC) 与 MyGate 网关之间的完整 API 交互协议。
> 基于 CC 实际行为 + MyGate (`src/router/anthropic.rs`) 实现整理。

---

## 1. 连接配置

CC 通过 `ANTHROPIC_BASE_URL` 和 `ANTHROPIC_API_KEY` 环境变量连接 MyGate：

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:16380
export ANTHROPIC_API_KEY=any-non-empty-string   # MyGate 不校验 key
```

CC 发出的所有请求都走 `/v1/messages` 端点，使用 Anthropic Messages API 格式。

---

## 2. 请求格式

### 2.1 端点

```
POST /v1/messages
Content-Type: application/json
```

### 2.2 请求体结构

```jsonc
{
  "model": "Plan",              // MyGate alias name，不是真实模型名
  "stream": true,               // CC 始终使用流式
  "max_tokens": 16384,          // 必填，CC 会设置
  "temperature": 1.0,           // 可选
  "system": "...",              // 系统提示词，见 §2.3
  "messages": [...],            // 对话历史，见 §2.4
  "tools": [...]                // 工具定义，见 §2.5
}
```

### 2.3 system 字段

CC 发送的 `system` 字段有两种形式：

**形式 A：纯字符串**
```json
{
  "system": "You are Claude, an AI assistant by Anthropic..."
}
```

**形式 B：数组（带 cache_control）**
```json
{
  "system": [
    {
      "type": "text",
      "text": "You are Claude...",
      "cache_control": {"type": "ephemeral"}
    }
  ]
}
```

MyGate 处理：数组形式会拼接所有 `text` 块（用 `\n` 连接），转换为内部 `role=system` 消息。
`cache_control` 字段被忽略（MyGate 不做缓存控制）。

### 2.4 messages 数组

#### 2.4.1 普通文本消息

```json
{
  "role": "user",
  "content": "Hello, how are you?"
}
```

content 可以是字符串或包含 text block 的数组：
```json
{
  "role": "user",
  "content": [
    {"type": "text", "text": "What's in this image?"},
    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "..."}}
  ]
}
```

#### 2.4.2 assistant 消息（含工具调用）

当 assistant 调用了工具，CC 会在后续对话中发送 assistant 的历史回复：

```json
{
  "role": "assistant",
  "content": [
    {"type": "text", "text": "I'll read that file for you."},
    {
      "type": "tool_use",
      "id": "toolu_01ABC123",
      "name": "Read",
      "input": {"file_path": "/home/user/src/main.rs"}
    }
  ]
}
```

关键点：
- `content` 始终是数组形式（当包含 tool_use 时）
- `id` 是工具调用的唯一标识，后续 tool_result 需要引用此 id
- `input` 是 JSON 对象（工具参数）
- **一个 assistant 消息可以包含多个 tool_use 块**（CC 可能一次调用多个工具）

#### 2.4.3 user 消息（含工具结果）

CC 将工具执行结果作为 `role=user` 消息发送，内容为 `tool_result` 块：

```json
{
  "role": "user",
  "content": [
    {
      "type": "tool_result",
      "tool_use_id": "toolu_01ABC123",
      "content": "file content here..."
    },
    {
      "type": "tool_result",
      "tool_use_id": "toolu_01DEF456",
      "content": "another result..."
    }
  ]
}
```

**⚠️ 关键陷阱**：一条 user 消息中可以包含**多个** `tool_result` 块，每个引用不同的 `tool_use_id`。
转换为 OpenAI 格式时，每个 tool_result **必须**拆分为独立的 `role: "tool"` 消息。
（这是 MyGate 开发过程中踩过的坑，参见 `openai_compat.rs` 中的 `flat_map` 处理。）

#### 2.4.4 对话历史结构示例

一个完整的包含工具调用的对话流：

```jsonc
[
  // 1. 用户提问
  {"role": "user", "content": "Read main.rs and fix the bug"},

  // 2. AI 回复（含工具调用）
  {
    "role": "assistant",
    "content": [
      {"type": "text", "text": "I'll read the file first."},
      {"type": "tool_use", "id": "toolu_01A", "name": "Read", "input": {"file_path": "main.rs"}},
      {"type": "tool_use", "id": "toolu_01B", "name": "Bash", "input": {"command": "ls -la"}}
    ]
  },

  // 3. 工具结果（可能合并为一条 user 消息）
  {
    "role": "user",
    "content": [
      {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "fn main() {...}"},
      {"type": "tool_result", "tool_use_id": "toolu_01B", "content": "total 42..."}
    ]
  },

  // 4. AI 继续回复（可能再次调用工具或给出最终答案）
  {
    "role": "assistant",
    "content": "I found the bug..."
  }
]
```

### 2.5 tools 数组

CC 发送的每个工具定义：

```json
{
  "name": "Read",
  "description": "Reads a file from the filesystem...",
  "input_schema": {
    "type": "object",
    "properties": {
      "file_path": {
        "type": "string",
        "description": "The absolute path to the file"
      }
    },
    "required": ["file_path"]
  }
}
```

映射到 OpenAI 格式：
```json
{
  "type": "function",
  "function": {
    "name": "Read",
    "description": "Reads a file from the filesystem...",
    "parameters": { /* 同 input_schema */ }
  }
}
```

CC 使用的核心工具列表（非完整，但最常见）：
- `Read` — 读取文件
- `Write` — 写入文件
- `Edit` — 编辑文件
- `Bash` — 执行 shell 命令
- `Agent` — 启动子代理
- `WebSearch` — 网络搜索
- `TaskCreate` / `TaskUpdate` / `TaskList` — 任务管理

---

## 3. 流式响应格式

CC 使用 `stream: true`，MyGate 必须返回 SSE (Server-Sent Events) 格式的流式响应。

### 3.1 SSE 事件序列

完整的流式响应事件序列：

```
event: message_start
data: {"type":"message_start","message":{...}}

event: content_block_start    // 可能多个
data: {"type":"content_block_start","index":0,"content_block":{...}}

event: content_block_delta    // 大量重复
data: {"type":"content_block_delta","index":0,"delta":{...}}

event: content_block_stop     // 配对 content_block_start
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"..."},"usage":{...}}

event: message_stop
data: {"type":"message_stop"}
```

### 3.2 message_start

```json
{
  "type": "message_start",
  "message": {
    "id": "msg_01XYZ",
    "type": "message",
    "role": "assistant",
    "content": [],
    "model": "Plan",
    "stop_reason": null,
    "stop_sequence": null,
    "usage": {"input_tokens": 0, "output_tokens": 0}
  }
}
```

- `model` 字段：MyGate 填入 alias 名（如 `"Plan"`），不是后端真实模型名
- `id`：唯一消息 ID，整个流式响应中保持不变

### 3.3 content_block_start

支持三种内容块类型：

#### thinking 块（推理模型）
```json
{
  "type": "content_block_start",
  "index": 0,
  "content_block": {
    "type": "thinking",
    "thinking": ""
  }
}
```
- 思考内容，对应后端推理模型（如 GLM-5.1）的 `reasoning_content` 字段
- 始终在 index 0

#### text 块
```json
{
  "type": "content_block_start",
  "index": 1,
  "content_block": {
    "type": "text",
    "text": ""
  }
}
```
- 文本回复
- 如果前面有 thinking 块，index 从 1 开始；否则从 0 开始

#### tool_use 块
```json
{
  "type": "content_block_start",
  "index": 2,
  "content_block": {
    "type": "tool_use",
    "id": "toolu_01ABC123",
    "name": "Read",
    "input": {}
  }
}
```
- 工具调用，id 必须全局唯一
- name 是工具名称
- input 初始为空对象，后续通过 `input_json_delta` 增量填充

### 3.4 content_block_delta

#### thinking_delta
```json
{
  "type": "content_block_delta",
  "index": 0,
  "delta": {
    "type": "thinking_delta",
    "thinking": "partial thinking text..."
  }
}
```

#### text_delta
```json
{
  "type": "content_block_delta",
  "index": 1,
  "delta": {
    "type": "text_delta",
    "text": "partial response text..."
  }
}
```

#### input_json_delta（工具参数增量）
```json
{
  "type": "content_block_delta",
  "index": 2,
  "delta": {
    "type": "input_json_delta",
    "partial_json": "{\"file_path\":\"/home/"
  }
}
```
- `partial_json` 是 JSON 字符串的片段
- CC 会拼接所有 `partial_json` 片段，最终解析为完整 JSON 对象
- **index 必须与对应的 content_block_start 的 index 一致**

### 3.5 content_block_stop

```json
{
  "type": "content_block_stop",
  "index": 2
}
```
- 关闭对应 index 的内容块
- 每个 content_block_start 必须有配对的 content_block_stop

### 3.6 message_delta

```json
{
  "type": "message_delta",
  "delta": {
    "stop_reason": "end_turn",
    "stop_sequence": null
  },
  "usage": {
    "output_tokens": 42
  }
}
```

stop_reason 取值：
| stop_reason | 含义 |
|---|---|
| `"end_turn"` | 正常结束，文本回复完成 |
| `"tool_use"` | AI 请求调用工具 |
| `"max_tokens"` | 达到 max_tokens 限制 |
| `"stop_sequence"` | 遇到停止序列（罕见） |

### 3.7 message_stop

```json
{"type": "message_stop"}
```

### 3.8 完整事件流示例

#### 示例 A：纯文本回复（推理模型）

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_001","type":"message","role":"assistant","content":[],"model":"Plan","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me analyze..."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" the code structure..."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"I found the issue."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":150}}

event: message_stop
data: {"type":"message_stop"}
```

#### 示例 B：工具调用

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_002","type":"message","role":"assistant","content":[],"model":"Plan","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I'll read that file."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01A2B3C","name":"Read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/home/user/main.rs\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":25}}

event: message_stop
data: {"type":"message_stop"}
```

---

## 4. 非流式响应格式

虽然 CC 主要使用流式，但 MyGate 也支持非流式响应：

```json
{
  "id": "msg_003",
  "type": "message",
  "role": "assistant",
  "model": "Plan",
  "content": [
    {"type": "text", "text": "Hello! How can I help?"},
    {
      "type": "tool_use",
      "id": "toolu_01XYZ",
      "name": "Read",
      "input": {"file_path": "/home/user/main.rs"}
    }
  ],
  "usage": {"input_tokens": 100, "output_tokens": 50},
  "stop_reason": "tool_use"
}
```

---

## 5. 错误响应格式

MyGate 返回的 HTTP 错误格式：

```json
{
  "error": {
    "message": "descriptive error message",
    "type": "gateway_error"
  }
}
```

HTTP 状态码：
| 状态码 | 含义 |
|---|---|
| 404 | 未知 model alias |
| 502 | 后端返回错误 |
| 503 | 所有后端均失败 |
| 500 | 内部错误 |

CC 对错误的处理：如果非流式请求返回错误，CC 会重试。如果流式中断，CC 会尝试重新发送请求。

---

## 6. 超时与连接管理

### 6.1 CC 侧行为
- CC 的请求可能持续数分钟（推理模型 + 长对话 + 工具调用循环）
- CC 会在收到 tool_use 后暂停流，执行工具，然后发送新的请求（包含完整对话历史）

### 6.2 MyGate 侧行为
- **连接超时**：30 秒（`connect_timeout`），仅限制 TCP 建连
- **流式传输无全局超时**：不设置 `request.timeout()`，避免长推理被截断
- **Per-chunk 超时**：60 秒无新数据则判定后端卡死，断开流
- **连接池**：`pool_idle_timeout=60s`，`tcp_keepalive=30s`

### 6.3 关键设计决策
- **不要对流式请求设置全局 timeout**：推理模型（GLM-5.1、DeepSeek-R1）可能在开始输出前思考数十秒
- **Per-chunk timeout 足够长**：60 秒，避免推理过程中的正常停顿被误判
- **连接池 keepalive 必须开启**：CC 高频发送请求，复用连接可减少延迟

---

## 7. CC 与 MyGate 的完整交互时序

```
CC                                 MyGate                          Backend (GLM-5.1)
 │                                    │                                │
 │  POST /v1/messages                 │                                │
 │  {model:"Plan", stream:true,       │                                │
 │   messages:[...], tools:[...]}     │                                │
 │───────────────────────────────────>│                                │
 │                                    │  POST /chat/completions        │
 │                                    │  {model:"glm-5.1", stream:true}│
 │                                    │───────────────────────────────>│
 │                                    │                                │
 │  SSE: message_start               │  SSE: data: {...}             │
 │  SSE: content_block_start(think)  │<───────────────────────────────│
 │  SSE: thinking_delta...           │                                │
 │  SSE: content_block_stop          │                                │
 │  SSE: content_block_start(text)   │                                │
 │  SSE: text_delta...               │                                │
 │  SSE: content_block_stop          │                                │
 │  SSE: message_delta(stop=end)     │  SSE: data: [DONE]            │
 │  SSE: message_stop                │<───────────────────────────────│
 │<───────────────────────────────────│                                │
 │                                    │                                │
 │  [CC processes response]           │                                │
 │                                    │                                │
 │  POST /v1/messages                 │                                │
 │  {model:"Plan", stream:true,       │                                │
 │   messages:[                       │                                │
 │     previous messages +            │                                │
 │     assistant tool_use +           │                                │
 │     user tool_result               │                                │
 │   ]}                               │                                │
 │───────────────────────────────────>│                                │
 │                                    │  [fallback chain applies]      │
 │                                    │───────────────────────────────>│
 │  ...stream...                      │                                │
```

---

## 8. 实现检查清单

实现一个新的 Anthropic 协议网关时，确保以下要点：

- [ ] `/v1/messages` 端点接受 POST
- [ ] 正确解析 `system` 字段（字符串或数组）
- [ ] 正确解析 `messages` 中的 `tool_use` 和 `tool_result` 内容块
- [ ] 将 Anthropic 工具定义格式（`input_schema`）转换为目标后端格式
- [ ] 流式响应使用 SSE，事件类型包括：`message_start`、`content_block_start`、`content_block_delta`、`content_block_stop`、`message_delta`、`message_stop`
- [ ] 正确处理推理模型的 thinking 块（`reasoning_content` → `thinking_delta`）
- [ ] 正确处理工具调用的增量参数（`tool_calls.function.arguments` → `input_json_delta`）
- [ ] `content_block_start/stop` 的 index 正确递增
- [ ] 流式传输不设全局超时，使用 per-chunk 超时检测卡死
- [ ] 多个 `tool_result` 在同一条 user 消息时，拆分为独立的后端消息
- [ ] `stop_reason` 正确映射：OpenAI `stop` → Anthropic `end_turn`，OpenAI `tool_calls` → Anthropic `tool_use`
