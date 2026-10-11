# ThirdC Design System

契约来源：**OpenFlow 前台设计系统**（`OpenFlow Dev/docs/DESIGN-SYSTEM.md` + `assets/tokens.css`）。
ThirdC 客户端不是自造一套视觉，而是**沿用同一份契约**，保证两个产品的观感同源。

## 三层结构（照搬）

```
server/web/tokens.css   设计契约：颜色（全 oklch）、字体、圆角、阴影、缓动、明暗模式
server/web/index.html   <style> 只放本页独有部件
```

硬规则：

| 规则 | 说明 |
|---|---|
| **零 hex / 零 rgb** | 颜色只能来自 token；派生用 `color-mix(in oklab, …)` |
| **动效统一弹簧** | 一律 `var(--ease-spring)`（cubic-bezier(.32,.72,0,1)）；`prefers-reduced-motion` 下即时 |
| **圆角三档** | `--r-lg:26` / `--r-md:18` / `--r-sm:12` |
| **图标只有一种** | 24×24 线框 svg，`stroke-width:1.8`，圆角端点，颜色跟 `currentColor`，不直出 emoji |
| **玻璃表面** | `--surface` / `--surface-strong` + `backdrop-filter: blur() saturate()` |
| **字体** | Space Grotesk（显示/正文）+ JetBrains Mono（代码/元信息）+ 系统中文字体回退 |
| **对比度** | `--faint` 亮 51% L / 暗 64% L，正文级小字 ≥4.5:1（WCAG AA） |

## ThirdC 增补 token

画布与对话是新场景，扩展了以下 token（同样遵守零 hex）：

| token | 用途 |
|---|---|
| `--node-bg` / `--node-bg-active` | 画布节点表面 |
| `--node-w` / `--node-shadow` | 节点宽度与投影 |
| `--grid-dot` | 画布点阵底纹 |
| `--edge` / `--edge-soft` | 知识图谱连线（强/弱） |
| `--type-doc` `--type-capture` `--type-asset` `--type-query` | 数据类型色：文档/采集/附件/检索 |
| `--chat-bg` / `--chat-w` | 对话舱 |

## 新增零件的门槛（继承 OpenFlow 规则）

- 第二个页面要用同一个东西，才进 `tokens.css`；只在一个视图用的留在页面 `<style>`。
- 页面 CSS 里不得出现 hex/rgb，不得重定义共享类。
- 新增必须给 ≤1080 / ≤860 降档，并同时检查亮暗两色。
- 命名沿用短名风格（`.node` `.n-head` `.n-hit`，不是 `.canvas-node-header`）。

## 已验证

`/assets/tokens.css` 与 `/assets/fonts/*.woff2` 由 daemon 直接托管（内嵌二进制 + 一年 immutable 缓存），
离线可用、无 CDN 依赖。

## 配色修订（2026-09-19）

反馈「太黑」后重做暗色与默认策略：

| 项 | 旧 | 新 |
|---|---|---|
| 默认主题 | 固定深色 | **跟随系统**（按钮循环 跟随系统 → 浅色 → 深色） |
| 暗色底 | `oklch(19% .014 70)`（近黑） | `oklch(26% .012 75)`（石墨），层级靠亮度差而非压黑 |
| 暗色表面 | 27% / 30% | 35% / 40%，玻璃更亮更"有厚度" |
| 亮色底 | 96.5% | 97.4% 暖纸 + `--bg-deep` 三级 |
| 新增 token | — | `--bg-deep` `--col-bg` `--col-head` `--axis`（看板/时间线用） |

仍是全 oklch、零 hex、弹簧缓动、圆角三档。

## 配色体系 v2（2026-10-12 · 工艺品化 PR①）

> 判据：任何一屏截图色彩层次 ≥4 级可见；对比度全部过 WCAG AA；oklch 零 hex。
> 三条都由 `server/tests/palette.rs` 在 `cargo test` 里强制，不靠肉眼复核。

### 层级 = 明度阶梯

五级表面 `bg-0 < bg-1 < surface-1 < surface-2 < surface-3`（k = 0…4），用公式而不是挑色：

| 主题 | 明度 L(k) | 色度 C(k) | 色相 h(k) | 借鉴 |
|---|---|---|---|---|
| 暗（主场景） | 22.5% + 3.2%·k | .007 + .0008·k | 70 + 2·k | Linear 的等差克制；越浮起越暖一点（Arc 的温度） |
| 亮 | 100% − 0.65%·(4−k)^1.5 | .016·(4−k)/4 | 84 | Things 的纸感：纸面最深，浮层趋白、色度褪尽 |

壳的落位（截图里可数出的层次）：

| 层 | token | 承载 |
|---|---|---|
| 框 | `bg-0` | 顶栏、底栏、移动导航；输入框/侧列等凹陷处（`--bg-soft`） |
| 底 | `bg-1` | 画布、Memo Hub/视图文件底（`--bg`） |
| 面 | `surface-1` | 侧栏、检查器、抽屉（`--surface` 玻璃） |
| 物 | `surface-2` | 画布节点（`--node-bg`） |
| 浮 | `surface-3` | 菜单、弹窗、选中态（`--surface-strong`） |

文字四级 `text-1…4`（旧名 `--fg/--muted/--faint` 映射前三级，`text-4` 只给占位符与禁用态），
边框两级 `border-1/2`（文字色的透明度，压在任何表面上都成立），语义四色 `ok/warn/danger/info` 各配 `-soft`，
新增 `--on-danger`、`--ring`、`--selection`。

### 对比度纪律（测试矩阵：亮/暗 × 两版强调色）

- `text-1/2/3`、`accent(-strong)`、`accent-2`、语义色 对五级表面全部 ≥ 4.5:1；
- 彩色文字压在自己的 `-soft` 底上 ≥ 4.5:1（徽章、标签、选中态）；
- `on-accent / on-accent-2 / on-danger` 压在对应底色上 ≥ 4.5:1；
- `text-4` 属 WCAG 1.4.3 豁免（占位/禁用/装饰），仍保证 ≥ 3:1；
- 暗色相邻表面级差 ≥ 1.08:1，保证在截图里可分辨。

### 强调色两版：石蕊双色 vs 经典蓝

主色都是石蕊蓝（`accent`，链接/选中/焦点环不变），差别只在副色 `accent-2` 与渐变 `--accent-grad`：

| | A · 石蕊双色（默认） | B · 经典蓝（`html[data-accent="blue"]`） |
|---|---|---|
| 副色 | 石蕊红 h≈10（遇酸显红） | 堇紫 h≈300（同色系演进） |
| 渐变 | 红 → 紫 → 蓝，oklch 插值走紫色短弧（石蕊的中性色恰好是紫） | 堇 → 蓝，安静 |
| 用在哪 | 只用于"显色"瞬间：AI 动词、Agent 署名与思考光、Magic 星标、启动环、光晕 | 同左 |

石蕊红**永不表示错误**：与 `--danger`（h≈29）色相相隔 ≥12°，测试强制；报错一律走 `--danger`。
切换：⌘K「切换强调色」，存 `localStorage.thirdc_accent`，启动脚本在首帧前落定，无闪烁。

![强调色 A/B 对照](../img/craft/accent-ab.png)

前后对照（上：v1，下：v2）：

![暗色 v1 → v2](../img/craft/palette-v2-dark.png)
![亮色 v1 → v2](../img/craft/palette-v2-light.png)

### 兼容

旧 token 名全部保留为别名（测试 `every_legacy_token_still_resolves` 守住），62 款阅读风格预设与插件
覆盖 `--bg/--fg/--accent…` 的方式不变。新原语（`surface-1…3`、`text-4`、`info`）不随预设变化，属有意为之：
它们是壳的骨架，不是阅读皮肤。

## 图标与字体纪律（2026-10-12 · 工艺品化 PR②）

### 图标：一套线性家族

- 唯一来源是 `index.html` 里的 `IC` 表 + `iconSvg(name, size)`：24 网格、圆端点圆拐角、`currentColor`；
  笔画统一由 `--icon-stroke: 2`（tokens.css）控制，`svg.ic` 与各容器的 `svg{stroke-width}` 都读它。
- 静态标记写 `<i data-ic="名" data-ics="尺寸">`，启动时注水；动态 HTML 直接拼 `iconSvg()`。
- 本次补齐 31 个图标（x / check / more / cmd / chevL / chevR / expand / spark / graph / quote / link / edit / move /
  folderPlus / copy / send / table / target / inbox / key / user / mail / film / book / square / branch / note / clip / long / code），
  替换掉 chrome 里全部 emoji：关闭 ✕ ×21、⋯、⌘ 底栏钮、🕸 图谱、编辑器工具栏、导出八格、右键菜单 41 项、
  画布文件夹节点（📁 → 节点头线性 folder 图标）、空态按钮、Agent 工具步骤 ✓/✕、AI 动词 ✦ → spark。
- 仍保留的字符：键盘记号 ⌘⇧↩（排印，不是图标）、外链 ↗、状态行里的 ✓/✗ 文字、写进用户文件的内容
  （📄 附件链接、🎙 录音）、62 款阅读风格的标题装饰（属"降级为装饰"，也在导出管线里）。
- `server/tests/craft.rs`：静态标记零象形符号；每个 `data-ic` / `iconSvg('…')` / 菜单 `{icon:'…'}` 都在 IC 表里。

### 字体排印：五级字号 × 行高配对

| token | 字号 | 行高 | 用途 |
|---|---|---|---|
| `--fs-1` | 11px | 1.45 | 微注：路径、计数、mono 标签、徽章 |
| `--fs-2` | 12.5px | 1.5 | 次要界面：按钮、菜单、侧栏条目 |
| `--fs-3` | 13.5px | 1.6 | 界面正文（body 默认）、对话、卡片正文 |
| `--fs-4` | 15px | 1.5 | 强调与面板标题、快记输入 |
| `--fs-5` | 19px | 1.3 | 展示标题：空态、引导、日历 |

- 原先 `<style>` 与静态标记里 19 种裸 px 字号（8.5…23）按"最近一级"归并到五级；`font:` 简写同时带上配对行高，
  长文阅读处（编辑器 1.8、memo 1.75）保留显式行高。`craft.rs` 禁止再出现裸 px 字号。
- 数字等宽：路径、计数、日期、分数、金额、计时器、表格单元一律 `font-variant-numeric: tabular-nums`，滚动/刷新时不跳。
- 标题类统一 `letter-spacing: -.01em`，Space Grotesk 在大字号下更紧致。
