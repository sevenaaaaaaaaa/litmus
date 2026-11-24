---
title: 22 · 四入口产品侧（P1-13 / P1-14）
date: 2026-11-24
status: 已确认 · 未开工（用户 2026-11-24）
tags: [执行, 灵魂卡, 入口, 范围变更]
---

# 22 · 四入口产品侧

> P1-10b 的 DoD 第 1 条是「四个入口产出的临时卡都出现升级入口」。四个入口在产品侧是 0 行代码，
> 所以这一条今天验证不了，而且会一直验证不了到四个入口做完为止。本文接这一半：
> P1-10b 只留「界面 + 升级入口 + 未确认」，四个入口另立 P1-13 / P1-14。

## 1. 为什么拆（2026-11-24 的范围变更）

**用户 2026-11-24 确认拆分。** P1-13 / P1-14 进 Phase 1 队列，P1-10b 只留界面。

**事实**（可复核）：

| 说法 | 证据 |
|---|---|
| 四个入口只在参考实现里 | `docs/spec/r2/reference/p1-02-soul-entrances/*.mjs`（Node）、`reference/p0-06-mac-capture/main.swift`（Swift 单文件） |
| 产品侧没有造卡代码 | `grep -rnE 'temporaryCard\|temporary_card\|entrances?\|quick[_-]?capture' server/src kernel/crates --include='*.rs'` → 只有 1 个测试名与 1 段注释 |
| 产品侧能写卡，但没人造内容 | `PUT /field/soul/card` 已能写卡与出处（`server/src/soul.rs:116`），调用它的产品代码是 0 |
| 界面从零开始 | `server/web/field.html` 529,859 字节，`grep -cE '灵魂卡\|soul'` = 0 |

**三条不并进 P1-10b 的理由**：

1. **没有可演示的终点。** 并进去，界面要等四个入口全做完才能第一次被人看见、被点、被截图。拆出来，界面这一半单独可验收。
2. **两种栈。** `field.html` 是单文件 Web；快捕是 Swift/AppKit。一行 DoD 里混两种栈，绿的判据只能落在最慢那半。
3. **一行 DoD 会被最慢那半拖红。** 现在「API 已落、DoD 仍 todo」的毛病，并进去只会变成更大的版本：界面做完了，行还是红的，红的原因在另一个栈里。

**代价**：多两行看板、多一次同步；「三入口共用一份确认与落盘」从一句话变成必须被测试钉住的约束（见 §3）。

**那条 DoD 去哪了**：原文保留在 `21-soul-upgrade-api.md` 的「还没做」一节与看板 P1-10b 行的删除线里，迁到 P1-14 的 DoD 第 3 条。三处都在，删一处不影响。

## 2. 四个入口在产品里的形态

| 入口 | 产品里长什么样 | 输入 | 出处 | 落地 |
|---|---|---|---|---|
| 访谈 | 六问向导：一问一屏，可跳过；答完立刻出候选 | 用户的回答（可语音转写） | `answer:belief` | `Notes/…md` + 旁挂 |
| 提炼 | 选一篇/一批笔记 → 按线索词挑句 → 逐条接受/改写/删 | 库里已有笔记 | `Notes/x.md:3` | 同上 |
| 粘贴 | 一个大输入框 → 切句 → 逐条确认 | 自我介绍 / 旧文 / 主页 | `paste:voice` | 同上 |
| 快捕 | ⌥Space 面板（P0-06 的骨架）→ 输入 → ↩ | 一句话 | `capture:quick` | 同上 |

四者只有「候选从哪来」不同；确认、去重、落盘、出处是同一条路径。

## 3. 一条落盘路径，不许四套

- 前端只有一个确认组件、一个候选端点和一个 `PUT /field/soul/card`（落盘）。四个入口各自再造一套 = 四份出处格式、四份锚点规则、四种失败文案。
- 服务端只有一处写卡：`card_put`。入口端点只返回候选，不写盘——这样「跳过」与「确认」走同一条路。
- 界面上只有一个升级入口组件（P1-10b 交的），四入口的卡复用它，不各画一个。

## 4. 前置与风险

| 风险 | 触发信号 | 应对 |
|---|---|---|
| 提炼移植到 Rust 后规则走样 | 同一篇笔记，Node 与 Rust 挑出的句子集合不一致 | 同一份 fixture + 同名同断言；Node 是判据，不是参考 |
| 快捕落点从「笔记」改成「卡」以后，Inbox 旧数据没人管 | 用户找不到昨天快捕的东西 | 迁移映射写清：旧 Inbox 笔记保留，新卡落 `Notes/soul/`；不静默搬 |
| 埋点缺「哪个入口」 | 仪表盘看不出入口分布 | P0-01 加 `soul.entry_used`（interview / extract / paste / capture），升 v3 |
| 四入口都做了，用户还是只用粘贴 | 入口分布长期 >70% 集中在一个 | 那是产品结论，不是实现缺陷；拿到数再说 |

## 5. DoD

### P1-13 · 三 Web 入口

1. 访谈 / 提炼 / 粘贴在产品里都能真造卡：候选 → 接受/改写/删除 → `confirmed: false` 的卡与旁挂一起落盘（`.litmus/soul-sources/…json`）。
2. 三入口共用同一个确认组件与同一条 `PUT /field/soul/card`；代码里只有一处写卡（`server/src/soul.rs::card_put`），入口端点不写盘。
3. 候选逐字可核：候选文字必须是输入里的原句子串，编造直接报错（与 P1-02 同一条断言，Rust 侧新增用例）。
4. 提炼规则在 Rust 侧实现，与 Node 参考同名同断言：标题 / 代码块 / 列表 / 链接 / 网址不进候选；CUES 从 `soul-rules.json` 读，不再硬编码（P1-02 新思路 2）。
5. 跳过 = 临时卡：中途跳过也落 `confirmed: false`，每条候选带出处；卡上的「未确认」由 P1-10b 的界面显示，本任务不重画。
6. 每个入口一条端到端测试：真起 router、真写盘、真读回（`GET /field/soul/card` 能读出同一批锚点）。
7. `soul.entry_used` 真实上报（`entry` 四值），P0-01 升 v3，仪表盘能按入口分。

### P1-14 · 快捕接入 + 四入口集成

1. ⌥Space 输入后落的是 `confirmed: false` 的卡（`Notes/…md` + 旁挂），出处 `capture:quick`，不再只写 Inbox 笔记。
2. 写盘失败不清空输入，状态行给可读原因（P0-06 的设计点搬进产品，不再只是原型）。
3. **四入口产出的临时卡，在 `field.html` 打开都出现升级入口；逐个升级成功，锚点超集，出处不丢**（从 P1-10b 迁来的那一条）。
4. 热键在别的 App 独占全屏时仍能唤起（CGEventTap 兜底 + 授权引导），或显式写「不支持，见未测清单」——不许沉默降级。
5. 一条可跑的验证：编译 + 启动 + 落卡 + 读回，命令与输出贴文档。

## 6. 验证命令

```bash
cargo test -p kernel-soul                    # 模型 / 出处 / 升级：18 + 9 + 7
cargo test -p litmus-server soul             # 端点的纯函数与 HTTP 面：5 + 4
node --test docs/spec/r2/reference/p1-02-soul-entrances/entrances.test.mjs   # Node 参考：提炼规则的判据
bash docs/spec/r2/reference/tests/run.sh     # 全部参考实现
```

界面这一半没有自动测试入口，判据是**可跑产物**：一条命令起服务、一张夹具卡、一张截图或一段录屏 —— 只有文字描述的界面一律算「未做」（看板反自欺条款）。

## 7. 顺序与成本

- **先 P1-10b 的界面，再 P1-13，最后 P1-14。** 升级入口组件先存在，四个入口才复用它；反过来会出现四个入口各画一个升级按钮，然后被删三次。
- P1-13 约是 P1-10b 界面那一半的 2.5–3 倍：三个入口各自的输入面 + 一份共用的候选确认 + Rust 侧规则移植。
- P1-14 另算：Swift 接入 + P0-06 四个已知问题（热键、IME 首帧、菜单栏、签名）里至少要处理前两个。
- 两者都不阻塞 P1-08 / P1-09；它们只阻塞「四个入口」这条 DoD。

## 8. 止损条件

- 提炼在 Rust 侧与 Node 参考的候选集合差 >10%，先停：要么规则没移植对，要么 Node 的规则本身不稳，别继续往上加 UI。
- 快捕接入要动 P0-06 之外的权限（辅助功能授权）才能过验收，先停，把「要不要这个权限」拿来拍板，不默认要。
- 三个入口做完后，入口分布仍集中在一个且访谈完成率 <20%：先别做第四个入口，回去看访谈的六个问题是不是问错了。

## 9. 新思路

1. **「四个入口」其实是一个入口的四种素材来源。** 它们共用的不是 UI，是「候选 → 确认 → 落盘」这条管道；当成一条管道加四个适配器，代码量掉一半，测试从四条变成一条 + 四个适配器用例。
2. **入口不该由我们推荐。** P1-02 新思路 4 已经说了：有话 → 访谈，有旧文 → 提炼，只有主页 → 粘贴。产品侧的默认入口应由「库里有没有 500 字以上的旧文」自动判，而不是每次都让用户选。
3. **计划哪里错了。** P1-10b 被写成一条 TODO，实际是「一个 API + 一个界面 + 四个入口」——三件不同栈、不同验证方式的事。教训不是「拆得不够细」，是**拆的时候只看了交付物，没看验证命令**：一行的 DoD 里如果出现两条不同的验证命令，就该拆。

## 10. 变更记录

| 日期 | 改了什么 | 为什么 |
|---|---|---|
| 2026-11-24 | 建立。P1-10b 拆成「界面」与「四入口」，后者落成 P1-13 / P1-14；P1-10b 第 1 条 DoD 迁到 P1-14 | 四个入口在产品侧是 0 行代码，那条 DoD 长期无法验证；Web 与 Swift 混在一行，绿的判据只能是「最慢那半做完」 |
| 2026-11-24 | 用户确认拆分；补附录 A（field.html 挂载点与三条判据）与附录 B（响应字段 → 界面位置） | 挂载点只在会话里说过一次，不落盘下一轮就得重新通读 529KB；附录 B 把「六态齐」从口号变成字段对应 |
| 2026-11-24 | P1-10b 的界面已落（未确认条 / 逐条出处 / 升级入口 / 六态）；本文 §7 的顺序推进到 P1-13 | 升级入口组件已经存在，四个入口复用它，不再各画一个 |

## 附录 A · field.html 的挂载点（给 P1-10b 那一轮，省一次通读）

壳是 529,859 字节的单文件，soul 相关 0 行。真正要读的只有这几处：

| 位置 | 是什么 | 怎么用 |
|---|---|---|
| `server/web/field.html:1753` `renderRead(d)` | 文档正文渲染入口；`d.source` 是原文 | 横幅挂这里：先看 frontmatter 有没有 `type: soul-card`，有才调 `GET /field/soul/card` |
| `field.html:2404` `openPanel(title, html)` / `closePanel()` | 已有面板原语（`#panel` / `#p-body`） | 升级确认、逐条出处明细、失败提示都塞进去，不新造一层 |
| `field.html:2416` `openPop(anchor, items)` | 锚定按钮的二级菜单 | 升级按钮的「升级 / 看差异 / 取消」 |
| `field.html:1010` `toast(h, bad)` | 一行提示，3.4 秒自收 | 成功与可重试失败 |
| `field.html:948` `api(path, opts)` | 已带 `Authorization: Bearer` 与 peer 头 | 三个端点直接调，不再自己拼 fetch |
| `field.html:891–894` | token 来源与登录跳转 | 401 已在 `api()` 里处理，界面不用重复判 |

三条判据，写错一条界面就会撒谎：

1. **HTTP 200 ≠ 是灵魂卡。** `GET` 对任何 `Notes/*.md` 都返回 200；`temporary` 只在 `confirmed: false` 时为真（`kernel-soul/src/lib.rs:299`）。横幅条件用 `temporary === true`。
2. **没有旁挂 ≠ 没有缺。** 没有旁挂时 `sourcesError: "no-sources"`、`sourceIssues: []`。只看数组长度，会把「整张卡没出处」画成「全都对得上」。卡级提示读 `sourcesError`，逐条提示读 `source === null`。
3. **升级失败有五种码**，各写一句人话：`upgrade-invalid-card` / `upgrade-already-confirmed` / `upgrade-no-confirmed-line` / `upgrade-lost-anchor`（明说「一个字节都没写」）/ `upgrade-broke-card`。

## 附录 B · 响应字段 → 界面位置

| 来源 | 字段 | 界面 |
|---|---|---|
| `GET /field/soul/card` | `temporary: true` | 卡级「未确认」条（P1-06 的 `--state-seed`），不加第六色 |
| 同上 | `sections[].items[].text` | 判断正文 |
| 同上 | `items[].source` | 有 → 出处（`Notes/x.md:3` / `answer:belief`）；null → 「没有记出处」 |
| 同上 | `sourcesError` | 非空 → 卡级「没有记出处」，并压住逐条的同名提示（别画六行一样的空） |
| 同上 | `sourceIssues[]` | 逐条：缺出处 / 悬空 / quote 不覆盖正文 |
| 同上 | `errors[]` / `warnings[]` | 卡级错误条；有 `errors` 时升级按钮 disabled |
| `POST /field/soul/upgrade` | `anchorsBefore` / `anchorsAfter` | 升级后「锚点 N → N，一条不少」；不相等就是 bug，不是一种展示 |
| 同上 | `missingSources` | 「还有 5 条没有出处」——升级前后同一个数 |
| 同上 | `sourceIssues[]` | 升级后重画的同一批提示 |

## 附录 C · 落地（2026-11-24 晚 · P1-13 第一版）

代码三处：

| 位置 | 做了什么 |
|---|---|
| `kernel/crates/kernel-soul/src/entrances.rs` | 切句 / 噪声过滤 / 三入口候选 / `assert_grounded` / `Review` / `assemble` / `temporary_card`；12 条测试与 Node 参考同名同断言，fixture 用同一份 `.md` |
| `kernel/crates/kernel-soul/soul-rules.json` | 线索词从 JSON 读；库里有 `.litmus/soul-rules.json` 就覆盖默认，有但不合法直接报错（不静默退回） |
| `server/src/entrances.rs` | `POST /field/soul/entrance`（候选）与 `POST /field/soul/draft`（组装），都不写盘；落盘只有一条 `PUT /field/soul/card` |
| `server/web/field.html` | `/灵魂卡` 命令 → 三入口面板 → 候选确认（接受 / 改写 / 删除）→ 一条 PUT；提炼页带「跳过，先猜一张」 |

**实测**：

| 项 | 数字 |
|---|---|
| `cargo test -p kernel-soul` | 12 条三入口 + 原有 18 + 10 + 7，全绿 |
| `cargo test -p litmus-server entrance` | 5 条 e2e（访谈→卡 / 提炼→行号 / 粘贴改写→原话仍在 / 坏决定 / 规则覆盖） |
| 浏览器实跑（粘贴） | 3 句 → 接受 1 / 改写 1 / 删 1 → 卡上 2 条 2 锚点、0 条缺出处；sidecar `voice-2` 的 `edited: true`，`quote` 仍是原话 |
| 提炼 | `Notes/写作观.md` → 4 条，行号 3 / 5 / 7 / 9，四个区块各一条 |
| 访谈 | 2 个回答 → 3 条候选 |
| 跳过 | 一键 → 4 条判断（每区块 1 条）落 `Notes/灵魂卡-2.md`，自动避开已存在的路径 |

**偏差与欠账**（三条，都是真的）：

1. **埋点没落。** 产品里没有事件管道（P0-01 只有字典，没有写入端），`soul.entry_used` 既没处写也没处看。要做就得单开一条实现管道——这件事不该塞进 P1-13，它会让「三入口」这一行被一个埋点系统拖住。
2. **访谈是六问一屏，不是一问一屏。** §2 写的是向导；v1 先用一屏把路径跑通，向导留给界面轮。
3. **改写的语义放宽了出处检查。** 新增旁挂字段 `edited`，`check_sources` 对它跳过逐字覆盖（见 20 号文档「改写过的条目」）。

**一条教训**：三入口共用的不是 UI，是「候选 → 确认 → 落盘」这条管道。真正省事的是让服务端重新推导候选、客户端只送「第几条、接受/改写/删除」——正文不能由客户端凭空给，否则「逐字可核」在 API 边界上就漏了。

## 附录 D · 落地（2026-11-24 深夜 · P1-14）

第四个入口接上了：复制 → Alt+Space → 一张临时卡，出处 `capture:quick`。

| 位置 | 做了什么 |
|---|---|
| `kernel-soul/src/entrances.rs` | `from_capture(text, rules, 3)`：只切句，不套提炼那套噪声过滤（这段字是用户自己选中的）；区块按线索词判，一条都没命中就落「我相信」 |
| `server/src/soul.rs` | 把 `write_card()` 从 HTTP handler 里抽出来：`PUT /field/soul/card` 与快捕共用同一条写路径；`capture_card()` 用它落 `Notes/快捕/捕捉-<ms>.md` + 旁挂 |
| `cli/src/main.rs` | `litmus capture <库>`：读参数或 stdin，落卡并打印路径——不开 GUI 也能验 |
| `desktop/src/lib.rs` | `quick_capture` 从「写 Inbox 笔记」改成调 `capture_card`；失败发 `quick-capture-failed` 事件并写日志，剪贴板原文不动 |

**实测**：

| 项 | 数字 |
|---|---|
| CLI 快捕 | `printf '短句、具体、少形容词。这句话没有任何线索词。' | litmus capture /tmp/soul-vault` → `Notes/快捕/捕捉-1795533004943.md`，2 个锚点；没有线索词的那句落 belief |
| Rust 测试 | `cargo test -p kernel-soul` 48 条（+`from_capture` 1 条）；`cargo test -p litmus-server` 161 条（+`capture_card` 1 条） |
| 四个入口的卡逐个升级 | 访谈 1→1 / 提炼 4→4 / 粘贴 2→2 / 快捕 2→2；`superset` 全 true；`missingSources` 升级前后都是 0；`sourceIssues` 0 |
| 界面 | 快捕卡打开即「未确认 · 2 条判断 · 0 条没有出处」，两条 `capture:quick` 小签；点升级 → 「已确认」，锚点 2 → 2 |

**偏差与欠账**：

1. **没有状态行。** 产品里的快捕没有面板（剪贴板进、卡片出），失败只写日志并发 `quick-capture-failed` 事件；P0-06 原型里那条状态行没有对应物。剪贴板原文不会被清空，这条成立。
2. **全屏独占时的热键没兜底。** 用的是 `tauri-plugin-global-shortcut`（Carbon 热键）；别的 App 独占全屏时可能收不到。CGEventTap + 辅助功能授权这条路没做——按 DoD 的写法，这里显式写「不支持」，不装成支持。
3. **桌面端只做了类型检查**（`cargo check -p litmus-desktop` 通过），没有真跑图形界面；`Alt+Space` 的端到端（剪贴板 → 卡）用 CLI 走的是同一个 `capture_card`。

**一条教训**：第四个入口差点又变成「再写一套落卡」。真正省事的是把 `write_card` 从 HTTP handler 里抽出来——快捕、三个入口、以后的任何客户端都走它。写卡的自由度只该有一份。
