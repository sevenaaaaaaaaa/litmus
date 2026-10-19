# shellcheck shell=bash
# Shared macOS build helpers for scripts/daily-release.sh (Nightly) and scripts/release.sh (formal).
# Expects: ROOT, APP_NAME and a log() function defined by the caller.
#
# Version policy (see CHANGELOG.md):
#   - Single source of truth: [workspace.package].version in Cargo.toml (tauri.conf.json has no
#     version; the Tauri CLI inherits it from the desktop crate).
#   - Formal releases: X.Y.0 (patch X.Y.Z only for urgent fixes), committed + tagged vX.Y.Z.
#   - Nightly: <next minor>.0-nightly.YYYYMMDD, injected at build time only, never committed.
#     Semver pre-release ordering keeps it above every X.Y.* and below the next formal X.(Y+1).0.

# cargo_version: the committed version from Cargo.toml [workspace.package].
cargo_version() {
  python3 - "$ROOT/Cargo.toml" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
m = re.search(r'\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', s, re.S | re.M)
print(m.group(1))
PY
}

# nightly_version <base X.Y.Z> <YYYYMMDD> → X.(Y+1).0-nightly.YYYYMMDD
nightly_version() {
  local ma mi _pa
  IFS=. read -r ma mi _pa <<<"$1"
  echo "${ma}.$((mi + 1)).0-nightly.$2"
}

# numeric_core <version> → X.Y.Z without any pre-release suffix (CFBundleVersion and friends).
numeric_core() { echo "${1%%-*}"; }

setup_macos_build_env() {
  # launchd starts with a minimal PATH. Append (never prepend) so the toolchain matches the
  # interactive shell: Homebrew rustc first, rustup's ~/.cargo/bin only for cargo-tauri etc.
  # Mixing toolchains between runs breaks proc-macro crates ("can't find crate for phf_macros").
  local d
  for d in /opt/homebrew/bin /usr/local/bin "$HOME/.cargo/bin" "$HOME/.local/bin" /usr/bin /bin /usr/sbin /sbin; do
    case ":$PATH:" in *":$d:"*) ;; *) PATH="$PATH:$d" ;; esac
  done
  export PATH
  if [[ "$(uname -s)" == "Darwin" ]]; then
    # New ld (Xcode 27) mis-aligns rustc proc-macro dylibs; classic linker avoids it (see CLAUDE.md).
    case " ${RUSTFLAGS:-} " in *"-ld_classic"*) ;; *) export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-ld_classic" ;; esac
  fi
}

run_kernel_tests() { # same set as CI kernel.yml
  (cd "$ROOT" && cargo test --quiet -p kernel-core -p kernel-store -p kernel-md -p kernel-a2ui -p kernel-sync \
    -p kernel-design -p kernel-deploy -p kernel-browser)
  if [[ -d "$(rustc --print sysroot)/lib/rustlib/wasm32-unknown-unknown" ]]; then
    (cd "$ROOT" && cargo check --quiet -p kernel-md -p kernel-a2ui --target wasm32-unknown-unknown)
  else
    log "⚠ 本机 Rust 无 wasm32-unknown-unknown target，跳过 wasm 可移植性检查（CI kernel.yml 会跑）"
  fi
}

# build_macos_app <version>: builds target/release/bundle/macos/<APP_NAME>.app with the given
# version injected through --config (no tracked file is edited). Sets APP_OUT.
build_macos_app() {
  local ver="$1" core cfg start
  core="$(numeric_core "$ver")"
  APP_OUT="$ROOT/target/release/bundle/macos/${APP_NAME}.app"

  if security find-identity -v -p codesigning 2>/dev/null | grep -q "Developer ID Application"; then
    log "检测到 Developer ID 证书（签名/公证按 Tauri 环境变量配置执行）"
  else
    log "无 Developer ID 证书：跳过 Developer ID 签名与公证，产物为 ad-hoc 签名（DMG 内置打开助手）"
    unset APPLE_SIGNING_IDENTITY APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD \
          APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH
  fi

  # CFBundleShortVersionString = full version (may carry -nightly.YYYYMMDD);
  # CFBundleVersion stays purely numeric. Updater archives are produced and signed by CI.
  cfg="{\"version\":\"$ver\",\"bundle\":{\"createUpdaterArtifacts\":false,\"macOS\":{\"bundleVersion\":\"$core\"}}}"
  start=$(date +%s)
  if ! (cd "$ROOT/desktop" && cargo tauri build --bundles app --config "$cfg"); then
    log "构建失败，按 release 规则清理 proc-macro dylib 后重试一次"
    find "$ROOT/target/release/deps" -name '*.dylib' -delete 2>/dev/null || true
    rm -rf "$ROOT/target/release/.fingerprint"
    (cd "$ROOT/desktop" && cargo tauri build --bundles app --config "$cfg")
  fi
  log "构建耗时 $(( $(date +%s) - start ))s"
  [[ -d "$APP_OUT" ]] || { log "✗ 未找到 $APP_OUT"; return 1; }
  local built
  built=$(defaults read "$APP_OUT/Contents/Info.plist" CFBundleShortVersionString)
  [[ "$built" == "$ver" ]] || { log "✗ 产物版本 $built ≠ $ver"; return 1; }
  if ! codesign --verify --deep "$APP_OUT" >/dev/null 2>&1; then
    codesign --force --deep -s - "$APP_OUT"
    log "已对 ${APP_NAME}.app 做 ad-hoc 签名"
  fi
}

# package_dmg <version>: DMG with the Gatekeeper helper. Sets DMG_OUT.
package_dmg() {
  local arch
  arch="$(uname -m)"; [[ "$arch" == "arm64" ]] && arch="aarch64"
  DMG_OUT="$ROOT/target/release/bundle/dmg/${APP_NAME}_${1}_${arch}.dmg"
  bash "$ROOT/packaging/macos/make-dmg.sh" "$APP_OUT" "$DMG_OUT" "$APP_NAME"
}

# install_local_app <version>: gracefully quit the running app, replace INSTALL_PATH, relaunch.
# Sets INSTALLED_NOTE.
install_local_app() {
  local ver="$1" installed staged was_running=0
  installed=$(defaults read "$INSTALL_PATH/Contents/Info.plist" CFBundleShortVersionString 2>/dev/null || echo none)
  # Same-day rebuilds share the version string; only skip when the executable is byte-identical too.
  local exe
  exe=$(defaults read "$APP_OUT/Contents/Info.plist" CFBundleExecutable 2>/dev/null || echo "")
  if [[ "$installed" == "$ver" ]] && [[ -n "$exe" ]] && cmp -s "$APP_OUT/Contents/MacOS/$exe" "$INSTALL_PATH/Contents/MacOS/$exe"; then
    INSTALLED_NOTE="$INSTALL_PATH 已是 $ver"; return 0
  fi
  app_running() { pgrep -f "$INSTALL_PATH/Contents/MacOS/" >/dev/null 2>&1; }
  if app_running; then
    was_running=1
    log "正在退出 ${APP_NAME}…"
    osascript -e "quit app \"$APP_NAME\"" >/dev/null 2>&1 || true
    for _ in $(seq 1 60); do app_running || break; sleep 1; done
    if app_running; then
      log "✗ $APP_NAME 60 秒内未退出（可能有未保存内容），未替换本机应用"
      return 1
    fi
  fi
  staged="$(dirname "$INSTALL_PATH")/.${APP_NAME}.app.new"
  rm -rf "$staged"
  ditto "$APP_OUT" "$staged"
  xattr -dr com.apple.quarantine "$staged" 2>/dev/null || true
  rm -rf "$INSTALL_PATH"
  mv "$staged" "$INSTALL_PATH"
  INSTALLED_NOTE="${INSTALL_PATH}（$installed → ${ver}）"
  if [[ "$was_running" == 1 ]]; then open "$INSTALL_PATH"; log "已重新启动 $APP_NAME"; fi
}

# install_footer: shared install-help footer for Release notes.
install_footer() {
  echo "### macOS 首次安装"
  echo "DMG 未经 Apple 公证。若提示「已损坏，无法打开」，请双击 DMG 里的「打不开？双击我.command」，详见「安装说明.txt」。"
  echo
  echo "Windows / Linux 安装包与应用内更新文件（latest.json）由 CI 构建后自动附加到本 Release。"
}
