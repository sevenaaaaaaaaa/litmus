# P1-10b · 升级路径的 API（界面还没做）

状态：**API 一半已落地，P1-10b 的 DoD 仍未满足**（三个判定里两条是界面）
建立：2026-11-24
前置：[P1-10a 模型进内核](19-soul-in-kernel.md) · [P1-12 出处落盘](20-soul-provenance.md)

## 为什么先做 API 再做界面

界面要显示什么，取决于 API 答得出什么。所以先把三个问题定死：

1. 这条判断有没有出处？（P1-12 的旁挂，逐条）
2. 升级会不会丢锚点？（超集这条判据在哪一层被执行）
3. 写盘失败时，用户看到的是一句话还是一个空页面？

这三个问题答完，界面上要画的东西就只剩位置与措辞了。

## 三个端点

| 方法 | 路径 | 做什么 |
|---|---|---|
| GET | `/field/soul/card?path=Notes/x.md` | 读一张卡：区块、逐条锚点与出处、errors/warnings、缺出处清单 |
| PUT | `/field/soul/card` | 写一张卡（可同时写出处）；默认拒绝覆盖已有真卡 |
| POST | `/field/soul/upgrade` | 临时卡 → 真卡；返回升级前后的锚点集合 |

三个都会写盘——所以都过同一道路径检查。

## 升级只翻一行

升级做的事只有一件：frontmatter 里 `confirmed: false` → `true`，**其余逐字节不动**。

这不是偷懒。改卡的自由度归 `reanchor`，升级本身不该有自由——19 号文档的原话是「升级不是换一张卡，
是把同一堆句子从「未确认」接到「已确认」」。逐字节不动带来三个直接好处：

- **超集是结构性结论，不是靠人记得**：正文没动，锚点自然一条不少；
- **旁挂自动继续对得上**：P1-12 的旁挂按锚点键，锚点不动就等于键不动；
- **diff 只有一行**：用户在版本库里能一眼看出这次升级到底改了什么。

拒绝的三种情况，都显式报：

| code | 什么时候 | HTTP |
|---|---|---|
| `upgrade-invalid-card` | 卡本来就不合法 | 400 |
| `upgrade-already-confirmed` | 已经是真卡 | 409 |
| `upgrade-no-confirmed-line` | frontmatter 里没有 `confirmed` 那一行 | 400 |
| `upgrade-lost-anchor` | 万一日后有人把升级改成重排+铸号，丢了锚点 | 400，且一个字节都不写 |

## 出处怎么跟着走

`POST /field/soul/upgrade` 写完卡之后会读一次旁挂，再跑一遍 P1-12 的 `check_sources`，
把结果放进 `sourceIssues`，并单独给一个 `missingSources` 计数。

**缺的不会补。** 升级前有多少条没出处，升级后还是多少条——因为出处不是升级该产生的东西，
它该在造卡那一刻就存在。这条在测试里钉死了：6 条判断只给 1 条出处，升级后仍然报 5 条缺。

## 路径安全

三个端点都接受用户给的路径，都写盘。所以只接受 `Notes/` 下的 `.md`：

- 绝对路径、`..`、非 `.md`、`Notes/` 之外的，一律 400；
- 旁挂文件名只转义 `%` 和 `/`，**转义必须单射**——撞名等于两条判断共用一份出处，
  那正是 P1-12 要防的那类静默错误。有专门的用例钉这一点。

## 测试

| 层 | 条数 | 钉什么 |
|---|---|---|
| `kernel-soul` 的 `tests/upgrade.rs` | 7 | 只改一行 / 锚点一条不少 / 已确认拒绝 / 没有那一行拒绝 / 坏卡拒绝 / 旁挂升级后仍对得上 / 升级后改字走 reanchor |
| `server/src/soul.rs` 的纯函数 | 5 | 路径四类拒绝 / 转义单射 / 原子写不留临时文件 |
| `server/src/soul.rs` 的 HTTP | 4 | 真起 router：升级真改文件 / 真卡 409 / 出处落盘且报缺不补 / 路径越界 400 |

HTTP 那 4 条用的是仓库里已有的 `test_router()` 形状，不是新发明一套。

## 还没做（P1-10b 剩下的两条）

> 2026-11-24 晚更新：第 1 条（界面）已落，见文末「变更」§3；第 2 条已迁 P1-13 / P1-14（同 §1）。下面原文保留。

1. **`field.html` 的界面。** 前提要说清：这个壳里**一行 soul 代码都没有**（`grep -c '灵魂卡|soul'` = 0），
   `server` 也刚在这一步才依赖 `kernel-soul`。所以这不是接线，是从零长一个界面：临时卡的
   「未确认」条、升级入口、每条判断的出处与「没有出处」的常态显示。要按六态画（hover/focus/
   disabled/loading/empty/error），颜色用 P1-06 的 `--state-seed` → `--state-grounded`，不加第六色。
2. **四个入口的产品侧。** P1-02 的访谈 / 提炼 / 粘贴三入口与 P0-06 的快捕，目前都只在 Node
   参考实现里。产品侧还没有造卡代码，所以这三个端点现在只能被手工 curl 用——「四个入口产出的
   临时卡都出现升级入口」这条 DoD 暂时无法在产品里成立。

两件都没做，所以 **P1-10b 的 DoD 没满足**，这一行仍是 `todo`。API 这一半落在本文与
`server/src/soul.rs` 里，不是「P1-10b 做完了」。

> 2026-11-24 晚：上面是当时的记录。界面已落（见 §3），看板上这一行已改成 `done`；四入口仍不在本任务里。

## 新思路

1. **「逐字节不动」可以推广成一条设计规矩。** 凡是不可逆的操作（升级、迁移、发布），先问一句：
   能不能把它做成逐字节不动？能，就没有「半途失败」这个状态。
2. **报缺的接口形状。** `missingSources` 一个数 + `sourceIssues` 逐条明细，界面可以直接照画，
   不用二次推导哪条缺。以后凡是「允许缺失但必须可见」的东西，都该长成这个形状。
3. **计划哪里错了。** 把 P1-10b 写成一条 TODO，实际的量是一个界面加四个入口。它至少该拆成三条；
   我这次只交了 API，拆分要不要动 DoD 由你定——动 DoD 不是我能自己改的。

## 变更（2026-11-24）

> 只追加，上面几节不动。

### 1. DoD 第 1 条迁出（范围变更；用户 2026-11-24 确认）

原文（保留）：「四个入口（访谈 / 提炼 / 粘贴 / 快捕）产出的临时卡都出现升级入口」。

四个入口在产品侧是 0 行代码：P1-02 与 P0-06 只在 `reference/` 里有 Node / Swift 实现，`server/src` 与
`kernel/crates` 里没有造卡路径。所以这一条在本任务内无法验证，也不该由本任务等。

- **迁到**：[22 号文档](22-soul-entrances-product.md) 的 P1-14 DoD 第 3 条。
- **P1-10b 的 DoD 改为**：一张 `confirmed: false` 的卡（由 `PUT /field/soul/card` 写入，夹具或 curl）
  在 `field.html` 打开即有「未确认」条 + 逐条出处 + 无出处处显式「没有记出处」；升级后锚点集合是升级前的
  超集、出处不丢、缺的数不减少；六态齐、只用 P1-06 五态色、对比度与 375/768/1280 实测达标；
  五种拒绝码各有可读提示。
- **为什么**：见 22 号文档 §1。

### 2. 复核 API 时发现的三处口径问题（本任务补，不新立任务）

1. **`GET /field/soul/card` 不报「缺几条」。** 没有旁挂时 `sourceIssues` 是空数组，也没有
   `missingSources` 字段——界面若只看数组长度，会把「整张卡没有出处」读成「全都对得上」。
   两句都要：GET 补一个 `missingSources`（数 `source` 为 null 的锚点），界面同时把
   `sourcesError: "no-sources"` 当成卡级「没有记出处」。一个是数，一个是话。
2. **`POST /field/soul/upgrade` 的 `"superset": true` 是字面量。** 判据在
   `kernel_soul::upgrade()` 里真执行了（丢锚点报 `upgrade-lost-anchor`，且一个字节不写），
   但响应没测量它。改成 `anchorsBefore.iter().all(|a| anchorsAfter.contains(a))`，
   客户端读到的才是结果而不是承诺。
3. **拒绝码实为 5 个，不是 4 个。** 表里列了 `invalid-card` / `already-confirmed` /
   `no-confirmed-line` / `lost-anchor`；代码里还有 `upgrade-broke-card`（升级后卡不合法，
   放弃写盘），HTTP 400。界面的错误提示按 5 个写。

### 3. 界面（2026-11-24 晚 · 第 1 条落地）

`server/web/field.html` 里只加了三样东西，颜色只用 P1-06 的五态（seed → grounded），没有第六色：

| 位置 | 元素 | 显示什么 |
|---|---|---|
| `renderRead` 之后 | `#soulbar` | `confirmed: false` → 「未确认」+ N 条判断 + M 条没有出处 + 升级按钮；`confirmed: true` → 「已确认」，没有按钮 |
| 每条判断的块尾 | `.soul-src` 小签 | 有出处 → `Notes/x.md:3`（悬停给原话）；没有 → 「— 没有记出处」 |
| 升级 / 失败 | 确认面板 + `.soulbad` / `.soulwarn` | 五种码各一句人话；失败不白屏，按钮变「再试一次」 |

**三个判据**，写错一个界面就会撒谎，实现里钉住了：

1. **200 ≠ 是灵魂卡。** 横幅只认 frontmatter `type: soul-card` 且 `temporary === true`（`kernel-soul/src/lib.rs:299`）。普通笔记打开没有横幅（实测）。
2. **没有旁挂 ≠ 0 条缺。** 卡级读 `sourcesError`，逐条读 `source === null`，数字读新加的 `missingSources`。
3. **锚点不在 HTML 属性里。** 产品渲染的正文保留 `^belief-1` 文本，小签按「块尾标记」挂上去，原文一个字没改。

**实测**（2026-11-24，`/tmp/soul-vault` 五张夹具卡）：

| 项 | 数字 |
|---|---|
| `cargo test -p litmus-server soul` | **12 passed**（原 9 + 缺出处 2 + 文案覆盖 1） |
| `cargo test -p litmus-server --test palette` | 9 passed（零 hex、五态 ink 压 soft 底 ≥4.5:1 都过） |
| 升级一张 6 条判断的卡 | 锚点 6 → 6，响应 `superset: true`（算出来的）；`missingSources` 2 → 2（不补）；文件只有第 6 行 `confirmed:` 变 |
| 逐条出处 | 6 条：4 条有出处、2 条「没有记出处」 |
| 三档 | 1280 / 768 / 375 横向滚动都是 0；375 下升级按钮宽 311、高 44 |
| 六态 | loading（CDP 加 1.2s 延迟抓到「正在读这张卡」）· focus（强制 `:focus-visible`，outline 2px `--state-grounded`）· hover（`.soulbar .btn:hover`，computed 背景从 ink 变 ink-2）· disabled（坏卡 2 处不合法 → 按钮禁）· empty（0 条判断 + 指路）· error（409 竞态 → 刷成真卡；400 invalid → 「这张卡本身不合法」） |
| 不画像 | 普通笔记打开无横幅 |

**没做到的三件事**（诚实清单）：

1. `upgrade-lost-anchor` / `upgrade-broke-card` 在现在的核心里不可达（`upgrade()` 自己就拒绝），只有文案没有实跑。
2. `upgrade-no-confirmed-line` 同理：没有 `confirmed` 行的卡 `temporary=false`，界面上没有按钮可点。
3. 加载态是 CDP 延迟抓的，不是真网络。

第 1、2 条由一条测试兜住：核心里出现的每个 `upgrade-` 码，界面必须有文案（`every_upgrade_error_code_has_copy_in_the_ui`，扫 `kernel-soul/src/lib.rs` 的字符串）。


