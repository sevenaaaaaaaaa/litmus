# Publish Pipeline Spec — 发布是管道，不是导出

状态：Phase 1 已落地（站点构建 + local/git 目标）；目标适配器见 publish-targets.md

## 管线

```
变更事件 → 增量构建（hash 清单比对，只重建变化资产）→ 合规检查（国内目标）→ 推送目标
```

目标：单篇改动 → 线上可见 < 30s。

## 产物

- AI-HTML 静态页：自包含单文件、语义化标签、内嵌 JSON-LD 块模型数据。
- 站点级：`llms.txt`、sitemap、RSS。
- 图片按图床策略解析为最终 URL（见 consistency.md 引用登记表）。

## 推送目标（适配器）

| 目标 | 说明 |
|---|---|
| 本地目录 / Nginx | 默认，零依赖 |
| S3 兼容 + CDN | OSS / COS / R2 / B2 |
| GitHub Pages | 经 Actions，走 git-backed 模式 |
| Cloudflare Pages / Vercel / Netlify | 海外版 |

## 图床策略（一级公民）

- 粘贴/拖入图片 → CAS 登记 → 按策略转存：vault（默认）/ S3 兼容 / GitHub 仓库+jsDelivr / 自定义上传 API（兼容 PicGo 配置）。
- 文档只引用稳定 ID 的相对路径；换图床不改文档。

## 国内合规钩子（目标必选配置，非可选）

1. 发布前敏感内容机审 API（可接自审服务）。
2. ICP/备案状态检查。
3. 发布日志留痕（append-only event log）。
4. 同步与发布服务物理隔离：独立端口、独立存储桶、独立账号体系。

## 构建缓存

`.litmus/local/publish/` 保存上次发布的 hash 清单；资产 hash 命名，CDN 永久缓存安全。
