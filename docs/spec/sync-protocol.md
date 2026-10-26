# Sync Protocol Spec — 同步是内核属性

状态：Phase 1 设计冻结，Phase 0 已有单机落盘

## 原则

同步不是功能。CRDT 从块模型层生效，所有操作离线可用、上线自动合流，不存在"同步 gated 功能"。

## 文档粒度

- 每篇文档（Notes/ 下每个 .md）对应一个 Automerge doc，键为相对路径。
- 库级索引（文档清单、标签、视图规格）为单独的 index doc。
- 附件走 CAS（sha256 内容寻址），与文档 ops 解耦，支持断点续传。

## 持久化：真增量日志（M2 已落地）

- 每次本地变更只追加 `save_after(last_heads)` 的增量字节到 `.litmus/local/ops/<flat>.am`。
- 加载即 `load_incremental` 一次读完拼接的增量流；崩溃安全（读到的必然是完整前缀）。
- 已覆盖测试：增量文件随编辑追加增长、跨进程恢复、双库经协议收敛、跨文档并发完整合流。

## 已知边界（ADR-0004 / ADR-0005）

- **已落地（M4）**：块级合并。文档为扁平标量 schema（`t`/`d:<bid>`/`o:<bid>`），
  顺序由分数索引键派生。并发编辑不同块、并发新增块，双方修改都保留。
- 剩余边界：同一块被两端并发编辑会产生两个块（各自新 bid）——无数据丢失，需人工合并。

## 拓扑矩阵

| 拓扑 | 传输 | 服务器要求 |
|---|---|---|
| 纯本地 | 无 | 无 |
| 多设备 P2P | LAN 直连（sync protocol over WebSocket/TCP） | 无，同网自动发现 |
| 自建服务器 | WebSocket + op-log 落盘 | `litmus-server` Docker 单容器 |
| 对象存储中转 | S3 兼容桶轮询 | 无服务器，NAS/VPS 免了也行 |

四种拓扑共用同一 Automerge sync protocol（交换状态摘要 → 补发缺失变更）。

## E2EE

- per-vault 密钥（machine.toml 本地保存，邀请码分发）。
- op-log 与 blob 双加密，服务器只见密文。
- 自建默认开启；关闭则服务器可做明文索引加速（用户显式选择）。

## GC / Compaction

- 定期把 op-log 快照化：全量快照 + 此后增量。
- tombstone 清理窗口可配（默认 30 天）。
- 旧版本时间线保留策略可配，与发布物版本共用存储。

## server 形态

- `litmus-server`：单二进制/Docker，无状态可横扩（op-log 放对象存储即可多副本）。
- 认证：设备密钥 + vault 邀请令牌；团队角色权限 Phase 2。
- 国内部署 profile：同步与发布服务不同端口/存储桶/账号体系，见 publish-pipeline.md。
