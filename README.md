# MyGate

轻量级 LLM API 网关，用 Rust 编写。支持模型别名路由、多级自动降级（fallback）、同时兼容 OpenAI 与 Anthropic API 协议。

## 特性

- **模型别名路由** — 用语义化别名（如 `Simple`、`Code`、`Plan`）替代真实模型名，解耦调用方与底层模型
- **多级自动降级** — 每个别名配置按优先级排列的后端链路，某节点失败时自动切换到下一个
- **双协议兼容** — 同时提供 OpenAI `/v1/chat/completions` 和 Anthropic `/v1/messages` 端点，内部统一转换
- **流式（SSE）与非流式** — 两种模式均完整支持，流式模式包含 per-chunk 超时保护
- **工具调用（Function Calling）** — 完整支持 Anthropic `tool_use`/`tool_result` 与 OpenAI `tool_calls` 的双向映射
- **推理/思考内容** — 支持后端推理模型（如 GLM-5.1、DeepSeek-R1）的 `reasoning_content`，转换为 Anthropic `thinking` 块
- **配置热重载** — 通过 `SIGHUP` 信号或 `POST /admin/reload` 端点无中断重载配置
- **Claude Code 就绪** — 可直接作为 Claude Code 的后端代理使用

## 架构概览

```
客户端 (Claude Code / 任意 OpenAI 客户端)
        │
        ▼
   ┌──────────┐
   │  MyGate  │  ← 别名解析 + 协议转换 + 降级调度
   └────┬─────┘
        │  fallback chain
   ┌────┼────┬─────────┐
   ▼    ▼    ▼         ▼
 GLM  DeepSeek MiniMax  ...（任意 OpenAI 兼容后端）
```

核心数据流：

1. 客户端请求到达（OpenAI 或 Anthropic 格式）
2. 解析为内部统一格式 `InternalRequest`
3. 通过别名解析得到降级链路（按 priority 排序）
4. 逐个尝试后端，首个成功即返回（429 / 5xx 自动降级）
5. 将后端响应转换为客户端所需的协议格式返回

## 快速开始

### 编译

```bash
# Debug 构建
./build.sh

# Release 构建
./build.sh release
```

构建产物输出到 `dist/` 目录，可直接拷贝到目标机器运行。

### 配置

复制示例配置并编辑：

```bash
cp config.example.toml config.toml
```

配置文件说明（`config.toml`）：

```toml
[server]
host = "127.0.0.1"
port = 8080
timeout_seconds = 30        # 非流式请求超时（流式使用 per-chunk 超时）

# 定义后端服务提供商
[providers.glm]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = "your-api-key"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "your-api-key"

# 定义模型别名及降级链路
[aliases.Simple]
description = "简单任务，使用低成本模型"
[[aliases.Simple.chain]]
provider = "deepseek"       # 引用 providers 中的名称
model = "deepseek-v4-flash" # 真实模型名
priority = 1                # 优先级，数字越小越优先

[[aliases.Simple.chain]]
provider = "glm"
model = "glm-4-flash"
priority = 2                # deepseek 不可用时自动降级到此

[aliases.Code]
description = "代码任务"
[[aliases.Code.chain]]
provider = "deepseek"
model = "deepseek-v4-pro"
priority = 1

[[aliases.Code.chain]]
provider = "glm"
model = "glm-5.1"
priority = 2

[aliases.Plan]
description = "规划 / 推理任务"
[[aliases.Plan.chain]]
provider = "glm"
model = "glm-5.1"
priority = 1

[[aliases.Plan.chain]]
provider = "deepseek"
model = "deepseek-v4-pro"
priority = 2
```

### 启动

```bash
# 直接运行
./mygate

# 指定配置文件路径
MYGATE_CONFIG=/path/to/config.toml ./mygate
```

### 部署

使用构建脚本生成部署包：

```bash
./build.sh release
# 将 dist/ 目录拷贝到目标机器
# 编辑 config.toml，运行 ./run.sh
```

## API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| `POST` | `/v1/chat/completions` | OpenAI 兼容的聊天补全接口 |
| `POST` | `/v1/messages` | Anthropic 兼容的 Messages 接口 |
| `GET` | `/v1/models` | 列出所有可用别名（OpenAI 模型列表格式） |
| `POST` | `/admin/reload` | 热重载配置文件 |
| `GET` | `/health` | 健康检查 |

## 与 Claude Code 配合使用

MyGate 可直接作为 Claude Code 的后端代理。配置环境变量：

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_API_KEY=any-non-empty-string   # MyGate 不校验 API Key
```

然后在 Claude Code 中使用配置的别名作为模型名（如 `Plan`、`Code`、`Simple`）。

MyGate 会自动处理：
- Anthropic ↔ OpenAI 协议的完整双向转换
- `system` 字段（字符串/数组形式）的解析
- `tool_use` / `tool_result` 内容块的拆分与映射
- 推理模型的 `thinking` 块（`reasoning_content` → `thinking_delta`）
- 流式响应的 SSE 事件转换（`message_start`、`content_block_start/delta/stop`、`message_delta/stop`）
- 多个 `tool_result` 在同一 `user` 消息中的正确拆分

## 降级策略

当后端返回以下错误时，MyGate 自动尝试链路中的下一个后端：

- HTTP 429（限流）
- HTTP 500-599（服务端错误）

所有后端均失败时返回 HTTP 503 及错误信息。

## 配置热重载

支持两种方式：

```bash
# 方式一：发送 SIGHUP 信号
kill -HUP <pid>

# 方式二：HTTP 端点
curl -X POST http://127.0.0.1:8080/admin/reload
```

重载时会校验新配置的合法性，校验失败不影响当前运行配置。

## 项目结构

```
src/
├── main.rs                 # 入口：启动服务、SIGHUP 处理、优雅关闭
├── config.rs               # 配置文件解析与校验
├── server.rs               # 路由注册
├── error.rs                # 错误类型与降级判断
├── router/
│   ├── openai.rs           # OpenAI 协议路由（请求解析、响应转换、SSE 流）
│   └── anthropic.rs        # Anthropic 协议路由（请求解析、响应转换、SSE 流）
├── backend/
│   └── openai_compat.rs    # OpenAI 兼容后端（请求发送、响应解析、SSE 解析）
└── core/
    ├── types.rs            # 内部统一类型（InternalRequest / InternalResponse）
    ├── alias.rs            # 别名解析（alias → 降级链路）
    └── fallback.rs         # 降级执行逻辑（逐个尝试后端）
```

## 技术栈

- **语言**：Rust
- **Web 框架**：Axum
- **异步运行时**：Tokio
- **HTTP 客户端**：Reqwest
- **序列化**：Serde + serde_json + toml
- **日志**：tracing + tracing-subscriber

## 许可证

MIT
