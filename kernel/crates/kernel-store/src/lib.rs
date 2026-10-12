//! kernel-store: 库布局、thirdc.toml 配置、内容寻址存储。
//!
//! 真相区（Notes/ Assets/ thirdc.toml）用户可直改；sidecar（.thirdc/）可随时删除重建。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub mod index;
pub mod refs;

pub const CONFIG_FILE: &str = "thirdc.toml";
pub const SIDECAR_DIR: &str = ".thirdc";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("not a thirdc vault: {0} (missing thirdc.toml)")]
    NotAVault(PathBuf),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("config: {0}")]
    Config(String),
    #[error("index: {0}")]
    Index(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VaultConfig {
    /// 库名。
    pub name: String,
    /// 同步拓扑：none | p2p | server | object-storage
    #[serde(default)]
    pub sync: SyncConfig,
    /// 图床策略：vault | s3 | github | custom（Phase 1 落地）
    #[serde(default)]
    pub assets: AssetConfig,
    /// 外部数据源连接（MCP server 等），"对接所有"的入口配置。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<ConnectionConfig>,
    /// 对话/agent 使用的模型端点（OpenAI 兼容）。缺省则对话走本地命令模式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<AiConfig>,
    /// 浏览器后端（页面消化：ego-lite / headless Chromium / CDP）
    #[serde(default)]
    pub browser: BrowserConfig,
    /// 发布：站点构建目录与推送目标
    #[serde(default)]
    pub publish: PublishConfig,
    /// Web 登录凭据覆盖。缺省 admin / machine token；设置了用户名密码则以这里为准。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// 回收站保留天数：删除的文档先进 `.thirdc/trash/`，到期自动清除。
    /// 缺省 30；`0` = 永久保留（不自动清除）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_days: Option<u32>,
    /// 备份：age 口令加密快照 → 多目标（WebDAV / 本地文件夹 / rclone 桥）。
    /// 3-2-1：本地库 1 份 + cloud ≥1 + nas/offsite ≥1。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupConfig>,
}

/// E2EE 备份配置。口令不设或留空则读环境变量 THIRDC_BACKUP_PASSPHRASE；
/// **口令即恢复的唯一凭证**——丢了 = 快照成废纸（age scrypt，无后门）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackupConfig {
    #[serde(default)]
    pub passphrase: String,
    /// 每目标保留快照数（超出删最旧），缺省 10。
    #[serde(default = "default_backup_keep")]
    pub keep: u32,
    /// 自动备份间隔（小时）；0 = 仅手动。
    #[serde(default)]
    pub auto_hours: u32,
    /// 把 .thirdc/ops/（CRDT 增量日志）也打进快照。缺省 false——文件真相已含全部内容，
    /// ops 只是协作历史；打开后单快照显著变大。
    #[serde(default)]
    pub include_ops: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<BackupTarget>,
}

/// 备份目标。kind：webdav（飞牛/群晖/坚果云/Nextcloud）| local（文件夹/iCloud Drive）| rclone（Proton Drive/Dropbox/任意）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackupTarget {
    pub name: String,
    pub kind: String,
    /// 层级：cloud | nas | offsite。缺省按 kind 推断（rclone→cloud，webdav→nas，local→cloud）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tier: String,
    /// webdav：集合 URL，如 https://nas.local:5006/home/Litmus/
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// webdav 凭据（也可经 THIRDC_WEBDAV_USER/PASS 环境变量）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// local：目标目录（iCloud Drive 例：~/Library/Mobile Documents/com~apple~CloudDocs/Litmus-Backup）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// rclone：远端与路径，如 proton:Litmus-Backup（需 rclone 配置同名 remote）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

fn default_backup_keep() -> u32 {
    10
}

/// 登录凭据覆盖（thirdc.toml [auth]）。API token 鉴权不受影响，这里只管 Web 登录表单。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// 发布配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PublishConfig {
    /// 站点构建目录（相对库根；git 目标会改写到各自的部署目录）
    #[serde(default = "default_site_dir")]
    pub site_dir: String,
    /// 默认目标名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<PublishTarget>,
    /// 公开站点根地址（用于 sitemap / 复制链接）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 外部变更后自动发布（PUB-5）
    #[serde(default)]
    pub auto: bool,
    /// 发布前检查（PUB-4 国内合规钩子）
    #[serde(default)]
    pub checks: ChecksConfig,
}

/// 发布前检查配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChecksConfig {
    /// 敏感词表：命中即阻断发布
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitive_words: Vec<String>,
    /// 外部机审 API：POST {files:[{path,sha256}]}，期望 {ok:bool, reasons?:[..]}
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_api: Option<String>,
    /// 机审不可达时是否阻断（默认仅告警）
    #[serde(default)]
    pub fail_on_audit_error: bool,
}

fn default_site_dir() -> String {
    ".thirdc/site".into()
}

/// 一个发布目标。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PublishTarget {
    pub name: String,
    /// local | git（GitHub Pages / Cloudflare Pages 的 Git 集成）| s3 | cf-pages
    pub kind: String,
    /// 部署目录（相对库根）；缺省：git 用 .thirdc/deploy/<name>，local 用 site_dir
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    // ---- git ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub force: bool,
    // ---- s3 兼容（S3 / R2 / OSS / COS / MinIO）----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    // ---- cloudflare pages ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
    /// API 基址（缺省 https://api.cloudflare.com；测试/国内网络可改）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    // ---- 国内合规（PUB-4）----
    /// 国内目标：同步与发布隔离策略、页脚备案、机审必过
    #[serde(default)]
    pub domestic: bool,
    /// ICP 备案号（domestic 时必填，注入页脚）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icp: Option<String>,
}

/// 页面消化后端配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BrowserConfig {
    /// ego-browser CLI 路径（缺省自动探测 ~/.local/bin/ego-browser）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ego: Option<String>,
    /// Chromium/Chrome 可执行文件（缺省自动探测）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome: Option<String>,
    /// 预留：已运行浏览器的 CDP 端点，如 http://127.0.0.1:9222
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp: Option<String>,
}

/// OpenAI 兼容的模型端点配置（OpenAI / DeepSeek / Ollama / vLLM / 任意兼容服务）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiConfig {
    /// 形如 https://api.openai.com/v1 或 http://localhost:11434/v1
    pub base_url: String,
    /// 可直接写，也可留空并用环境变量 THIRDC_AI_API_KEY（更安全）
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    /// Embeddings 模型（密向量召回，走 {base_url}/embeddings）。留空 = 不启用，检索退回 TF-IDF。
    /// 向量只进 index.db 旁路表 doc_vectors，可随时删除重建（文件真相纪律 §1）。
    #[serde(default)]
    pub embedding_model: String,
    /// 单轮对话最大工具步数
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
}

fn default_max_steps() -> usize {
    6
}

impl AiConfig {
    /// 密钥解析：配置优先，其次环境变量。
    pub fn resolved_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        std::env::var("THIRDC_AI_API_KEY").unwrap_or_default()
    }
}

/// 一个外部数据源连接。MCP server 以子进程方式启动并走 stdio 协议。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConnectionConfig {
    /// 连接名（导入路径与去重键的一部分）。
    pub name: String,
    /// 启动命令，如 npx / uvx / 本地二进制。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncConfig {
    #[serde(default)]
    pub mode: String,
    /// 同步 endpoint（自建服务器 URL 或对象存储桶）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// E2EE 开关（自建推荐默认开）
    #[serde(default)]
    pub e2ee: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetConfig {
    #[serde(default = "default_asset_mode")]
    pub mode: String,
}

impl Default for AssetConfig {
    fn default() -> Self {
        AssetConfig {
            mode: default_asset_mode(),
        }
    }
}

fn default_asset_mode() -> String {
    "vault".into()
}

/// 设备身份与本地 API 令牌。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MachineConfig {
    pub device_id: String,
    pub token: String,
}

/// 已打开的库。
#[derive(Debug, Clone)]
pub struct Vault {
    pub root: PathBuf,
    pub config: VaultConfig,
}

impl Vault {
    /// 打开已有库；无配置则报 NotAVault。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        let cfg_path = root.join(CONFIG_FILE);
        if !cfg_path.is_file() {
            return Err(StoreError::NotAVault(root));
        }
        let raw = fs::read_to_string(&cfg_path)?;
        let config: VaultConfig = toml::from_str(&raw).map_err(|e| StoreError::Config(e.to_string()))?;
        Ok(Vault { root, config })
    }

    /// 初始化新库：thirdc.toml + Notes/ + Assets/ + .thirdc/ 全套布局。
    pub fn init(root: impl AsRef<Path>, name: &str) -> Result<Self, StoreError> {
        let root = root.as_ref();
        fs::create_dir_all(root)?;
        let cfg_path = root.join(CONFIG_FILE);
        if cfg_path.exists() {
            return Err(StoreError::Config(format!(
                "{} already exists",
                cfg_path.display()
            )));
        }
        fs::create_dir_all(root.join("Notes"))?;
        fs::create_dir_all(root.join("Assets"))?;
        for d in ["ops", "index", "blobs", "publish"] {
            fs::create_dir_all(root.join(SIDECAR_DIR).join(d))?;
        }
        let config = VaultConfig {
            name: name.to_string(),
            ..Default::default()
        };
        let toml_str = toml::to_string_pretty(&config)
            .map_err(|e| StoreError::Config(e.to_string()))?;
        fs::write(&cfg_path, toml_str)?;
        Ok(Vault {
            root: root.to_path_buf(),
            config,
        })
    }

    pub fn notes_dir(&self) -> PathBuf {
        self.root.join("Notes")
    }
    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("Assets")
    }
    pub fn sidecar(&self) -> PathBuf {
        self.root.join(SIDECAR_DIR)
    }
    pub fn ops_dir(&self) -> PathBuf {
        self.sidecar().join("ops")
    }
    /// 事件/审计目录（append-only 日志）。
    pub fn events_dir(&self) -> PathBuf {
        self.sidecar().join("events")
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.sidecar().join("blobs")
    }

    /// 读取/生成设备身份与本地 API 令牌（.thirdc/machine.toml）。
    pub fn ensure_machine(&self) -> Result<MachineConfig, StoreError> {
        let path = self.sidecar().join("machine.toml");
        if path.is_file() {
            let raw = fs::read_to_string(&path)?;
            let m: MachineConfig = toml::from_str(&raw).map_err(|e| StoreError::Config(e.to_string()))?;
            if !m.token.is_empty() {
                return Ok(m);
            }
        }
        let m = MachineConfig {
            device_id: ulid::Ulid::new().to_string(),
            // 两段 ULID = 128 位随机量，仅用于本机回环认证
            token: format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new()).to_lowercase(),
        };
        let raw = toml::to_string_pretty(&m).map_err(|e| StoreError::Config(e.to_string()))?;
        fs::write(&path, raw)?;
        Ok(m)
    }
}

/// 内容寻址：sha256 -> ab/cd/<hash> 两级目录，真相区 Assets/ 与 sidecar blobs/ 同规则。
pub struct Cas;

impl Cas {
    /// 写入字节，返回 hex hash。已存在则跳过（天然去重）。
    pub fn put(base: &Path, data: &[u8]) -> Result<String, StoreError> {
        let hash = Self::hash_hex(data);
        let path = Self::path_for(base, &hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            // 先写临时文件再 rename，崩溃安全
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, data)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(hash)
    }

    pub fn get(base: &Path, hash: &str) -> Result<Vec<u8>, StoreError> {
        Ok(fs::read(Self::path_for(base, hash))?)
    }

    pub fn exists(base: &Path, hash: &str) -> bool {
        Self::path_for(base, hash).is_file()
    }

    pub fn path_for(base: &Path, hash: &str) -> PathBuf {
        base.join(&hash[..2]).join(&hash[2..4]).join(hash)
    }

    /// 带扩展名的内容寻址（附件用：Obsidian 等可直接渲染）。
    pub fn path_for_ext(base: &Path, hash: &str, ext: &str) -> PathBuf {
        let name = if ext.is_empty() {
            hash.to_string()
        } else {
            format!("{hash}.{ext}")
        };
        base.join(&hash[..2]).join(&hash[2..4]).join(name)
    }

    /// 写入附件字节，保留扩展名；返回 (hash, 绝对路径)。已存在则天然去重。
    pub fn put_with_ext(
        base: &Path,
        data: &[u8],
        ext: &str,
    ) -> Result<(String, PathBuf), StoreError> {
        let hash = Self::hash_hex(data);
        let path = Self::path_for_ext(base, &hash, ext);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, data)?;
            fs::rename(&tmp, &path)?;
        }
        Ok((hash, path))
    }

    pub fn hash_hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        hex::encode(h.finalize())
    }
}

/// 事件目录（server 侧使用）。
pub fn events_dir(vault: &Vault) -> PathBuf {
    vault.events_dir()
}

/// 生成文档 ID（ULID，时间有序）。
pub fn new_doc_id() -> String {
    ulid::Ulid::new().to_string()
}

/// 原样文本（verbatim）文档：代码 / 结构化文本，字节级往返，不做 Markdown 规范化。
pub const VERBATIM_EXTS: &[&str] = &[
    "py", "js", "mjs", "cjs", "ts", "jsx", "tsx", "json", "jsonc", "yaml", "yml", "toml", "ini",
    "cfg", "conf", "css", "scss", "less", "sh", "bash", "zsh", "fish", "rs", "go", "c", "h",
    "cpp", "hpp", "java", "kt", "swift", "rb", "php", "sql", "xml", "csv", "tsv", "log", "txt",
];

pub fn is_verbatim_rel(rel: &str) -> bool {
    let ext = rel.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    rel.contains('.') && VERBATIM_EXTS.contains(&ext.as_str())
}

/// 遍历 Notes/ 下所有文档文件（相对路径）：Markdown / HTML 一等文档 / verbatim 代码与结构化文本。
pub fn list_docs(vault: &Vault) -> Result<Vec<PathBuf>, StoreError> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(vault.notes_dir())
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        // 跳过 macOS 元数据垃圾（._foo 是 AppleDouble，.DS_Store 是 Finder）
        let name = entry.file_name().to_string_lossy();
        if name.starts_with("._") || name == ".DS_Store" {
            continue;
        }
        let is_doc = p.is_file()
            && p.extension()
                .map_or(false, |e| {
                    let e = e.to_string_lossy();
                    e == "md" || e == "html" || e == "htm"
                        || VERBATIM_EXTS.contains(&e.as_ref())
                });
        if is_doc {
            out.push(p.strip_prefix(&vault.root).unwrap().to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// 非文档文件（Notes/ 下任意扩展名）：文件即真相——内核不解析 ≠ 不可检索不可管理。
pub fn list_non_doc_files(vault: &Vault) -> Result<Vec<PathBuf>, StoreError> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(vault.notes_dir())
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy();
        if name.starts_with("._") || name == ".DS_Store" {
            continue;
        }
        let is_doc = p.is_file()
            && p.extension().map_or(false, |e| {
                let e = e.to_string_lossy();
                e == "md" || e == "html" || e == "htm"
                    || VERBATIM_EXTS.contains(&e.as_ref())
            });
        if p.is_file() && !is_doc {
            out.push(p.strip_prefix(&vault.root).unwrap().to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_open_cas_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::init(dir.path(), "test").unwrap();
        assert!(v.root.join("Notes").is_dir());
        assert!(v.sidecar().join("ops").is_dir());
        let v2 = Vault::open(dir.path()).unwrap();
        assert_eq!(v2.config.name, "test");

        let h = Cas::put(&v.blobs_dir(), b"hello").unwrap();
        assert!(Cas::exists(&v.blobs_dir(), &h));
        assert_eq!(Cas::get(&v.blobs_dir(), &h).unwrap(), b"hello");
        // 去重：再写返回同 hash
        assert_eq!(Cas::put(&v.blobs_dir(), b"hello").unwrap(), h);
    }

    #[test]
    fn open_rejects_non_vault() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Vault::open(dir.path()),
            Err(StoreError::NotAVault(_))
        ));
    }
}
