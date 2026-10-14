# 模式体系与发布期数

## 一、七种模式（一种 = 一套界面配置 + 一组默认行为）

| 模式 | 界面配置 | 默认行为 | 状态 |
|---|---|---|---|
| **Studio** | 画布 + 对话舱 + 检查器 | 全能工作台（默认） | ✅ |
| **整理** | 看板视图，隐藏对话 | 空白拖拽**圈选** → 批量移动/打标签/删除；侧栏拖拽移动 | ✅ |
| **学习** | 抽屉=纯阅读（AI-HTML 预览），隐藏编辑 | 沉浸阅读，Esc 返回 | ✅ |
| **展示** | 全屏 AI-HTML + 上/下一篇导航 | 隐藏一切编辑界面；`←→` 翻页 | ✅ |
| **RAG** | ⌘K 检索优先 | 检索即用，命中即开，回答带引用（FTS5 bm25） | ✅（密向量召回已落地：`[ai].embedding_model` 配置后三路 RRF，TF-IDF 兜底） |
| **线上** | 发布面板 | 构建站点/推送目标/产物列表 | ✅ |
| **Agent** | 对话舱放大（72vh），画布压暗 | 对话优先，产出自动落画布 | ✅ |

模式是**界面配置**而非并列的 App——数据、内核、权限完全同一套。

## 二、生态对接（openflow / mflow / learnflow）

统一通过 **MCP 连接器**接入（thirdc.toml `[[connections]]`）：
- **OpenFlow**（图表）：整理模式里选中多篇 → 「生成架构图」→ OpenFlow DSL → 画布卡片
- **MFlow**（流程）：采集管道的流程编排
- **LearnFlow**（学习）：学习模式的路径推荐
- 生态工具也可反向把 ThirdC 当 MCP server 用（`thirdc mcp`）

## 三、发布期数（今天 2026-09-19）

| 里程碑 | 内容 | 期数 | 交付日 |
|---|---|---|---|
| EXT-1 | 浏览器扩展 MV3：整页/选区/截图采集 + native messaging | 1 周 | **09-26** |
| DESK-1 | Tauri 桌面壳（内嵌内核，同一 web 客户端） | 1 周 | ✅ 09-20（提前） |
| A2UI-3 | 原生 A2UI DOM 渲染器（去 iframe，交互直传 agent） | 1 周 | ✅ 10-06（提前，Shadow DOM + 直调 `/a2ui/action`，iframe/postMessage 桥已移除） |
| WEB-1 | Web WASM 内核 + OPFS 真文件库（PWA） | 2 周 | **评估后暂缓（10-06）**：OPFS 文件外部工具不可直读，与宪法 §1 文件真相冲突；兼容性已由 web+daemon（任意浏览器）与桌面/移动原生壳覆盖。通往原生的可移植性投资改走：CI 增加 kernel 纯逻辑 crate（kernel-md / kernel-a2ui，零 IO 依赖）的 wasm32 检查编译 |
| UX-6 | 协作光标 v2（选区/文档内光标 + 评论） | 2 周 | **11-07** |
| AGENT-1 | Agent 记忆（对话历史入库、可检索） | 并行 | 10 月 |
| BACKUP-1 | E2EE 备份（age 快照 → WebDAV/local/rclone；3-2-1 合规检查） | 2 天 | ✅ 10-07（kernel-backup + CLI + /backup/*；Proton/Dropbox 经 rclone 桥，NAS 走 WebDAV，iCloud 走 local 目标；spec：backup.md） |

## 四、Open Knowledge Format（OKF）· 开放知识格式规范 ✅

默认文件规范：**Markdown 语义 + YAML 元数据 + 块锚点**，"机器可读、人类可懂"的原子知识包：

| 层 | 载体 | 实现 |
|---|---|---|
| 元数据 | YAML frontmatter（`---` 头） | kernel-md 解析/序列化，往返无损（不再是 Divider） |
| 正文 | Markdown 语义 / 语义化 HTML | p/h2/blockquote/table/... 少用 div/span（设计模式除外） |
| 结构化数据 | **JSON-LD**（内嵌 AI-HTML） | 块模型全量可被 agent 提取 |
| 块引用 | `^anchor` 锚点（槽位对齐，随内容走） | Obsidian 兼容写法；跨页引用/双向绑定的 ID 根 |
| 索引层 | SQLite（FTS5 + 标签/元数据/引用登记表） | 后台实时扫描（watcher → sync_all） |

回归语义化 HTML：`to_html` 产出自包含 AI-HTML（article/h1/h2/p/blockquote/table/hr + JSON-LD）。
编辑体验（已落地）：**光标离开即渲染**（源码不被覆盖，预览即时刷新）+ **Notion 式 `/` 插入菜单**（表格/列表/引用/代码/锚点/标签）。

## 五、连接器矩阵（MCP 优先 + 源注册表扩展）

| 生态 | 接入方式 | 状态 |
|---|---|---|
| DeepSeek（API/CLI） | `[ai]` base_url 配置即用；CLI 可包成 MCP server 接入 | ✅ AI 对话已通 |
| Chrome / ego-lite | `import_page_design` / CDP（已落地 ego 三档后端） | ✅ |
| 飞书 / Slack / Telegram | 各自的 MCP server（社区有）经 `[[connections]]` 接入 | 配置即用 |
| 主题采集 | wikipedia / hackernews / arxiv 内置 + MCP 扩展 | ✅ |

### DESK-1 已落地的形态

- 单进程：内核 + daemon + webview 同一个二进制，`127.0.0.1` 随机端口，不和 `thirdc serve` 抢 7700
- 免登录：token 由壳直接带给首帧（`?token=`），前端落 localStorage
- 默认库 `~/Documents/ThirdC`，`THIRDC_VAULT` 覆盖；目录不存在自动 init
- 失败不白屏：内核起不来时窗口渲染兜底页（库路径 + 原因 + 四条下一步）
- macOS 原生感：透明标题栏 + 内容区顶到顶（web 端 `html.desktop` 让出 78px 拖拽区）
- 打包：`bundle.targets = app / dmg`，图标全套由 `scripts/gen_icons.py` 生成（oklch → sRGB，无 hex）

## 六、发布与监测

- **已开源**：github.com/sevenaaaaaaaaa/litmus（main）
- **服务器**：Docker 化部署于 172.96.253.73（127.0.0.1:7700），域名挂载 `nownexts.com/thirdc`（Apache Alias，按 DEPLOY-CONVENTIONS 需手动确认后加）
- **监测**：`/metrics`（Prometheus 文本）；**审计**：`events/publish.jsonl` + a2ui-actions
