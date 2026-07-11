# MyGate

> LLM API 网关 — 多后端 fallback、协议互转、零侵入
> 单一二进制，OpenAI / Anthropic 双协议同时支持

## 一句话

```
客户端 (OpenAI 或 Anthropic 协议)
      ↓
  MyGate 解析 → 选后端 → 转换 → 转发
      ↓
任意后端 (OpenAI 兼容 / Anthropic 兼容)
```

## 特性

- **多协议北向**：同时接受 OpenAI Chat Completions 和 Anthropic Messages
- **多协议南向**：自动按 `provider_type` 选 OpenAI 兼容 / Anthropic 直通
- **跨协议调度**：OpenAI 客户端可走 Anthropic 后端，自动转 SSE 协议
- **fallback 链**：每个 alias 一个 provider 列表，按顺序试
- **南向基线化**：GLM / DeepSeek / MiniMax / Anthropic 官方契约
- **流式 / 工具调用 / thinking**：完整支持
- **可观测**：`/metrics` Prometheus 端点
- **可热重载**：`/admin/reload` + SIGHUP
- **零侵入部署**：单 binary, 无 DB

## 快速开始

### 1. 复制配置

```bash
cp config.example.toml config.toml
vi config.toml  # 填 API key
```

### 2. 启动

```bash
./target/release/mygate
# 或 debug 模式:
RUST_LOG=info,mygate=debug ./target/release/mygate
```

### 3. 测试

```bash
# 健康检查
curl http://127.0.0.1:8080/health

# 列 alias
curl http://127.0.0.1:8080/v1/models

# OpenAI 协议调用 (DeepSeek 后端, 通过 "Simple" alias)
curl -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model":"Simple",
    "messages":[{"role":"user","content":"hi"}],
    "stream":true
  }'

# Anthropic 协议调用 (MiniMax Anthropic 后端, 通过 "Plan" alias)
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model":"Plan",
    "max_tokens":100,
    "messages":[{"role":"user","content":"hi"}]
  }'
```

## 配置

```toml
[server]
host = "127.0.0.1"           # 监听地址
port = 8080                  # 监听端口
timeout_seconds = 30         # 非流式超时 (秒)
admin_token = "your-secret"  # /admin/reload 鉴权 token (None = 端点禁用)

# === Provider 定义 (南向后端) ===
[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "sk-..."
provider_type = "openai"      # "openai" | "anthropic"
auth_style = "bearer"        # "bearer" | "anthropic"

[providers.minimax-anthropic]
base_url = "https://api.minimaxi.com/anthropic"  # MiniMax 的 Anthropic 兼容端
api_key = "sk-..."
provider_type = "anthropic"
auth_style = "bearer"        # 注意: MiniMax 用 Bearer, 不是 x-api-key

# === Alias 定义 (客户端调用入口) ===
[aliases.Simple]
[[aliases.Simple.chain]]
provider = "deepseek"
model = "deepseek-chat"       # 后端实际模型名
priority = 1                 # 多个 provider 时按 priority 顺序试
[[aliases.Simple.chain]]
provider = "minimax-openai"
model = "MiniMax-M3"
priority = 2
```

## 端点

| Method | Path | 说明 |
|---|---|---|
| GET | `/health` | 健康检查 (`ok`) |
| GET | `/v1/models` | 列出所有 alias (OpenAI 格式) |
| POST | `/v1/chat/completions` | OpenAI Chat Completions 入口 |
| POST | `/v1/messages` | Anthropic Messages 入口 |
| POST | `/admin/reload` | 重载配置 (需 `X-Admin-Token` header) |
| GET | `/metrics` | Prometheus 指标 |

## 跨协议调度

```
OpenAI 客户端  →  /v1/chat/completions  →  alias (e.g. "Plan")
                                              →  provider (minimax-anthropic, provider_type=anthropic)
                                              →  POST /v1/messages
                                              →  Anthropic SSE 响应
                                              →  MyGate 转 OpenAI SSE 给客户端
```

`provider_type` 决定南向协议，`alias` 决定哪个 providers 列表，`priority` 决定 fallback 顺序。

## 协议转换 (P0-2)

OpenAI 北向 → Anthropic 南向：自动按 chunk 检测协议 (data 字段含 `message_start` → Anthropic) 并转 OpenAI SSE (delta.content / tool_calls / finish_reason)。

MiniMax 实测：其 `/anthropic/v1/messages` 端**返回 OpenAI 格式**（不是真 Anthropic 格式），但 MyGate 透传对客户端也工作（OpenAI 客户端能解析）。

## 项目结构

```
src/
├── main.rs              # 入口 (启动 + SIGHUP + 优雅关闭)
├── config.rs            # 配置解析 + 校验
├── server.rs            # 路由注册
├── error.rs              # 错误类型
├── metrics.rs            # Prometheus 指标
├── backend/
│   ├── mod.rs           # BackendAdapter trait + factory
│   ├── openai_compat.rs # OpenAI 兼容适配器 (非流 + 流)
│   └── anthropic_passthrough.rs # Anthropic 直通适配器
├── core/
│   ├── alias.rs         # 别名 → fallback 链
│   ├── fallback.rs      # fallback 调度
│   └── types.rs         # InternalRequest / InternalResponse
└── router/
    ├── openai.rs        # OpenAI 协议入口
    └── anthropic.rs     # Anthropic 协议入口

tests/
├── l4_e2e.sh            # L4 端到端集成测试 (23 场景)
└── conformance_*.rs     # 契约测试 (MockBackend)
```

## 开发

### 构建

```bash
cargo build --release
```

### 测试

```bash
# 单元 + 契约测试
cargo test

# L4 端到端 (用真实 DeepSeek + MiniMax key)
./tests/l4_e2e.sh
```

### 关键设计文档 (veps/)

- `veps/cr-300-northbound-api-standard.md` — 北向契约 (OpenAI + Anthropic)
- `veps/cr-301-conformance-test-framework.md` — 测试框架
- `veps/cr-400-southbound-baseline.md` — 南向基线
- `veps/cr-410-southbound-baseline.md` — 3 provider 实测
- `veps/southbound/{glm,deepseek,minimax,...}/spec.md` — 各 provider 规格
- `veps/contract/openai/{glm,deepseek,minimax}/schema.json` — 机器可读契约
- `TODO.md` — 待办列表

## 已测试 provider

| Provider | 端点 | 类型 | 状态 |
|---|---|---|---|
| DeepSeek | `/v1/chat/completions` | openai | ✅ L4 |
| MiniMax | `/v1/chat/completions` | openai | ✅ L4 |
| MiniMax | `/anthropic/v1/messages` | anthropic | ✅ L4 |

(其他待 L4: GLM, Anthropic, vLLM, SGLang — 见 `veps/southbound/*/spec.md`)

## License

Mulan
