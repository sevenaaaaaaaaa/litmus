#!/usr/bin/env bash
# Litmus 正式发版（只发主版本 X.Y.0；X.Y.Z 仅用于正式版的紧急修复）
#
# 用法:
#   scripts/release.sh 0.2.0                 # 正式发版
#   scripts/release.sh 0.2.0 --no-install    # 发版但不替换本机 /Applications/Litmus.app
#   scripts/release.sh 0.2.0 --updater-only  # 补救：本机有私钥时，本地签名打 macOS 更新归档 + latest.json 并上传
#   scripts/release.sh 0.2.0 --macos-sign   # 本机无私钥（只有仓库 Secret）时：打 tar.gz → CI 代签 → 合并 darwin 条目
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
#   2. CI 依赖仓库 Secret TAURI_SIGNING_PRIVATE_KEY（minisign 私钥，本地在 ~/.tauri/litmus-updater.key）。
#      Secret 丢失/损坏时 CI 报 "Missing comment in secret key"，用 gh api PUT secrets/... 重写。
#   3. 重发时先删 Release 上同名资产（gh release delete-asset <tag> <name> --yes），否则 tauri-action 上传撞名失败。
#   4. 本机 DNS 拦 uploads.github.com（NXDOMAIN）：上传走 DoH 解析 + curl --resolve（scripts/lib/gh-upload.sh）。
set -euo pipefail

APP_NAME="Litmus"
TITLE_PREFIX="Litmus"
REPO_SLUG="sevenaaaaaaaaa/litmus"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL_PATH="/Applications/${APP_NAME}.app"
cd "$ROOT"

VER="${1:?用法: scripts/release.sh X.Y.Z [--no-install|--updater-only]}"
VER="${VER#v}"
shift
INSTALL=1 UPDATER_ONLY=0 MACOS_SIGN=0
for arg in "$@"; do
  case "$arg" in
    --no-install) INSTALL=0 ;;
    --updater-only) UPDATER_ONLY=1 ;;
    --macos-sign) MACOS_SIGN=1 ;;
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
  (cd desktop && TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/litmus-updater.key 2>/dev/null || cat ~/.tauri/thirdc-updater.key)" \
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
    cargo tauri build --bundles app --config '{"bundle":{"createUpdaterArtifacts":true}}')
  ARCHIVE="target/release/bundle/macos/${APP_NAME}.app.tar.gz"
  SIG="${ARCHIVE}.sig"
  [[ -f "$ARCHIVE" && -f "$SIG" ]] || { echo "✗ 归档/签名未产出"; exit 1; }
  LATEST_JSON="$(mktemp -d)/latest.json"
  EXISTING="$(mktemp -d)/latest.json"
  # 合并已发布 latest.json 的其它平台条目：CI（Win/Linux）在 macOS runner 不可用的时期
  # 仍会生成这两条，整份覆盖会让它们的 App 内更新失效。
  gh release download "$TAG" -R "$REPO_SLUG" -p latest.json -O "$EXISTING" --clobber >/dev/null 2>&1 || true
  python3 - "$VER" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$SIG" "$LATEST_JSON" "$REPO_SLUG" "$APP_NAME" "$EXISTING" <<'PY'
import json, sys, pathlib
ver, pub, sig, out, slug, app, existing = sys.argv[1:8]
platforms = {}
try:
    platforms = json.loads(pathlib.Path(existing).read_text()).get("platforms", {}) or {}
except Exception:
    platforms = {}
platforms.pop("darwin-aarch64", None)
platforms.pop("darwin-aarch64-app", None)
darwin = {
    "signature": pathlib.Path(sig).read_text().strip(),
    "url": f"https://github.com/{slug}/releases/download/v{ver}/{app}.app.tar.gz",
}
platforms["darwin-aarch64"] = darwin
platforms["darwin-aarch64-app"] = darwin
latest = {"version": ver, "notes": f"Litmus v{ver}", "pub_date": pub, "platforms": platforms}
pathlib.Path(out).write_text(json.dumps(latest, ensure_ascii=False, indent=2))
PY
  gh_upload_asset "$TAG" "$LATEST_JSON" --clobber
  gh_upload_asset "$TAG" "$ARCHIVE" --clobber
  gh_upload_asset "$TAG" "$SIG" --clobber
  echo "🎉 完成。验证: curl -sL https://github.com/$REPO_SLUG/releases/latest/download/latest.json | python3 -m json.tool"
  exit 0
fi

# ---------- --macos-sign: 本机无私钥时，让 CI 用 Secret 签 macOS 更新归档并补 latest.json ----------
# 背景：TAURI_SIGNING_PRIVATE_KEY 只在仓库 Secret 里（本机 ~/.tauri 可能什么都没有），而本账号的
# GitHub macOS runner 会被秒取消，macOS 更新归档只能本地打。签名与平台无关——ubuntu runner 拿同一把
# 钥匙签 .app.tar.gz 等价，所以这里走 .github/workflows/sign-asset.yml 代签，公钥与 Secret 都不动。
if [[ "${MACOS_SIGN}" == 1 ]]; then
  git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null || { echo "✗ tag ${TAG} 不存在"; exit 1; }
  [[ "$(cargo_version)" == "${VER}" ]] || { echo "✗ Cargo.toml 版本 $(cargo_version) ≠ ${VER}"; exit 1; }
  ASSET="${APP_NAME}.app.tar.gz"
  SIG_ASSET="${ASSET}.sig"
  WORK="$(mktemp -d)"

  # 1) 打更新归档：优先用已构建的 .app，没有就从本机 DMG 里取
  APP_DIR="${APP_OUT:-}"
  if [[ -z "$APP_DIR" || ! -d "$APP_DIR" ]]; then
    DMG_LOCAL="$(ls -t "$ROOT"/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)"
    [[ -n "$DMG_LOCAL" ]] || { echo "✗ 既没有 .app 也没有 DMG，先跑一次构建（不带 --macos-sign 的发版流程）"; exit 1; }
    MNT="$(mktemp -d)"
    log "从 DMG 提取 ${APP_NAME}.app：$(basename "$DMG_LOCAL")"
    hdiutil attach -nobrowse -quiet -mountpoint "$MNT" "$DMG_LOCAL"
    cp -R "$MNT/${APP_NAME}.app" "$WORK/"
    hdiutil detach -quiet "$MNT"
    APP_DIR="$WORK/${APP_NAME}.app"
  fi
  BUILT_VER="$(defaults read "$APP_DIR/Contents/Info.plist" CFBundleShortVersionString)"
  [[ "$BUILT_VER" == "${VER}" ]] || { echo "✗ 归档里的版本是 ${BUILT_VER}，期望 ${VER}"; exit 1; }
  PACK="$WORK/$ASSET"
  tar czf "$PACK" -C "$(dirname "$APP_DIR")" "${APP_NAME}.app"
  log "打包 $(basename "$PACK")（$(du -h "$PACK" | cut -f1)）"
  gh_upload_asset "${TAG}" "$PACK" --clobber

  # 2) 等 desktop.yml 产出 latest.json：win/linux 两条必须留着，不能被我们整份覆盖掉
  log "等待 Release 上出现 latest.json（desktop.yml 产出）"
  deadline=$(( $(date +%s) + ${LATEST_WAIT:-1800} ))
  until gh_release_has_asset "${TAG}" latest.json; do
    [[ "$(date +%s)" -lt "$deadline" ]] || { echo "✗ 等 latest.json 超时（LATEST_WAIT=${LATEST_WAIT:-1800}s）"; exit 1; }
    printf '.'; sleep 10
  done
  echo

  # 3) 触发 CI 代签
  log "触发 sign-asset.yml（tag=${TAG} asset=${ASSET}）"
  gh workflow run sign-asset.yml -R "${REPO_SLUG}" -f tag="${TAG}" -f asset="$ASSET" >/dev/null
  RUN_ID=""
  for _ in $(seq 1 12); do
    RUN_ID="$(gh run list -R "${REPO_SLUG}" --workflow sign-asset.yml -L1 --json databaseId --jq '.[0].databaseId' 2>/dev/null || true)"
    [[ -n "$RUN_ID" && "$RUN_ID" != "null" ]] && break
    sleep 5
  done
  [[ -n "$RUN_ID" && "$RUN_ID" != "null" ]] || { echo "✗ 没找到 sign-asset 的运行"; exit 1; }
  log "运行 $RUN_ID"
  deadline=$(( $(date +%s) + ${SIGN_WAIT:-1800} ))
  while :; do
    ST="$(gh run view "$RUN_ID" -R "${REPO_SLUG}" --json status,conclusion --jq '.status + "/" + (.conclusion // "-")')"
    case "$ST" in
      completed/success) break ;;
      completed/*) echo "✗ 签名失败：${ST}（gh run view ${RUN_ID} --log）"; exit 1 ;;
    esac
    [[ "$(date +%s)" -lt "$deadline" ]] || { echo "✗ 等签名超时"; exit 1; }
    sleep 10
  done
  log "签名完成"

  # 4) 取回 .sig，并核对签名钥匙 = 客户端内嵌公钥（轮换过就别装哑巴，直接停下）
  gh release download "${TAG}" -R "${REPO_SLUG}" -p "$SIG_ASSET" --clobber -O "$WORK/$SIG_ASSET"
  python3 - "$WORK/$SIG_ASSET" "$ROOT/desktop/tauri.conf.json" <<'PY'
import base64, json, sys
sig_path, conf_path = sys.argv[1], sys.argv[2]
sig = base64.b64decode(base64.b64decode(open(sig_path).read().strip()).decode().splitlines()[1])
sig_kid = sig[2:10].hex().upper()
conf = json.load(open(conf_path))
pub = base64.b64decode(base64.b64decode(conf["plugins"]["updater"]["pubkey"]).decode().splitlines()[1])
pub_kid = pub[2:10].hex().upper()
print(f"  签名钥匙 {sig_kid} / 客户端公钥 {pub_kid}")
if sig_kid != pub_kid:
    sys.exit("  ✗ 钥匙不一致：这个包客户端会拒收（私钥被轮换过？）")
PY
  log "钥匙一致"

  # 5) 合并 darwin 条目（保留其它平台）
  gh release download "${TAG}" -R "${REPO_SLUG}" -p latest.json --clobber -O "$WORK/latest.json"
  python3 - "$WORK/latest.json" "$WORK/$SIG_ASSET" "${VER}" "${REPO_SLUG}" "${APP_NAME}" <<'PY'
import json, pathlib, sys
latest_path, sig_path, ver, slug, app = sys.argv[1:6]
d = json.loads(pathlib.Path(latest_path).read_text())
sig = pathlib.Path(sig_path).read_text().strip()
url = f"https://github.com/{slug}/releases/download/v{ver}/{app}.app.tar.gz"
entry = {"signature": sig, "url": url}
d.setdefault("platforms", {})
d["platforms"]["darwin-aarch64"] = entry
d["platforms"]["darwin-aarch64-app"] = entry
d["version"] = ver
d.setdefault("pub_date", "")
pathlib.Path(latest_path).write_text(json.dumps(d, ensure_ascii=False, indent=2) + "\n")
print("  platforms:", ", ".join(sorted(d["platforms"])))
PY
  gh_upload_asset "${TAG}" "$WORK/latest.json" --clobber

  rm -rf "$WORK"
  echo
  echo "  macOS 自动更新已就绪"
  echo "  归档:     $ASSET"
  echo "  签名:     $SIG_ASSET"
  echo "  验证:     curl -sL https://github.com/${REPO_SLUG}/releases/latest/download/latest.json | python3 -m json.tool"
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
echo "  macOS 更新: CI 无 macOS runner，等 CI 跑完后补一次：scripts/release.sh $VER --updater-only"
echo "  移动端（CI 不含 Android）: source .android-env.sh && cd desktop && cargo tauri android build --apk --target aarch64"
echo "            然后 gh release upload $TAG <apk>（重命名 Litmus-mobile-$VER-arm64.apk）"
