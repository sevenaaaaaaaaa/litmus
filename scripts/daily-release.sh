#!/usr/bin/env bash
# 鹿蕊 Litmus 每日 Nightly 构建：同步 → 测试 → 以 X.(Y+1).0-nightly.YYYYMMDD 本地打 macOS app + DMG
# → 强制移动 tag `nightly` 并推送（触发 CI 三平台 Nightly 产物 + latest.json）→ 更新唯一的 GitHub
# 预发布「Nightly」→ 更新本机 /Applications。
#
# 不改版本号、不提交、不打 vX tag：正式版本只由 scripts/release.sh X.Y.0 发布。
# Nightly 是 prerelease 且 --latest=false，应用内更新读 releases/latest/... 永远拿不到它。
#
# 用法:
#   scripts/daily-release.sh              # Nightly 构建（自上次 Nightly 无新提交则跳过）
#   scripts/daily-release.sh --dry-run    # 只测试 + 本地构建打包，不打 tag/推送/发布/安装
#   scripts/daily-release.sh --no-install # 发布 Nightly 但不替换本机 /Applications/Litmus.app
#   scripts/daily-release.sh --force      # 自上次 Nightly 无新提交也重新构建
#
# 可由 launchd 无人值守运行：无交互；仅真正失败时非零退出；日志 ~/Library/Logs/litmus-daily-release.log。
# 最后一行固定输出结果标记，供 ~/.local/bin/daily-release-all.sh 判定：
#   RESULT: built nightly <version> <sha> | RESULT: skipped <原因> | RESULT: dry-run <version> <sha> | RESULT: failed <step>
set -euo pipefail

APP_NAME="Litmus"
REPO_SLUG="sevenaaaaaaaaa/litmus"
NIGHTLY_TAG="nightly"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$HOME/Library/Logs/litmus-daily-release.log"
INSTALL_PATH="/Applications/${APP_NAME}.app"

DRY_RUN=0 INSTALL=1 FORCE=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --no-install) INSTALL=0 ;;
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done
[[ "$DRY_RUN" == 1 ]] && INSTALL=0

mkdir -p "$(dirname "$LOG")"
exec 3>&2   # original stderr, used by on_exit (tee may already be gone)
exec > >(tee -a "$LOG") 2>&1

STEP="init"
START_TS=$(date +%s)
log()  { printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*"; }
step() { STEP="$1"; echo; log "== $1"; }
result() { echo "RESULT: $*"; }
DONE=0
skip() { log "SKIPPED: $*"; result "skipped $*"; DONE=1; exit 0; }
on_exit() {
  local rc=$?
  rm -rf "${LOCK_DIR:-/nonexistent}" 2>/dev/null || true
  # bash 3.2 reports $?=0 in the EXIT trap after `set -u` aborts, so success is tracked explicitly.
  if [[ $rc -ne 0 || "$DONE" != 1 ]]; then
    [[ $rc -ne 0 ]] || rc=1
    local msg
    msg="[$(date '+%H:%M:%S')] ✗ FAILED at step: $STEP (exit $rc)"
    echo "$msg" >> "$LOG"; echo "$msg" >&3
    echo "RESULT: failed $STEP (exit $rc)" >> "$LOG"; echo "RESULT: failed $STEP (exit $rc)" >&3
    exit "$rc"
  fi
}
trap on_exit EXIT

# shellcheck source=lib/macos-build.sh
source "$ROOT/scripts/lib/macos-build.sh"
setup_macos_build_env

echo
log "########## nightly $(date '+%Y-%m-%d') dry_run=$DRY_RUN install=$INSTALL force=$FORCE"
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
  if [[ "$DRY_RUN" == 1 ]]; then log "dry-run：继续（改动会进入本地构建，但不会提交）"; else skip "工作区不干净"; fi
fi

step "sync with origin"
fetch_ok=0
for attempt in 1 2 3; do
  # --force: the nightly tag is moved every day, plain fetch would refuse to update it.
  if git fetch --quiet --tags --force origin; then fetch_ok=1; break; fi
  log "git fetch 失败（第 $attempt 次），10s 后重试"; sleep 10
done
if [[ "$fetch_ok" == 0 ]]; then
  if [[ "$DRY_RUN" == 1 ]]; then log "⚠ 无法连接 origin，dry-run 用本地已有的 origin/main 继续"
  else log "✗ 无法连接 origin（网络/DNS），本次不构建"; exit 1; fi
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
if [[ "$DRY_RUN" == 0 && "$AHEAD" -gt 0 ]]; then
  # Nightly must be built from what is on GitHub; never push main on the user's behalf.
  skip "本地 main 领先 origin/main ${AHEAD} 个提交（请先 push）"
fi

# ---------- 2. decide: skip / resume / new ----------
HEAD_SHA="$(git rev-parse HEAD)"
SHORT_SHA="$(git rev-parse --short HEAD)"
NIGHTLY_SHA="$(git rev-parse -q --verify "refs/tags/${NIGHTLY_TAG}^{commit}" 2>/dev/null || true)"
# Version recorded in the annotated nightly tag ("nightly <version>"); CI reads the same line.
NIGHTLY_TAG_VER="$(git for-each-ref "refs/tags/$NIGHTLY_TAG" --format='%(contents:subject)' 2>/dev/null \
  | grep -oE '[0-9]+\.[0-9]+\.[0-9]+-nightly\.[0-9]{8}' || true)"
BASE_VER="$(cargo_version)"
LAST_FORMAL="$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -1)"

RESUME=0
if [[ -n "$NIGHTLY_SHA" && "$NIGHTLY_SHA" == "$HEAD_SHA" && -n "$NIGHTLY_TAG_VER" && "$FORCE" == 0 ]]; then
  VER="$NIGHTLY_TAG_VER"
  if [[ "$DRY_RUN" == 1 ]]; then
    log "dry-run：HEAD 已是 Nightly $VER，仍按该版本本地构建"
  elif gh release view "$NIGHTLY_TAG" -R "$REPO_SLUG" --json assets --jq '.assets[].name' 2>/dev/null \
        | grep -Fq "${APP_NAME}_${VER}_" \
     && { [[ "$INSTALL" == 0 ]] || [[ "$(defaults read "$INSTALL_PATH/Contents/Info.plist" CFBundleShortVersionString 2>/dev/null)" == "$VER" ]]; }; then
    skip "自上次 Nightly（$VER @ ${SHORT_SHA}）以来没有新提交（--force 可强制重建）"
  else
    RESUME=1
    log "HEAD 即 Nightly $VER，但上次未完成，继续未完成的步骤"
  fi
else
  VER="$(nightly_version "$BASE_VER" "$(date +%Y%m%d)")"
fi
log "Nightly 版本：${VER}（版本源 ${BASE_VER}，上个正式版 ${LAST_FORMAL:-无}，HEAD ${SHORT_SHA}）"

# ---------- 3. tests ----------
if [[ "$RESUME" == 0 ]]; then
  step "tests (same as CI kernel.yml)"
  run_kernel_tests
fi

# ---------- 4. build macOS app + DMG ----------
ARCH="$(uname -m)"; [[ "$ARCH" == "arm64" ]] && ARCH="aarch64"
APP_OUT="$ROOT/target/release/bundle/macos/${APP_NAME}.app"
DMG_OUT="$ROOT/target/release/bundle/dmg/${APP_NAME}_${VER}_${ARCH}.dmg"
if [[ "$RESUME" == 1 && -f "$DMG_OUT" && -d "$APP_OUT" \
      && "$(defaults read "$APP_OUT/Contents/Info.plist" CFBundleShortVersionString 2>/dev/null)" == "$VER" ]]; then
  log "已有本地构建产物 $(basename "$DMG_OUT")，跳过构建"
else
  step "build ${APP_NAME}.app $VER"
  build_macos_app "$VER"
  step "package DMG"
  package_dmg "$VER"
fi

if [[ "$DRY_RUN" == 1 ]]; then
  echo
  log "dry-run 完成：$VER  DMG=$DMG_OUT ($(du -h "$DMG_OUT" | cut -f1))  总耗时 $(( $(date +%s) - START_TS ))s"
  result "dry-run $VER $SHORT_SHA"
  DONE=1; exit 0
fi

# ---------- 5. move nightly tag + GitHub pre-release ----------
# shellcheck source=lib/gh-upload.sh
export GH_REPO_SLUG="$REPO_SLUG"
source "$ROOT/scripts/lib/gh-upload.sh"

step "move tag $NIGHTLY_TAG → $SHORT_SHA"
if [[ "$RESUME" == 0 || "$(git ls-remote origin "refs/tags/${NIGHTLY_TAG}^{}" | cut -f1)" != "$HEAD_SHA" ]]; then
  git tag -f -a "$NIGHTLY_TAG" -m "nightly $VER" HEAD >/dev/null
  # Triggers .github/workflows/desktop.yml in nightly mode (prerelease, version from this tag message).
  git push --quiet --force origin "refs/tags/$NIGHTLY_TAG"
else
  log "远端 $NIGHTLY_TAG 已指向 HEAD，跳过推送"
fi

step "GitHub pre-release $NIGHTLY_TAG"
TODAY="$(date +%Y-%m-%d)"
TITLE="Nightly · $TODAY"
NOTES="$(mktemp)"
{
  echo "每日自动构建 \`$VER\`（提交 \`$SHORT_SHA\`）。可能不稳定，应用内更新只推送正式版本。"
  echo
  echo "## 自 ${LAST_FORMAL:-首个提交} 以来的变更"
  git log --no-merges --format='- %s' "${LAST_FORMAL:+$LAST_FORMAL..}HEAD" \
    | grep -vF "chore(release): " | head -200 || true
  echo
  install_footer
} > "$NOTES"
if gh release view "$NIGHTLY_TAG" -R "$REPO_SLUG" >/dev/null 2>&1; then
  gh release edit "$NIGHTLY_TAG" -R "$REPO_SLUG" --title "$TITLE" --notes-file "$NOTES" \
    --prerelease --latest=false >/dev/null
else
  gh release create "$NIGHTLY_TAG" -R "$REPO_SLUG" --verify-tag --title "$TITLE" --notes-file "$NOTES" \
    --prerelease --latest=false >/dev/null
fi
rm -f "$NOTES"

# Drop assets of older nightlies (versioned file names). Version-less names (latest.json,
# updater archives) are overwritten by CI for the new build.
gh release view "$NIGHTLY_TAG" -R "$REPO_SLUG" --json assets --jq '.assets[].name' 2>/dev/null \
  | grep -E -- '-nightly\.[0-9]{8}' | grep -vF -- "$VER" \
  | while IFS= read -r stale; do
      gh release delete-asset "$NIGHTLY_TAG" "$stale" -R "$REPO_SLUG" --yes >/dev/null && log "已删除旧资产 $stale"
    done || true

gh_upload_asset "$NIGHTLY_TAG" "$DMG_OUT" --clobber
RELEASE_URL="https://github.com/$REPO_SLUG/releases/tag/$NIGHTLY_TAG"

# ---------- 6. update local app ----------
INSTALLED_NOTE="未安装（--no-install）"
if [[ "$INSTALL" == 1 ]]; then
  step "install to $INSTALL_PATH"
  install_local_app "$VER"
fi

# ---------- 7. summary ----------
step "summary"
echo "  版本:     $VER（$SHORT_SHA）"
echo "  Release:  $RELEASE_URL"
echo "  DMG:      $DMG_OUT"
echo "  本机应用: $INSTALLED_NOTE"
echo "  CI:       gh run list -R $REPO_SLUG --workflow=desktop.yml --limit 1（Win/Linux 包 + latest.json）"
echo "  耗时:     $(( $(date +%s) - START_TS ))s   日志: $LOG"
result "built nightly $VER $SHORT_SHA"
DONE=1
