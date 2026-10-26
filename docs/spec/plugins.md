# 插件系统规范（v0 + v1 视图 + v2 工具贡献已上线）

> 设计参照：**Obsidian**（清单式 manifest、库内目录、启停管理、社区分发）× **Agent harness 工具思路**（能力声明、宿主注入、最小权限）。
> 原则延续本仓库契约：**文件真相**（插件落盘为普通目录）、**入口唯一家**（Widget 工具区 + 插件面板）、**无构建步骤**。

## 一、形态与位置

- 插件安装到库内 `kb/.litmus/local/plugins/<id>/`，随库走（git / 同步 / 备份都会带上，换设备即插即用）。
- 每个插件一个目录：

```
.litmus/local/plugins/
  demo-daily-note/
    manifest.json      # 清单（唯一必需）
    ...其余文件        # v0 不执行；预留样式 / 预设 / 资源
  installed.json       # 安装状态（enabled / installedAt），由宿主维护，与上游 manifest 分离
```

## 二、manifest.json

```json
{
  "id": "demo-daily-note",
  "name": "每日速记",
  "version": "1.0.0",
  "description": "示例插件：向编辑器插入今日速记模板",
  "author": "you",
  "homepage": "https://github.com/you/demo-daily-note",
  "contributions": {
    "commands": [
      { "id": "insert-daily", "title": "插入今日速记模板",
        "type": "insert-snippet", "payload": "## 今日速记\n\n- " },
      { "id": "open-docs", "title": "打开插件主页", "type": "open-url",
        "payload": "https://github.com/you/demo-daily-note" },
      { "id": "call-api", "title": "推送到我的服务", "type": "http-post",
        "payload": "https://example.com/api", "body": { "k": "v" } }
    ]
  }
}
```

字段约定：
- `id` 必填、全库唯一、只允许字母数字连字符（同时是目录名）；`name` / `version` 必填。
- `contributions.commands[].type` v0 白名单（声明式，宿主代执行，**不运行插件代码**）：
  | type | 行为 |
  |---|---|
  | `insert-snippet` | 向当前编辑器光标处插入 `payload` 文本 |
  | `open-url` | 新标签打开 `payload` |
  | `http-post` | 向 `payload` POST JSON（`body` 可选；跨域由目标服务 CORS 决定） |

## 三、安装：GitHub 链接导入

- 入口：Widget「工具」标签 → `＋ 从 GitHub 安装插件`（或插件面板）。
- `POST /plugins/install {url}`：接受 `https://github.com/<owner>/<repo>[/tree/<branch>[/<subdir>]]`。
  - 服务端经 GitHub API 下载 tarball（≤25MB）→ 系统 `tar` 解包（≤50MB 校验）→
    仓库**根**或**一层子目录**含 `manifest.json` 即认定插件（支持一仓库多插件）→
    复制进 `.litmus/local/plugins/<id>/` 并登记 `installed.json`（默认启用）。
- 其余端点：`GET /plugins`（列表+状态）、`POST /plugins/toggle {name,enabled}`、`POST /plugins/remove {name}`。

## 四、UI 挂载（入口唯一家）

- 已启用插件的命令渲染在 **Widget「工具」标签底部的「插件」区**（`插件名 · 命令名` 按钮）。
- 启停 / 删除 / 安装统一在**插件面板**（`⚙ 管理插件`）。
- 更多菜单与 ⌘K 不重复挂插件入口。

## 五、安全模型（分阶段）

| 阶段 | 能力 | 边界 |
|---|---|---|
| **v0（已上线）** | 声明式命令（snippet / url / http-post） | 插件代码不执行；tarball 25MB、解压 50MB 上限；manifest 字段白名单；`id` 防路径穿越 |
| **v1（已上线）** | 视图扩展：`contributions.views` 声明 HTML 入口，经 `/plugins/view/{id}/{file}` 服务（宿主注入 RPC 垫片 `window.litmus.*`），专用沙箱 iframe 渲染（`sandbox` 无同源——父页面与插件互不可达，实测合成点击也无法穿透）。RPC：`getDocument / insertText / saveDocument / toast / setViewTitle`，宿主按 manifest `permissions` 逐项校验（`doc:read` / `doc:write` / `editor:insert`），装前面板展示权限 | RPC 校验失败一律拒绝并回报原因；插件文件静态供给（防路径穿越）；无任意代码执行面 |
| **v2（已上线）** | Agent 工具贡献：`contributions.tools` 声明工具（`http` 模板化调用外部 API / `kb-search` 库内检索），注册进 agent 工具循环（`plugin_<pid>_<tid>`，仅 AI 对话模式可选）。宿主代执行：SSRF 防护（拒绝内网/回环）、10s 超时、结果 1500 字截断、审计落 `.litmus/local/events/plugin-tools.jsonl`；工具级独立启停（面板操作，installed.json `tools` 覆盖表） | 插件代码不执行；http 仅 http(s) 且 `{参数}` 必须全部提供；命令模式（无模型）不加载插件工具 |
| **v3** | 社区索引：官方 JSON 注册表（仓库地址 + 审核标记），插件面板内一键浏览安装（Obsidian 社区插件模式） |

### 视图 manifest 示例

```json
"permissions": ["doc:read", "editor:insert"],
"contributions": {
  "views": [
    { "id": "panel", "title": "速记面板", "entry": "view.html", "permissions": ["doc:read", "editor:insert"] }
  ]
}
```

插件页面内直接调用（垫片已注入）：

```js
const doc = await litmus.getDocument();      // 需 doc:read
await litmus.insertText('文本');              // 需 editor:insert
await litmus.saveDocument(markdown);          // 需 doc:write
await litmus.toast('提示');                   // 恒可用
await litmus.setViewTitle('新标题');           // 恒可用
```

### 工具 manifest 示例（v2）

```json
"contributions": {
  "tools": [
    { "id": "kbtag", "title": "按标签检索库", "description": "检索知识库并返回相关文档",
      "type": "kb-search" },
    { "id": "wtime", "title": "世界时间", "description": "查询指定时区当前时间",
      "type": "http", "method": "GET", "url": "https://worldtimeapi.org/api/timezone/{zone}",
      "args": { "zone": { "type": "string", "description": "时区名", "required": true } } }
  ]
}
```

- 工具名注册为 `plugin_<插件id>_<工具id>`，仅出现在 AI 对话的工具列表（模型自主选用；命令模式不加载）。
- `http` 工具：`{参数}` 模板替换进 URL（percent-encoded），未出现在 URL 的参数并入 POST body；每次执行写审计。

## 六、测试与验收

- 服务端：手动链路（GitHub 真仓库安装 / 404 错误路径 / toggle / remove / 列表）+ 本地放置 manifest 验证列表。
- 前端：Widget 命令按钮渲染、命令执行（insert-snippet 插入光标处）、面板启停即时生效。
- 门禁同主契约：SW bump → `cargo test --workspace` → `perf-check.sh` → 模式 × 断点截图。
