//! kernel-deploy::webdav — 全量备份到 WebDAV（坚果云/Nextcloud/群晖等）。

use kernel_store::PublishTarget;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

use crate::{DeployError, DeployReport, DeployState};

/// 全量备份：把库根（Notes/ + Assets/ + litmus.toml）PUT 到 WebDAV。
/// 增量：DeployState 记内容哈希，未变跳过。
pub fn backup_webdav(
    vault_root: &Path,
    target: &PublishTarget,
    state: &mut DeployState,
) -> Result<DeployReport, DeployError> {
    let url = target
        .endpoint
        .clone()
        .ok_or_else(|| DeployError::Missing("webdav 需要 endpoint（如 https://dav.jianguoyun.com/dav/kb/）".into()))?;
    let user = std::env::var(target.access_key_env.clone().unwrap_or_else(|| "WEBDAV_USER".into()))
        .map_err(|_| DeployError::Missing("缺 WEBDAV 用户名环境变量".into()))?;
    let pass = std::env::var(target.secret_key_env.clone().unwrap_or_else(|| "WEBDAV_PASS".into()))
        .map_err(|_| DeployError::Missing("缺 WEBDAV 密码环境变量".into()))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| DeployError::Io(std::io::Error::other(e.to_string())))?;

    // 收集需要备份的文件
    let mut files = Vec::new();
    for sub in ["Notes", "Assets"] {
        let base = vault_root.join(sub);
        if !base.is_dir() { continue; }
        let mut stack = vec![base];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() { stack.push(p); continue; }
                if let Ok(rel) = p.strip_prefix(vault_root) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    files.push((rel_str, p));
                }
            }
        }
    }
    files.push(("litmus.toml".into(), vault_root.join("litmus.toml")));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut uploaded = 0usize;
    let mut skipped = 0usize;
    let mut bytes = 0u64;
    let mut errors: Vec<String> = Vec::new();

    for (rel, abs) in &files {
        let body = match std::fs::read(abs) {
            Ok(b) => b,
            Err(_) => { skipped += 1; continue; }
        };
        let hash = hex::encode(Sha256::digest(&body));
        if state.files.get(rel) == Some(&hash) {
            skipped += 1;
            continue;
        }
        let dav_url = format!("{}/{}", url.trim_end_matches('/'), rel);
        let resp = client
            .put(&dav_url)
            .basic_auth(&user, Some(&pass))
            .body(body.clone())
            .send();
        match resp {
            Ok(r) if r.status().is_success() => {
                state.files.insert(rel.clone(), hash);
                uploaded += 1;
                bytes += body.len() as u64;
            }
            Ok(r) => errors.push(format!("{}: {}", rel, r.status())),
            Err(e) => errors.push(format!("{}: {}", rel, e)),
        }
        // 限速：100ms 间隔，别把网盘打爆
        std::thread::sleep(Duration::from_millis(100));
    }
    // 保存增量状态
    if errors.is_empty() {
        let sp = vault_root.join(".litmus/local/deploy/webdav-backup.json");
        if let Some(d) = sp.parent() { let _ = std::fs::create_dir_all(d); }
        let _ = std::fs::write(&sp, serde_json::to_string_pretty(&state).unwrap_or_default());
    }
    Ok(DeployReport {
        target: target.name.clone(),
        kind: "webdav".into(),
        uploaded,
        skipped,
        bytes,
        detail: if errors.is_empty() {
            format!("备份到 {} 完成", url)
        } else {
            format!("{} 个失败：{}", errors.len(), errors.iter().take(3).cloned().collect::<Vec<_>>().join("; "))
        },
        url: Some(url),
    })
}
