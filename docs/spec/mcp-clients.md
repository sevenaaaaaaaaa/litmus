# MCP 互通：谁调谁、怎么接（0.30）

鹿蕊是双向的：外部 agent 用 **MCP 客户端** 调 litmus 的库（读写、搜、记忆、放附件）；litmus 用 **[[connections]]** 反过来把外部工具的数据灌进库。

## 外部工具 → 调 Litmus（一条命令接上）

``bash
litmus mcp-install ~/Documents/Litmus --client cursor --write   # 只打印就去掉 --write
``

| client | 配置落点 | 形态 |
| --- | --- | --- |
| cursor | ~/.cursor/mcp.json | mcpServers.litmus = { command, args } |
| claude | ~/Library/Application Support/Claude/claude_desktop_config.json | 同上 |
| claude-code | 官方命令 | `claude mcp add litmus -- litmus mcp <库>` |
| codex | ~/.codex/config.toml | [mcp_servers.litmus] |
| opencode | ~/.config/opencode/opencode.json | mcp.servers.litmus（local） |
| dsh / hermes / obsidian | 打印指引（各有自己的配置方式） | stdio 入口同上 |
| generic | 打印 mcpServers JSON | 自贴 |

每次写文件前先把原文件备份为 `*.bak-litmus-mcp`。stdio 入口：`litmus mcp <库根>`；工具共 14 个（search / read / write / delete_doc / put_asset / design / memory / vault_status），见 mcp/src/lib.rs。

## Litmus → 外部工具（设计、流程图、笔记）

``bash
litmus conn presets ~/Documents/Litmus   # 打印 Notion / Obsidian / Canva / Open Design / Lovart 的 TOML 样板
litmus conn add ~/Documents/Litmus --name canva --url https://mcp.canva.com/mcp --header 'Authorization=Bearer {{secret:canva_token}}'
litmus conn probe ~/Documents/Litmus canva
litmus conn pull  ~/Documents/Litmus canva     # 采集 resources 进库（ext: 来源，原件不动）
``

- 远程 MCP 填 `url`；本机命令型填 `command` + `args`（env / headers 可用 `{{secret:xxx}}` 从 /凭证 取）。
- 流程图（.mermaid / .drawio / .excalidraw）直接拖库，按附件进 Assets/；社区 MCP server 成熟后照样板接。
- 发回去（write 侧）按 connector kind 提供能力，见 server/src/connectors.rs。
