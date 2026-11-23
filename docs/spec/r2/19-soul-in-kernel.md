---
title: P1-10a · 灵魂卡模型进内核（Rust）
date: 2026-11-24
status: done
tags: [执行, 灵魂卡, kernel, rust]
---

# P1-10a · 灵魂卡模型进内核

## 为什么先做这一步

P1-10 是「临时卡 → 真卡」的升级路径。动它之前必须先回答一个问题：**升级的对象在代码里存在吗？**

答案是不存在。整个仓库的 `kernel` 里搜不到任何 soul / provisional 结构；P1-01 的模型只活在
`docs/spec/r2/reference/p1-01-soul/soul.mjs`（357 行 Node + 17 条测试）。也就是说，
P1-10 的地基是一份**参考实现**，不是产品代码。从它直接开始做界面，等于在一个不存在的模型上画状态机。

所以 P1-10 拆成两步：

| 步 | 做什么 | 判定 |
|---|---|---|
| **P1-10a** | 把模型搬进 Rust 内核，同一批用例在 `cargo test` 下重跑 | 本文件 |
| P1-10b | 升级路径本身：API + 界面（临时卡的「未确认」态、升级入口、锚点超集） | 下一份文档 |

## 落了什么

| 文件 | 改动 |
|---|---|
| `kernel/crates/kernel-soul/src/lib.rs` | 解析 / 校验 / diff / 锚点重挂 / 引用还原；纯逻辑零 IO，serde 输出 camelCase 可直接进 HTTP JSON |
| `kernel/crates/kernel-soul/tests/soul.rs` | 17 条测试，与 Node 参考实现同名同断言 |
| `Cargo.toml` | crate 进 workspace members + default-members |
| `.github/workflows/kernel.yml` | 内核门禁加 `-p kernel-soul` |
| `CLAUDE.md` | 常用命令的测试行加 `-p kernel-soul` |

## 真实数字

`cargo test -p kernel-soul` → **17 passed; 0 failed**。

对齐关系：Node 版 17 条（解析 8 + diff 3 + 重挂 3 + 引用/相似度/取号 3），Rust 版逐条对应，
不是「另写了一个像它的东西」。示例卡与临时卡两个 fixture 与参考实现逐字节相同
（`diff` 无输出），所以两边跑的是同一批输入。

## 与 Node 版的唯一偏差（写出来，不藏在代码里）

`reanchor` 铸新号：Node 的 `historical` 集合在铸造时不回写，同一轮新增两条会拿到同一个号
（现有用例只新增一条，所以没暴露）。Rust 版把铸出的号立刻并入 `historical`，
并把它算进 `used`。这是有意修的，不是无意漂移。

## API 面（P1-10b 直接用）

| 函数 | 作用 |
|---|---|
| `parse(text)` | 六区块 + 块锚点 + 错误/提醒；临时卡 `temporary=true` |
| `validate(&mut doc)` | 唯一性、前缀一致性、空卡/薄卡；解析后自动跑一次 |
| `diff(old, new)` | 只报 added / removed / edited / unchanged，按锚点比对 |
| `format_diff(&d)` | 人能读的三行式 |
| `reanchor(old, new)` | 改字不换锚点；新条目铸新号；历史锚点参与取号 |
| `resolve_refs(doc, ids)` | 「读了哪几条」还原成句子，未知锚点显式列出 |
| `is_anchor / split_anchor / dice / all_anchors` | 上面几件事的零件，测试与 CLI 共用 |

## 还没做（P1-10b）

- server 侧端点：读卡 / 写卡 / 升级；写盘走 `kernel-store`，不要绕开索引与 undo 栈。
- `server/web/field.html`：临时卡的「未确认」条与升级入口。颜色用 P1-06 的
  `--state-seed`（未确认）→ `--state-grounded`（已升级），不新增第六色。
- 四个入口（访谈 / 提炼 / 粘贴 / 快捕）产出的临时卡都必须出现升级入口；
  升级后卡的锚点集合必须是升级前的**超集**。

## 新思路

1. **产品**：升级不是「换一张卡」，是把同一堆句子从「未确认」接到「已确认」。所以 P1-10b 的界面不该出现「临时卡 / 真卡」两栏对照——用户会以为自己在做一次迁移，而实际发生的只是确认。
2. **实现**：`kernel-soul` 只认 Markdown，不认「这张卡是谁产出的、从哪个入口来」。P1-02 的「临时卡带出处」目前只活在 Node 参考实现里，升级路径要用它，P1-10b 得先补一层 provenance，否则升级后无法回答「这条判断最初是从哪句话来的」。
3. **计划哪里错了**：把「模型进内核」写成一条 TODO 低估了它的作用——它顺手画出了「参考实现 vs 产品代码」的边界。17 条测试能一字不改地搬过去，说明 P1-01 当初把规则写进测试是对的；反过来，凡是没配测试的规格（P1-08 / P1-09），搬进内核时都会把规则重新发明一遍。
