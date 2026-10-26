# MCP Spec — 双向 MCP

状态：M9 出口侧（MCP server）已落地；入口侧（MCP client）待办。

## 出口：知识库暴露给 agent（已落地）

`litmus mcp <vault>` 以 stdio 运行 MCP server，协议 JSON-RPC 2.0。
传输与协议解耦：`McpServer::handle()` 为纯消息处理，后续 HTTP/SSE 复用同一实现。

### 客户端接入

Claude Desktop（`claude_desktop_config.json`）/ Cursor（`~/.cursor/mcp.json`）：

```json
{
  "mcpServers": {
    "litmus": {
      "command": "/绝对路径/litmus",
      "args": ["mcp", "/绝对路径/MyVault"]
    }
  }
}
```

### 协议

- 支持版本：`2024-11-05`、`2025-03-26`、`2025-06-18`（客户端请求在列表内则回显，否则返回最新）。
- capabilities：`tools`、`resources`。
- 通知（无 `id`）不产生响应，符合规范。

### Tools

| 名称 | 参数 | 说明 |
|---|---|---|
| `search_vault` | `query`, `limit?` | FTS5 检索，返回路径与 bm25 相关度 |
| `list_docs` | — | 全部文档（路径 + 标题） |
| `read_doc` | `path` | Markdown + AI-HTML + 块模型 |
| `write_doc` | `path`, `markdown` | 创建/覆盖（文件 + op-log + 索引） |
| `delete_doc` | `path` | 删除文档 |
| `put_asset` | `name`, `content_base64` | 附件入库，返回可粘贴的 Markdown 引用 |
| `vault_status` | — | 文档数 / 索引数 / 附件数 |

每次调用前自动 `sync_all()`，保证 agent 看到外部编辑后的最新状态。
返回值同时给出 `content[].text`（人/LLM 友好）与 `structuredContent`（结构化）。

### Resources

- URI：`knowledge://Notes/xxx.md`
- `resources/read` 返回**两种表示**：`text/markdown` 与 `text/html`（AI-HTML，自包含 + 内嵌 JSON-LD）。
- 这让任意 agent 既能读结构，也能拿到可直接呈现的富文档。

### 安全

- 与 HTTP API 共用同一路径校验（`is_safe_doc_path`）：仅 `Notes/` 下、禁止 `..` 与反斜杠。
- 错误码：`-32700` 解析失败、`-32601` 未知方法、`-32602` 参数/路径非法、`-32603` 内部错误。
- stdio 模式无网络暴露；写入动作计入文件真相，天然可 diff、可回溯。

## 入口：任意 MCP server 变成数据源（已落地）

内核作 MCP **client**，以子进程方式启动第三方 MCP server 并走 stdio 协议。

### 配置（litmus.toml）

```toml
[[connections]]
name = "knowledge-base"
command = "python3"          # 或 npx / uvx / 本地二进制
args = ["/path/to/server.py"]
# env = { TOKEN = "..." }
```

### CLI

| 命令 | 说明 |
|---|---|
| `litmus conn list <vault>` | 列出已配置连接 |
| `litmus conn probe <vault> <name>` | 握手 + 列出 tools / resources |
| `litmus conn pull <vault> <name>` | 采集该连接**全部 resources** 入库 |
| `litmus conn call <vault> <name> <tool> --args '{"k":"v"}' [--import]` | 调用 tool；`--import` 把返回文本也入库 |

### 采集语义

- 落点：`Notes/Sources/<connection>/<标题>.md`（Unicode 标题保留为可读文件名）。
- 顶部插入可见来源行：`> 来源：[uri](uri)　连接：name　类型：mime　采集：时间戳`。
- 机器溯源存索引 `items(connection, uri, rel, fetched_at)` 表。
- **幂等**：同一 `connection + uri` 重复采集**更新同一篇文档**，不产生副本。
- 采集结果走正常管道：文件真相 + op-log + 全文索引，立即可检索、可协作。

### 实现要点

- 子进程 stdio + 独立读线程 + mpsc 通道，请求级别超时（30s），不会因 server 卡死而挂住内核。
- initialize 握手后自动发送 `notifications/initialized`；请求按 id 匹配，跳过通知与无关输出。
- 失败隔离：单个 resource 读取失败只 skip 并报告，不影响其余采集。

### 待办

- HTTP/SSE 与 Streamable HTTP 传输（远程 MCP server）。
- 定时/事件触发采集（自动化触发器）。
- 工具调用 → 结构化块的映射（当前为文本入库）。
