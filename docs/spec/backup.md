# Backup Spec — E2EE 备份与 3-2-1 纪律

状态：v1 已落地（kernel-backup crate + CLI + /backup/* 端点）

## 威胁模型（先于一切）

云盘服务商、NAS 管理员、对象存储运维**不可信**。快照在本机完成
**tar.gz → gzip → age（口令 scrypt 派生）** 流式加密后才出网——
远端只见 `.tar.age` 密文。iCloud / 群晖 / Proton / 任何中间人都解不开；
**口令是唯一恢复凭证**（无后门，丢了 = 快照作废）。口令只存 litmus.toml
`[backup] passphrase`（不入 git）或环境变量 `LITMUS_BACKUP_PASSPHRASE`。

## 3-2-1 纪律

- **第 1 份**：本地库本身（文件真相，普通文件）。
- **第 2 份**：cloud ≥1（Proton Drive / Dropbox / iCloud Drive / S3）。
- **第 3 份**：nas/offsite ≥1（飞牛 fnOS / 群晖 / 另一台服务器）。
- `litmus backup` 结束时输出合规判定；`GET /backup/status` 持续可查。
  目标可显式标 `tier = "cloud"|"nas"|"offsite"`，缺省按 kind 推断
  （rclone→cloud，webdav→nas，local→cloud）。

## 快照制

| 文件 | 内容 | 加密 |
|---|---|---|
| `snap-<ts>.json` | 元数据（id/vault/host/files/bytes/created/format） | 明文（免密列举快照清单） |
| `snap-<ts>.tar.age` | 库内容归档（流式管道，常量内存） | age 口令加密 |

排除（默认）：`.litmus/local/index/`（可重建）、`.litmus/local/machine.toml`（机器凭据）、
`.litmus/local/backup-state.json`、`.litmus/local/deploy/`、`.DS_Store`/`._*`/`*.tmp`。
`.litmus/local/ops/`（CRDT 日志）默认排除——文件真相已含全部内容，ops 只是协作
历史；`include_ops = true` 可带上（快照显著变大）。
**包含**：`Notes/`、`Assets/`、`litmus.toml`、`.litmus/local/` 其余（secrets.json 等，E2EE 后无泄）。

保留：每目标 `keep` 份（缺省 10），超出自动删最旧（json+age 成对）。

## 目标矩阵

| kind | 覆盖 | 说明 |
|---|---|---|
| `webdav` | **飞牛 fnOS**、**群晖**（WebDAV Server 套件）、坚果云、Nextcloud | 原生客户端：MKCOL 建目录 + PUT；PROPFIND 列举；DELETE 修剪。凭据 `username/password` 或 `LITMUS_WEBDAV_USER/PASS` |
| `local` | **iCloud Drive**（指向 `~/Library/Mobile Documents/com~apple~CloudDocs/…`）、挂载盘、U 盘 | 目录即目标；写出的 `.tar.age` 在 iCloud 侧同样密文不可读 |
| `rclone` | **Proton Drive**、**Dropbox** 及 rclone 全部后端 | 桥接本机 rclone（需已 `rclone config`）：copyto / lsf / deletefile |
| `s3`（规划 v1.5） | B2/R2/OSS | 复用 kernel-deploy SigV4 |

## 配置（litmus.toml）

```toml
[backup]
passphrase = "…"        # 或环境变量 LITMUS_BACKUP_PASSPHRASE；空 = 备份禁用
keep = 10               # 每目标保留快照数
auto_hours = 24         # 自动备份间隔；0 = 仅手动
include_ops = false     # 是否把 .litmus/local/ops/ 打进快照

[[backup.targets]]
name = "群晖 NAS"
kind = "webdav"
url = "https://nas.local:5006/home/Litmus/"
username = "litmus"
password = "…"
tier = "nas"            # 可省略（webdav 默认 nas）

[[backup.targets]]
name = "iCloud Drive"
kind = "local"
path = "~/Library/Mobile Documents/com~apple~CloudDocs/Litmus-Backup"

[[backup.targets]]
name = "Proton Drive"
kind = "rclone"
remote = "proton:Litmus-Backup"
```

## 入口

- CLI：`litmus backup <vault>`（全部目标）、`--target 名`、`--list`、
  `--restore [快照id] --into <目录>`（缺省恢复最新）。
- HTTP：`POST /backup/run {"target"?}`（后台任务）、`GET /backup/status`
  （running / last_error / 各目标 last_ok / 3-2-1 合规）。
- 自动：daemon 每 30 分钟巡检，`auto_hours` 到期自跑（preheat 线程，不占锁）。

## 恢复安全

`restore` 拒绝归档内越界路径（zip-slip 防护）；`--into` 目录由用户指定，
恢复内容 = 完整库结构（Notes/ Assets/ litmus.toml .litmus/local/），可直接
`litmus serve` 打开。首次备份前**务必**先做一次恢复演练（备份没有验证过
= 没有备份）。

## 已知边界

- 全量快照制：库很大时每份快照 = 全量（压缩后）。增量/去重（borg/restic 式
  分块）是 v2 方向；当前 keep×快照大小是磁盘预算公式。
- WebDAV/rclone 无自动化集成测试（需真实服务），逻辑由 local 往返测试覆盖，
  协议实现保持薄。
- index.db 不入包：恢复后内核自动重建（文件真相纪律的直系好处）。
