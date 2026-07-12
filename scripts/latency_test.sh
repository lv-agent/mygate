#!/usr/bin/env bash
# MyGate 时延对比测试
# 路径 A: CC 协议 (Anthropic Messages) → MyGate → MiniMax Anthropic
# 路径 B: CC 协议 (Anthropic Messages) → MiniMax Anthropic 直连
# 测量：首字节时间 (TTFB) + 总耗时
set -euo pipefail

CONFIG_FILE="/home/lvtao/lv/mygate/dist/config.toml"
API_KEY=$(grep -A 10 'minimax-anthropic' "$CONFIG_FILE" | grep 'api_key' | sed 's/api_key = "\(.*\)"/\1/')
MINIMAX_DIRECT="https://api.minimaxi.com/anthropic/v1/messages"
MYGATE_URL="http://127.0.0.1:8080/v1/messages"
ITERATIONS=${1:-3}

PROMPT='Explain TCP in three sentences.'

echo "=== MyGate 时延对比测试 (Anthropic 协议) ==="
echo "路径A: curl → MyGate(127.0.0.1:8080) → MiniMax Anthropic"
echo "路径B: curl → MiniMax Anthropic 直连"
echo "模型: MiniMax-M3 (via provider_type=anthropic)"
echo "问题: $PROMPT"
echo "迭代: $ITERATIONS 轮"
echo ""

echo "--- 先做一轮热身 (warmup) ---"
curl -s -o /dev/null -X POST "$MYGATE_URL" \
    -H "Content-Type: application/json" -H "x-api-key: mygate" \
    -H "anthropic-version: 2023-06-01" \
    -d "{\"model\":\"Plan\",\"max_tokens\":50,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}" 2>&1 || true

curl -s -o /dev/null -X POST "$MINIMAX_DIRECT" \
    -H "Content-Type: application/json" -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -d "{\"model\":\"MiniMax-M3\",\"max_tokens\":50,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}" 2>&1 || true
sleep 2
echo "热身完成"
echo ""

declare -a mygate_ttfb mygate_total direct_ttfb direct_total

for ((i=1; i<=ITERATIONS; i++)); do
    echo "--- 第 $i 轮 ---"

    # 路径 A: 经 MyGate
    RESULT_A=$(curl -s -o /dev/null -w "TTFB=%{time_starttransfer}\nTotal=%{time_total}\nHTTP=%{http_code}" \
        -X POST "$MYGATE_URL" \
        -H "Content-Type: application/json" \
        -H "x-api-key: mygate" \
        -H "anthropic-version: 2023-06-01" \
        -d "{\"model\":\"latency-test\",\"max_tokens\":150,\"messages\":[{\"role\":\"user\",\"content\":\"$PROMPT\"}],\"temperature\":0}" \
        2>&1)
    echo "  MyGate:  $RESULT_A"

    # 路径 B: 直连 MiniMax
    RESULT_B=$(curl -s -o /dev/null -w "TTFB=%{time_starttransfer}\nTotal=%{time_total}\nHTTP=%{http_code}" \
        -X POST "$MINIMAX_DIRECT" \
        -H "Content-Type: application/json" \
        -H "x-api-key: $API_KEY" \
        -H "anthropic-version: 2023-06-01" \
        -d "{\"model\":\"MiniMax-M3\",\"max_tokens\":150,\"messages\":[{\"role\":\"user\",\"content\":\"$PROMPT\"}],\"temperature\":0}" \
        2>&1)
    echo "  Direct:  $RESULT_B"

    # 解析数值
    ttfb_a=$(echo "$RESULT_A" | grep TTFB | sed 's/TTFB=//')
    total_a=$(echo "$RESULT_A" | grep Total | sed 's/Total=//')
    ttfb_b=$(echo "$RESULT_B" | grep TTFB | sed 's/TTFB=//')
    total_b=$(echo "$RESULT_B" | grep Total | sed 's/Total=//')

    mygate_ttfb+=("$ttfb_a")
    mygate_total+=("$total_a")
    direct_ttfb+=("$ttfb_b")
    direct_total+=("$total_b")

    echo "  Δ TTFB: $(echo "scale=3; $ttfb_a - $ttfb_b" | bc)s  Δ Total: $(echo "scale=3; $total_a - $total_b" | bc)s"
    sleep 1
done

echo ""
echo "===== 汇总 ====="
echo ""

# 计算均值
calc_avg() {
    local sum=0
    for v in "$@"; do sum=$(echo "$sum + $v" | bc); done
    echo "scale=3; $sum / $# " | bc
}

avg_mg_ttfb=$(calc_avg "${mygate_ttfb[@]}")
avg_mg_total=$(calc_avg "${mygate_total[@]}")
avg_dir_ttfb=$(calc_avg "${direct_ttfb[@]}")
avg_dir_total=$(calc_avg "${direct_total[@]}")

echo "              TTFB(首字节)   Total(总耗时)"
echo "  MyGate       ${avg_mg_ttfb}s          ${avg_mg_total}s"
echo "  Direct       ${avg_dir_ttfb}s          ${avg_dir_total}s"
echo "  ─────────────────────────────────────"
echo "  Overhead     $(echo "scale=3; $avg_mg_ttfb - $avg_dir_ttfb" | bc)s          $(echo "scale=3; $avg_mg_total - $avg_dir_total" | bc)s"
echo "  Overhead %   $(echo "scale=1; ($avg_mg_total - $avg_dir_total) / $avg_dir_total * 100" | bc)%"
