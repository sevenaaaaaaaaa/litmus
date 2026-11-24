---
title: P1-01 · 灵魂卡数据模型 + 块锚点
date: 2026-11-23
status: done
tags: [执行, 灵魂卡, 数据模型]
---

# P1-01 · 灵魂卡数据模型 + 块锚点

## 目标（看板原话）

> 解析器能读附录 A 的模板；非法卡有可读报错；diff 能只显示改动的锚点。

三个目标都达成了，另外顺手修掉一个会让"引用指向另一条判断"的真缺陷。

## 数据模型

**一张卡 = 一个 Markdown 文件**，六个区块，每条判断一行，行尾一个块锚点。

```markdown
---
type: soul-card
version: 1
updated: 2026-11-23
visibility: private
confirmed: true
---

## 我相信
- 知识不是收藏，是被用过的判断。 ^belief-1
```

| 区块 | key | 锚点前缀 |
|---|---|---|
| 我相信 | belief | `belief-` |
| 我如何判断 | criteria | `criteria-` |
| 我拒绝 | refuse | `refuse-` |
| 我的声音 | voice | `voice-` |
| 我在追的问题 | question | `question-` |
| 我的读者 / 作品 | reader | `reader-` 或 `work-`（同一区块允许两个前缀） |

**frontmatter 五个键**：`type`、`version`、`updated`、`visibility`、`confirmed`。
`confirmed: false` 就是**临时灵魂卡**——AI 输出必须标「未确认」（这条直接连 P1-10）。

## 三条不变量（这个模型的全部价值）

1. **锚点唯一。** 重复锚点是错误，并且报错里要指出另一处行号。
2. **锚点属于"这一条判断"，不属于"这一行字符串"。** 改一个字、挪一段位置，锚点不变。
3. **新条目才铸新号，编号永不复用。** 删掉 `voice-4` 之后再新增，拿到的是 `voice-5`。

第 3 条是这轮发现的真问题：原来的取号是"现有最大值 + 1"，删掉最大号后新条目会**复用**那个号，于是卡背上"读了 ^voice-4"、已经发布的页面里的引用，会悄悄指向另一条判断。修法是把历史锚点也纳入取号范围。

## 怎么用

```bash
node soul-cli.mjs validate soul-example.md          # VALID 16 条 / 16 锚点
node soul-cli.mjs parse soul-template.md --json     # 临时卡：temporary=true
node soul-cli.mjs diff 旧卡.md 新卡.md               # 只报 added / removed / edited
node soul-cli.mjs reanchor 旧卡.md 改过的卡.md       # 把锚点跟着判断走
node soul-cli.mjs refs soul-example.md belief-1      # 「读了哪几条」还原成句子
```

`reanchor` 的做法：先按行做 LCS 对齐（整行没变直接带过去），再用**字符二元组相似度**（阈值 0.55，同区块内）配对"改了一两个字"的条目；都不匹配才铸新号。删除的条目会出现在 `deleted` 里，不会静默消失。

## 证据

```
$ node --test p1-01-soul/soul.test.mjs
# tests 17  # pass 17  # fail 0

$ node p1-01-soul/soul-cli.mjs validate p1-01-soul/soul-example.md
VALID 16 条 / 16 锚点
$ node p1-01-soul/soul-cli.mjs validate p1-01-soul/soul-template.md
VALID 8 条 / 8 锚点        （confirmed: false，识别为临时卡）
```

18 条测试里最值钱的一条是**稳定性**：随机改字 40 轮，原 16 个锚点一条不丢、零新铸。

## 已知边界（这轮没做）

- **没有 UI**：只有解析器与 CLI，卡背"读了哪几条"还没接上（等 P1-11）。
- **没有版本目录**：`diff` 只比较两个文件；"这个月哪些判断被推翻"需要版本历史（P2-03）。
- **合并两条判断会丢一个锚点**：现在只会报 deleted，没有"合并"语义。
- **相似度阈值 0.55 是拍的**：还没有真实改稿样本校准。
- **大卡性能未测**：现在假设一张卡 < 200 行；如果是 2000 行要重测 LCS。

## 新思路

1. **锚点该是一个产品级对象，不只是行尾字符串。** 现在它能被引用、能被 diff；但还不能回答"这条判断被引用过多少次、被推进过几次"。应该有一份 `anchors` 索引（anchor → 文本 + 版本 + 引用次数），"每周被推进的卡"这类指标才有对象级口径。
2. **"编号永不复用"需要一个持久账本，而不是每次从两个文件推。** 现在的修法只保证"这次改稿"内不复用；如果把卡回滚到三个月前的版本再新增，历史号还是会撞。真正的解法是 frontmatter 里一个单调计数器（`anchor_seq`）或一份 tombstone 文件。
3. **合并与拆分是灵魂卡的日常动作，模型还没表达。** 用户会把两条合成一条（`^belief-2` + `^belief-3` → 新 `^belief-4`？），也会把一条拆成两条。现在只会在 diff 里看到一删一增，引用就断了。需要 `supersedes` / `split-from` 两种关系。
4. **`confirmed` 这个布尔太粗。** 一张卡里往往是我确认了信念、但声音那段是 AI 提炼的。需要按区块记来源（`user` / `distilled` / `suggested`），否则"未确认"标注只能整卡一刀切。
5. **diff 的下一步不是"看差异"，是"做决定"。** 每月复盘真正要的是"这几条被证据推翻了，改还是不改"——所以 diff 的输出应该能直接变成 3 个候选动作（改卡 / 改来源 / 并存），这正好接 P1-11 的三色引用。

## 对后续的影响

- **P1-11（证据/三色引用）**：引用单元就是这里的锚点，三色标记挂在锚点上，不另建一套 id。
- **P1-10（临时卡升级）**：`confirmed: false` 已有解析支持，升级动作只需要改 frontmatter + 保留锚点。
- **P1-05（推进闭环参考实现）**：连回来源时写的就是 `^anchor`，幂等键可以用 anchor + 内容哈希。
