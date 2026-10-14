# CLAUDE.md — ThirdC（鹿蕊 Litmus）

## 项目简介

鹿蕊 Litmus（开发代号 `thirdc`，林下 Understory 出品）：本地优先的「知识操作系统」。数据是磁盘上的普通 Markdown/HTML/附件，Rust 内核负责存储与索引，`thirdc serve` 为守护进程，提供 Web UI（五模式：Flow 画布 / Feynman / Memo / Agent / Studio）、HTTP API + MCP、CLI、Tauri 桌面/移动客户端、浏览器扩展。远端 `github.com/sevenaaaaaaaaa/litmus`，许可 AGPL-3.0。

## 技术栈

- Rust workspace（edition 2021，Rust 1.85+），`Cargo.lock`；axum + tokio、rusqlite（bundled）、automerge、reqwest（rustls）
- 桌面/移动：Tauri v2（`desktop/`，productName `Litmus`，自更新走 GitHub Releases `latest.json`）
- 前端：纯静态 `server/web/`（无 Node 构建）
- 浏览器扩展：`extension/`（Chrome/Safari）、`extension-firefox/`

## 目录结构

- `kernel/crates/` — 内核 crates（kernel-core / md / store / sync / design / browser / a2ui / deploy / backup / convert）
- `server/` — `thirdc` 二进制（serve 守护进程 + Web UI `server/web/`）
- `cli/`、`mcp/` — CLI 与 MCP server；`desktop/` — Tauri 应用（`desktop/gen/` 为生成物）
- `packaging/macos/` — DMG 打包脚本与 Gatekeeper 打开助手模板、launchd 定时模板
- `deploy/` — 服务器部署脚本、systemd unit、Apache 反代配置、R2 同步
- `scripts/` — 每日发版入口 `daily-release.sh`、手动发版 `release.sh`、性能门禁 `perf-check.sh`、各类一次性 patch 脚本
- `docs/` — 产品/设计规范（`docs/spec/`）、ADR、性能契约 `docs/PERF.md`
- `third_party/swift-rs` — vendored 补丁；`_cowork/` — 本地补丁/bundle（gitignored）

## 常用命令

```bash
# 构建 / 运行
cargo build --release -p thirdc
./target/release/thirdc init ./kb MyKnowledge
./target/release/thirdc serve ./kb --addr 127.0.0.1:7700
docker compose up -d                       # 7700 端口

# 测试（同 CI kernel.yml）
cargo test -p kernel-core -p kernel-store -p kernel-md -p kernel-a2ui -p kernel-sync -p kernel-design -p kernel-deploy -p kernel-browser
cargo check -p kernel-md -p kernel-a2ui --target wasm32-unknown-unknown
bash scripts/perf-check.sh                 # 性能预算门禁

# 桌面 / 移动
cd desktop && cargo tauri build
source .android-env.sh && cd desktop && cargo tauri android build --apk --target aarch64
```

lint：无专门配置（可用 `cargo clippy`）。

## 部署方式

- **服务端**：宝塔服务器 `root@172.96.253.73`（SSH 端口 `28766`），目录 `/www/wwwroot/thirdc`，systemd 服务 `thirdc` 监听 `127.0.0.1:7700`，Apache 以 `Alias/ProxyPass /thirdc` 挂到 https://nownexts.com/thirdc/ （`deploy/thirdc.conf`）。
  - **唯一入口（双轨）**：`./deploy/deploy.sh [-m "提交信息"] [--skip-build] [--server-only] [--dry-run]`
    - GitHub 轨（公开仓库，只放干净代码）：gitleaks 扫描暂存区 → `git add -A`（`.gitignore` 已排除 keystore、签名口令、`server-kb/`、`.deploy-ssh/` 等）→ commit → `git push origin main`；无变更则跳过 commit
    - 服务器轨：从工作区 rsync 源码到 `/www/wwwroot/thirdc/src/`（沿用旧排除项与 `--delete`，另排除 `*.keystore`、`desktop/android-signing.properties`）→ 服务器 cargo build → 重启 systemd
    - `--server-only` 跳过 git；`--dry-run` 只做 `rsync -n` 预演，不提交/推送/构建/重启
  - 本地 `.git/hooks/pre-commit` 会跑 gitleaks，发现密钥即阻止提交
  - Android 签名口令：环境变量 `TAURI_KEYSTORE_PASSWORD` / `TAURI_KEY_PASSWORD`，或 gitignored 的 `desktop/android-signing.properties`；keystore `desktop/thirdc-release.keystore` 仅本地保存（已移出 git 跟踪）
  - 服务器上自更新：`/www/wwwroot/thirdc/update.sh`（从 GitHub `main` 拉取 → 构建 → 重启）
  - 备选：`deploy/native.sh`；推荐用 CI `server.yml` 产出的 musl 静态二进制替换 `bin/thirdc`，避免生产机 cargo OOM。
- **桌面发版**：日常用 `scripts/daily-release.sh`（见下文「发版（每日）」）；手动指定版本用 `scripts/release.sh vX.Y.Z` → 推 `v*` tag 触发 `.github/workflows/desktop.yml`（三平台产物 + `latest.json` → GitHub Release，可选同步 R2）。先 bump `desktop/tauri.conf.json` 的 `version`；Android 需手动构建上传。详见 `.cursor/rules/release.mds`。
- CI：`kernel.yml`、`server.yml` 在 push `main` 时触发（server 限相关路径），`desktop.yml` 由 tag 或手动触发。

## 发版（每日）

**硬性规则：每天开发结束必须跑一次 daily-release（或由 launchd 自动跑）。**

```bash
scripts/daily-release.sh              # 正式发版
scripts/daily-release.sh --dry-run    # 只测试 + 本地打 app/DMG，不 commit/tag/push/release/安装
scripts/daily-release.sh --no-install # 发版但不替换本机 /Applications/Litmus.app
scripts/daily-release.sh --force      # 自上个 tag 无新提交也发
```

- 流程：必须在 `main` 且工作区干净（不干净则打印改动并以「skipped」退出 0，绝不替你提交 WIP）→ `git fetch`，落后则 `pull --rebase --autostash`（冲突即 abort 并失败退出）→ 跑内核测试（同 `kernel.yml`；本机无 wasm32 target 时跳过 wasm 检查）→ 自上个 `v*` tag 无新提交则跳过 → patch 版本 +1，同步写入 `Cargo.toml`（`[workspace.package]`）、`desktop/tauri.conf.json`、`Cargo.lock` → 提交 `chore(release): vX.Y.Z` + 附注 tag → 本地 `cargo tauri build --bundles app`（自动带 `-ld_classic`）+ `packaging/macos/make-dmg.sh` 打 DMG → push main 与 tag → `gh release create`（说明 = 上个 tag 以来的提交标题）并上传 DMG → 优雅退出本机 Litmus、`ditto` 替换 `/Applications/Litmus.app`、去隔离、原来在运行则重启。
- 中途失败可直接重跑：HEAD 若已是 `chore(release)` 提交会续跑未完成的步骤；全部完成则跳过。日志：`~/Library/Logs/litmus-daily-release.log`。
- CI 分工：推 tag 后 `desktop.yml` 仍负责 Windows/Linux 包、签名的 macOS 更新包 `Litmus_aarch64.app.tar.gz` 与 **`latest.json`（应用内更新靠它，不能缺）**。macOS 侧 CI 只打 `.app`，再用同一个 `make-dmg.sh` 出 DMG；若 Release 上已有本地上传的同名 DMG 则跳过，不覆盖。
- **macOS 打开助手**：本机只有 Apple Development 证书，无 Developer ID / 公证，DMG 为 ad-hoc 签名。每个 DMG 内含 `Litmus.app`、`Applications` 链接、`打不开？双击我.command`（去 `com.apple.quarantine`，签名损坏时 ad-hoc 重签，然后打开应用）、`安装说明.txt`。模板在 `packaging/macos/`（`open-helper.command`、`install-readme.txt`，`@APP_NAME@` 打包时替换）。
- 定时：`packaging/macos/com.nownexts.litmus.daily-release.plist`（每天 23:30，安装方法见文件头注释，需手动 `launchctl bootstrap`）。
- `scripts/release.sh vX.Y.Z` 仍可用于手动指定版本发版 / `--local` 补救。

## 笔记数据（硬性规则）

- 本地主力笔记库：`~/Documents/Litmus`；服务器笔记库：`/www/wwwroot/thirdc/kb`（systemd `thirdc serve` 打开的 vault）。两边由 Litmus 自身的同步能力保持双向最新，不要另起 rsync/cron 去覆盖任何一侧。
- 冲突策略：两边都保留，同名不同内容另存冲突副本，绝不静默覆盖。
- **GitHub（公开仓库 `sevenaaaaaaaaa/litmus`）上一篇真实笔记都不能有**：代码仓库里的 `kb/`、`server-kb/` 已 gitignore；部署脚本排除 `/kb`、`server-kb`、`.thirdc`，且只同步到服务器 `src/`，不碰服务器 `kb/`。笔记库的 git 远端只能指向自己的服务器，禁止指向 GitHub。
- 笔记库里的 `.thirdc/`（op-log、索引、`machine.toml` 设备密钥）永不出设备；`gitops::ensure_repo` 会自动写入 `.gitignore` 保证这一点。
- 2026-10-14 已核查：代码仓库历史中不含真实笔记（仅 `examples/plugins` 示例）。

## 注意事项

- 环境变量（仅名称）：`THIRDC_VAULT`、`THIRDC_LISTEN`、`THIRDC_HOST`、`THIRDC_AI_API_KEY`、`THIRDC_BACKUP_PASSPHRASE`、`THIRDC_WEBDAV_USER`、`THIRDC_WEBDAV_PASS`（或 `WEBDAV_USER`/`WEBDAV_PASS`）、`THIRDC_CHROME`、`THIRDC_PYTHON`、`THIRDC_EXTRACT_SCRIPT`；CI Secrets：`TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`、`R2_ACCESS_KEY_ID`、`R2_SECRET_ACCESS_KEY`、`R2_ENDPOINT`、`R2_BUCKET`。
- 勿读取/提交：`.deploy-ssh/`、`deploy/.ssh_key`、`server-kb/`（含登录 token）、`kb/.thirdc/machine.toml`、`desktop/thirdc-release.keystore`、`desktop/android-signing.properties`、`~/.tauri/thirdc-updater.key`。
- 本机新版 ld 会导致 proc-macro dylib 不对齐，本地构建可能需 `-ld_classic`；本机 DNS 拦截 `uploads.github.com` 等（见 release 规则）。
- 存在本地分支 `design/5-craft-bold`、`integrate/seveno`。
