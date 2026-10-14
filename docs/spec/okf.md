# Open Knowledge Format（OKF）· 开放知识格式

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
