#!/bin/bash
# Gatekeeper helper shipped inside the macOS DMG.
# Removes the com.apple.quarantine attribute from @APP_NAME@.app so macOS stops
# reporting "已损坏，无法打开". No network access, no other system changes.
# @APP_NAME@ is substituted by packaging/macos/make-dmg.sh at packaging time.
set -u

APP_NAME="@APP_NAME@"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

say() { printf '%s\n' "$*"; }
finish() {
  say ""
  read -r -n 1 -s -p "按任意键关闭此窗口…" _ 2>/dev/null || true
  say ""
  exit "${1:-0}"
}

say "=============================================="
say "  ${APP_NAME} 打开助手"
say "=============================================="
say ""

# Prefer the copy in /Applications, then ~/Applications, then the one next to this script.
APP=""
for cand in "/Applications/${APP_NAME}.app" "$HOME/Applications/${APP_NAME}.app" "${SCRIPT_DIR}/${APP_NAME}.app"; do
  if [ -d "$cand" ]; then APP="$cand"; break; fi
done

if [ -z "$APP" ]; then
  say "没有找到 ${APP_NAME}.app。"
  say "请先把 ${APP_NAME}.app 拖进「应用程序」(Applications) 文件夹，再双击本脚本。"
  finish 1
fi

case "$APP" in
  "${SCRIPT_DIR}/"*)
    say "提示：${APP_NAME} 还没拖进「应用程序」文件夹，将直接从安装盘打开。"
    say "      建议先拖进「应用程序」，以后才能正常自动更新。"
    say ""
    ;;
esac

say "找到应用：$APP"
say "正在移除 macOS 的「隔离」标记（com.apple.quarantine）…"

if xattr -dr com.apple.quarantine "$APP" 2>/dev/null; then
  say "✓ 已移除隔离标记"
else
  say ""
  say "当前账户没有权限修改该应用（通常是它由其他用户安装）。"
  say "接下来会请求管理员密码，仅用于执行这一条命令："
  say "  sudo xattr -dr com.apple.quarantine \"$APP\""
  say "（输入密码时屏幕不显示字符，属正常现象）"
  if sudo xattr -dr com.apple.quarantine "$APP"; then
    say "✓ 已移除隔离标记"
  else
    say "✗ 移除失败。可以在「终端」里手动执行上面那条命令。"
    finish 1
  fi
fi

# Only re-sign ad-hoc when the existing signature is broken.
if ! codesign --verify --deep "$APP" >/dev/null 2>&1; then
  say "签名校验未通过，正在做本机临时签名（ad-hoc）…"
  if codesign --force --deep -s - "$APP" >/dev/null 2>&1 \
     || sudo codesign --force --deep -s - "$APP"; then
    say "✓ 已完成本机签名"
  else
    say "⚠ 本机签名失败，仍尝试打开。"
  fi
fi

say ""
say "正在打开 ${APP_NAME}…"
if open "$APP"; then
  say "✓ 完成！以后直接从「应用程序」或启动台打开即可，无需再运行本脚本。"
  finish 0
else
  say "✗ 打开失败。请在「系统设置 → 隐私与安全性」中点「仍要打开」。"
  finish 1
fi
