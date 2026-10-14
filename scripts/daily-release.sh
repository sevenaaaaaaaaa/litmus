#!/usr/bin/env bash
# 鹿蕊 Litmus 每日发版入口：同步 → 测试 → bump patch → 本地打 macOS app + DMG（含打开助手）
# → push main + tag（触发 CI 三平台 + latest.json）→ GitHub Release → 更新本机 /Applications。
#
# 用法:
#   scripts/daily-release.sh              # 正式发版（无新提交则跳过）
#   scripts/daily-release.sh --dry-run    # 只测试 + 本地构建打包，不 commit/tag/push/release/安装
#   scripts/daily-release.sh --no-install # 发版但不替换本机 /Applications/Litmus.app
#   scripts/daily-release.sh --force      # 自上个 tag 以来无新提交也发版
#
# 可由 launchd 无人值守运行：无交互；仅真正失败时非零退出；日志 ~/Library/Logs/litmus-daily-release.log。
set -euo pipefail

APP_NAME="Litmus"
TITLE_PREFIX="鹿蕊 Litmus"
REPO_SLUG="sevenaaaaaaaaa/litmus"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$HOME/Library/Logs/litmus-daily-release.log"
INSTALL_PATH="/Applications/${APP_NAME}.app"

DRY_RUN=0 INSTALL=1 FORCE=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --no-install) INSTALL=0 ;;
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done
[[ "$DRY_RUN" == 1 ]] && INSTALL=0

# launchd starts with a minimal PATH. Append (never prepend) so the toolchain matches the
# interactive shell: Homebrew rustc first, rustup's ~/.cargo/bin only for cargo-tauri etc.
# Mixing toolchains between runs breaks proc-macro crates ("can't find crate for phf_macros").
for d in /opt/homebrew/bin /usr/local/bin "$HOME/.cargo/bin" "$HOME/.local/bin" /usr/bin /bin /usr/sbin /sbin; do
  case ":$PATH:" in *":$d:"*) ;; *) PATH="$PATH:$d" ;; esac
done
export PATH

mkdir -p "$(dirname "$LOG")"
exec 3>&2   # original stderr, used by on_exit (tee may already be gone)
exec > >(tee -a "$LOG") 2>&1

STEP="init"
START_TS=$(date +%s)
log()  { printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*"; }
step() { STEP="$1"; echo; log "== $1"; }
DONE=0
skip() { log "SKIPPED: $*"; DONE=1; exit 0; }
on_exit() {
  local rc=$?
  rm -rf "${LOCK_DIR:-/nonexistent}" 2>/dev/null || true
  # bash 3.2 reports $?=0 in the EXIT trap after `set -u` aborts, so success is tracked explicitly.
  if [[ $rc -ne 0 || "$DONE" != 1 ]]; then
    [[ $rc -ne 0 ]] || rc=1
    local msg
    msg="[$(date '+%H:%M:%S')] ✗ FAILED at step: $STEP (exit $rc)"
    echo "$msg" >> "$LOG"; echo "$msg" >&3
    exit "$rc"
  fi
}
trap on_exit EXIT

echo
log "########## daily-release $(date '+%Y-%m-%d') dry_run=$DRY_RUN install=$INSTALL force=$FORCE"
cd "$ROOT"

# ---------- single instance ----------
LOCK_DIR="${TMPDIR:-/tmp}/litmus-daily-release.lock"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  if kill -0 "$(cat "$LOCK_DIR/pid" 2>/dev/null || echo 0)" 2>/dev/null; then
    LOCK_DIR=""; skip "another daily-release is running"
  fi
  rm -rf "$LOCK_DIR"; mkdir "$LOCK_DIR"
fi
echo $$ > "$LOCK_DIR/pid"

# ---------- 1. preconditions ----------
step "preconditions"
BRANCH="$(git branch --show-current)"
[[ "$BRANCH" == "main" ]] || skip "当前分支是 ${BRANCH}，不是 main"

if [[ -n "$(git status --porcelain)" ]]; then
  log "工作区有未提交改动（不会替你提交）："
  git status --short
  if [[ "$DRY_RUN" == 1 ]]; then log "dry-run：继续（改动不会进入任何提交）"; else skip "工作区不干净"; fi
fi

step "sync with origin"
fetch_ok=0
for attempt in 1 2 3; do
  if git fetch --quiet --tags origin; then fetch_ok=1; break; fi
  log "git fetch 失败（第 $attempt 次），10s 后重试"; sleep 10
done
if [[ "$fetch_ok" == 0 ]]; then
  if [[ "$DRY_RUN" == 1 ]]; then log "⚠ 无法连接 origin，dry-run 用本地已有的 origin/main 继续"
  else log "✗ 无法连接 origin（网络/DNS），本次不发版"; exit 1; fi
fi
read -r BEHIND AHEAD < <(git rev-list --left-right --count origin/main...HEAD)
log "相对 origin/main：落后 ${BEHIND}，领先 $AHEAD"
if [[ "$BEHIND" -gt 0 ]]; then
  if [[ "$DRY_RUN" == 1 ]]; then
    log "dry-run：不执行 pull（正式运行会 git pull --rebase --autostash）"
  elif ! git pull --rebase --autostash --quiet origin main; then
    git rebase --abort 2>/dev/null || true
    log "✗ 与 origin/main rebase 冲突，已 abort，请手动处理后重跑"
    exit 1
  fi
fi

# ---------- version bookkeeping ----------
conf_version()  { python3 -c "import json;print(json.load(open('desktop/tauri.conf.json'))['version'])"; }
cargo_version() { python3 - <<'PY'
import re
s = open("Cargo.toml").read()
m = re.search(r'\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', s, re.S | re.M)
print(m.group(1))
PY
}
max_version() { printf '%s\n' "$@" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1; }

LATEST_TAG="$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -1)"
RESUME=0
if [[ -n "$LATEST_TAG" ]]; then
  SINCE=$(git rev-list --count "$LATEST_TAG..HEAD")
  # Resume a previous run that already made the release commit + tag but did not finish.
  if [[ "$SINCE" == 0 && "$(git log -1 --format=%s)" == "chore(release): $LATEST_TAG" ]]; then
    RESUME=1
  fi
else
  SINCE=$(git rev-list --count HEAD)
fi

if [[ "$RESUME" == 1 ]]; then
  VER="${LATEST_TAG#v}"
  # Already fully done? (tag on remote, DMG on the Release, local app up to date)
  if [[ "$FORCE" == 0 ]] \
     && git ls-remote --exit-code --tags origin "refs/tags/$LATEST_TAG" >/dev/null 2>&1 \
     && gh release view "$LATEST_TAG" -R "$REPO_SLUG" --json assets --jq '.assets[].name' 2>/dev/null \
        | grep -q '\.dmg$' \
     && { [[ "$INSTALL" == 0 ]] || [[ "$(defaults read "$INSTALL_PATH/Contents/Info.plist" CFBundleShortVersionString 2>/dev/null)" == "$VER" ]]; }; then
    skip "$LATEST_TAG 已发布且本机已是最新，自那以后没有新提交"
  fi
  log "HEAD 即 $LATEST_TAG 的发版提交，但发版未完成，继续未完成的步骤"
else
  if [[ "$SINCE" == 0 && "$FORCE" == 0 ]]; then
    skip "自 $LATEST_TAG 以来没有新提交（--force 可强制发版）"
  fi
  BASE=$(max_version "${LATEST_TAG#v}" "$(conf_version)" "$(cargo_version)")
  IFS=. read -r MA MI PA <<<"$BASE"
  VER="$MA.$MI.$((PA + 1))"
fi
TAG="v$VER"
log "目标版本：${TAG}（上个 tag：${LATEST_TAG:-无}，新提交 $SINCE 个）"

# ---------- 2. tests ----------
if [[ "$(uname -s)" == "Darwin" ]]; then
  # New ld (Xcode 27) mis-aligns rustc proc-macro dylibs; classic linker avoids it (see CLAUDE.md).
  export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-ld_classic"
fi
if [[ "$RESUME" == 0 ]]; then
  step "tests (same as CI kernel.yml)"
  cargo test --quiet -p kernel-core -p kernel-store -p kernel-md -p kernel-a2ui -p kernel-sync \
    -p kernel-design -p kernel-deploy -p kernel-browser
  if [[ -d "$(rustc --print sysroot)/lib/rustlib/wasm32-unknown-unknown" ]]; then
    cargo check --quiet -p kernel-md -p kernel-a2ui --target wasm32-unknown-unknown
  else
    log "⚠ 本机 Rust 无 wasm32-unknown-unknown target，跳过 wasm 可移植性检查（CI kernel.yml 会跑）"
  fi
fi

# ---------- 3. bump + commit + tag ----------
bump_version() { # bump_version <ver>
  python3 - "$1" <<'PY'
import json, re, sys
ver = sys.argv[1]
old = re.search(r'\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"',
                open("Cargo.toml").read(), re.S | re.M).group(1)
p = "desktop/tauri.conf.json"
s = open(p).read()
s2, n = re.subn(r'("version"\s*:\s*")[^"]+(")', lambda m: m.group(1) + ver + m.group(2), s, count=1)
assert n == 1, "tauri.conf.json version not found"
json.loads(s2)
open(p, "w").write(s2)
p = "Cargo.toml"
s = open(p).read()
s2, n = re.subn(r'(\[workspace\.package\][^\[]*?^version\s*=\s*")[^"]+(")',
                lambda m: m.group(1) + ver + m.group(2), s, count=1, flags=re.S | re.M)
assert n == 1, "workspace.package version not found"
open(p, "w").write(s2)
# Cargo.lock: workspace members are the path packages (no `source`) at the old version.
p = "Cargo.lock"
blocks = open(p).read().split("\n[[package]]\n")
for i, b in enumerate(blocks[1:], 1):
    if "\nsource = " not in b and f'\nversion = "{old}"\n' in b:
        blocks[i] = b.replace(f'\nversion = "{old}"\n', f'\nversion = "{ver}"\n', 1)
open(p, "w").write("\n[[package]]\n".join(blocks))
PY
}

if [[ "$RESUME" == 0 && "$DRY_RUN" == 0 ]]; then
  step "bump version → $VER"
  bump_version "$VER"
  [[ "$(conf_version)" == "$VER" && "$(cargo_version)" == "$VER" ]] || { log "✗ 版本号写入不一致"; exit 1; }
  git add desktop/tauri.conf.json Cargo.toml Cargo.lock
  git commit --quiet -m "chore(release): $TAG"
  git tag -a "$TAG" -m "$TITLE_PREFIX $TAG"
  log "已提交 chore(release): $TAG 并打 tag"
fi

# ---------- 4. build macOS app + DMG ----------
ARCH="$(uname -m)"; [[ "$ARCH" == "arm64" ]] && ARCH="aarch64"
APP_OUT="$ROOT/target/release/bundle/macos/${APP_NAME}.app"
DMG_OUT="$ROOT/target/release/bundle/dmg/${APP_NAME}_${VER}_${ARCH}.dmg"

if security find-identity -v -p codesigning 2>/dev/null | grep -q "Developer ID Application"; then
  log "检测到 Developer ID 证书（签名/公证按 Tauri 环境变量配置执行）"
else
  log "无 Developer ID 证书：跳过 Developer ID 签名与公证，产物为 ad-hoc 签名（DMG 内置打开助手）"
  unset APPLE_SIGNING_IDENTITY APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD \
        APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH
fi

tauri_build() {
  # Version is passed explicitly so --dry-run builds the would-be version without editing files.
  # Updater archives are produced and signed by CI (latest.json); not needed locally.
  (cd desktop && cargo tauri build --bundles app \
    --config "{\"version\":\"$VER\",\"bundle\":{\"createUpdaterArtifacts\":false}}")
}

step "build ${APP_NAME}.app $VER"
BUILD_START=$(date +%s)
if ! tauri_build; then
  log "构建失败，按 release 规则清理 proc-macro dylib 后重试一次"
  find target/release/deps -name '*.dylib' -delete 2>/dev/null || true
  rm -rf target/release/.fingerprint
  tauri_build
fi
log "构建耗时 $(( $(date +%s) - BUILD_START ))s"
[[ -d "$APP_OUT" ]] || { log "✗ 未找到 $APP_OUT"; exit 1; }
BUILT_VER=$(defaults read "$APP_OUT/Contents/Info.plist" CFBundleShortVersionString)
[[ "$BUILT_VER" == "$VER" ]] || { log "✗ 产物版本 $BUILT_VER ≠ $VER"; exit 1; }
if ! codesign --verify --deep "$APP_OUT" >/dev/null 2>&1; then
  codesign --force --deep -s - "$APP_OUT"
  log "已对 ${APP_NAME}.app 做 ad-hoc 签名"
fi

step "package DMG"
bash packaging/macos/make-dmg.sh "$APP_OUT" "$DMG_OUT" "$APP_NAME"

if [[ "$DRY_RUN" == 1 ]]; then
  echo
  log "dry-run 完成：$TAG  DMG=$DMG_OUT ($(du -h "$DMG_OUT" | cut -f1))  总耗时 $(( $(date +%s) - START_TS ))s"
  DONE=1; exit 0
fi

# ---------- 5. push + GitHub Release ----------
# shellcheck source=lib/gh-upload.sh
export GH_REPO_SLUG="$REPO_SLUG"
source "$ROOT/scripts/lib/gh-upload.sh"

step "push main + $TAG"
git push --quiet origin main
git push --quiet origin "$TAG"   # triggers .github/workflows/desktop.yml (3 platforms + latest.json)

step "GitHub Release $TAG"
PREV_TAG="$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | grep -vFx "$TAG" | head -1)"
NOTES="$(mktemp)"
{
  echo "## 变更"
  git log --no-merges --format='- %s' "${PREV_TAG:+$PREV_TAG..}$TAG" | grep -vF "chore(release): " || echo "- 维护性更新"
  echo
  echo "### macOS 首次安装"
  echo "DMG 未经 Apple 公证。若提示「已损坏，无法打开」，请双击 DMG 里的「打不开？双击我.command」，详见「安装说明.txt」。"
  echo
  echo "Windows / Linux 安装包与应用内更新文件（latest.json）由 CI 构建后自动附加到本 Release。"
} > "$NOTES"

if gh release view "$TAG" -R "$REPO_SLUG" >/dev/null 2>&1; then
  log "Release 已存在（CI 可能先创建），更新说明"
  gh release edit "$TAG" -R "$REPO_SLUG" --notes-file "$NOTES" >/dev/null
else
  gh release create "$TAG" -R "$REPO_SLUG" --verify-tag --title "$TITLE_PREFIX $TAG" --notes-file "$NOTES" >/dev/null
fi
rm -f "$NOTES"

# CI (desktop.yml) skips its own DMG upload when this asset already exists, so nothing is clobbered.
DMG_NAME="$(basename "$DMG_OUT")"
if gh_release_has_asset "$TAG" "$DMG_NAME"; then
  log "Release 已有 ${DMG_NAME}，跳过上传"
else
  gh_upload_asset "$TAG" "$DMG_OUT"
fi
RELEASE_URL="https://github.com/$REPO_SLUG/releases/tag/$TAG"

# ---------- 6. update local app ----------
INSTALLED_NOTE="未安装（--no-install）"
if [[ "$INSTALL" == 1 ]]; then
  step "install to $INSTALL_PATH"
  INSTALLED_VER=$(defaults read "$INSTALL_PATH/Contents/Info.plist" CFBundleShortVersionString 2>/dev/null || echo none)
  if [[ "$INSTALLED_VER" == "$VER" ]]; then
    INSTALLED_NOTE="$INSTALL_PATH 已是 $VER"
  else
    app_running() { pgrep -f "$INSTALL_PATH/Contents/MacOS/" >/dev/null 2>&1; }
    WAS_RUNNING=0
    if app_running; then
      WAS_RUNNING=1
      log "正在退出 ${APP_NAME}…"
      osascript -e "quit app \"$APP_NAME\"" >/dev/null 2>&1 || true
      for _ in $(seq 1 60); do app_running || break; sleep 1; done
      if app_running; then
        log "✗ $APP_NAME 60 秒内未退出（可能有未保存内容），未替换本机应用；Release 已发布"
        exit 1
      fi
    fi
    STAGED="$(dirname "$INSTALL_PATH")/.${APP_NAME}.app.new"
    rm -rf "$STAGED"
    ditto "$APP_OUT" "$STAGED"
    xattr -dr com.apple.quarantine "$STAGED" 2>/dev/null || true
    rm -rf "$INSTALL_PATH"
    mv "$STAGED" "$INSTALL_PATH"
    INSTALLED_NOTE="${INSTALL_PATH}（$INSTALLED_VER → ${VER}）"
    if [[ "$WAS_RUNNING" == 1 ]]; then open "$INSTALL_PATH"; log "已重新启动 $APP_NAME"; fi
  fi
fi

# ---------- 7. summary ----------
step "summary"
echo "  版本:     $TAG"
echo "  Release:  $RELEASE_URL"
echo "  DMG:      $DMG_OUT"
echo "  本机应用: $INSTALLED_NOTE"
echo "  CI:       gh run list -R $REPO_SLUG --workflow=desktop.yml --limit 1（Win/Linux 包 + latest.json）"
echo "  耗时:     $(( $(date +%s) - START_TS ))s   日志: $LOG"
DONE=1
