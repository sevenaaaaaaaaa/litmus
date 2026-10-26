#!/usr/bin/env bash
# Litmus 双轨部署（唯一入口）
#   GitHub 轨：gitleaks 扫描 → git add -A（.gitignore 已排除密钥/本地数据）→ commit → push origin main
#   服务器轨：rsync 工作区源码（含 gitignored 但服务器需要的文件）→ 服务器 cargo build → 重启 systemd
# 用法：./deploy/deploy.sh [--skip-build] [--server-only] [--dry-run] [-m "提交信息"]
#   --skip-build   服务器端跳过 cargo build（沿用旧参数）
#   --server-only  跳过 git，只同步服务器
#   --dry-run      rsync -n 预演；不 commit/push、不构建、不重启
# 分支约束：正式部署（含 --server-only）只允许在 main 上进行；--dry-run 任意分支可跑（仅告警）
# ops/（使用工具产生的脚本/批次，gitignored）不进 src/，单独同步到 /www/ops/litmus/（--update，只增不删）
set -euo pipefail
cd "$(dirname "$0")/.."

SSH_KEY="./.deploy-ssh/key"
SSH_PORT="28766"; USER_="root"; HOST="172.96.253.73"
REMOTE_DIR="/www/wwwroot/litmus"
OPS_DIR="/www/ops/litmus"   # outside any web root; never --delete
SSH_OPTS="ssh -i ${SSH_KEY} -p ${SSH_PORT} -o StrictHostKeyChecking=accept-new"
SSH_CMD="${SSH_OPTS} ${USER_}@${HOST}"
GITLEAKS="${GITLEAKS:-$HOME/.local/bin/gitleaks}"

SKIP_BUILD=""; SERVER_ONLY=0; DRY_RUN=0
MSG="chore: sync $(date '+%Y-%m-%d %H:%M')"
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1 ;;
    --server-only) SERVER_ONLY=1 ;;
    --dry-run) DRY_RUN=1 ;;
    -m) shift; MSG="${1:?-m 需要提交信息}" ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "未知参数: $1（用法见 --help）"; exit 1 ;;
  esac
  shift
done

step() { printf "\n\033[1;36m=== %s ===\033[0m\n" "$*"; }

# Deploy only from main (dry-run: warn only)
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" != main ]; then
  if [ "$DRY_RUN" = 1 ]; then
    echo "⚠ 当前分支 ${BRANCH}，部署只允许从 main 进行；功能分支请先合并（dry-run 仅预演，继续）"
  else
    echo "✗ 当前分支 ${BRANCH}，部署只允许从 main 进行；功能分支请先合并"; exit 1
  fi
fi

# ─── 1. GitHub 轨 ───
if [ "$SERVER_ONLY" = 1 ]; then
  echo "--server-only：跳过 git"
elif [ "$DRY_RUN" = 1 ]; then
  step "git（dry-run：只展示待提交变更，不 commit/push）"
  git status --short
  "$GITLEAKS" git --pre-commit --redact --no-banner || { echo "gitleaks 发现泄漏（未暂存变更），中止"; exit 1; }
else
  step "gitleaks 扫描 + git 提交推送"
  if [ -n "$(git status --porcelain)" ]; then
    git add -A
    if ! "$GITLEAKS" git --pre-commit --staged --redact --no-banner; then
      echo "gitleaks 发现泄漏，已中止（变更仍在暂存区，请修复或 git reset 后重试）"; exit 1
    fi
    git commit -m "$MSG" | head -2
  else
    echo "无未提交变更（跳过 commit）"
  fi
  git push origin main
fi

# ─── 2a. 服务器还是旧布局（/www/wwwroot/thirdc 是真目录）时先一次性迁移，否则会在新路径起一个空库 ───
if $SSH_CMD "test -d /www/wwwroot/thirdc -a ! -L /www/wwwroot/thirdc"; then
  if [ "$DRY_RUN" = 1 ]; then
    echo "⚠ 服务器还是旧布局 /www/wwwroot/thirdc：正式部署时会先跑 deploy/migrate-from-thirdc.sh"
  else
    step "服务器迁移 thirdc → litmus（一次性）"
    $SSH_CMD 'bash -s' < deploy/migrate-from-thirdc.sh
  fi
fi

# ─── 2. 服务器轨：rsync 源码（保留旧排除项；--delete 沿用旧脚本） ───
step "rsync → ${HOST}:${REMOTE_DIR}/src/"
RSYNC_FLAGS="-az"; [ "$DRY_RUN" = 1 ] && RSYNC_FLAGS="-azn --itemize-changes"
rsync $RSYNC_FLAGS --delete -e "${SSH_OPTS}" \
  --exclude '.git' --exclude 'target' --exclude '.litmus/local' --exclude '.thirdc' \
  --exclude 'server-kb' --exclude '.deploy-ssh' \
  --exclude 'desktop/gen' --exclude 'third_party' --exclude '.v2c' --exclude '.video_agent' \
  --exclude '.DS_Store' \
  --exclude '*.keystore' --exclude 'desktop/android-signing.properties' \
  --exclude '.gradle-mobile' --exclude '.venv*' --exclude '_cowork' --exclude 'node_modules' --exclude '/kb' \
  --exclude '/ops' \
  ./ "${USER_}@${HOST}:${REMOTE_DIR}/src/"

# ops/ → ${OPS_DIR}/ (outside web root, --update, no delete). Skips when ops/ is empty.
if [ -d ops ] && [ -n "$(ls -A ops 2>/dev/null)" ]; then
  step "rsync ops/ → ${HOST}:${OPS_DIR}/"
  rsync $RSYNC_FLAGS --update -e "${SSH_OPTS}" --exclude '.DS_Store' \
    --rsync-path="mkdir -p ${OPS_DIR} && rsync" ./ops/ "${USER_}@${HOST}:${OPS_DIR}/"
fi

if [ "$DRY_RUN" = 1 ]; then
  echo "dry-run 完成：未构建、未重启"; exit 0
fi

# ─── 3. 服务器构建 + 重启（与旧脚本一致） ───
step "服务器构建 + 重启 litmus"
$SSH_CMD "cd ${REMOTE_DIR}/src && ${SKIP_BUILD:+echo skip-build || }cargo build --release -p litmus 2>&1 | tail -1; mkdir -p ${REMOTE_DIR}/bin ${REMOTE_DIR}/kb && cp ${REMOTE_DIR}/src/target/release/litmus ${REMOTE_DIR}/bin/litmus.new && mv -f ${REMOTE_DIR}/bin/litmus.new ${REMOTE_DIR}/bin/litmus"
$SSH_CMD "systemctl disable --now thirdc 2>/dev/null; cp ${REMOTE_DIR}/src/deploy/litmus.service /etc/systemd/system/ && systemctl daemon-reload && systemctl enable litmus && systemctl restart litmus"
echo "Litmus: https://nownexts.com/litmus/  (daemon on 127.0.0.1:7700)"
