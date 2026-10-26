# 对标 WeKnora（腾讯开源知识库）与运行就绪

对标对象：**tencent/WeKnora**（开源知识库框架）。
我们采纳其**产品基础能力**的方向，同时保持 Litmus 的差异化（文件真相 + 块级 CRDT + AI-HTML + 设计规范）。

## 从 WeKnora 学到的（落地映射）

| WeKnora 能力 | Litmus 现状 | 计划 |
|---|---|---|
| 多格式文档解析（PDF/DOCX/图片OCR） | md/html/图片 | DOC-1: PDF/DOCX 解析管道（采集管道扩展） |
| 混合检索（BM25+向量+重排） | FTS5 trigram ✅ | RAG-1: sqlite-vec 混合召回 + Rerank |
| 知识图谱/引用溯源 | 引用登记表 + provenance ✅ | 图谱视图 |
| **容器化部署** | **Dockerfile + compose ✅** | 镜像发布 |
| **运行监测（Prometheus）** | **/metrics ✅（docs/assets/captured/publishes）** | Grafana 面板模板 |
| **审计** | events/publish.jsonl ✅ | 操作级事件全覆盖（UX-6 后） |
| 批量导入/导出 | CLI put/ingest | 批量向导 |

## 部署双轨（用户建议，已采纳）
1. **愿装本地包**：DESK-1 Tauri 安装包（期 10-03）——内嵌内核，离线全功能。
2. **不愿装**：浏览器扩展 MV3（09-26）+ **PWA**（WEB-1，10-24，WASM 内核 + OPFS）+ **CDP 联动**（M8）实现高级能力（已登录页面采集、页面内操作）。
3. **自托管**：Docker（本仓库 Dockerfile/compose）或裸机 systemd（deploy/）。
