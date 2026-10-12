#!/usr/bin/env bash
# 把发布产物（dist/）同步到 Cloudflare R2（S3 兼容）。
# 供 CI（desktop.yml 的「同步到 R2」步骤）或本地手动使用。
#
# 环境变量：
#   AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY   R2 API Token 的 Access Key / Secret
#   R2_ENDPOINT   https://<account_id>.r2.cloudflarestorage.com
#   R2_BUCKET     bucket 名
#   可选 R2_PREFIX  对象前缀，默认 thirdc/releases
#
# 前置：runner/本机有 aws CLI（GitHub Ubuntu runner 预装；本地 brew install awscli）。
# 用法：deploy/r2-sync.sh [要同步的目录，默认 dist]
set -euo pipefail

DIR="${1:-dist}"
: "${AWS_ACCESS_KEY_ID:?需要 R2_ACCESS_KEY_ID}"
: "${AWS_SECRET_ACCESS_KEY:?需要 R2_SECRET_ACCESS_KEY}"
: "${R2_ENDPOINT:?需要 R2_ENDPOINT（https://<account>.r2.cloudflarestorage.com）}"
: "${R2_BUCKET:?需要 R2_BUCKET}"

export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-auto}"
PREFIX="${R2_PREFIX:-thirdc/releases}"

if [ ! -d "$DIR" ]; then
  echo "目录不存在：$DIR" >&2
  exit 1
fi

aws s3 sync "$DIR" "s3://${R2_BUCKET}/${PREFIX}/" \
  --endpoint-url "$R2_ENDPOINT" \
  --cache-control "public, max-age=300" \
  --no-progress

echo "✓ 已同步 $DIR → s3://${R2_BUCKET}/${PREFIX}/"
