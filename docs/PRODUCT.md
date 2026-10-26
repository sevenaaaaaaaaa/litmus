# Litmus 产品方向 · 十条原则

> 一句话：**给「一人公司（OPC）」和独立创造者的 AI 原生知识底座**——
> 你的知识是文件，是画布，也是 agent 的工具。开源、可自托管、无锁定。

## 0. 目标用户（先明确，否则每条原则都会走偏）

| 用户 | 要什么 | Litmus 的答案 |
|---|---|---|
| **OPC / 一人公司** | 一个人的产出要顶一个团队 | agent 采集 + 整理 + 发布全链路，知识自动变内容 |
| 独立开发者 | 数据在自己手里，能改能扩 | 文件真相 + 插件/MCP + 单二进制 |
| AI 爱好者 | 把玩最新模型与协议 | MCP 双向、A2UI、设计规范消化 |
| 生态伙伴 | 能被集成、也能集成别人 | 开放 API + MCP + 稳定文件格式（OKF） |

> 大库（2 万+）性能是**迁移一次性问题**，不是日常路径。日常必须极快——
> 这是「更易用」的第一条工程约束。

---

## 1. 更易用（Zero-friction first）

**原则**：任何功能，第一次使用不能超过 3 步；出错必须给出「下一步怎么办」。

落地机制：
- 三秒落库：热键 / 右键 / 粘贴 / 拖入，四个入口都通
- 空库不是空白：采集向导（主题一键建库）、模板、示例
- 每个错误都带**修复建议**（如「机审不可达 → 可配 fail_on_audit_error=false」）
- **同步永不阻塞**：单次同步有批量上限，daemon 永远可响应（已落地：`sync` 分批）
- 一个刷新按钮解决 90% 的「我看不到更新」

## 2. 更敏捷（快是正确的副产品）

- 所有写路径先落文件（毫秒级），索引/向量/op-log 都是**可重建的派生层**
- 派生层异步补：写 = 立即成功，索引 = 秒级跟上
- 单二进制、零外部依赖（SQLite bundled、纯 Rust TF-IDF）
- 发布 = 增量构建 + 增量上传（改一篇 → 线上 < 30s）

## 3. 更自我进化（Self-evolving）

**原则**：产品应该越用越懂你，而不是越用越乱。

- 设计规范可消化（design.md / 页面 / SKILL 目录）→ 输出自动遵循你的审美
- 一键整理（按标签/语义聚类）→ 结构自己长出来
- Agent 记忆（规划中）：对话历史入库、可检索、可复盘
- 使用信号 → 建议（如「这篇和那篇高度重合，要合并吗」）

## 4. 更 AI Agent（工具的一等用户是 agent）

- **MCP 双向**：既是 server（库给 agent 用），也是 client（别人给库供数据）
- 工具集覆盖完整闭环：`search_vault` / `search_semantic` / `read_doc` / `write_doc` / `put_asset` / `import_design` / `import_page_design` / `publish`
- 每次工具调用前自动 `sync`，agent 永远看到最新状态
- 产出落到画布（可视化因果），而不是消失在对话里

## 5. 更 AI Native（不是加个聊天框）

- **OKF 开放知识格式**：Markdown 语义 + YAML 元数据 + `^锚点`，机器可读、人类可懂
- **AI-HTML**：语义标签 + 内嵌 JSON-LD，人与 agent 双可读
- **A2UI**：agent 推声明式 UI，客户端原生渲染（不执行任意代码）
- **混合检索**：FTS5 + TF-IDF 向量 + RRF，语义可达
- 文档即上下文：任何一篇都能直接喂给模型（含结构化元数据）

## 6. 更开放（Open by default）

- AGPL-3.0 开源，规范文档（docs/spec）与实现同仓
- 数据格式公开（Markdown / HTML / OKF / JSON-LD）
- 无私有云依赖：本地 / 自建 / 任意对象存储
- 导出永不锁死：Markdown + 附件原样可走

## 7. 更可定制（Everything is a plugin or a config）

- **连接器 = 配置**（`[[connections]]` 加一个 MCP server，30 秒接新数据源）
- **设计 = 数据**（11 套预设 + 自定义 design.md）
- **视图 = 声明式**（Views 层规划：表/看板/时间线/日历）
- **发布 = 适配器**（local/git/S3/CF Pages/WebDAV，加一个 kind 就是新目标）
- 插件沙箱（规划：WASM）——解析器与渲染器可第三方提供

## 8. 更兼容（Meet users where they are）

- 文件：Obsidian 库直接导入（实测 1.2 万篇）、Markdown/HTML 双向
- 编辑器：任何编辑器改文件，内核无损跟随
- 生态：GitHub 生态（git-backed 库、Pages 发布）、对象存储、WebDAV 网盘
- 浏览器：Chrome/Edge/Firefox/Safari 扩展
- 客户端：Web/PWA、CLI、MCP、HTTP API、（桌面 Tauri、WASM 在途）

## 9. 面向生态伙伴 / 开发者 / AI 爱好者

- **开发者**：内核 crate 化（kernel-md/store/sync/a2ui/deploy 可单独用）；HTTP API 全量覆盖；`AGENTS.md` + `docs/spec/*`
- **生态伙伴**：MCP 即接口（任何产品接一个 MCP server 就能读写 Litmus）；发布到他们的 Git 仓库
- **AI 爱好者**：A2UI 原生渲染、页面消化（ego-lite/CDP）、设计规范学习
- **可被集成**：`litmus mcp` 让 Claude/Cursor 立刻拥有你的知识库

## 10. 面向 OPC（一人公司）

一个人的时间是最稀缺资源，所以：
- **采集代替整理**：主题建库 + 网页本地化 + PDF/DOCX 解析 → 知识自动进来
- **agent 代替流程**：一键整理、AI 视图、发布自动化
- **一份知识多次变现**：库 → AI-HTML 站点 → 整库 PPT → 加密分享链接
- **零运维**：单二进制 / Docker / systemd，一键部署（实测 3 分钟上线）

---

##  roadmap（按「原则 → 里程碑」映射）

| 里程碑 | 覆盖原则 | 内容 | 期 |
|---|---|---|---|
| **SYNC-1 分批同步** | 1 敏捷 | 单次同步有上限，daemon 永不阻塞 | ✅ 本次 |
| **LAZY-1 大库懒加载** | 2 敏捷 | Automerge doc LRU 驱逐（仅迁移场景需要） | 按需 |
| **AGENT-1 Agent 记忆** | 3 自进化 | 对话/决策入库，可检索复盘 | Day 6 |
| **VIEW-1 Views 层** | 7 定制 | 声明式表/看板/时间线视图（内联数据库） | Day 7 |
| **PLUG-1 插件沙箱** | 6/7 开放可定制 | WASM 解析器/渲染器 | Day 8 |
| **DESK-1 Tauri 打包** | 8 兼容 | 桌面安装包（内核内嵌） | ✅ 已落地 |
| **WEB-1 WASM** | 8 兼容 | 纯浏览器版（OPFS 真文件库） | Day 10 |
| **ECO-1 伙伴接入指南** | 9 生态 | 「30 分钟接入 Litmus」文档 + MCP 模板 | Day 11 |
| **OPC-1 内容流水线** | 10 OPC | 采集 → 整理 → 视图 → 发布 → 分发 一键模板 | Day 12 |
