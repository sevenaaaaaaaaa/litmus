# 多端发布规划（Web / macOS / Windows / Linux / Android / iOS）

> 原则：**一套内核 + 一套前端 + 一张能力表**。任何改动都要在「能力表」里标注对各端的影响，
> 不允许出现「线上改了、客户端忘了」或「mac 改了、win 忘了」。

## 0. 架构：一份代码，多种外壳

```
            ┌──────────────── 同一份 server/web（单文件前端）────────────────┐
            │                                                                 │
   Web ─────┤  kernel-core / kernel-store / kernel-sync / kernel-md ...       │
   macOS ───┤  litmus-server（axum daemon，唯一业务真相）                     │
   Windows ─┤      ↑ /desktop/* 仅在 LITMUS_DESKTOP=1 时可用                  │
   Linux ───┤                                                                 │
   Android ─┤                                                                 │
   iOS ─────┘                                                                 │
```

- 业务只写一次：内核（Rust）+ 前端（`server/web/field.html`，知识场）。
- 各端差异**只**存在于「外壳」：窗口/权限/进程/系统集成。
- 桌面端不复制前端：webview 直接加载本地 daemon（`http://127.0.0.1:{随机端口}/?token=`）。

## 1. 各端形态与优先级

| 端 | 形态 | 优先级 | 说明 |
|---|---|---|---|
| Web | 浏览器 | P0 | 线上 `kb.nownexts.com`，能力上限（无 `/desktop/*`） |
| macOS | Tauri `.app`/`.dmg` | P0 | 已交付；本机内容/算力/终端 |
| Windows | Tauri `.msi`/`.exe` | P1 | `textutil` → `pandoc`/PowerShell COM 或 `dotext` |
| Linux | Tauri `.AppImage`/`.deb` | P1 | 抽取层用 `pandoc`/`libreoffice`/`pdftotext` |
| Android | Tauri v2 移动 或 PWA | P2 | 轻客户端：浏览/记录/语音；重活回本地 daemon |
| iOS | Tauri v2 移动 或 PWA | P2 | 同上，受 App Store 限制更多 |

**抽取层必须可插拔**（`extract_text()` 按扩展名/平台探测可用工具），
不同系统用不同外部程序，缺工具时降级为 `pending_transcode`，绝不让转码失败拖垮导入。

## 2. 能力矩阵（改任何功能都要更新此表）

| 能力 | Web | macOS | Windows | Linux | Android |
|---|---|---|---|---|---|
| 视图/编辑/检索/发布 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 登录 + 远端拉取 | ✅（同源） | ✅ | ✅ | ✅ | ✅ |
| 本机目录导入 | ❌ | ✅ | ✅ | ✅ | 受限 |
| 文档转码（OKF） | ❌ | ✅ | ✅ | ✅ | ❌ |
| 去重/去噪扫描 | ❌ | ✅ | ✅ | ✅ | ❌ |
| 内嵌终端 | ❌ | ✅ | ✅ | ✅ | ❌ |
| 本地模型/嵌入 | ❌ | ✅ | ✅ | ✅ | ⚠️ 设备级 |
| 系统集成（快捷键/托盘） | ❌ | ✅ | ✅ | ✅ | ⚠️ |

## 3. 发布与版本

- **单一版本号**：`Cargo.toml` workspace `version`，全端共用；每次发布打 tag `vX.Y.Z`。
- **单一真相**：GitHub `main`。线上与各端都从同一 commit 构建。
- **发布清单（Runbook）**——每完成一个功能，按此逐项走：
  1. `cargo test` 全绿。
  2. 若改了 `server/web`：确认 `BASE` 前缀、登录门禁顺序（token 早于 DOM 绑定）、SW 缓存名 bump。
  3. 若改了内核/服务端：本地起 daemon 冒烟（含 `/desktop/*` 与线上 404 门禁）。
  4. 构建并验证各端：`cargo build --release -p litmus`（线上）→ 部署；`-p litmus-desktop` → 打包；Windows/Linux 在 CI 出包。
  5. 更新 `docs/spec/platform.md` 的能力矩阵与 `docs/RELEASE.md`。
  6. 提交信息里写明「影响端」。

## 4. CI（目标）

- GitHub Actions 矩阵：`macos-latest` / `windows-latest` / `ubuntu-latest`。
- 每个 job：`cargo test` + `cargo tauri build`，产物上传 Release（打 tag 时）。
- 线上部署仍由服务器 `update.sh` 从 `main` 拉取构建（当前），后续可换成 CI 产物直发。

## 5. 约定（避免「忘了某端」）

- **改动自检**：提交前问三个问题——Web 能跑吗？桌面能跑吗？能力矩阵更新了吗？
- **不写端专属业务**：需要平台差异时，收敛到 `extract_text`、外壳配置、`/desktop/*` 门禁三处。
- **降级优先**：任何独占能力缺失时，前端 `try/catch` + 能力探测（`/desktop/info`），按钮隐藏而非报错。
- **文档即契约**：本文件 + `desktop.md` + `PRODUCT.md` 是发布的检查依据。
