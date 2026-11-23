---
title: P1-04 · 推进闭环协议 + 端点核实清单
date: 2026-11-23
status: done
tags: [执行, 闭环, 端点, 幂等]
---

# P1-04 · 推进闭环协议

> 看板 DoD：幂等键算法写死；取消零写入；连回可撤销；与现有端点的差距逐条列出。

## 闭环八步（每一格都有不变量）

```
范围 scope → 意图 intent → 候选 candidate → 来源 citations
   → 核验 verify → 确认 confirm → 落盘 commit（+ 连回）→ 撤销 undo
```

| 步 | 状态 | 不变量 | 怎么证明 |
|---|---|---|---|
| 范围 / 意图 | `open` | 选区快照 + 一句人话，不猜 | runKey 由两者算出 |
| 候选 | `candidate` | **不写盘** | Notes 快照前后相同（有测试） |
| 核验 | `verified` / `blocked` | 没来源、引文对不上 → 不许落盘 | 复用 P1-11 的 `verifyClaim` |
| 确认 | — | 只有用户确认才进 commit | 未核验的 run 调 commit 直接抛错 |
| 落盘 | `committed` | 不覆盖同名文件；账本记一笔 | 同名文件已存在 → 抛错，不覆盖 |
| 连回 | — | 只追加，不改写来源卡 | 追加一行 wikilink，改前先备份 |
| 撤销 | `undone` | 用户文件逐字节还原 | Notes 快照与落盘前 `same: true` |
| 取消 | `cancelled` | **零写入**（连账本都不产生） | Notes 快照相同 + 账本文件不存在 |

## 幂等键：写死，不留解释空间

```
runKey   = sha256( space | 排序后的锚点 | intent | 去空白的意图原文 | mode )[0:24]
writeKey = sha256( runKey | 去空白的候选正文 )[0:24]
```

三条硬约束：

1. **锚点先排序**再进哈希——同一个选区，选中的顺序不同不算两件事（有测试）。
2. **意图原文参与哈希**——用户改了那句话就是另一件事，不能顶替。
3. **候选正文进 writeKey**——改过候选就是另一张卡，不能复用原卡。

## 闸门：什么能落盘

| 核验结果 | 能落盘吗 | 卡片状态 |
|---|---|---|
| `grounded`（有逐字命中） | 可以 | `grounded` |
| `weak`（只有转述） | 可以，但带提醒 | `seed` |
| `inferred`（只有推断） | 可以，但标「含推断」 | `seed` |
| `ungrounded`（没有来源） | **不行**——它不是推进结果，只是一张新种子卡 | — |
| `broken`（引文对不上 / 锚点不存在） | **不行** | — |

## 端到端实跑（不是描述）

```
$ node p1-04-advance/advance-cli.mjs
1) 取消后 Notes 变化：{"added":[],"removed":[],"changed":[],"same":true}
2) 无来源 -> blocked（没有来源）
3) 核验 -> verified（grounded）
   落盘：Notes/写作/advance-557d63c5d0519fc10ae78086.md　连回 a-1
   来源卡末行：- 由 ^a-1 推进 → [[advance-557d63c5d0519fc10ae78086]]
4) 重试同一件事：duplicated=true，卡片数=1
5) 撤销后 Notes 与开始时：{"added":[],"removed":[],"changed":[],"same":true}
```

`node --test p1-04-advance/advance.test.mjs` → **14/14**。

## 端点核实清单

**方法**：直接在已安装的二进制里扫路由字符串（buffer 级、大小写敏感），不靠文档、不靠记忆。

```
$ node p1-04-advance/verify-endpoints.mjs
OK   /field/place         x3
OK   /agent/apply         x1
OK   /chat/stream         x1
OK   /publish/site        x1
OK   /search/hybrid       x1
OK   /browse/tree         x1
OK   /trash/restore       x5
OK   /write-result        x0   ← 规划声称可复用，实际不存在
…15 条命中 / 0 条不符
```

| 端点 | 用途 | 状态 |
|---|---|---|
| `/field/place` `/field/note` `/field/react` | 落卡 / 备注 / 状态 | 存在 |
| `/agent/apply` | 应用 agent 产物（最可能是落盘入口） | 存在 |
| `/agent/recall` `/search/hybrid` `/browse/tree` | 召回 / 检索 / 目录 | 存在 |
| `/chat` `/chat/stream` | 对话与流式（候选来源） | 存在 |
| `/publish/site` | 发布 | 存在 |
| `/trash/restore` | 回收站恢复（撤销兜底） | 存在 |
| `/export/` `/organize/apply` `/health/status` `/docs/sync` | 导出 / 整理 / 健康 / 同步 | 存在 |
| `/write-result` | 规划 §3.3 声称可复用 | **不存在** |

**三条差距**（已写进 [endpoints.json](reference/p1-04-advance/endpoints.json) 的 `gaps`）：

1. 规划里的 `write-result` 在二进制里 0 次出现 → 改用 `/agent/apply` 或 `/field/place`，P1-05 联调前必须用真实请求确认入参。
2. P0-03 的性能脚本假设有 `/docs/tree`，实际是 `/docs/sync` 与 `/browse/tree` → 已改。
3. **所有"存在"只证明路由字符串在二进制里**，方法名、入参、返回结构全部未知。

## 边界（这条最重要）

这个闭环是**文件层的参考实现**：它直接往 `Notes/<空间>/advance-<key>.md` 写卡。正式产品应该走 `/agent/apply` 或 `/field/place`，让内核负责索引、同步、撤销栈。这里先用文件层把协议钉死，是为了让"幂等 / 零写入 / 逐字节还原"这三件事可以被测试，而不是等联调时才发现。

其他没做：并发锁（两个 run 同时 commit 同一张卡）、跨设备冲突、撤销时间窗、账本压缩。

## 新思路

1. **账本是产品资产，不是实现细节。** 它记着"这张卡是哪次推进来的"，正好能在卡背上回答"它怎么来的"。建议把 `ledger.jsonl` 的格式公开，并在卡背显示一条可点回的来历。
2. **无限期撤销是错的。** 现在随时能撤销，账本会无限增长。真实产品应该是：10 秒内软撤销（原地、无确认），之后进回收站保留 7 天。P0-01 里的 `undo_within_10s` 就是为这个字段留的。
3. **连回是唯一"改用户文件"的动作，必须先 preview。** 现在只追加、且有备份，但没有给用户看"我要在你那张卡末尾加一行"。落盘前应该显示 diff，让用户点一次。
4. **幂等只对"完全不动"有效。** 意图原文进哈希，意味着用户改一个字就是新 run，"重试"会变成两张卡。需要 run 内 revision（同一次推进的第 2 版），否则用户在候选上改一个字就会绕开幂等。
5. **端点清单应该成为升级回归。** App 每次更新后跑一次 `verify-endpoints.mjs`，哪个端点没了立刻知道。这比读发布说明可靠——这次就是靠它发现 `write-result` 根本不存在。

## 对后续的影响

- **P1-05**：把这套协议从文件层搬到端点层（`/agent/apply` 或 `/field/place`），并用真实入参出参替换 `commit()` 的写文件部分。
- **P2-02 矛盾检测**：直接消费 `blocked` 与 `weak`，不另造状态。
- **发布前**：`verify-endpoints.mjs` 进 CI，作为"复用链路还在不在"的回归。
