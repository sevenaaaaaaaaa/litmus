#!/usr/bin/env bash
# ThirdC 性能门禁：起本地 daemon 实测端点延迟，超预算即失败（退出码 1）。
# 预算依据 docs/PERF.md；改热路径代码前后各跑一次对比。
# 用法：scripts/perf-check.sh [--vault <库路径>]   # 默认 ./server-kb（无则报错）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/debug/thirdc"
[ -x "$BIN" ] || BIN="$ROOT/target/release/thirdc"
[ -x "$BIN" ] || { echo "✗ 先 cargo build -p thirdc"; exit 1; }

VAULT="$ROOT/server-kb"
if [ "${1:-}" = "--vault" ]; then VAULT="${2:?--vault 需要路径}"; fi
[ -d "$VAULT/Notes" ] || { echo "✗ 库不存在：${VAULT}（可用 --vault 指定）"; exit 1; }

PORT=7799
TOKEN=$(grep '^token' "$VAULT/.thirdc/machine.toml" | sed 's/.*= *"//;s/"//')
ALL_DOCS=$(find "$VAULT/Notes" -name '*.md' -type f -print)
DOC=${ALL_DOCS%%$'\n'*}
REL=${DOC#"$VAULT"/}
ENC=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "$REL")

"$BIN" serve "$VAULT" --addr 127.0.0.1:$PORT >/tmp/thirdc-perf.log 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT

# 等 daemon 起来
for _ in $(seq 1 30); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break
  sleep 1
done
A="Authorization: Bearer $TOKEN"
B="http://127.0.0.1:$PORT"

echo "── 预热（首次全量同步，冷启动不计预算）──"
curl -s -o /dev/null -w "冷启动 /doc: %{time_total}s\n" -H "$A" "$B/doc?path=$ENC"

echo "── 预算内测量 ──"
fail=0
check() { # name budget_seconds curl-args...
  local name="$1" budget="$2"; shift 2
  local t
  t=$(curl -s -o /dev/null -w '%{time_total}' "$@")
  awk -v n="$name" -v t="$t" -v b="$budget" 'BEGIN{
    mark=(t<=b)?"✓":"✗ OVER"; printf "%s %-22s %8.3fs  (预算 %ss)\n", mark, n, t, b}'
  awk -v t="$t" -v b="$budget" 'BEGIN{exit (t<=b)?0:1}' || fail=1
}

check "首页 /"          0.05  "$B/"
check "/status"         0.05  -H "$A" "$B/status"
check "/tree"           2.0   -H "$A" "$B/tree"
check "/browse 根"      1.0   -H "$A" "$B/browse?path=Notes"
check "warm /doc"       0.10  -H "$A" "$B/doc?path=$ENC"
check "/search"         0.30  -H "$A" "$B/search?q=the"
check "/memo/docs 首建" 3.0   -H "$A" "$B/memo/docs"
check "/memo/docs 缓存" 0.05  -H "$A" "$B/memo/docs"
check "知识场 /field"   0.05  "$B/field"
check "/field/data 首建" 20.0 -H "$A" "$B/field/data"
check "/field/data 缓存" 0.30 -H "$A" "$B/field/data"
check "/field/react"    0.15  -H "$A" "$B/field/react?q=sync"

echo
if [ "$fail" = "0" ]; then echo "✓ 全部在预算内"; else echo "✗ 有端点超预算（见上）"; exit 1; fi
