#!/usr/bin/env bash
# 鹿蕊 Litmus 正式发版（只发主版本 X.Y.0；X.Y.Z 仅用于正式版的紧急修复）
#
# 用法:
#   scripts/release.sh 0.2.0                 # 正式发版
#   scripts/release.sh 0.2.0 --no-install    # 发版但不替换本机 /Applications/Litmus.app
#   scripts/release.sh 0.2.0 --updater-only  # 补救：CI 挂掉时本地签名打 macOS 更新归档 + latest.json 并上传
#
# 流程：main + 干净 + 与 origin 同步 → CHANGELOG.md 必须已有「## X.Y.Z — …」一节（补丁为「### X.Y.Z — …」）
# → 内核测试 → 写入唯一版本源 Cargo.toml [workspace.package]（+ Cargo.lock）→ 提交 `chore(release): vX.Y.Z`
# + 附注 tag → 本地打 app + DMG（含打开助手）→ push main + tag（触发 desktop.yml：三平台 + latest.json）
# → `gh release create vX.Y.Z --latest`（说明 = CHANGELOG 该节 + 安装说明）并上传 DMG → 更新本机应用。
# 中途失败可直接重跑：HEAD 已是该版本的发版提交时从未完成的步骤续跑。
#
# 关键教训（勿重蹈）：
#   1. 发版必须产出 latest.json，否则 App 内"检查更新"报
#      "Could not fetch a valid release JSON from the remote"。推 tag 让 CI 生成（tauri-action includeUpdaterJson）。
#   2. CI 依赖仓库 Secret TAURI_SIGNING_PRIVATE_KEY（minisign 私钥，本地在 ~/.tauri/thirdc-updater.key）。
#      Secret 丢失/损坏时 CI 报 "Missing comment in secret key"，用 gh api PUT secrets/... 重写。
#   3. 重发时先删 Release 上同名资产（gh release delete-asset <tag> <name> --yes），否则 tauri-action 上传撞名失败。
#   4. 本机 DNS 拦 uploads.github.com（NXDOMAIN）：上传走 DoH 解析 + curl --resolve（scripts/lib/gh-upload.sh）。
set -euo pipefail

APP_NAME="Litmus"
TITLE_PREFIX="鹿蕊 Litmus"
REPO_SLUG="sevenaaaaaaaaa/litmus"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL_PATH="/Applications/${APP_NAME}.app"
cd "$ROOT"

VER="${1:?用法: scripts/release.sh X.Y.Z [--no-install|--updater-only]}"
VER="${VER#v}"
shift
INSTALL=1 UPDATER_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --no-install) INSTALL=0 ;;
    --updater-only) UPDATER_ONLY=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done
[[ "$VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "✗ 版本需为 X.Y.Z（不带预发布后缀）"; exit 1; }
TAG="v$VER"

log() { printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*"; }
# shellcheck source=lib/macos-build.sh
source "$ROOT/scripts/lib/macos-build.sh"
# shellcheck source=lib/gh-upload.sh
export GH_REPO_SLUG="$REPO_SLUG"
source "$ROOT/scripts/lib/gh-upload.sh"
setup_macos_build_env

# ---------- --updater-only: local remediation of the macOS updater archive + latest.json ----------
if [[ "$UPDATER_ONLY" == 1 ]]; then
  git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || { echo "✗ tag $TAG 不存在"; exit 1; }
  [[ "$(cargo_version)" == "$VER" ]] || { echo "✗ Cargo.toml 版本 $(cargo_version) ≠ $VER"; exit 1; }
  log "本地签名打包（createUpdaterArtifacts=true）..."
  (cd desktop && TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/thirdc-updater.key)" \
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
    cargo tauri build --bundles app --config '{"bundle":{"createUpdaterArtifacts":true}}')
  ARCHIVE="target/release/bundle/macos/${APP_NAME}.app.tar.gz"
  SIG="${ARCHIVE}.sig"
  [[ -f "$ARCHIVE" && -f "$SIG" ]] || { echo "✗ 归档/签名未产出"; exit 1; }
  LATEST_JSON="$(mktemp -d)/latest.json"
  python3 - "$VER" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$SIG" "$LATEST_JSON" "$REPO_SLUG" "$APP_NAME" <<'PY'
import json, sys, pathlib
ver, pub, sig, out, slug, app = sys.argv[1:7]
latest = {
    "version": ver,
    "notes": f"鹿蕊 Litmus v{ver}",
    "pub_date": pub,
    "platforms": {"darwin-aarch64": {
        "signature": pathlib.Path(sig).read_text().strip(),
        "url": f"https://github.com/{slug}/releases/download/v{ver}/{app}.app.tar.gz",
    }},
}
pathlib.Path(out).write_text(json.dumps(latest, ensure_ascii=False, indent=2))
PY
  gh_upload_asset "$TAG" "$LATEST_JSON" --clobber
  gh_upload_asset "$TAG" "$ARCHIVE" --clobber
  gh_upload_asset "$TAG" "$SIG" --clobber
  echo "🎉 完成。验证: curl -sL https://github.com/$REPO_SLUG/releases/latest/download/latest.json | python3 -m json.tool"
  exit 0
fi

# ---------- 1. preconditions ----------
[[ "$(git branch --show-current)" == "main" ]] || { echo "✗ 请在 main 上发版"; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo "✗ 工作区不干净，先提交"; git status --short; exit 1; }
git fetch -q --tags --force origin
RESUME=0
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  if [[ "$(git rev-parse "$TAG^{commit}")" == "$(git rev-parse HEAD)" \
        && "$(git log -1 --format=%s)" == "chore(release): $TAG" ]]; then
    RESUME=1; log "HEAD 即 $TAG 的发版提交，续跑未完成的步骤"
  else
    echo "✗ tag $TAG 已存在且不在 HEAD"; exit 1
  fi
fi
if [[ "$RESUME" == 0 ]]; then
  [[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || {
    echo "✗ 本地 main 与 origin/main 不同步（先 pull/push）"; exit 1; }
  CUR="$(cargo_version)"
  [[ "$(printf '%s\n%s\n' "$CUR" "$VER" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)" == "$VER" && "$CUR" != "$VER" ]] \
    || { echo "✗ $VER 不高于当前版本 $CUR"; exit 1; }
  [[ "${VER##*.}" == 0 ]] || log "⚠ $VER 是补丁版本：仅用于正式版的紧急修复"
fi

# Release notes = the CHANGELOG section for this version (heading "## X.Y.Z — …" or "### X.Y.Z — …").
NOTES="$(mktemp)"
python3 - "$VER" CHANGELOG.md > "$NOTES" <<'PY'
import re, sys
ver, path = sys.argv[1], sys.argv[2]
lines = open(path, encoding="utf-8").read().splitlines()
start = level = None
for i, l in enumerate(lines):
    m = re.match(r'^(#{2,3}) ' + re.escape(ver) + r'(\s|$)', l)
    if m:
        start, level = i, len(m.group(1)); break
if start is None:
    sys.exit(f"CHANGELOG.md 中没有 {ver} 一节")
out = [lines[start]] if level == 3 else []   # patch heading carries the fix text itself
for l in lines[start + 1:]:
    if re.match(r'^#{2,%d} ' % level, l):
        break
    out.append(l)
print("\n".join(out).strip())
PY
{ echo; echo; install_footer; } >> "$NOTES"
log "Release 说明："; sed 's/^/    /' "$NOTES"

# ---------- 2. tests + bump + commit + tag ----------
if [[ "$RESUME" == 0 ]]; then
  log "== 内核测试"
  run_kernel_tests
  log "== 写入版本 $VER"
  python3 - "$VER" <<'PY'
import re, sys
ver = sys.argv[1]
s = open("Cargo.toml").read()
old = re.search(r'\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', s, re.S | re.M).group(1)
s2, n = re.subn(r'(\[workspace\.package\][^\[]*?^version\s*=\s*")[^"]+(")',
                lambda m: m.group(1) + ver + m.group(2), s, count=1, flags=re.S | re.M)
assert n == 1, "workspace.package version not found"
open("Cargo.toml", "w").write(s2)
# Cargo.lock: workspace members are the path packages (no `source`) at the old version.
blocks = open("Cargo.lock").read().split("\n[[package]]\n")
for i, b in enumerate(blocks[1:], 1):
    if "\nsource = " not in b and f'\nversion = "{old}"\n' in b:
        blocks[i] = b.replace(f'\nversion = "{old}"\n', f'\nversion = "{ver}"\n', 1)
open("Cargo.lock", "w").write("\n[[package]]\n".join(blocks))
PY
  [[ "$(cargo_version)" == "$VER" ]] || { echo "✗ 版本写入失败"; exit 1; }
  git add Cargo.toml Cargo.lock
  git commit --quiet -m "chore(release): $TAG" -- Cargo.toml Cargo.lock
  git tag -a "$TAG" -m "$TITLE_PREFIX $TAG"
  log "已提交 chore(release): $TAG 并打附注 tag"
fi

# ---------- 3. build macOS app + DMG ----------
log "== 构建 ${APP_NAME}.app $VER"
build_macos_app "$VER"
package_dmg "$VER"

# ---------- 4. push + GitHub Release ----------
log "== push main + $TAG"
git push --quiet origin main
git push --quiet origin "$TAG"   # triggers .github/workflows/desktop.yml (3 platforms + latest.json)

log "== GitHub Release $TAG"
if gh release view "$TAG" -R "$REPO_SLUG" >/dev/null 2>&1; then
  gh release edit "$TAG" -R "$REPO_SLUG" --title "$TITLE_PREFIX $TAG" --notes-file "$NOTES" --latest >/dev/null
else
  gh release create "$TAG" -R "$REPO_SLUG" --verify-tag --title "$TITLE_PREFIX $TAG" \
    --notes-file "$NOTES" --latest >/dev/null
fi
rm -f "$NOTES"
DMG_NAME="$(basename "$DMG_OUT")"
if gh_release_has_asset "$TAG" "$DMG_NAME"; then
  log "Release 已有 ${DMG_NAME}，跳过上传"
else
  gh_upload_asset "$TAG" "$DMG_OUT"
fi

# ---------- 5. local app ----------
INSTALLED_NOTE="未安装（--no-install）"
[[ "$INSTALL" == 1 ]] && install_local_app "$VER"

echo
echo "  版本:     $TAG"
echo "  Release:  https://github.com/$REPO_SLUG/releases/tag/$TAG"
echo "  DMG:      $DMG_OUT"
echo "  本机应用: $INSTALLED_NOTE"
echo "  CI:       gh run list -R $REPO_SLUG --workflow=desktop.yml --limit 1（Win/Linux 包 + latest.json）"
echo "  移动端（CI 不含 Android）: source .android-env.sh && cd desktop && cargo tauri android build --apk --target aarch64"
echo "            然后 gh release upload $TAG <apk>（重命名 Litmus-mobile-$VER-arm64.apk）"
