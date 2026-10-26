# 安全与隐私

> 原则：**开放性 / 兼容性优先**。库永远是本地明文文件（Markdown/HTML/附件），任何编辑器、
> git、脚本都能直接读写——不为了"防别的工具"而牺牲可移植性。
>
> 数据安全的重心因此收敛为两件事：**不被远程篡改** + **备份上云后网盘方看不到原始数据**。

## 1. 威胁模型与边界（诚实版）

| 想防谁 | 能不能防 | 手段 |
|---|---|---|
| 远端服务器 / 网盘服务方 | ✅ | 客户端加密（AEAD），服务端只存密文 |
| 备份被中途/落地篡改 | ✅ | AEAD tag + 每文件 SHA-256 清单 + 设备签名 |
| 忘记密码后想恢复 | ❌（设计如此） | 丢失密码 = 丢失备份 |
| 本机其他进程读你的库文件 | ❌ 不承诺 | 需要 OS 级沙箱/权限，会破坏"任何编辑器可开" |
| 本机进程读 webview localStorage 里的 token | ❌ 不承诺 | 同上 |

**不把"访问控制"当"隔离/DRM"**：MCP/API（token + 审计）用于约束守规矩的集成方，
不是对抗任意本地进程的安全边界。

## 2. 加密备份（v2，已落地）

实现：`kernel-deploy::encrypted_backup`（`ring`）。

- **机密性**：AES-256-GCM（AEAD）。
- **抗爆破**：PBKDF2-HMAC-SHA256，随机 16B 盐，**600,000** 次迭代。
- **完整性**：AEAD tag 认证整包；包内另带**每文件 SHA-256 清单**（root 哈希）。
- **来源真实性**：Ed25519 **设备密钥签名**（私钥只在本机 `.litmus/local/backup-signing.key`，0600，不进备份）。
- **容器**：`MAGIC("3CBK") ver kdf iters salt nonce pubkey | sig | ct_len ciphertext`；
  头部作为 AEAD 的 AAD 一并认证。
- **不进备份的机密内容**：`.litmus/local/machine.toml`（API token）、签名私钥/公钥、`.litmus/local/backups/`。
- **兼容**：v1（旧 SHA256-CTR 格式）仍可解密，但不再用于新备份；v1 无清单，不能直接还原文件。

API：

- `encrypt_vault(root, password) -> Vec<u8>`
- `decrypt_vault(data, password) -> Vec<u8>`（不落盘；v1/v2 自动识别）
- `restore_vault(data, password, dest_root) -> usize`（校验清单后还原文件）

测试：`kernel/crates/kernel-deploy/tests/backup.rs`
（往返、篡改必被发现、机密文件不入包、清单校验）。

## 3. 路线

- **P2 · 远端同步 E2EE**：落地 `litmus.toml` 的 `[sync] e2ee`——客户端加密、服务器只存密文；
  pull 时按哈希校验。
- **恢复入口**：把 `restore_vault` 接到 `/desktop/backup/restore`（还原到指定目录，默认不覆盖现有库）+ 设置面板。
- **git 快照**：可选对提交签名（Ed25519），并把设备公钥写进 `.litmus/local/`，便于校验历史未被篡改。
- **密钥管理**：设备签名私钥目前落在库内 `.litmus/local/`；后续可迁移到系统 Keychain。

## 4. 边界纪律

- 任何"加密/校验"都必须有**测试向量**，禁止自造加密原语（v1 就是反面教材）。
- 新增出网点 / 上传目标，必须在 `docs/RELEASE.md` 与能力矩阵里标注数据流向。
