# MyGate

A lightweight LLM API gateway written in Rust. Supports model alias routing, multi-level automatic fallback, and is compatible with both OpenAI and Anthropic API protocols.

## Features

- **Model Alias Routing** — Use semantic aliases (e.g., `Simple`, `Code`, `Plan`) instead of real model names, decoupling callers from underlying models
- **Multi-level Automatic Fallback** — Each alias configures a priority-ordered backend chain; automatically switches to the next provider on failure
- **Dual Protocol Compatibility** — Provides both OpenAI `/v1/chat/completions` and Anthropic `/v1/messages` endpoints with internal unified conversion
- **Streaming (SSE) & Non-streaming** — Both modes fully supported, with per-chunk timeout protection for streaming
- **Function Calling** — Full bidirectional mapping between Anthropic `tool_use`/`tool_result` and OpenAI `tool_calls`
- **Reasoning / Thinking Content** — Supports backend reasoning models (e.g., GLM-5.1, DeepSeek-R1) `reasoning_content`, converted to Anthropic `thinking` blocks
- **Hot Config Reload** — Reload configuration without downtime via `SIGHUP` signal or `POST /admin/reload` endpoint
- **Claude Code Ready** — Can be used directly as a backend proxy for Claude Code

## Architecture Overview

```
Client (Claude Code / any OpenAI client)
        │
        ▼
   ┌──────────┐
   │  MyGate  │  ← alias resolution + protocol conversion + fallback dispatch
   └────┬─────┘
        │  fallback chain
   ┌────┼────┬─────────┐
   ▼    ▼    ▼         ▼
 GLM  DeepSeek MiniMax  ... (any OpenAI-compatible backend)
```

Core data flow:

1. Client request arrives (OpenAI or Anthropic format)
2. Parsed into unified internal format (`InternalRequest`)
3. Alias resolution produces a fallback chain (sorted by priority)
4. Backends are tried sequentially; first success is returned (429 / 5xx triggers fallback)
5. Backend response is converted to the client's expected protocol format

## Quick Start

### Build

```bash
# Debug build
./build.sh

# Release build
./build.sh release
```

Build artifacts are output to the `dist/` directory and can be copied directly to the target machine.

### Configuration

Copy the example config and edit:

```bash
cp config.example.toml config.toml
```

Configuration file reference (`config.toml`):

```toml
[server]
host = "127.0.0.1"
port = 8080
timeout_seconds = 30        # non-streaming request timeout (streaming uses per-chunk timeout)

# Define backend providers
[providers.glm]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = "your-api-key"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "your-api-key"

# Define model aliases and fallback chains
[aliases.Simple]
description = "Simple tasks, cheap model"
[[aliases.Simple.chain]]
provider = "deepseek"        # references a provider name from [providers]
model = "deepseek-v4-flash"  # real model name
priority = 1                 # lower number = higher priority

[[aliases.Simple.chain]]
provider = "glm"
model = "glm-4-flash"
priority = 2                 # fallback to this when deepseek is unavailable

[aliases.Code]
description = "Code tasks"
[[aliases.Code.chain]]
provider = "deepseek"
model = "deepseek-v4-pro"
priority = 1

[[aliases.Code.chain]]
provider = "glm"
model = "glm-5.1"
priority = 2

[aliases.Plan]
description = "Planning / reasoning tasks"
[[aliases.Plan.chain]]
provider = "glm"
model = "glm-5.1"
priority = 1

[[aliases.Plan.chain]]
provider = "deepseek"
model = "deepseek-v4-pro"
priority = 2
```

### Run

```bash
# Run directly
./mygate

# Specify config file path
MYGATE_CONFIG=/path/to/config.toml ./mygate
```

### Deploy

Use the build script to generate a deployment package:

```bash
./build.sh release
# Copy the dist/ directory to the target machine
# Edit config.toml, then run ./run.sh
```

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/chat/completions` | OpenAI-compatible chat completions |
| `POST` | `/v1/messages` | Anthropic-compatible messages |
| `GET` | `/v1/models` | List all available aliases (OpenAI model list format) |
| `POST` | `/admin/reload` | Hot-reload configuration file |
| `GET` | `/health` | Health check |

## Using with Claude Code

MyGate can serve as a direct backend proxy for Claude Code. Set the environment variables:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_API_KEY=any-non-empty-string   # MyGate does not validate API keys
```

Then use the configured alias names (e.g., `Plan`, `Code`, `Simple`) as model names in Claude Code.

MyGate automatically handles:
- Complete bidirectional conversion between Anthropic ↔ OpenAI protocols
- Parsing of the `system` field (string or array form)
- Splitting and mapping of `tool_use` / `tool_result` content blocks
- `thinking` blocks from reasoning models (`reasoning_content` → `thinking_delta`)
- SSE event conversion for streaming responses (`message_start`, `content_block_start/delta/stop`, `message_delta/stop`)
- Correct splitting of multiple `tool_result` blocks within a single `user` message

## Fallback Strategy

MyGate automatically tries the next backend in the chain when the current one returns:

- HTTP 429 (rate limiting)
- HTTP 500–599 (server errors)

When all backends fail, HTTP 503 is returned along with an error message.

## Hot Config Reload

Two methods are supported:

```bash
# Method 1: Send SIGHUP signal
kill -HUP <pid>

# Method 2: HTTP endpoint
curl -X POST http://127.0.0.1:8080/admin/reload
```

The new configuration is validated before applying; a validation failure does not affect the running configuration.

## Project Structure

```
src/
├── main.rs                 # Entry point: server startup, SIGHUP handler, graceful shutdown
├── config.rs               # Configuration file parsing and validation
├── server.rs               # Route registration
├── error.rs                # Error types and fallback decision logic
├── router/
│   ├── openai.rs           # OpenAI protocol route (request parsing, response conversion, SSE stream)
│   └── anthropic.rs        # Anthropic protocol route (request parsing, response conversion, SSE stream)
├── backend/
│   └── openai_compat.rs    # OpenAI-compatible backend (request sending, response parsing, SSE parsing)
└── core/
    ├── types.rs            # Internal unified types (InternalRequest / InternalResponse)
    ├── alias.rs            # Alias resolution (alias → fallback chain)
    └── fallback.rs         # Fallback execution logic (try backends sequentially)
```

## Tech Stack

- **Language**: Rust
- **Web Framework**: Axum
- **Async Runtime**: Tokio
- **HTTP Client**: Reqwest
- **Serialization**: Serde + serde_json + toml
- **Logging**: tracing + tracing-subscriber

## License

Mulan
