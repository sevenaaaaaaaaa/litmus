# 在线更新（桌面端自更新 + 发布流程）

状态：已落地（Tauri updater，签名发布）。浏览器版随服务器更新；桌面端支持应用内一键自更新。

## 一、桌面端自更新

### 组成

| 部件 | 位置 |
|---|---|
| 更新器插件 | `desktop/Cargo.toml` → `tauri-plugin-updater` |
| 更新源 / 公钥 | `desktop/tauri.conf.json` → `plugins.updater` |
| 后端命令 | `desktop/src/lib.rs` → `desktop_check_update` / `desktop_install_update` / `desktop_app_version` |
| 前端入口 | 知识场 `/更新`，或桌面菜单「检查更新」（`server/web/field.html` 的 `openUpdate`） |
| 构建开关 | `desktop/tauri.conf.json` → `bundle.createUpdaterArtifacts = true` |

流程：`关于` → `检查更新` → 命中新版本则显示版本号与说明 → `下载并安装` → 下载完成后 `app.restart()` 重启加载新版。

### 签名密钥（务必妥善保管）

自更新包用 minisign 私钥签名，客户端内置公钥校验，防止被替换。

- 私钥：`~/.tauri/litmus-updater.key`（**不要提交进仓库**）
- 公钥：已写入 `tauri.conf.json` 的 `plugins.updater.pubkey`

生成（若需重做，会同时更换公钥，旧客户端将无法收到更新）：

```bash
cargo tauri signer generate -w ~/.tauri/litmus-updater.key
```

### CI 必需的仓库 Secret

在 GitHub 仓库 Settings → Secrets and variables → Actions 添加：

| Secret | 值 |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | `~/.tauri/litmus-updater.key` 文件全文 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 私钥口令（本仓库生成时为空，填空字符串） |

未配置这两个 Secret 时，`v*` 标签构建会在签名步骤失败——这是刻意的（不能让未签名包混进更新通道）。

### 发布流程

推 `v*` 标签（版本号唯一来源是 `Cargo.toml`），
`.github/workflows/desktop.yml` 经 `tauri-action` 完成：构建 → 签名 → 建 Release →
生成 `latest.json`（含各平台签名与下载地址）。客户端按 `latest.json` 比对版本。

### 更新源

默认指向 GitHub Releases 的 `latest.json`：

```
https://github.com/sevenaaaaaaaaa/litmus/releases/latest/download/latest.json
```

迁到 R2 后改为 R2 自定义域名下的 `latest.json`（见下）。

## 二、静态资源迁 Cloudflare R2

### 可行性结论

- **可迁**：发布产物（dmg/msi/AppImage/deb/rpm + updater 归档 + `latest.json`）、已发布站点、插件索引等纯静态资源。
- **不可迁**：运行中的后端（axum + WebSocket + SQLite + automerge + 索引 + AI）。R2 是对象存储，无计算；要迁只能重写为 Workers/Containers，代价过大。
- **稳定性**：源站在洛杉矶、面向国内用户时，走 Cloudflare 网络（R2 自定义域名必经 CF）对国内访问可能更慢/更不稳，与 [ADR-0006](adr/0006-direct-origin-no-cdn.md)「直连、不用 CF 代理」相悖。海外用户则更快更省（R2 零出网费）。

### 迁移步骤

1. Cloudflare 控制台 → R2 → 新建 bucket（如 `litmus`）。
2. 绑定自定义域名（如 `dl.nownexts.com`），用于公网读取。
3. 创建 R2 API Token（读+写该 bucket），得到 Access Key ID / Secret / endpoint。
4. 同步产物：

   ```bash
   export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
   export R2_ENDPOINT=https://<account>.r2.cloudflarestorage.com R2_BUCKET=litmus
   deploy/r2-sync.sh dist            # dist 含 dmg/msi/*.sig/*.tar.gz/latest.json
   ```

5. 把 `tauri.conf.json` 的 `plugins.updater.endpoints` 改为 R2 上的 `latest.json` 地址。
6. （可选）在仓库 Settings → Variables 设 `R2_ENABLED=true`，并加 Secrets
   `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY` / `R2_ENDPOINT` / `R2_BUCKET`，
   CI 会在构建后自动同步到 R2（见 `desktop.yml` 的「同步到 R2」步骤）。

### 已发布站点

站点发布已内置 `s3` 目标，直接指向 R2 即可（见 [publish-targets.md](publish-targets.md)）：

```toml
[[publish.targets]]
name = "r2"
kind = "s3"
endpoint = "https://<account>.r2.cloudflarestorage.com"
bucket = "litmus"
region = "auto"
prefix = "site"
access_key_env = "R2_ACCESS_KEY_ID"
secret_key_env = "R2_SECRET_ACCESS_KEY"
public_base_url = "https://dl.nownexts.com"
```
