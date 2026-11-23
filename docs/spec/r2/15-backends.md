---
title: P1-05 · 可替换后端
date: 2026-11-23
status: done
tags: [执行, 后端, 契约]
---

# P1-05 · 可替换后端

> 看板 DoD：真库上跑通一次；重复执行不产生第二张卡；撤销后文件逐字节回到原样。
> 这轮额外做成：**文件后端真跑，端点后端只留接口**——因为 `/agent/apply` 的入参出参还不知道。

## 接口只有五个成员

```
id · capabilities · describe() · commit(run, resolve) · undo(writeKey)
```

`checkContract()` 会检查后端有没有漏方法。新增后端少一个就红，不用等到运行时。

## 两个后端

| | 文件后端 | 端点后端 |
|---|---|---|
| 写在哪 | `Notes/<空间>/advance-<writeKey>.md` | `/agent/apply`（未验证） |
| 连回 | 来源卡末尾追加一行 wikilink | 未知（可能由内核负责） |
| 撤销 | 删卡 + 从备份逐字节还原 | 未知（可能走 `/trash/restore`） |
| capabilities | card / link / undo / ledger 全 true | **四项全 false**，因为一项都没验过 |
| 现在能用吗 | 能，14 条闭环测试护着 | 不能，调用即抛结构化错误 |

## 端点后端要补的五件事（写死在 JSON 里）

[endpoint-contract.json](reference/p1-05-backend/endpoint-contract.json)：

1. `method` —— GET / POST / POST+SSE？
2. `pathParams` —— 有没有 `:id` 之类的路径参数？
3. `requestBody` —— 最小可用 body 长什么样（卡文本 / 锚点 / 目标空间）？
4. `responseShape` —— 返回卡 id 还是整个文档？失败返回什么？
5. `errorCodes` —— 冲突、无权限、索引中分别是什么码？

**怎么拿到**：内核运行时发一次真实请求、看 `.litmus/local/ops/` 日志回显、或用 devtools 抓一次前端调用。三条都写在 JSON 的 `howToGetIt` 里。

## 最重要的一条纪律：不许静默降级

```
selectBackend({ endpoint: { mode: 'endpoint' } })   // 没给参数
→ BackendContractError: missing-endpoint-params（列出五缺）
```

要求走端点却没参数，直接失败。**不能**偷偷退回文件后端——那会让"联调通过"变成一句假话。

## 实跑

```
$ node p1-05-backend/demo.mjs
文件后端落盘：Notes/写作/advance-934ecc211e5aa7f50d0efac0.md，撤销={"ok":true,...,"restored":1}
端点后端失败（预期）：endpoint-contract-unknown 缺 method,pathParams,requestBody,responseShape,errorCodes
选后端失败（预期，不降级）：missing-endpoint-params
```

`node --test p1-05-backend/backends.test.mjs` → **9/9**。

## 边界

- 端点后端**没有任何实现**，只有一个会失败的壳。
- 文件后端直接复用 P1-04 的 `commit`/`undo`，没有另写一套（避免两份真相）。
- 没有并发控制、没有跨设备语义、没有回收站保留期。

## 新思路

1. **空壳比假实现有用。** 它把"未知"变成可枚举的五个字段，联调时按表填，不会漏掉 responseShape 这种最容易忘的东西。
2. **capabilities 比方法存在更重要。** UI 可以据此隐藏"撤销"按钮，而不是让用户点了才发现不支持。下一步每个后端都应该报"我能做什么"，而不是"我实现了什么"。
3. **"不静默降级"要扩展到所有适配层。** 同步、导出、模型路由都一样：缺参数就报错，别用默认值蒙混。
4. **两个后端未来会有语义差。** 文件层撤销是"删卡+还原备份"（硬删），端点层可能是"回收站软删 7 天"。协议要允许能力差异，但不能允许"看起来一样、其实不一样"——比如撤销后文件是否真的不存在。
5. **契约检查应该进 CI。** 现在靠测试文件调 `checkContract`；更好的做法是每次新增后端自动跑一遍，缺方法直接红。

## 对后续的影响

- **拿到端点参数后**：只改 `EndpointBackend`，文件后端与 14 条闭环测试不动；再加一组"同一条协议跑两个后端"的对照测试。
- **P1-10 / P1-11**：它们走的是文件读写，可以复用 FileBackend 的原子写与备份能力。
