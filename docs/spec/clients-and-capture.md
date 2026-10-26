# Clients & Capture Spec — 内核是唯一的脑，客户端是感官与手脚

状态：设计冻结，随里程碑逐步落地

## 0. 总原则

内核（Rust）是唯一业务逻辑载体。所有客户端只做两件事：
**把外界数据送进来（capture）** 与 **把库里的东西呈现/送出去（present / publish）**。
客户端之间不允许出现"独有能力"——桌面能做的，Web、扩展、agent 都能通过同一 API 做。

## 1. 内核交付接口（daemon）

- `litmus serve`：本地 loopback 上的 HTTP + WebSocket（JSON-RPC），令牌认证（machine.toml）。
- **MCP 双向**：
  - 出口：内核作为 MCP server（stdio 给本地 agent，HTTP/SSE 给远程 agent），把库暴露成 tools/resources。
  - 入口：内核作为 MCP client，把任意 MCP server 变成数据源（"对接所有"）。
- 三种内核形态、同一份代码：
  - 内嵌（桌面/移动 App 进程内）
  - 独立守护进程（`litmus serve`，可被多客户端共享）
  - Server（`litmus-server` Docker，自建同步/发布主机）

## 2. 全平台客户端矩阵

| 平台 | 技术 | 内核形态 | 能力定位 |
|---|---|---|---|
| macOS/Windows/Linux | Tauri v2 | 内嵌 | 全功能：编辑、视图、协作、发布 |
| iOS / Android | Tauri v2 mobile | 内嵌（受限） | 采集、速记、阅读、检索；重任务委托桌面/server |
| Web | PWA | 远程 daemon 或浏览器内 WASM 内核 | 见 §5 |
| CLI / headless | Rust | 内嵌 | 同步守护、批处理、CI |

一套 Rust 内核 + 一套跨端 UI 组件库，Tauri 与浏览器共用，避免三套前端。

## 3. 浏览器扩展与浏览器联动

MV3，Chrome / Edge / Firefox / Safari：

- **采集**：整页 / 选区 / 截图 / 阅读模式 → MD 或 AI-HTML；自动元数据（标题、作者、时间、来源 URL）；页内图片自动走附件 CAS（图床地基）。
- **联动**：
  - Native Messaging 直连本地内核，数据不经云。
  - 高亮/标注回流：网页标注进库，重新打开网页时回显。
  - 标签会话：一键把本次研究的 N 个标签页存成一个集合（研究快照）。
  - 右键菜单 / 快捷键 / 地址栏采集——"3 秒落库"的主入口。
  - 反向呈现：库中 AI-HTML 在侧边栏渲染，边读边记。
- **WebMCP**：站点暴露工具后，扩展作为桥，让 Studio agent 直接调用站点能力。
- **权限最小化**：默认仅当前页；不读浏览历史；权限清单公开可审计。

## 4. CDP 与 ego-lite

- 内核内置 **CDP 客户端**，连接用户已登录的真实浏览器（`--remote-debugging-port` 或扩展中转）。
  - 不自带浏览器、复用真实会话——能抓需登录页面（Notion/飞书/内网），把 DOM 转成 AI-HTML。
- **ego-lite**（面向 agent 的 Chromium）作为可选浏览器后端：同一 CDP 接口，agent 开真实窗口、人机同屏协作。
- 统一抽象 `browser` 模块：`attach → snapshot(AX/DOM) → act(click/type/navigate) → capture(html/screenshot)`。
- 安全：仅 loopback、显式授权、每步操作写入审计日志（event log）可回放。

## 5. Web 版本（两条路，同一代码）

- **A. 瘦客户端 PWA**：连本地 daemon（localhost）或自建 server（远程）。功能与桌面等价：协作、视图、发布。
- **B. 纯浏览器本地库**：内核编译为 WASM，数据落 OPFS / File System Access API（真文件目录），同步协议不变。不装任何东西、打开即用、数据留在本地/浏览器。
- 发布物天然是 AI-HTML：分享 = 给链接，或给一个自包含文件（人可读、agent 可解析）。

## 6. Computer Use

- **方向一（推荐先做）**：内核作为 MCP server / tools provider，任何 computer-use agent 都能读写知识库、采集网页、发布。
- **方向二**：桌面端暴露无障碍树 + 截图，agent 直接操作 Studio UI（AX 驱动比录屏坐标稳，跨分辨率不失真）。
- **方向三**：Studio 内置 agent 反向操作浏览器（CDP）与系统，跑通"研究 → 采集 → 整理 → 发布"全链路。
- **权限模型**：能力分级 `read / write / capture / publish / browser / system`，逐能力授权；所有动作进审计日志。

## 7. 数据流

```
入口  文件 · 剪藏 · 扩展 · CDP · MCP · API · IM/邮件
        └─▶ Item（原始）─▶ 采集管道 ─▶ Doc + Asset（真相区）
出口  发布(AI-HTML/llms.txt) · MCP · Webhook · API · 导出
```

## 8. 里程碑映射

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M4 | daemon + HTTP API（`litmus serve`）+ 块级 CRDT 合并 | 已落地 |
| M5 | 浏览器扩展 MV3 采集 + native messaging | 待办 |
| M6 | Tauri 桌面客户端 | 待办 |
| M7 | Web（PWA + WASM 内核） | 待办 |
| M8 | browser 模块（CDP + ego-lite）、computer-use 能力分级 | 待办 |
| M9 | MCP server（出口，`litmus mcp`）与 MCP client（入口，`litmus conn` 采集第三方 server） | 双向已落地 |
