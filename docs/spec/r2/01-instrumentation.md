---
title: P0-01 · 埋点事件字典 v0
date: 2026-11-23
status: done
tags: [执行, 埋点]
---

# P0-01 · 埋点事件字典 v0

## 目标（来自规划 §8 Phase 0）

> 埋点：本地优先、可关闭；测首日漏斗、落卡延迟、推进次数、AI 接受率、发布转化、同步冲突、崩溃。

## 做了什么

产物：[events.schema.json](reference/p0-01-events/events.schema.json)（v2：17 个事件）

| 组 | 事件 | 关键字段 |
|---|---|---|
| 会话 | `app.session` | phase、cold_start_ms、crash_free |
| 捕捉 | `capture.opened` / `capture.submitted` / `capture.saved` | channel 七值、to_input_ms、latency_ms、ok、error_kind |
| 卡 | `card.state_changed` | from、to、trigger、source_count |
| 推进 | `advance.opened` / `advance.candidate` / `advance.settled` | scope、intent、mode、sourced、verdict、wrote_to_disk、linked_back、undo_within_10s |
| 关系 | `link.created` | method、accepted |
| 发布 | `publish.done` / `publish.visit` | channel、card_count、visitors（仅聚合） |
| 灵魂卡 | `soul.read` / `soul.edit` | blocks、anchors、confirmed、channel |
| 信任 | `sync.conflict` / `error.shown` / `deleted` | silent_overwrite、has_next_step、undo_available |

**三条硬规矩**（写进 schema 的 `rules`）：

1. **只写结构，不写内容。** 没有正文、标题、文件名、路径、剪贴板、模型请求体。PII 分三档：`none` / `count` / `bucket`。
2. **本地 JSONL。** 落在 `<vault>/.litmus/local/events/YYYY-MM-DD.jsonl`，不外传。
3. **可关。** 关掉后不写新事件；已有事件可一键清空。

## 验证

```
$ node p0-02-metrics/validate.mjs
WARN  diagnostic 事件（只排查用）: capture.opened, capture.submitted, advance.opened,
      advance.candidate, publish.visit, soul.read, soul.edit, deleted, ui.entry_used
OK 10 个指标 / 17 个事件全部对得上
```

事件与字段被 10 个指标逐个引用；字段名写错会被 `validate.mjs` 抓出来（有反例测试）。

## 卡在哪

- 8 个事件没有任何指标消费。它们是"我想知道"而不是"我要用它做决定"——这正是埋点膨胀的起点。
- `capture.submitted` 与 `capture.saved` 之间的差额才是落卡延迟；但原型里我把 `latency_ms` 直接写在 `capture.saved` 上，两种口径都能算，会让报表出现两个不同的"落卡延迟"。

## 新思路

1. **从指标倒推事件，不要先列事件。** 这轮是"先想测什么"再发明事件，代价就是 8 个孤儿。下一轮改成生成式：写好指标公式 → 解析出需要的事件与字段 → 缺什么补什么。孤儿事件在机制上就不该存在。
2. **延迟必须是两条事件的差，不是一条事件的字段。** 自报的 `latency_ms` 会被重试、时钟漂移、前后台切换污染。应改成 `capture.submitted` 与 `capture.saved` 的 ts 差值，并给每条事件一个 `attempt_id` 对齐。
3. **"不写内容"这条承诺被 `bytes_bucket` 削弱了。** 四档桶（<200 / 200-2k / 2k-20k / >20k）已经能指纹化一段特定文本。压到两档（<2k / ≥2k）就够做产品判断。
4. **开关必须在 App 之外也能生效。** Mac 快捕是独立进程、iOS 分享扩展是另一个进程，设置里的开关它们读不到。需要一份 `.litmus/local/telemetry.json`，所有进程读它。
5. **孤儿事件里最该升级成指标的是 `deleted.undo_available`。** "任何删除都可撤销"是 §9.3 的体验预算第 4 条，现在只有事件没有指标，等于没有承诺。

## 增补 v2（2026-11-23，用户确认）

两件事：

1. **加 `ui.entry_used`**（entry / action / surface）。这是减法评审的前置条件：没有入口使用率，删哪个入口都是赌博（见 [P0-04](04-subtraction.md) 新思路 5）。
2. **每个事件加 `role`：`metric` 或 `diagnostic`。** 只有 `role=metric` 却没有指标消费才算孤儿；`diagnostic` 是"排查时要看、但不支撑指标"的事件，永远不会被孤儿检查唠叨。

结果：17 个事件 = 8 个 metric + 9 个 diagnostic，孤儿数 **0**。

这一条是 P0-01 新思路 1（从指标倒推事件）的第一次落地：先承认"有些事件本来就不该有指标"，再区分两类角色。

## 对后续的影响

- P0-02 的指标表按"指标→事件"重排了一遍（见 [P0-02](02-metrics.md)）。
- P1-04 推进闭环必须自带 `attempt_id`，否则延迟与接受率都算不准。
