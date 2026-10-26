# Local API Spec — `litmus serve`

状态：M4 已落地。这是所有客户端（桌面/移动/Web/扩展/agent）的唯一入口。

## 启动

```
litmus serve <vault> [--addr 127.0.0.1:7700]
```
- 默认仅绑定 **loopback**，不对外网暴露。
- 启动时输出 `api token`；令牌持久化于 `.litmus/local/machine.toml`（首次自动生成，128 位随机）。
- 同时启动内建文件 watcher：外部编辑在 daemon 运行时实时合入 op-log 与索引。

## 认证

每个请求需带 `Authorization: Bearer <token>` 或 `x-litmus-token: <token>`。不匹配一律 401。

## 端点

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | `{ok, name, version}` |
| GET | `/status` | 库名、根路径、文档数、索引数、附件数 |
| GET | `/search?q=` | FTS5 检索，返回 `{hits:[{path, rank}]}` |
| GET | `/doc?path=Notes/x.md` | `{path, title, blocks, markdown, html}` |
| PUT | `/doc?path=Notes/x.md` | body = markdown；写文件 + op-log + 索引 |
| DELETE | `/doc?path=Notes/x.md` | **软删除**：移入 `.litmus/local/trash/`（文件与文件夹皆可），回收索引；返回 `{deleted, trash, retention_days}` |
| GET | `/trash` | 回收站列表 `{retention_days, entries:[{path, deleted_at, days_left}]}`；顺带清除过期项 |
| POST | `/trash/restore` | `{path}` → 恢复到原路径（目标已存在则 409） |
| POST | `/trash/purge` | `{path}` 或 `{all:true}` → 彻底删除（不可恢复） |
| POST | `/view/resolve` | body `{spec}`（yamlish 字符串）或 `{prompt}`（AI 一句话建视图，需配置模型否则 501）→ `{spec, spec_text, mode, rows, html}` |
| POST | `/asset?name=photo.png` | body = 字节；内容寻址入库，返回 hash/path/markdown |
| POST | `/sync` | 扫描外部改动，返回 `{changed}` |

`html` 字段为预留（AI-HTML 渲染管线接入后填充）。

**回收站保留期**：`litmus.toml` 可配 `trash_days = N`（缺省 30；`0` = 永久保留）。到期项在每次 `DELETE`/`GET /trash` 时自动清除。

## 安全

- **路径校验**：文档路径必须 `Notes/` 前缀、不含 `..`、不以 `/` 开头、不含 `\`，防目录穿越。
- 令牌仅本机，日志不打印请求体。
- 未来（M5+）：能力分级授权（read/write/capture/publish/browser/system）、审计事件落 `event log`。

## 并发模型（M4 边界）

- 单个 `Kernel`（含 rusqlite 连接与 OpLog）由 `Arc<Mutex<>>` 保护；HTTP 处理与 watcher 线程共享同一实例，避免双写 op-log。
- **同一个库同一时刻只应有一个 daemon 进程**（多进程直接写 `.litmus/local/` 不受支持）。多客户端通过同一 daemon 协作；跨设备通过同步协议（M2）而非共享目录。

## 客户端形态映射

- 桌面 / 移动 / Web PWA / 浏览器扩展 → 都调这套 API。
- 内嵌模式（桌面 App 进程内）直接链接 `kernel-core`，不需要 HTTP；两种形态行为一致。
