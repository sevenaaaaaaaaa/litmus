#!/usr/bin/env bash
# 鹿蕊 Litmus 一键发版
#
# 用法:
#   scripts/release.sh v0.1.13          # 主路径：推 tag 触发 CI，三平台产物 + 全平台 latest.json
#   scripts/release.sh v0.1.13 --local  # 补救：本地打包 macOS 更新归档并上传（CI 挂时用）
#
# 关键教训（勿重蹈）：
#   1. 发版必须产出 latest.json，否则 App 内"检查更新"报
#      "Could not fetch a valid release JSON from the remote"。
#      推 tag 让 CI 生成（tauri-action includeUpdaterJson: true）是正道。
#   2. CI 依赖仓库 Secret TAURI_SIGNING_PRIVATE_KEY（minisign 私钥，可整体 base64 或原文，
#      本地在 ~/.tauri/thirdc-updater.key）。Secret 丢失/损坏时 CI 报
#      "Missing comment in secret key"，用 gh api PUT secrets/... 重写。
#   3. 重发时先删 Release 上同名资产（gh release delete-asset <tag> <name> --yes），
#      否则 tauri-action 上传撞名失败。
#   4. 本机 DNS 拦 uploads.github.com（NXDOMAIN）：上传走 DoH 解析 + curl --resolve。
set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${1:?用法: scripts/release.sh vX.Y.Z [--local]}"
LOCAL=0
[[ "${2:-}" == "--local" ]] && LOCAL=1

# ---------- 前置检查 ----------
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "✗ tag 需为 vX.Y.Z 格式"; exit 1; }
VER="${TAG#v}"
[[ -z "$(git status --porcelain)" ]] || { echo "✗ 工作区不干净，先提交"; exit 1; }
BRANCH="$(git branch --show-current)"
[[ "$BRANCH" == "main" ]] || { echo "✗ 当前在 $BRANCH，请切到 main"; exit 1; }
git fetch -q origin main
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || {
  echo "✗ 本地 main 与 origin/main 不同步，先 push"; exit 1; }

CONF_VER=$(python3 -c "import json;print(json.load(open('desktop/tauri.conf.json'))['version'])")
[[ "$CONF_VER" == "$VER" ]] || {
  echo "✗ desktop/tauri.conf.json 版本为 $CONF_VER ≠ $VER，先 bump 再发"; exit 1; }

# ---------- tag 处理 ----------
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "⚠ tag $TAG 已存在（$(git rev-parse --short "$TAG^{commit}")）"
  if [[ "$LOCAL" == 1 ]]; then echo "  （--local 模式直接复用现有 tag）"; else
    echo "  重触发 CI: git push origin :refs/tags/$TAG && git push origin $TAG"
    exit 1
  fi
else
  git tag "$TAG" && git push origin "$TAG"
  echo "✅ tag $TAG 已推送，CI 构建三平台产物 + 全平台 latest.json"
  echo "   盯进度: gh run watch \$(gh run list --workflow=desktop.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
fi

[[ "$LOCAL" == 1 ]] || {
  echo
  echo "别忘了移动端（CI 不含 Android）:"
  echo "  source .android-env.sh && cd desktop && cargo tauri android build --apk --target aarch64"
  echo "  gh release upload $TAG app-universal-release.apk（重命名 Litmus-mobile-$VER-arm64.apk）"
  exit 0
}

# ---------- 本地打包（macOS 更新归档 + latest.json）----------
echo "→ 本地签名打包（createUpdaterArtifacts=true）..."
(cd desktop && TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/thirdc-updater.key)" \
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
  cargo tauri build --bundles app \
  --config '{"bundle":{"createUpdaterArtifacts":true}}')

ARCHIVE="target/release/bundle/macos/Litmus.app.tar.gz"
SIG="${ARCHIVE}.sig"
[[ -f "$ARCHIVE" && -f "$SIG" ]] || { echo "✗ 归档/签名未产出"; exit 1; }

PUB_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
python3 - "$VER" "$PUB_DATE" "$SIG" <<'PY'
import json, sys, pathlib
ver, pub, sig = sys.argv[1], sys.argv[2], pathlib.Path(sys.argv[3]).read_text().strip()
latest = {
    "version": ver,
    "notes": f"鹿蕊 Litmus v{ver}",
    "pub_date": pub,
    "platforms": {"darwin-aarch64": {
        "signature": sig,
        "url": f"https://github.com/sevenaaaaaaaaa/litmus/releases/download/v{ver}/Litmus.app.tar.gz",
    }},
}
pathlib.Path("/tmp/latest.json").write_text(json.dumps(latest, ensure_ascii=False, indent=2))
print("✅ /tmp/latest.json 生成")
PY

# ---------- 上传（含 DNS fallback）----------
# shellcheck source=lib/gh-upload.sh
source scripts/lib/gh-upload.sh
upload_asset() { gh_upload_asset "$TAG" "$1" --clobber; } # upload_asset <本地文件>

gh release view "$TAG" --json assets --jq '.assets[].name' | grep -q "^latest.json$" \
  && gh release delete-asset "$TAG" latest.json --yes
upload_asset /tmp/latest.json
upload_asset "$ARCHIVE"
upload_asset "$SIG"
echo "🎉 完成。endpoint 验证: curl -sL https://github.com/sevenaaaaaaaaa/litmus/releases/latest/download/latest.json | python3 -m json.tool"