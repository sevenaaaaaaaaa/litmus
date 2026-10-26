# ADR-0002 — 文件为内容真相，op-log 为并发真相

日期：2026-09-18 · 状态：已采纳

## 决策

`Notes/`、`Assets/` 是内容真相，可直接被任何编辑器修改；`.litmus/local/ops/`（Automerge）只承担并发合流，可随时删除重建。

## 理由

- Obsidian/VSCode/脚本可无损互操作，用户不被数据库锁定。
- 库=目录，git-backed 模式（GitHub 生态兼容）零成本成立。
- 崩溃后最多丢内存态，文件与 op-log 双侧都可独立校验收敛。

## 否决的替代方案

- 数据库为唯一真相（Notion 式）：破坏互操作与 git 兼容。
- 纯文件无 CRDT（Obsidian 式）：并发冲突只能整文件覆盖，协作残废。

## 后果

- 需要确定性序列化器（kernel-md），保证文件↔ops 互译稳定、git diff 干净。
- 细节见 docs/spec/consistency.md。
