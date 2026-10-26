#!/usr/bin/env bash
# Litmus 线上更新：服务器从 GitHub 拉取 → 构建 → 重启（幂等）
# 用法（服务器上）：/www/wwwroot/litmus/update.sh
set -euo pipefail
APP=/www/wwwroot/litmus
REPO=https://github.com/sevenaaaaaaaaa/litmus.git
export PATH="$HOME/.cargo/bin:$PATH"

echo "① 拉取最新代码"
if [ -d "$APP/src/.git" ]; then
  # 浅克隆下 origin/main 可能不刷新，用 FETCH_HEAD 更稳
  cd "$APP/src" && git fetch --depth=1 origin main && git reset --hard FETCH_HEAD
else
  rm -rf "$APP/src" && git clone --depth=1 "$REPO" "$APP/src"
fi
echo "   当前版本: $(cd "$APP/src" && git rev-parse --short HEAD)"

echo "② 构建（增量，通常 1-3 分钟）"
cd "$APP/src" && cargo build --release -p litmus 2>&1 | tail -2

echo "③ 替换二进制并重启"
systemctl stop litmus || true
cp "$APP/src/target/release/litmus" "$APP/bin/litmus"
systemctl start litmus
sleep 2

echo "④ 验证"
curl -s -o /dev/null -w "   本机 / → %{http_code}\n" http://127.0.0.1:7700/ || true
echo "✓ 更新完成: $(cd "$APP/src" && git rev-parse --short HEAD)"
