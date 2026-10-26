# 发布目标 — 从库到线上

## 一、两段式：构建 → 推送

```
库（文件真相）→ build_site()  → 站点目录（自包含 AI-HTML 站点）→ deploy(target) → 线上
                  · 每篇文档 = 一个 AI-HTML 页（套用有效设计规范）
                  · index.html / llms.txt / sitemap.xml / manifest.json
                  · Assets/ 原样镜像（文档里的相对引用直接可用）
```

`manifest.json` 记录每个文件的 sha256：增量判断、审计、回滚的依据。
再次构建时，上一版清单里消失的文件会被删除（`.git` 之类保留）。

## 二、目标适配器

| kind | 状态 | 说明 |
|---|---|---|
| `local` | ✅ 已落地 | 站点落在 sidecar，daemon 直接托管；用于本地预览/离线分发 |
| `git` | ✅ 已落地 | 站点目录变成 git 仓库并 push 到分支 —— **GitHub Pages 与 Cloudflare Pages 的 Git 集成都走这条** |
| `s3` | ✅ 已落地 | S3 / R2 / OSS / COS / MinIO：SigV4 直传，path-style + prefix，按内容哈希增量 |
| `cf-pages` | ✅ 已落地 | 直传 API：multipart（manifest 按哈希映射、同内容文件去重上传）；整站指纹未变则跳过 |

### 配置（litmus.toml）

```toml
[publish]
site_dir = ".litmus/local/site"          # 构建目录
default_target = "gh-pages"
base_url = "https://kb.example.com" # 有它才生成 sitemap.xml / 复制链接

[[publish.targets]]
name = "local"
kind = "local"
dir = ".litmus/local/site"

[[publish.targets]]
name = "gh-pages"                  # GitHub Pages
kind = "git"
remote = "git@github.com:me/kb.git"
branch = "gh-pages"
force = true                       # 生成站点目标默认允许强推

# Cloudflare Pages：在 CF 面板连同一个仓库，分支填 gh-pages 即可
# [[publish.targets]]
# name = "cf-pages-git"
# kind = "git"
# remote = "git@github.com:me/kb.git"
# branch = "gh-pages"

[[publish.targets]]                # S3 / R2 / OSS / COS / MinIO 通用
name = "r2"
kind = "s3"
endpoint = "https://<account>.r2.cloudflarestorage.com"
bucket = "kb"
region = "auto"                    # AWS 用真实 region，R2 用 auto
prefix = "site"                    # 可选，对象键前缀
access_key_env = "R2_ACCESS_KEY_ID"      # 从环境变量读，不落盘
secret_key_env = "R2_SECRET_ACCESS_KEY"
public_base_url = "https://kb.example.com"   # 复制链接/Sitemap 用
```

`s3` 目标细节：
- **path-style** 请求（`{endpoint}/{bucket}/{key}`），兼容 R2/MinIO/OSS/COS 等；固定走 `PUT` 对象。
- 每对象带 `x-amz-content-sha256`（真实负载哈希）与 `x-amz-date`，`content-type` 按扩展名。
- **增量**：`DeployState` 记录每个路径的内容哈希，未变即跳过，不产生请求。
- 密钥只从环境变量读取，**任何情况下不写入库配置**。

`git` 目标细节：
- 仓库不存在则 `git init -b <branch>`；远端 URL 变了自动 `set-url`。
- 提交身份用 `-c user.name=litmus -c user.email=litmus@localhost`，**不碰你的全局 git 配置**。
- 无变更（`git status --porcelain` 为空）→ 不产生空提交，报告「无变更」。
- 凭据交给 git 自己（SSH key / credential helper / GITHUB_TOKEN），**我们不存任何密钥**。

## 三、接口

| 入口 | 用法 |
|---|---|
| CLI | `litmus publish <vault> [--target 名称] [--build-only]` |
| HTTP | `GET /publish/targets` 目标与站点目录 |
| HTTP | `POST /publish/site {target?}` 只构建，返回文件数与预览 |
| HTTP | `POST /publish/deploy {target}` 构建并推送，返回 `{uploaded, skipped, detail, url}` |
| HTTP | `POST /publish {path}`（原有）单篇渲染成 AI-HTML 预览产物 |
| 客户端 | 发布面板：目标列表 + 「构建」/「部署」按钮 + 报告；单页产物链接 |

## 四、验证方式（真跑，不是纸面）

- `kernel-deploy` 单测：**真建一个裸仓库**，部署 → `git show gh-pages:index.html` 能看到内容；
  再次部署报告「无变更」；改内容后再部署，裸仓库内容更新。
- **SigV4 黄金向量**：`tests/vectors/sigv4.json` 由本机 **botocore 1.42.97**（AWS 官方 Python SDK）
  在固定时间与 AWS 测试凭据下生成（含中文与空格/`+`/`~` 路径）。Rust 实现逐条比对
  canonical request / string-to-sign / signature / Authorization，并断言我们的 URI 编码器
  与 botocore 完全一致。（其中 `GET /` 的签名与 AWS 官方 SigV4 测试套件 `get-vanilla` 相同。）
- **S3 集成测试**：本地起极简 HTTP 服务当"桶"，验证每次 PUT 都带 SigV4 与内容哈希、
  path-style + prefix 正确、内容未变时不再发请求、改一个文件只传一个。
- 端到端（真实进程 + 活 mock 桶）：`litmus publish --target r2-mock` → **16 个签名 PUT**，
  对象键形如 `/kb/site/agent-%E7%9A%84-html.html`；二次发布 0 请求。
- HTTP：`/publish/site` 返回 15 文件 183KB；`/publish/deploy` 返回 `uploaded=16`。

## 五、下一步

| 里程碑 | 内容 |
|---|---|
| PUB-1 | 站点构建器 + local/git 目标 + CLI/HTTP/客户端 | ✅ 已落地 |
| PUB-2 | S3 兼容 SigV4 直传（增量去重） | ✅ 已落地 |
| PUB-3 | Cloudflare Pages 直传（多部分上传 + manifest 哈希） | ✅ 已落地 |
| PUB-4 | 发布前检查：ICP 必填+页脚注入、敏感词、内链/图片完整性、外部机审 API、审计留痕 | ✅ 已落地 |
| PUB-5 | 变更触发自动发布（watcher 驱动，审计留痕） | ✅ 已落地 |

## 六、PUB-3/4/5 实现记录

**PUB-3 Cloudflare Pages 直传**：`POST /client/v4/accounts/{aid}/pages/projects/{p}/deployments`，
multipart 里 `manifest` = `{路径: sha256}`，文件 part 以内容哈希命名（**同内容文件只传一次**）。
整站指纹未变不建部署。`api_base` 可改（测试与国内网络）。
实测：本地 mock 校验了 manifest 存在、bearer 令牌、同内容去重、指纹跳过、变更重建。

**PUB-4 发布前检查**（server 与 CLI 都接入，阻断级问题不推送）：
- 国内目标 `domestic = true` → `icp` 必填，构建时自动注入页脚
- 敏感词表（`checks.sensitive_words`）扫站点 HTML，命中即阻断
- 内链/图片完整性：HTML 里 href/src 引用的本地文件必须存在
- 外部机审 API（`checks.audit_api`）：POST 文件清单哈希 → `{ok, reasons}`；不可达可配 warn/block
- **审计留痕**：每次发布（含检查结果）追加 `events/publish.jsonl`

**PUB-5 自动发布**：`[publish] auto = true` → daemon 的 watcher 合入外部变更后
自动构建并推送到默认目标（审计记 `auto-publish`）。
实测：daemon 运行中外部追加一段文字 → gh-pages 裸仓库出现新提交，内容含新段落。

**踩坑**：单页预览产物目录（`publish/`）与站点目录（`site/`、`deploy/`）是两套产物；
合规检查扫的是部署目录——旧版本构建的残留页也会被查到，清掉即可。
