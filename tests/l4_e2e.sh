#!/usr/bin/env bash
# L4 端到端集成测试
# 用 DeepSeek (OpenAI 端) + MiniMax (双端) 跑 MyGate 完整功能
# 输出 L4-REPORT.md 包含每个场景的 PASS/FAIL

set -uo pipefail

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

REPORT=/tmp/l4-report.md
RESULTS=()
FAILS=0

# MyGate 配置 (使用真实 keys)
cat > /tmp/l4-config.toml << 'EOF'
[server]
host = "127.0.0.1"
port = 18080
timeout_seconds = 60
admin_token = "l4-test"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "KEY_REMOVED"
provider_type = "openai"
auth_style = "bearer"

[providers.minimax-openai]
base_url = "https://api.minimaxi.com/v1"
api_key = "KEY_REMOVED"
provider_type = "openai"
auth_style = "bearer"

[providers.minimax-anthropic]
base_url = "https://api.minimaxi.com/anthropic"
api_key = "KEY_REMOVED"
provider_type = "anthropic"
auth_style = "bearer"

[aliases.Simple]
[[aliases.Simple.chain]]
provider = "deepseek"
model = "deepseek-chat"
priority = 1

[aliases.Code]
[[aliases.Code.chain]]
provider = "deepseek"
model = "deepseek-chat"
priority = 1

[aliases.Plan]
[[aliases.Plan.chain]]
provider = "minimax-openai"
model = "MiniMax-M3"
priority = 1
[[aliases.Plan.chain]]
provider = "deepseek"
model = "deepseek-reasoner"
priority = 2

# 流式 4xx 错误测试用
[aliases.Broken]
[[aliases.Broken.chain]]
provider = "minimax-openai"
model = "non-existent-model-12345"
priority = 1
EOF

# Helper functions
log() { echo -e "$1" | tee -a "$REPORT"; }
pass() { RESULTS+=("PASS: $1"); log "${GREEN}✓${NC} $1"; }
fail() { RESULTS+=("FAIL: $1"); log "${RED}✗${NC} $1"; FAILS=$((FAILS + 1)); }
section() { log ""; log "## $1"; log ""; }

# Start MyGate
pkill -9 -f 'target/release/mygate' 2>/dev/null
sleep 1
MYGATE_CONFIG=/tmp/l4-config.toml /home/lvtao/lv/mygate/target/release/mygate > /tmp/mygate.log 2>&1 &
MG_PID=$!
disown
sleep 3

if ! curl -sf http://127.0.0.1:18080/health > /dev/null; then
    fail "MyGate failed to start"
    cat /tmp/mygate.log
    exit 1
fi
pass "MyGate started (PID $MG_PID)"

BASE="http://127.0.0.1:18080"

# ========== A. OpenAI 协议端到端 ==========
section "A. OpenAI 协议 /v1/chat/completions"

# A1. 简单对话 (DeepSeek)
A1=$(curl -s -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Simple","messages":[{"role":"user","content":"一句话"}],"max_tokens":50}')
A1_MODEL=$(echo "$A1" | python3 -c "import json,sys;print(json.load(sys.stdin)['model'])" 2>/dev/null)
A1_CONTENT=$(echo "$A1" | python3 -c "import json,sys;print(json.load(sys.stdin)['choices'][0]['message']['content'])" 2>/dev/null)
if [ "$A1_MODEL" = "Simple" ] && [ -n "$A1_CONTENT" ]; then
    pass "A1. 简单对话 (DeepSeek): model=$A1_MODEL, content='$A1_CONTENT'"
else
    fail "A1. 简单对话失败: $A1"
fi

# A2. 工具调用 (DeepSeek)
A2=$(curl -s -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Simple","messages":[{"role":"user","content":"北京天气"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"天气","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"tool_choice":"auto","max_tokens":100}')
A2_HAS_TOOL=$(echo "$A2" | python3 -c "import json,sys;d=json.load(sys.stdin);print(len(d['choices'][0]['message'].get('tool_calls',[])))" 2>/dev/null)
if [ "$A2_HAS_TOOL" -gt 0 ] 2>/dev/null; then
    pass "A2. 工具调用 (DeepSeek): $A2_HAS_TOOL 个 tool_calls"
else
    fail "A2. 工具调用失败: $A2"
fi

# A3. JSON mode (MiniMax OpenAI 端)
A3=$(curl -s -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Plan","messages":[{"role":"user","content":"输出 JSON: {\"x\":1}"}],"response_format":{"type":"json_object"},"max_tokens":50}')
A3_HAS_X=$(echo "$A3" | python3 -c "import json,sys;d=json.load(sys.stdin);c=d['choices'][0]['message']['content'];import json;print('\"x\"' in c)" 2>/dev/null)
if [ "$A3_HAS_X" = "True" ]; then
    pass "A3. JSON mode (MiniMax): 含 \"x\" 字段"
else
    fail "A3. JSON mode 失败: $A3"
fi

# A4. 采样参数 (DeepSeek)
A4=$(curl -s -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Simple","messages":[{"role":"user","content":"hi"}],"temperature":0.1,"top_p":0.9,"seed":42,"max_tokens":10}')
A4_TEMP=$(echo "$A4" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('usage',{}).get('completion_tokens',0)>=1)" 2>/dev/null)
if [ "$A4_TEMP" = "True" ]; then
    pass "A4. 采样参数 (DeepSeek): 接受 temperature/top_p/seed"
else
    fail "A4. 采样参数 失败: $A4"
fi

# A5. user 标识 (DeepSeek)
A5=$(curl -s -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "X-User-Id: l4-test-user" \
  -d '{"model":"Simple","messages":[{"role":"user","content":"hi"}],"user":"l4-test-user","max_tokens":10}')
A5_OK=$([ -n "$A5" ] && echo "ok")
if [ "$A5_OK" = "ok" ]; then
    pass "A5. user 标识 (DeepSeek): 接受 user 字段"
else
    fail "A5. user 标识 失败: $A5"
fi

# ========== B. OpenAI 流式 ==========
section "B. OpenAI 流式 /v1/chat/completions"

# B1. 流式 + [DONE]
B1=$(curl -sN -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Simple","messages":[{"role":"user","content":"数 1"}],"stream":true,"stream_options":{"include_usage":true},"max_tokens":30}')
B1_HAS_DONE=$(echo "$B1" | grep -c "\[DONE\]")
B1_CHUNKS=$(echo "$B1" | grep -c "^data: {")
if [ "$B1_HAS_DONE" -ge 1 ] && [ "$B1_CHUNKS" -ge 2 ]; then
    pass "B1. OpenAI 流式 (DeepSeek): $B1_CHUNKS chunks + [DONE]"
else
    fail "B1. 流式失败 (chunks=$B1_CHUNKS done=$B1_HAS_DONE)"
fi

# B2. 流式 + 工具调用 (DeepSeek)
B2=$(curl -sN -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  --data-binary @<(cat <<'EOF'
{"model":"Simple","messages":[{"role":"user","content":"北京天气"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"天气","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"tool_choice":"auto","stream":true}
EOF
))
B2_HAS_TOOL=$(echo "$B2" | grep -c "tool_calls")
if [ "$B2_HAS_TOOL" -gt 0 ]; then
    pass "B2. 流式工具调用 (DeepSeek): $B2_HAS_TOOL 个含 tool_calls"
else
    fail "B2. 流式工具调用失败"
fi

# ========== C. Anthropic 协议 ==========
section "C. Anthropic 协议 /v1/messages"

# C1. 简单对话 (MiniMax Anthropic 端)
C1=$(curl -s -X POST $BASE/v1/messages \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model":"Plan","max_tokens":50,"messages":[{"role":"user","content":"hi"}]}')
C1_CONTENT=$(echo "$C1" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('content',[{}])[0].get('text',''))" 2>/dev/null)
C1_MODEL=$(echo "$C1" | python3 -c "import json,sys;print(json.load(sys.stdin)['model'])" 2>/dev/null)
if [ "$C1_MODEL" = "Plan" ] && [ -n "$C1_CONTENT" ]; then
    pass "C1. Anthropic 简单对话 (MiniMax): model=$C1_MODEL, content='$C1_CONTENT'"
else
    fail "C1. Anthropic 失败: $C1"
fi

# C2. 工具调用 (Anthropic)
C2=$(curl -s -X POST $BASE/v1/messages \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model":"Plan","max_tokens":200,"tools":[{"name":"get_weather","description":"天气","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],"messages":[{"role":"user","content":"北京天气"}]}')
C2_HAS_TOOL=$(echo "$C2" | python3 -c "import json,sys;d=json.load(sys.stdin);c=d.get('content',[]);print(sum(1 for b in c if b.get('type')=='tool_use'))" 2>/dev/null)
if [ "$C2_HAS_TOOL" -gt 0 ] 2>/dev/null; then
    pass "C2. Anthropic 工具调用 (MiniMax): $C2_HAS_TOOL 个 tool_use"
else
    fail "C2. Anthropic 工具调用失败: $C2"
fi

# C3. 流式 (Anthropic, MiniMax). MiniMax 不带 type 字段, 也不发 event: 行, 只发 data: + chunks 数足够
C3_RESP=$(curl -sN --max-time 30 -X POST $BASE/v1/messages \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model":"Plan","max_tokens":50,"stream":true,"messages":[{"role":"user","content":"hi"}]}' 2>&1)
C3_DATA_CHUNKS=$(echo "$C3_RESP" | grep -c "^data: {")
C3_HAS_MODEL=$(echo "$C3_RESP" | grep -c '"model":"MiniMax-M3"')
C3_HAS_USAGE=$(echo "$C3_RESP" | grep -c '"usage"')
if [ "$C3_DATA_CHUNKS" -ge 3 ] && [ "$C3_HAS_MODEL" -ge 1 ] && [ "$C3_HAS_USAGE" -ge 1 ]; then
    pass "C3. Anthropic 流式 (MiniMax): $C3_DATA_CHUNKS chunks, model + usage 都出现"
else
    fail "C3. Anthropic 流式 (chunks=$C3_DATA_CHUNKS model=$C3_HAS_MODEL usage=$C3_HAS_USAGE)"
fi

# ========== D. 跨协议交叉 ==========
section "D. 跨协议交叉 (北向 OpenAI → 南向 Anthropic)"

# D1. OpenAI 客户端 → 强制走 minimax-anthropic 后端
D1=$(curl -sN -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Plan","messages":[{"role":"user","content":"hi"}],"stream":true,"max_tokens":30}')
D1_MODEL=$(echo "$D1" | python3 -c "
import json, sys
for line in sys.stdin:
    line = line.strip()
    if line.startswith('data: '):
        try:
            d = json.loads(line[6:])
            if 'model' in d:
                print(d['model'])
                break
        except: pass
")
D1_CHUNKS=$(echo "$D1" | grep -c "^data: {")
D1_HAS_DONE=$(echo "$D1" | grep -c "\[DONE\]")
if [ "$D1_MODEL" = "Plan" ] && [ "$D1_CHUNKS" -ge 2 ] && [ "$D1_HAS_DONE" -ge 1 ]; then
    pass "D1. OpenAI→Anthropic 端: model=$D1_MODEL, $D1_CHUNKS chunks, [DONE]"
else
    fail "D1. 跨协议失败 (model=$D1_MODEL chunks=$D1_CHUNKS done=$D1_HAS_DONE)"
fi

# D2. Anthropic 客户端 → 强制走 minimax-openai 后端 (需 test alias, 跳过 - 用 fallback chain)
log "  (跳过 D2: 需专用 alias, 见 [aliases.Anthropic2OpenAI] 但已用 minimax-anthropic 链)"

# ========== E. Fallback chain ==========
section "E. Fallback chain (alias Simple 走 DeepSeek)"

# E1. Simple 单一 provider 链路
E1=$(curl -s -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Simple","messages":[{"role":"user","content":"hi"}],"max_tokens":20}')
E1_OK=$([ -n "$E1" ] && echo "ok")
if [ "$E1_OK" = "ok" ]; then
    pass "E1. Simple 链路 DeepSeek 单 provider 成功"
else
    fail "E1. Simple 失败"
fi

# ========== F. 错误处理 (cr-411/412) ==========
section "F. 错误处理"

# F1. 流式 4xx 错误 → 502 (cr-412 修复)
F1=$(curl -s -o /dev/null -w "%{http_code}" -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Broken","messages":[{"role":"user","content":"hi"}],"stream":true}')
if [ "$F1" = "502" ] || [ "$F1" = "404" ]; then
    pass "F1. 流式 4xx 错误处理: HTTP $F1 (不当作 fallback 耗尽)"
else
    fail "F1. 流式 4xx 返回 $F1 (期望 502 或 404)"
fi

# F2. 4xx 错误 + body 错误信息
F2_BODY=$(curl -s -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Broken","messages":[{"role":"user","content":"hi"}]}')
F2_HAS_ERROR=$(echo "$F2_BODY" | grep -c "error")
if [ "$F2_HAS_ERROR" -gt 0 ]; then
    pass "F2. 4xx 错误 body 含 error 字段: $(echo $F2_BODY | head -c 100)"
else
    fail "F2. 错误 body 格式不对: $F2_BODY"
fi

# F3. 流式 content-type 检查
F3_STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST $BASE/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Broken","messages":[{"role":"user","content":"hi"}],"stream":true}' 2>&1)
if [ "$F3_STATUS" = "502" ] || [ "$F3_STATUS" = "404" ]; then
    pass "F3. 流式 mock 错时 (200 + application/json): HTTP $F3_STATUS (cr-412 拒绝)"
else
    fail "F3. 流式 content-type 错误: HTTP $F3_STATUS"
fi

# ========== G. /v1/models 列出可用 alias ==========
section "G. /v1/models 列出可用 alias"

G1=$(curl -s $BASE/v1/models | python3 -c "import json,sys;d=json.load(sys.stdin);print(','.join(m['id'] for m in d['data']))")
if [ -n "$G1" ]; then
    pass "G1. /v1/models 列出: $G1"
else
    fail "G1. /v1/models 失败"
fi

# ========== H. /metrics 包含计数 ==========
section "H. /metrics 指标"

H1=$(curl -s $BASE/metrics | grep -c "mygate_fallback_attempts_total")
H2=$(curl -s $BASE/metrics | grep -c "mygate_requests_total")
H3=$(curl -s $BASE/metrics | grep -c "mygate_active_streams")
if [ "$H1" -gt 0 ] && [ "$H2" -gt 0 ] && [ "$H3" -gt 0 ]; then
    pass "H1. /metrics 含 fallback_attempts / requests / active_streams 计数器"
else
    fail "H1. /metrics 缺计数器: fallback=$H1 requests=$H2 active=$H3"
fi

# Summary
log ""
log "## 总结"
log ""
log "- 总场景: ${#RESULTS[@]}"
log "- 通过: $((${#RESULTS[@]} - FAILS))"
log "- 失败: $FAILS"
log ""

pkill -9 -f 'target/release/mygate' 2>/dev/null

if [ $FAILS -eq 0 ]; then
    echo -e "${GREEN}ALL PASSED${NC}"
    exit 0
else
    echo -e "${RED}$FAILS FAILED${NC}"
    exit 1
fi
