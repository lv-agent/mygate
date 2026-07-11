#!/usr/bin/env bash
# 启动 Claude Code 走 MyGate 网关
# CC 不需要任何 API key, 所有 key 在 MyGate 上
# 用法: ./run_cc.sh Simple  或  ./run_cc.sh Plan

set -euo pipefail

MYGATE_URL="${MYGATE_URL:-http://127.0.0.1:8080}"
ALIAS="${1:-Plan}"  # 默认 Plan (推理 + thinking)

# 验证 MyGate 在跑
if ! curl -sf "$MYGATE_URL/health" > /dev/null; then
    echo "❌ MyGate 不在跑: $MYGATE_URL"
    echo "  先启动: cd $(dirname $0) && cargo build --release && MYGATE_CONFIG=$(pwd)/config.toml ./target/release/mygate &"
    exit 1
fi

# 验证 alias 存在
ALIASES=$(curl -s "$MYGATE_URL/v1/models" | python3 -c "import json,sys;print(','.join(m['id'] for m in json.load(sys.stdin)['data']))")
if [[ ",$ALIASES," != *",$ALIAS,"* ]]; then
    echo "❌ Alias '$ALIAS' 不存在, 可用: $ALIASES"
    exit 1
fi

echo "✅ MyGate 在跑, $MYGATE_URL"
echo "✅ Alias '$ALIAS' 可用"
echo ""

# 启动 CC. CC 默认会从 ~/.claude/settings.json 或环境变量读 ANTHROPIC_BASE_URL
# 我们覆盖关键环境变量. 注意: CC 仍会尝试读 ANTHROPIC_AUTH_TOKEN, 但 MyGate 不验证, 随便设.
export ANTHROPIC_BASE_URL="$MYGATE_URL"
export ANTHROPIC_AUTH_TOKEN="mygate-does-not-validate-token"
# 关键: 告诉 CC 用 alias 名称作为 model 名
export ANTHROPIC_MODEL="$ALIAS"

echo "🚀 启动 Claude Code, model=$ALIAS (后端: MyGate 路由)"
echo ""

cd "${2:-$(pwd)}"
exec claude "$@"
