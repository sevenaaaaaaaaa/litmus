# 知识场（Field）

> 状态：F0–F4、G1–G4、H、筛选、对接已实现；0.14.0 起知识场是唯一的界面（经典界面退役）；0.16.0 起有上手引导与后台任务
> 与 `os.md`（宪法）、`interaction.md`（交互契约）并列。经典壳的结构规则见 `content-workspace.md`；知识场是下一代默认壳的候选，F2 起替代它的信息结构部分。

## 0. 为什么要做

v0.2 到 v0.10 的界面改版，改的都是配色、阴影、字体和入口位置，骨架始终是「文件树 + 卡片画布 + 文档阅读 + 底部输入」。这套骨架撑不起「AI 原生的知识操作系统」：

1. 知识之间的关系不可见：画布上的文档是卡片或文件夹，看不出谁和谁相关。
2. 名词多：Studio / 学习 / 展示 / 整理 / Memo / Agent / RAG / 魔法，各有入口。
3. AI 不可见：混合检索、关联、整理都在内核里，界面只露出输入框和一段文字。
4. 品牌没进产品：Litmus = 石蕊试纸，这个隐喻没有被用上。

## 1. 核心隐喻

**知识遇到问题，就会显色。** 整座库是一张安静的纸（只有纸和墨）。一提问，相关的知识从最相关的那篇开始向外显出蓝色；答案在这些来源之间生成一张卡，用线连回每一处出处。

## 2. 五条原则

| # | 原则 | 落地 |
|---|---|---|
| 1 | 一片场，没有模式 | 整库按意思排在一个平面。缩放即层级：远 = 区域名与点，中 = 标题，近 = 标题 + 摘要。阅读是右侧滑出的纸页，场一直在旁边 |
| 2 | 一个入口：问、找、记、令 | 底部唯一一行，按输入判意图：问句 → 显色 + 结晶答案；几个词 → 显色，回车打开最相关的一篇；`+` 开头 → 随手记；`/` 开头 → 指令 |
| 3 | 颜色只为意义出现 | 石蕊蓝 = 与此刻意图相关，越相关越蓝；石蕊红 = 刚记下、尚未沉淀。其余全是纸与墨 |
| 4 | 结果落在原处，成为知识 | 答案在来源质心旁结晶，`[n]` 可点回原文；「收下」写成 `Notes/答案/<问题>.md`（来源用 `[[精确路径]]`）。划选正文得到的解释写在原段落下方 |
| 5 | 结构自己长出来，时间是一个维度 | 随手记写入 `Notes/Memos/`（与经典壳同格式），自动落到意思最近的地方；底部沉积条拖回任意一天；「时间」排布按日期 × 区域重新铺开 |

## 3. 实现

### 3.1 内核：`kernel-core/src/field.rs`

全部是派生层（宪法 §1）。输入是正文语料，输出是坐标 / 区域 / 关联线。

- **向量**：英文词 + 中文二元组（标题不跳过、权重 ×3，不收单字），次线性 TF × IDF，L2 归一，每篇留 160 个词。词表只收出现在 ≥2 篇且 ≤60% 篇的词。
- **近邻**：倒排表生成候选（只用 df ≤ clamp(N/20, 40, 200) 的词），再用完整点积精排，每篇 8 个。
- **布局**：首次 = 哈希随机投影（语义相近 → 投影相近）+ 近邻弹簧 / 网格斥力 / 外圈向心，260 轮。之后 = **增量**：旧坐标一位不动，新文档落到前 3 个近邻的加权质心旁，只让新文档局部松弛 60 轮。变动超过三成自动整体重排；`?rebuild=1` 强制重排。
- **区域**：坐标上的确定性 k-means（k ≈ √N / 1.6，最远点初始化），按人数重新编号。区域名 = 与区域质心向量最像的那篇文档的短标题（去副标题、去 `Spec`、编号打头的取后半）。二元组切词给不出好词，人写的标题读得懂。
- **确定性**：不依赖 HashMap 遍历序，不用随机数；同一语料两次构建坐标逐位一致（有测试）。
- 性能（3 千篇，release）：向量 0.2s，近邻 0.3s，整体布局 1.8s，区域命名 0.08s。基准：`cargo run --release -p kernel-core --example field_bench -- <Notes 目录>`。

### 3.2 HTTP：`server/src/field.rs`

| 端点 | 用途 |
|---|---|
| `GET /field` | 新壳页面（`server/web/field.html`，tokens.css 内联） |
| `GET /field/data[?rebuild=1]` | `{epoch, regions[{name,count}], docs[{p,t,k,d,ex,x,y,r,m,g}], edges[[i,j,w]]}`；按 `data_epoch` 缓存序列化好的 Bytes；坐标写 `.litmus/local/field.json` |
| `GET /field/react?q=` | 全库相关分：场向量余弦 × 0.65 + FTS 排名 × 0.35 + 标题包含整句 0.3，按最大值归一，最多 400 条 |
| `GET /field/react?near=路径` | 以一篇文档自己的向量显色（阅读时让近邻亮起） |
| `POST /field/place {text}` | 一段文字该落在哪：前 3 近邻质心 + 区域 + 近邻 |
| `POST /field/crystal {q, paths}` | 读来源原文，挑与问题最相关的 ≤3 段（≤900 字），无工具补全写出带 `[n]` 的答案；没配模型 / 模型失败时退化为每篇最相关的一句，并在 `note` 里说明 |
| `POST /field/note {task, text, title}` | 划选后「解释给我听」「考考我」；没配模型返回 409 与可执行的提示 |
| `GET/POST /field/ai`、`POST /field/ai/test` | 模型设置：读（不回明文密钥）/ 写 litmus.toml 的 [ai]（toml_edit 原地改，保留注释；密钥留空表示不改）/ 测连通 |
| `POST /field/agent {message, context}` | Agent：SSE 推送每一步工具调用（参数只留路径 / 查询等短字段）与最终回复 |
| `GET /assets/reading-styles.js` | 62 款阅读风格（与经典壳同一份风格表的拷贝） |

`/field/data` 还带 `links`（`[[目标]]` 与 `](Notes/…)` 解析出的真实链接，按 精确路径 → 补 .md → 文件名 → 标题 解析）与每篇的 `m`（修改时间）。

文档类型 `k`：`memo`（Notes/Memos/）、`answer`（Notes/答案/）、`source`（Sources / Clippings / Web）、`html`、`doc`。日期：frontmatter `created:` / `date:`，否则修改时间（北京日历日）。随手记的标题取首句并去掉「HH:MM 」。

### 3.3 壳：`server/web/field.html`

独立文件，不往经典壳里加。复用 `/doc`（阅读：内核渲染的 HTML 只取正文；编辑：Markdown 源 + 700ms 自动保存，失败保留原文并 4 秒后重试，切换 / 关闭前先存完）、`PUT /doc`（随手记、收下答案）、`/status`。

- 显色：输入 110ms 防抖请求 `/field/react`，上一个请求作废；最相关节点变化时，其余节点按与它的距离延迟显色（0.55ms / 世界单位），纸上色斑用 canvas 径向渐变（亮色 multiply，暗色 lighter）。
- 标签：场坐标横向 ×2.0、纵向 ×1.15 后，≤300 篇做标题避让；更大的库按「此刻相关度 → 关联数」网格查重，压住别人的只留点。>1500 篇远景改用 canvas 点阵。区域名同样避让。
- 随手记：写入前先取落位预告（库未变，走缓存），写入后立即放临时节点从输入行飞过去，后台刷新由内核坐标接管。
- 主题沿用 `litmus_theme`；桌面端 `html.desktop` 留出红绿灯、顶部 44px 拖拽区、IPC 桥兜底与原生明暗同步。
- 渲染只在动画期跑（PERF 纪律），视口外节点不写样式。

### 3.4 唯一的界面（0.14 起）

- 经典界面（`index.html`）已退役删除。`/` 与 `/field` 都是知识场；`/?doc=Notes/…` 深链直接打开那篇（浏览器扩展、托盘、旧书签）。
- 网页版没有令牌时，知识场自己显示登录（`POST /auth/login`），登录后留在原地址。
- 桌面 App 原生菜单与托盘的事件（`menu:*`、`tray-*`）都由知识场接住。
- Service Worker 的壳版本 = `field.html` + `tokens.css` 内容哈希。

## 4. 测试

- `cargo test -p kernel-core field`：切词、确定性、语义邻近、增量不挪旧点、区域名、落位、空库。
- `cargo test -p litmus-server --lib field`：页面免令牌、接口要令牌、data → react → near → place → crystal → note 端到端、新增文档不挪旧坐标。
- `cargo test -p litmus-server --test field --test craft --test palette`：壳的字号走 token、零 hex、无 emoji、毛玻璃两档、圆角走 token、纸与墨及石蕊蓝 / 红在两种明暗下 ≥ 4.5:1；分享页同一套。
- `node --test server/tests/field.mjs`：壳里的纯函数（意图判断、切节、frontmatter、逐行对比、标签主干）。
- `scripts/perf-check.sh` 新增 4 条预算（见 `PERF.md`）。

## 5. F2：把经典界面的功能接回来

| 批次 | 内容 | 状态 |
|---|---|---|
| F2-1 文档基本功 | /新建；阅读页「⋯」重命名或移动（链接随之改写）、复制路径、导出 Markdown / Word / HTML / 打印 PDF、删除进回收站并可撤销；页头「目录」；底部「链出 / 链入」；场上写下的链接画实线、意思相近画点线；/最近、/回收站、/同步；粘贴网址收录、拖入文件导入、/采集 主题、/导入；/设置 模型（`GET/POST /field/ai` 只改 litmus.toml 的 [ai]，`POST /field/ai/test` 测连通） | 已完成 2026-10-26 |
| F2-2 写作与整理 | 「⋯」魔法排版 / AI 排版（`POST /doc/tidy` → PUT，可撤销）；离开编辑时自动命名（`POST /doc/autoname`，跳转到新路径）；「⋯」分栏（编辑框 + 每次保存后的渲染）；「⋯」阅读风格（`/assets/reading-styles.js`，沙箱 iframe srcdoc，双链经 postMessage 回到壳，选择存 `localStorage.litmus_style`）；编辑时粘贴 / 拖入附件（`POST /asset`，正文里 `Assets/` 图片改走 `/asset-file`）；标签（`/field/data` 每篇带 `g`，`#标签` 意图、阅读页标签条、/标签） | 已完成 2026-10-27 |
| F2-3 发布与分享 | 「⋯」发布为公开网页（`POST /publish`）；/发布：目标、构建（`/publish/site`）、部署（`/publish/deploy`）、已发布列表；/展示（`/presentation`）。加密分享链接仍在经典界面 | 已完成 2026-10-27 |
| F2-4 Agent | 意图「办」（帮我 / 请你 / 替我 / ! 开头，或 /做）→ `POST /field/agent`（SSE：start / step{tool,args,ok,summary} / reply / done / error；配了模型走带工具循环，否则离线命令模式；写进会话记录）。步骤卡逐条显示；读写的文档点亮并连成轨迹，当前那篇画呼吸环，镜头跟随；搜索按查询淡显色；写出的文档刷新后以石蕊红出现；结束给出产物与找回按钮 | 已完成 2026-10-27 |
| F2-5 系统 | /插件（安装、启停、删除；命令与视图进 / 指令；视图沙箱 + 与经典壳同一 postMessage 协议）、/设计、/连接、/存档（git）、/备份、/视图 描述、`/ws` 在线数 | 已完成 2026-10-27 |
| F3-1 流式与手感 | 结晶 `POST /field/crystal {stream:true}` 与 Agent 都走 SSE 逐字（`litmus_mcp::agent::chat_stream`：`stream:true`，正文增量回调，工具调用按 index 拼回；服务商回整块 JSON 也兼容）；区域改名（`POST /field/region {auto,name}`，按自动名记在 `.litmus/local/field-names.json`，中心文档换了就回到自动名）；桌面菜单 / 托盘事件（`menu:*`、`tray-*`）由知识场接住，/更新；双指捏合同时平移、松手惯性、空白双击放大、WebKit gesture 事件、手机下拉收起阅读页 | 已完成 |
| F3-2 批量整理 | `POST /field/organize` 只给建议：根目录散文档 → 所在区域成员最多的文件夹（≥3 篇且占区域三分之一）、占位文件名 → 正文 H1、近邻相似度 ≥ 0.86 → 可能重复（只提示）。执行逐条调 `/doc/move`（现在会改 `[[路径]]`、`[[文件名]]`、`](路径)` 三种链接）与 `/doc/autoname`，记录反向操作供撤销 | 已完成 |
| F3-3 加密分享 | `server/src/share.rs`：`POST /share` 存密文到 `.litmus/local/shares/`；`GET /s/{id}` 免令牌返回内嵌密文的查看页（`share.html`，CSP 禁外连，正文在无脚本沙箱 iframe）；有公开目标时同一页写到站点 `e/{id}.html` 并部署；`GET /shares` 列表、`DELETE /share/{id}` 撤回（公开副本以空页覆盖）。密钥：随机 256 位放 `#k2=`，或密码经 PBKDF2-SHA256 21 万轮；兼容经典壳的 `#k=` | 已完成 |
| F3-4 视角 | `GET/PUT /field/views`（`.litmus/local/field-views.json`）：名字、排布、镜头（世界坐标中心 + 缩放）、文档路径、可选查询。存下 = 此刻显色的文档，没有显色就取镜头里的；切回 = 换排布、点亮这组、镜头回位 | 已完成 |
| F4-1 账户与库 | /凭证（`/secrets`，默认遮住）、/库（桌面 `desktop_vaults` / `desktop_switch_vault` / `desktop_create_vault`；网页 `localStorage.litmus_workspaces`）、/退出登录、/记忆（`/agent/memory`，按行「忘掉」= 改 `Notes/Agent/memory.md`）、/记住 | 已完成 |
| F4-2 学习与日历 | 学这一篇：按 `##`（或 `###`，没有就按段落凑 500 字）切节，原文模糊，复述 → `POST /field/learn {task:question|check}` → 自评；`POST /field/learn/grade` 简化 SM-2 排下次；`GET /field/learn/state`；/复习、打开时每日提醒一次；/日历 按文档日期的月历 + 到期复习 | 已完成 |
| F4-3 同步与 AI 整理 | 视角、区域名、复习进度存库根 `.litmus/{views,regions,learn}.json`（git 同步只排除 `.litmus/local/`，又不在 `Notes/` 下，不会变成文档）；读不到时回落 0.12 的 `.litmus/local/` 旧文件。`POST /field/organize {ai:true}` 追加 `tags`（优先已有标签，丢掉带空白的）与 `titles`，执行改 frontmatter / 一级标题 + autoname，可撤销 | 已完成 |
| F4-4 插件索引与网页本地化 | /插件 内嵌 `/plugin-index` 一键装；`POST /field/localize {url, html?}`：取 `<article>`/`<main>`/`<body>`，去导航页眉页脚侧栏表单，kernel-convert 转 Markdown，图片 `kc-img://` 占位后下载入 Assets（单张 10MB、合计 60MB、最多 40 张），失败的留原地址；桌面先 `desktop_render_page` | 已完成 |
| G1 大库提速 | 见 `PERF.md`「知识场大库纪律」 | 已完成 |
| G2 全库整理 | `server/src/organize.rs`。场构建时算信号（正文字数、TODO、计划日期）；`POST /field/organize {scope}` 增加 `fragments`（< 280 字的文档按近邻 ≥ 0.3 连通，3–12 篇一组，按跨越时间排序）、`stale`（superseded / expired / todo / aging）、`options`（区域与日期范围）；`POST /field/merge`（plain / ai，原文 keep / archive，返回撤销说明）；`POST /field/refresh`（参照更新版起草，不写盘）；`/field/patrol`（估算 / 开始 / 进度 / 停下，每批 30 篇补标签起标题、每批 6 篇核实过时，结果存 `.litmus/local/patrol.json`） | 已完成 |
| G3 数据源 | `server/src/sources.rs`。配置 `.litmus/local/sources.json`，索引 `.litmus/local/sources/index.json`，抽出的文字 `.litmus/local/sources/text/<hash>.txt`（设备本地）。种类 folder / agent / mcp / google / rest；`/sources`（列表、增改、`onboarded`）、`/sources/detect`（只数个数）、`/sources/{id}/scan`（后台增量：路径 + 大小 + 修改时间不变就沿用）、`/sources/connection`（写 litmus.toml 的 [[connections]]）、`/sources/google/start|callback`（OAuth 本机回环，refresh token 存凭证库）。知识场把条目当 `ext:<源>/<条目>` 节点铺进场（`k=ext`、`s`=来源名），缓存键加 `sources::REV`；`/field/ext` 只读、`/field/ext/import`（move / copy）；结晶能读库外原文；整理排除库外。截图 OCR：macOS Vision（首次编译一个 Swift 小程序到临时目录）→ tesseract | 已完成 |
| G4 退役经典界面 | 删 `index.html` 与 `workspace.mjs`；`/` 直接是知识场；登录与 `?doc=` 深链进知识场；工艺测试改查 `field.html`（毛玻璃两档、圆角 token；7px 以内的节点标记允许 1–2px） | 已完成 |
| H 上手与后台 | `server/src/jobs.rs`：首次「边用边引导」五步（记 / 问 / 接一个来源 / AI 学一小批 / 看进度，进度随库同步 `.litmus/onboarding.json`；库里已有 ≥20 篇的老库只给新东西）；每个 `X.Y.0` 第一次打开取内置 CHANGELOG 那一节做「这一版的新东西」；AI 学习与巡检前的隐私告知（同意按服务商主机记在 `.litmus/local/consent.json`，换主机重问，`/设置` 可撤回，服务端 428 强制）；数据源先只接最近 N 个（`Source.limit`），扫描在低优先级线程、先文字后截图、每 4 秒交一批（场上分批出现，输入/拖动/编辑时不刷新）；`GET /jobs` 汇总接入 / AI 学习 / 巡检，壳左上角小进度条可暂停、继续、停下；AI 学习学成「要点 + 出处」草稿存 `.litmus/local/digest.json`，`/待收下` 确认后才写进 `Notes/学习/` 并排进复习 | 已完成 |
| 筛选 | `server/src/curate.rs`：规则把每篇分成十二类（会话、过程文档、生成物默认剔除），把握低于 0.6 的分批交模型复核（需隐私同意），结果是清单（`.litmus/local/curate.json`），用户改判后 `/field/curate/apply` 执行：库内进回收站（`trash_move`），库外从索引拿掉并记入 excluded（重扫不回来）；每批可撤销，改判记 overrides；扫描新条目先 `hold_new`，像过程文档的扣下等确认；设置随库同步 `.litmus/curate.json`；「帮我…剔除…会话/过程」直接打开筛选 | 已完成 |
| 对接 | `server/src/connectors.rs`：数据源新种类 github / notion / sanity / openflow / mflow（`Source.conf` 存设置，密钥在凭证库：github_token / notion_token / sanity_token / openflow_api_key / mflow_token），`Item.etag` 做增量；`/connectors` 说明与可发目标、`/connectors/test` 测连接、`/connectors/publish`（confirm 必填）发回去：GitHub contents PUT、Notion MCP post-page、Sanity mutate 草稿、OpenFlow MCP article_create（草稿）、MFlow /api/item/upsert；发过的记 `.litmus/local/outbound.json`。MCP 客户端加 HTTP 传输（`ConnectionConfig.url/headers`，JSON 或 SSE 回应，Mcp-Session-Id） | 已完成 |

其他未做：学习按节自评，不做逐题间隔重复；凭证库是明文存本机（0600），没有加密；过时检测靠规则 + 可选 AI 核实，不做跨文档事实冲突比对。

## 6. 风险

- **用户会记住位置**：所以增量落位不挪旧点；整体重排只在变动超过三成或用户主动 `/重排` 时发生。
- **大库首建**：3 千篇约 4.5s，1.2 万篇预计 15s 左右，首屏显示「正在铺开你的库」说明原因；之后只算新增。
- **离开文件夹的不安感**：阅读页顶部始终显示文件路径；需要逐个文件夹整理时回经典壳。
