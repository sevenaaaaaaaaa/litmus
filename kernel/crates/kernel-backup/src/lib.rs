//! kernel-backup — E2EE 备份引擎（3-2-1 纪律）。
//!
//! 威胁模型：云盘 / NAS 管理员 / 网盘服务商**不可信**。快照在本机完成
//! tar.gz → age（口令 scrypt 派生）流式加密后才出网，远端只见 `.tar.age` 密文；
//! iCloud/群晖/Proton 谁都解不开，口令是唯一凭证（丢了 = 快照作废，无后门）。
//!
//! 快照制：每次备份产出一对文件——
//! - `snap-<ts>.json`   明文元数据（id/created/files/bytes/host/vault）：免密列举
//! - `snap-<ts>.tar.age` 密文载荷（流式管道，常量内存，2GB 服务器友好）
//!
//! 目标：webdav（飞牛 fnOS / 群晖 WebDAV Server / 坚果云 / Nextcloud）
//!      local（文件夹——指向 iCloud Drive 目录即 iCloud 目标；任何挂载盘）
//!      rclone（Proton Drive / Dropbox / 任意 rclone 后端，桥接本机 rclone）
//!
//! 3-2-1：本地库恒为第 1 份；targets 按 tier（cloud|nas|offsite）计数，
//! cloud≥1 且 nas/offsite≥1 才算合规。

use kernel_store::{BackupConfig, BackupTarget};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("backup: {0}")]
    Config(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("age: {0}")]
    Age(String),
    #[error("webdav {0}: {1}")]
    WebDav(String, String),
    #[error("rclone: {0}")]
    Rclone(String),
    #[error("tar: {0}")]
    Tar(String),
}

/// 快照明文元数据（随快照上传，免密可列举）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SnapshotMeta {
    pub id: String,
    pub created: u64,
    pub vault: String,
    pub host: String,
    pub files: u64,
    pub bytes: u64,
    /// 引擎版本（格式演进时区分）。
    pub format: u32,
}

/// 单目标执行报告。
#[derive(Debug, Clone, Serialize)]
pub struct TargetReport {
    pub target: String,
    pub kind: String,
    pub snapshot: String,
    pub bytes: u64,
    pub seconds: u64,
    pub pruned: usize,
    pub ok: bool,
    pub error: String,
}

/// 备份状态（.litmus/local/backup-state.json，跨次运行持久）。
#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct BackupState {
    #[serde(default)]
    pub targets: std::collections::BTreeMap<String, TargetState>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct TargetState {
    #[serde(default)]
    pub last_ok: u64,
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub last_snapshot: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub tier: String,
}

// ── 快照创建 ────────────────────────────────────────────────────────────

/// 默认排除：可重建的索引 / 机器本地凭据 / 自身状态 / 垃圾文件。
const DEFAULT_EXCLUDE_PARTS: &[&str] = &[
    ".litmus/local/index",
    ".litmus/local/machine.toml",
    ".litmus/local/backup-state.json",
    ".litmus/local/deploy",
    ".litmus/local/tmp",
    ".git",
    "target",
];

fn excluded(rel: &str, include_ops: bool) -> bool {
    let rel = rel.replace('\\', "/");
    for p in DEFAULT_EXCLUDE_PARTS {
        if rel == *p || rel.starts_with(&format!("{p}/")) {
            return true;
        }
    }
    if !include_ops && (rel == ".litmus/local/ops" || rel.starts_with(".litmus/local/ops/")) {
        return true;
    }
    let name = rel.rsplit('/').next().unwrap_or(&rel);
    name == ".DS_Store" || name.starts_with("._") || name.ends_with(".tmp")
}

fn snapshot_id() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let (y, mo, da, h, mi, s) = {
        // UTC 时间戳直排，避免引 chrono：按天秒换算
        let days = secs / 86400;
        let rem = secs % 86400;
        let civil = civil_from_days(days as i64);
        (civil.0, civil.1, civil.2, rem / 3600, (rem % 3600) / 60, rem % 60)
    };
    format!("snap-{y:04}{mo:02}{da:02}-{h:02}{mi:02}{s:02}")
}

/// Howard Hinnant civil_from_days：天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn resolve_passphrase(cfg: &BackupConfig) -> Result<String, BackupError> {
    let p = if cfg.passphrase.is_empty() {
        std::env::var("LITMUS_BACKUP_PASSPHRASE").or_else(|_| std::env::var("THIRDC_BACKUP_PASSPHRASE")).unwrap_or_default()
    } else {
        cfg.passphrase.clone()
    };
    if p.is_empty() {
        return Err(BackupError::Config(
            "未配置备份口令：litmus.toml [backup] passphrase 或环境变量 LITMUS_BACKUP_PASSPHRASE".into(),
        ));
    }
    Ok(p)
}

/// 创建加密快照 → (元数据, 密文临时文件)。流式管道：tar → gzip → age → 临时文件。
pub fn create_snapshot(
    vault_root: &Path,
    cfg: &BackupConfig,
) -> Result<(SnapshotMeta, PathBuf), BackupError> {
    let passphrase = resolve_passphrase(cfg)?;
    let vault_name = read_vault_name(vault_root);
    let id = snapshot_id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let host = std::env::var("LITMUS_HOST").or_else(|_| std::env::var("THIRDC_HOST")).unwrap_or_else(|_| hostname());

    // 收集文件清单（先统计，供元数据与进度）
    let mut files: Vec<(String, PathBuf, u64)> = Vec::new();
    for entry in walkdir::WalkDir::new(vault_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(vault_root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if excluded(&rel, cfg.include_ops) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push((rel, entry.path().to_path_buf(), size));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let total_bytes: u64 = files.iter().map(|(_, _, s)| s).sum();

    let meta = SnapshotMeta {
        id: id.clone(),
        created: now,
        vault: vault_name,
        host,
        files: files.len() as u64,
        bytes: total_bytes,
        format: 1,
    };

    let tmp = tempfile::Builder::new()
        .prefix(&format!("{id}."))
        .suffix(".tar.age")
        .tempfile()
        .map_err(BackupError::Io)?;
    let tmp_path = tmp.into_temp_path().keep().map_err(|e| BackupError::Io(std::io::Error::other(e)))?;

    let out = std::fs::File::create(&tmp_path)?;
    let recipient = age::scrypt::Recipient::new(age::secrecy::SecretString::from(passphrase.clone()));
    let encryptor = age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
        .map_err(|e| BackupError::Age(e.to_string()))?;
    let mut age_writer = encryptor
        .wrap_output(out)
        .map_err(|e| BackupError::Age(e.to_string()))?;
    let gz = flate2::write::GzEncoder::new(&mut age_writer, flate2::Compression::new(6));
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    for (rel, abs, _) in &files {
        tar.append_path_with_name(abs, rel)
            .map_err(|e| BackupError::Tar(format!("{rel}: {e}")))?;
    }
    let gz = tar.into_inner().map_err(|e| BackupError::Tar(e.to_string()))?;
    gz.finish().map_err(BackupError::Io)?;
    age_writer.finish().map_err(|e| BackupError::Age(e.to_string()))?;

    Ok((meta, tmp_path))
}

fn read_vault_name(vault_root: &Path) -> String {
    std::fs::read_to_string(vault_root.join("litmus.toml"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.trim_start().starts_with("name"))
                .and_then(|l| l.split('=').nth(1))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| {
            vault_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "vault".into())
        })
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown-host".into())
}

// ── 目标客户端 ──────────────────────────────────────────────────────────

/// webdav 前缀：url 与 rel 拼接 + 逐级 MKCOL。
fn dav_url_for(base: &str, rel: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

fn dav_client() -> Result<reqwest::blocking::Client, BackupError> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| BackupError::WebDav("client".into(), e.to_string()))
}

fn dav_auth(t: &BackupTarget) -> Result<(String, String), BackupError> {
    let user = t
        .username
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LITMUS_WEBDAV_USER").or_else(|_| std::env::var("THIRDC_WEBDAV_USER")).ok())
        .ok_or_else(|| BackupError::Config("webdav 缺用户名（target.username 或 LITMUS_WEBDAV_USER）".into()))?;
    let pass = t
        .password
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LITMUS_WEBDAV_PASS").or_else(|_| std::env::var("THIRDC_WEBDAV_PASS")).ok())
        .ok_or_else(|| BackupError::Config("webdav 缺密码（target.password 或 LITMUS_WEBDAV_PASS）".into()))?;
    Ok((user, pass))
}

/// 逐级确保远端集合存在（MKCOL；405/301 = 已存在或已重定向，视为成功）。
fn dav_mkcol_all(
    client: &reqwest::blocking::Client,
    base: &str,
    user: &str,
    pass: &str,
    rel_dirs: &[String],
) -> Result<(), BackupError> {
    let mut cur = base.trim_end_matches('/').to_string();
    for d in rel_dirs {
        cur = format!("{cur}/{}", d);
        let resp = client
            .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &cur)
            .basic_auth(user, Some(pass))
            .send()
            .map_err(|e| BackupError::WebDav("mkcol".into(), e.to_string()))?;
        let st = resp.status().as_u16();
        if !(st == 200 || st == 201 || st == 405 || st == 301 || st == 409) {
            return Err(BackupError::WebDav("mkcol".into(), format!("HTTP {st} at {cur}")));
        }
    }
    Ok(())
}

fn dav_push(
    t: &BackupTarget,
    base: &str,
    rel: &str,
    file: &Path,
) -> Result<(), BackupError> {
    let (user, pass) = dav_auth(t)?;
    let client = dav_client()?;
    // 父目录链（相对 base 的目录部分）
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let dirs: Vec<String> = parts.split_last().map(|(_, rest)| rest.iter().map(|s| s.to_string()).collect()).unwrap_or_default();
    dav_mkcol_all(&client, base, &user, &pass, &dirs)?;
    let body = std::fs::read(file)?;
    let resp = client
        .put(dav_url_for(base, rel))
        .basic_auth(&user, Some(&pass))
        .body(body)
        .send()
        .map_err(|e| BackupError::WebDav(rel.into(), e.to_string()))?;
    if !resp.status().is_success() {
        return Err(BackupError::WebDav(
            rel.into(),
            format!("PUT HTTP {}", resp.status()),
        ));
    }
    Ok(())
}

/// PROPFIND Depth:1 列举（解析 href 里的文件名）。
fn dav_list(t: &BackupTarget, base: &str) -> Result<Vec<String>, BackupError> {
    let (user, pass) = dav_auth(t)?;
    let client = dav_client()?;
    let resp = client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), base)
        .basic_auth(&user, Some(&pass))
        .header("Depth", "1")
        .body(r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#)
        .send()
        .map_err(|e| BackupError::WebDav("propfind".into(), e.to_string()))?;
    if !resp.status().is_success() {
        return Err(BackupError::WebDav(
            "propfind".into(),
            format!("HTTP {}", resp.status()),
        ));
    }
    let body = resp.text().map_err(|e| BackupError::WebDav("propfind".into(), e.to_string()))?;
    let mut out = Vec::new();
    for seg in body.split('<') {
        let Some(rest) = seg.strip_prefix("D:href>").or_else(|| seg.strip_prefix("d:href>")) else {
            continue;
        };
        let href = rest.split('<').next().unwrap_or("");
        let name = href
            .split('?').next()
            .unwrap_or("")
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        if name.starts_with("snap-") && (name.ends_with(".json") || name.ends_with(".tar.age")) {
            // href 可能是百分号编码
            let dec = percent_decode(name);
            if !out.contains(&dec) {
                out.push(dec);
            }
        }
    }
    Ok(out)
}

fn dav_delete(t: &BackupTarget, base: &str, name: &str) -> Result<(), BackupError> {
    let (user, pass) = dav_auth(t)?;
    let client = dav_client()?;
    let resp = client
        .delete(dav_url_for(base, name))
        .basic_auth(&user, Some(&pass))
        .send()
        .map_err(|e| BackupError::WebDav(name.into(), e.to_string()))?;
    if !resp.status().is_success() && resp.status().as_u16() != 404 {
        return Err(BackupError::WebDav(
            name.into(),
            format!("DELETE HTTP {}", resp.status()),
        ));
    }
    Ok(())
}

fn dav_download(t: &BackupTarget, base: &str, name: &str, into: &Path) -> Result<(), BackupError> {
    let (user, pass) = dav_auth(t)?;
    let client = dav_client()?;
    let mut resp = client
        .get(dav_url_for(base, name))
        .basic_auth(&user, Some(&pass))
        .send()
        .map_err(|e| BackupError::WebDav(name.into(), e.to_string()))?;
    if !resp.status().is_success() {
        return Err(BackupError::WebDav(
            name.into(),
            format!("GET HTTP {}", resp.status()),
        ));
    }
    let mut f = std::fs::File::create(into)?;
    std::io::copy(&mut resp, &mut f)?;
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 && i + 2 <= b.len() - 1 + 1 {
            let hex_ok = i + 2 < b.len() + 1 && i + 2 <= b.len();
            if hex_ok {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        } else if b[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn local_dir(t: &BackupTarget) -> Result<PathBuf, BackupError> {
    let p = t
        .path
        .clone()
        .ok_or_else(|| BackupError::Config("local 目标缺 path".into()))?;
    Ok(PathBuf::from(shellexpand_home(&p)))
}

fn shellexpand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest).display().to_string();
        }
    }
    p.to_string()
}

fn target_dir(t: &BackupTarget) -> Result<PathBuf, BackupError> {
    let dir = local_dir(t)?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn rclone_check() -> Result<(), BackupError> {
    let ok = std::process::Command::new("rclone")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(BackupError::Rclone(
            "rclone 不可用——安装后 `rclone config` 配好同名 remote（Proton Drive / Dropbox 等）".into(),
        ))
    }
}

fn rclone_run(args: &[&str]) -> Result<String, BackupError> {
    let out = std::process::Command::new("rclone")
        .args(args)
        .output()
        .map_err(|e| BackupError::Rclone(e.to_string()))?;
    if !out.status.success() {
        return Err(BackupError::Rclone(
            String::from_utf8_lossy(&out.stderr).chars().take(300).collect(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn rclone_remote(t: &BackupTarget) -> Result<String, BackupError> {
    t.remote
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BackupError::Config("rclone 目标缺 remote（如 proton:Litmus-Backup）".into()))
}

// ── 快照上传 / 列举 / 修剪 / 恢复 ────────────────────────────────────────

/// 上传一对快照文件到目标。
fn push_pair(t: &BackupTarget, meta_file: &Path, age_file: &Path, id: &str) -> Result<(), BackupError> {
    let meta_name = format!("{id}.json");
    let age_name = format!("{id}.tar.age");
    match t.kind.as_str() {
        "local" => {
            let dir = target_dir(t)?;
            std::fs::copy(meta_file, dir.join(&meta_name))?;
            std::fs::copy(age_file, dir.join(&age_name))?;
            Ok(())
        }
        "webdav" => {
            let base = t
                .url
                .clone()
                .ok_or_else(|| BackupError::Config("webdav 目标缺 url".into()))?;
            dav_push(t, &base, &meta_name, meta_file)?;
            dav_push(t, &base, &age_name, age_file)?;
            Ok(())
        }
        "rclone" => {
            rclone_check()?;
            let remote = rclone_remote(t)?;
            rclone_run(&["copyto", &meta_file.display().to_string(), &format!("{remote}/{meta_name}")])?;
            rclone_run(&["copyto", &age_file.display().to_string(), &format!("{remote}/{age_name}")])?;
            Ok(())
        }
        other => Err(BackupError::Config(format!(
            "未知目标 kind：{other}（支持 webdav / local / rclone）"
        ))),
    }
}

/// 列举目标上全部快照（按 id 降序 = 新→旧）。
pub fn list_snapshots(t: &BackupTarget) -> Result<Vec<SnapshotMeta>, BackupError> {
    let names: Vec<String> = match t.kind.as_str() {
        "local" => {
            let dir = local_dir(t)?;
            let mut out = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.filter_map(|e| e.ok()) {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with("snap-") && n.ends_with(".json") {
                        out.push(n);
                    }
                }
            }
            out
        }
        "webdav" => {
            let base = t
                .url
                .clone()
                .ok_or_else(|| BackupError::Config("webdav 目标缺 url".into()))?;
            dav_list(t, &base)?
                .into_iter()
                .filter(|n| n.ends_with(".json"))
                .collect()
        }
        "rclone" => {
            rclone_check()?;
            let remote = rclone_remote(t)?;
            let out = rclone_run(&["lsf", "--files-only", &format!("{remote}/")])?;
            out.lines()
                .map(|l| l.trim().to_string())
                .filter(|n| n.starts_with("snap-") && n.ends_with(".json"))
                .collect()
        }
        other => return Err(BackupError::Config(format!("未知目标 kind：{other}"))),
    };
    let mut metas = Vec::new();
    for n in names {
        let id = n.trim_end_matches(".json").to_string();
        match fetch_meta(t, &id) {
            Ok(m) => metas.push(m),
            Err(_) => continue, // 孤儿 json（age 缺失等）不阻塞列举
        }
    }
    metas.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(metas)
}

/// 拉取单个快照元数据（落到临时文件读出）。
fn fetch_meta(t: &BackupTarget, id: &str) -> Result<SnapshotMeta, BackupError> {
    let tmp = tempfile::NamedTempFile::new()?;
    let p = tmp.into_temp_path().keep().map_err(|e| BackupError::Io(std::io::Error::other(e)))?;
    match t.kind.as_str() {
        "local" => {
            std::fs::copy(local_dir(t)?.join(format!("{id}.json")), &p)?;
        }
        "webdav" => {
            let base = t.url.clone().unwrap_or_default();
            dav_download(t, &base, &format!("{id}.json"), &p)?;
        }
        "rclone" => {
            let remote = rclone_remote(t)?;
            rclone_run(&["copyto", &format!("{remote}/{id}.json"), &p.display().to_string()])?;
        }
        other => return Err(BackupError::Config(format!("未知目标 kind：{other}"))),
    }
    let meta = serde_json::from_str(&std::fs::read_to_string(&p)?)
        .map_err(|e| BackupError::Config(format!("快照元数据解析失败：{e}")))?;
    let _ = std::fs::remove_file(&p);
    Ok(meta)
}

/// 修剪：保留最新 keep 份，删多余（json+age 成对）。
fn prune(t: &BackupTarget, keep: u32) -> Result<usize, BackupError> {
    let metas = list_snapshots(t)?;
    let mut pruned = 0usize;
    for m in metas.iter().skip(keep.max(1) as usize) {
        let json = format!("{}.json", m.id);
        let age = format!("{}.tar.age", m.id);
        let r: Result<bool, BackupError> = match t.kind.as_str() {
            "local" => {
                let dir = local_dir(t)?;
                let a = std::fs::remove_file(dir.join(&json)).is_ok();
                let b = std::fs::remove_file(dir.join(&age)).is_ok();
                Ok(a || b)
            }
            "webdav" => {
                let base = t.url.clone().unwrap_or_default();
                dav_delete(t, &base, &json)?;
                dav_delete(t, &base, &age)?;
                Ok(true)
            }
            "rclone" => {
                let remote = rclone_remote(t)?;
                rclone_run(&["deletefile", &format!("{remote}/{json}")])?;
                rclone_run(&["deletefile", &format!("{remote}/{age}")])?;
                Ok(true)
            }
            _ => Ok(false),
        };
        if r.is_ok() {
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// 推断目标层级（tier 未显式标注时）。
fn tier_of(t: &BackupTarget) -> String {
    if !t.tier.is_empty() {
        return t.tier.clone();
    }
    match t.kind.as_str() {
        "rclone" => "cloud".into(),
        "webdav" => "nas".into(),
        _ => "cloud".into(), // local：iCloud Drive/网盘挂载目录常见
    }
}

/// 运行一次备份：建快照 → 推全部目标（或 filter 指定名）→ 修剪 → 落状态。
pub fn run_backup(
    vault_root: &Path,
    cfg: &BackupConfig,
    filter: Option<&str>,
) -> Result<(Vec<TargetReport>, BackupState), BackupError> {
    if cfg.targets.is_empty() {
        return Err(BackupError::Config(
            "未配置备份目标：litmus.toml [[backup.targets]]（kind: webdav / local / rclone）".into(),
        ));
    }
    let (meta, age_file) = create_snapshot(vault_root, cfg)?;
    let scope = tempfile::tempdir().map_err(BackupError::Io)?;
    let meta_file = scope.path().join(format!("{}.json", meta.id));
    std::fs::write(&meta_file, serde_json::to_vec_pretty(&meta).map_err(|e| BackupError::Config(e.to_string()))?)?;
    let age_size = std::fs::metadata(&age_file).map(|m| m.len()).unwrap_or(0);

    let mut reports = Vec::new();
    let mut state = load_state(vault_root);
    for t in &cfg.targets {
        if let Some(f) = filter {
            if t.name != f {
                continue;
            }
        }
        let t0 = std::time::Instant::now();
        let mut rep = TargetReport {
            target: t.name.clone(),
            kind: t.kind.clone(),
            snapshot: meta.id.clone(),
            bytes: age_size,
            seconds: 0,
            pruned: 0,
            ok: false,
            error: String::new(),
        };
        match push_pair(t, &meta_file, &age_file, &meta.id).and_then(|_| prune(t, cfg.keep)) {
            Ok(pruned) => {
                rep.pruned = pruned;
                rep.ok = true;
            }
            Err(e) => rep.error = e.to_string(),
        }
        rep.seconds = t0.elapsed().as_secs();
        state.targets.insert(
            t.name.clone(),
            TargetState {
                last_ok: if rep.ok {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                } else {
                    state.targets.get(&t.name).map(|s| s.last_ok).unwrap_or(0)
                },
                last_error: rep.error.clone(),
                last_snapshot: if rep.ok { meta.id.clone() } else { String::new() },
                bytes: if rep.ok { age_size } else { 0 },
                tier: tier_of(t),
            },
        );
        reports.push(rep);
    }
    let _ = std::fs::remove_file(&age_file);
    save_state(vault_root, &state);
    Ok((reports, state))
}

pub fn state_path(vault_root: &Path) -> PathBuf {
    vault_root.join(".litmus/local").join("backup-state.json")
}

pub fn load_state(vault_root: &Path) -> BackupState {
    std::fs::read_to_string(state_path(vault_root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(vault_root: &Path, st: &BackupState) {
    let p = state_path(vault_root);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, serde_json::to_vec_pretty(st).unwrap_or_default());
}

/// 3-2-1 合规：第 1 份是库本身；要求 cloud≥1 且 (nas|offsite)≥1。
pub fn compliance(state: &BackupState) -> (bool, String) {
    let mut cloud = 0;
    let mut nas = 0;
    let mut ok_targets = 0;
    for t in state.targets.values() {
        if t.last_ok > 0 && t.last_error.is_empty() {
            ok_targets += 1;
            match t.tier.as_str() {
                "cloud" => cloud += 1,
                "nas" | "offsite" => nas += 1,
                _ => {}
            }
        }
    }
    let ok = cloud >= 1 && nas >= 1;
    let msg = if ok {
        format!("3-2-1 ✓（本地 1 + cloud {cloud} + nas/offsite {nas}）")
    } else {
        format!(
            "3-2-1 ✗（本地 1 + cloud {cloud} + nas/offsite {nas}）——还差{}；建议：cloud 加 Proton Drive/Dropbox（rclone），nas 加 WebDAV（飞牛/群晖）",
            if cloud == 0 && nas == 0 { "两类目标" } else if cloud == 0 { "1 个 cloud 目标" } else { "1 个 nas/offsite 目标" }
        )
    };
    (ok && ok_targets >= 2, msg)
}

/// 恢复：下载快照 → 流式解密解包到 into_dir。zip-slip 防护：拒绝越界路径。
pub fn restore(
    t: &BackupTarget,
    snapshot_id: &str,
    into_dir: &Path,
    passphrase: &str,
) -> Result<(u64, u64), BackupError> {
    if passphrase.is_empty() {
        return Err(BackupError::Config("缺恢复口令".into()));
    }
    std::fs::create_dir_all(into_dir)?;
    let age_name = format!("{snapshot_id}.tar.age");
    let tmp = tempfile::NamedTempFile::new()?;
    let tmp_path = tmp.into_temp_path().keep().map_err(|e| BackupError::Io(std::io::Error::other(e)))?;
    match t.kind.as_str() {
        "local" => {
            std::fs::copy(local_dir(t)?.join(&age_name), &tmp_path)?;
        }
        "webdav" => {
            let base = t.url.clone().unwrap_or_default();
            dav_download(t, &base, &age_name, &tmp_path)?;
        }
        "rclone" => {
            rclone_check()?;
            let remote = rclone_remote(t)?;
            rclone_run(&["copyto", &format!("{remote}/{age_name}"), &tmp_path.display().to_string()])?;
        }
        other => return Err(BackupError::Config(format!("未知目标 kind：{other}"))),
    }

    let f = std::fs::File::open(&tmp_path)?;
    let decryptor = age::Decryptor::new(&f).map_err(|e| BackupError::Age(e.to_string()))?;
    let identity = age::scrypt::Identity::new(age::secrecy::SecretString::from(passphrase.to_string()));
    let reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| BackupError::Age(format!("解密失败（口令错误或快照损坏）：{e}")))?;
    let gz = flate2::read::GzDecoder::new(reader);
    let mut tar = tar::Archive::new(gz);
    tar.set_preserve_permissions(true);
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in tar.entries().map_err(|e| BackupError::Tar(e.to_string()))? {
        let mut entry = entry.map_err(|e| BackupError::Tar(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| BackupError::Tar(e.to_string()))?
            .to_path_buf();
        // zip-slip：解析后的路径必须仍在 into_dir 内
        let dest = into_dir.join(&path);
        if !dest.starts_with(into_dir) {
            return Err(BackupError::Tar(format!("快照内越界路径：{}", path.display())));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let size = entry.size() as u64;
        entry
            .unpack(&dest)
            .map_err(|e| BackupError::Tar(format!("{}: {}", path.display(), e)))?;
        files += 1;
        bytes += size;
    }
    let _ = std::fs::remove_file(&tmp_path);
    Ok((files, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_store::BackupTarget;

    fn vault_fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("Notes/子目录")).unwrap();
        std::fs::create_dir_all(d.path().join(".litmus/local/index")).unwrap();
        std::fs::write(d.path().join("litmus.toml"), "[vault]\nname = \"测试库\"\n[auth]\n".to_string()).unwrap();
        std::fs::write(d.path().join("Notes/中文 文档.md"), "# 你好\n\n正文内容。".to_string()).unwrap();
        std::fs::write(d.path().join("Notes/子目录/nested.json"), "{\"a\":1}".to_string()).unwrap();
        std::fs::write(d.path().join(".litmus/local/index/index.db"), "SHOULD-NOT-BACKUP".to_string()).unwrap();
        std::fs::write(d.path().join(".litmus/local/machine.toml"), "token = \"x\"".to_string()).unwrap();
        std::fs::write(d.path().join(".DS_Store"), "junk".to_string()).unwrap();
        d
    }

    fn cfg_with(targets: Vec<BackupTarget>) -> BackupConfig {
        BackupConfig {
            passphrase: "correct horse battery staple 测试口令".into(),
            keep: 2,
            auto_hours: 0,
            include_ops: false,
            targets,
        }
    }

    #[test]
    fn local_roundtrip_and_excludes() {
        let vault = vault_fixture();
        let dest = tempfile::tempdir().unwrap();
        let cfg = cfg_with(vec![BackupTarget {
            name: "本地".into(),
            kind: "local".into(),
            path: Some(dest.path().display().to_string()),
            ..Default::default()
        }]);
        let (reports, state) = run_backup(vault.path(), &cfg, None).unwrap();
        assert!(reports[0].ok, "report: {:?}", reports);
        assert!(reports[0].bytes > 0);

        // 快照对存在
        let names: Vec<String> = std::fs::read_dir(dest.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with(".tar.age")));
        assert!(names.iter().any(|n| n.ends_with(".json")));

        // 密文不含明文片段（E2EE 的底线）
        let age_blob = names
            .iter()
            .find(|n| n.ends_with(".tar.age"))
            .map(|n| std::fs::read(dest.path().join(n)).unwrap())
            .unwrap();
        let needle = "\u{4f60}\u{597d}".as_bytes();
        assert!(
            !age_blob.windows(needle.len()).any(|w| w == needle),
            "明文不得出现在密文里"
        );

        // 恢复到新目录 → 内容相等
        let out = tempfile::tempdir().unwrap();
        let (files, bytes) =
            restore(&cfg.targets[0], &reports[0].snapshot, out.path(), &cfg.passphrase).unwrap();
        assert!(files >= 3);
        assert!(bytes > 0);
        assert_eq!(
            std::fs::read_to_string(out.path().join("Notes/中文 文档.md")).unwrap(),
            "# 你好\n\n正文内容。"
        );
        assert_eq!(std::fs::read_to_string(out.path().join("Notes/子目录/nested.json")).unwrap(), "{\"a\":1}");
        assert!(!out.path().join(".litmus/local/index/index.db").exists(), "排除项不得入包");
        assert!(!out.path().join(".litmus/local/machine.toml").exists(), "机器凭据不得入包");
        assert!(!out.path().join(".DS_Store").exists());
        // 状态与合规
        assert!(state.targets.contains_key("本地"));
        let (ok, msg) = compliance(&state);
        assert!(!ok, "单 local 目标不满足 3-2-1（cloud+nas 需各 ≥1）：{msg}");
    }

    #[test]
    fn wrong_passphrase_rejected() {
        let vault = vault_fixture();
        let dest = tempfile::tempdir().unwrap();
        let cfg = cfg_with(vec![BackupTarget {
            name: "本地".into(),
            kind: "local".into(),
            path: Some(dest.path().display().to_string()),
            ..Default::default()
        }]);
        let (reports, _) = run_backup(vault.path(), &cfg, None).unwrap();
        let out = tempfile::tempdir().unwrap();
        let err = restore(&cfg.targets[0], &reports[0].snapshot, out.path(), "错口令").unwrap_err();
        assert!(err.to_string().contains("解密失败"), "实际：{err}");
    }

    #[test]
    fn prune_keeps_latest_n() {
        let vault = vault_fixture();
        let dest = tempfile::tempdir().unwrap();
        let cfg = cfg_with(vec![BackupTarget {
            name: "本地".into(),
            kind: "local".into(),
            path: Some(dest.path().display().to_string()),
            ..Default::default()
        }]);
        for _ in 0..4 {
            run_backup(vault.path(), &cfg, None).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1100)); // 秒级 id 唯一
        }
        let metas = list_snapshots(&cfg.targets[0]).unwrap();
        assert_eq!(metas.len(), 2, "keep=2 应只留 2 份：{}", metas.len());
    }

    #[test]
    fn snapshot_id_is_timestamp_shaped() {
        let id = snapshot_id();
        assert!(id.starts_with("snap-") && id.len() == 20, "{id}");
    }

    #[test]
    fn excluded_rules() {
        assert!(excluded(".litmus/local/index/index.db", false));
        assert!(excluded(".litmus/local/ops/abc.am", false));
        assert!(!excluded(".litmus/local/ops/abc.am", true));
        assert!(excluded(".litmus/local/machine.toml", false));
        assert!(excluded(".DS_Store", false));
        assert!(excluded("Notes/._foo.md", false));
        assert!(!excluded("Notes/中文.md", false));
        assert!(!excluded(".litmus/local/secrets.json", false), "凭证库属于用户数据，E2EE 后入包");
    }
}
