# Index & Search Spec — 检索层

状态：M1 已落地

## 架构

- 引擎：SQLite FTS5（bundled，无外部依赖），位于 `.litmus/local/index/index.db`。
- **索引在 sidecar，可随时删除重建**——真相永远是文件，索引只是投影。
- token 有 hash（sha256 内容哈希），hash 未变零成本跳过，O(变更) 而非 O(全库)。

## 分词与中文

- `tokenize='trigram'`：任意 ≥3 字符子串可查（中文/英/混排一致），无需分词器。
- <3 字符查询回退 LIKE 全扫（带转义）。
- 查询转义：引号 doubling，用户输入永远安全。

## 排序

bm25 rank，相关度排序，LIMIT 50。

## 生命周期

| 事件 | 动作 |
|---|---|
| put_doc | upsert 索引 |
| 外部改动（watcher / status / search 前的 sync_all） | hash 比对后 upsert |
| 文件删除 | sync_all 回收索引行 |
| sidecar 删除 | 下次打开自动重建 |

## 混合检索（已落地）

- 两路：FTS5 bm25 + TF-IDF（离线零依赖，CJK 字符二元组）→ RRF 合并。语料按 (文档数, 最大 mtime) 缓存，库没变不重建。
- **三路（配置后启用）**：`litmus.toml` 的 `[ai].embedding_model` 指向任意 OpenAI 兼容 `/embeddings` 端点后，密向量作为第三路并入 RRF（`rrf_merge_n`）。
- 密向量纪律：向量只进 index.db 旁路表 `doc_vectors`，**可整表删除重建**；增量补齐（mtime 比对），watcher 改动 → `vector_stale` → 批量嵌入（16/批）。
- 入口：`POST /rag/reindex`（后台任务，`GET /rag/status` 看进度）、CLI `litmus reindex`、daemon 启动 preheat 自动增量。
- 查询向量在内核锁外取（网络调用不进锁）；embeddings 失败静默降级两路，错误落 `/rag/status`。
- 客户端一致性：不同 embedding 代次混存的向量按维度对齐做余弦，换模型后跑一次 `litmus reindex` 全量重刷（stale 判定含 model 变更）。

## 后续

- 三角洲更新：watcher 增量路径级同步，替代全扫描（当前全扫描，万篇内无感）。
- 规模超万篇 × 千维后：内存 DenseIndex 换 HNSW / sqlite-vec。
