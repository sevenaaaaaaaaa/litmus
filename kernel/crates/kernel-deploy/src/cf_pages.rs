//! kernel-deploy::cf_pages — Cloudflare Pages 直传（PUB-3）。

use serde_json::Value;
use sha2::{Digest, Sha256};
use kernel_store::PublishTarget;
use std::path::Path;
use std::time::Duration;

use crate::{DeployError, DeployReport, DeployState, s3::walk_files};

// ---------- Cloudflare Pages 直传（PUB-3） ----------
//
// API：POST /client/v4/accounts/{account_id}/pages/projects/{project}/deployments
// multipart：`manifest` = {"<path>":"<sha256>"}，每个文件一个 part（name = hash，filename = path）。
// 增量策略：整站哈希集合未变 → 跳过部署；有任何变化 → 整站重传（CF 的 API 语义如此）。

pub fn site_fingerprint(site_dir: &Path) -> Result<String, DeployError> {
    let mut hashes: Vec<String> = Vec::new();
    for (rel, p) in walk_files(site_dir)? {
        let h = hex::encode(Sha256::digest(&std::fs::read(&p)?));
        hashes.push(format!("{rel}:{h}"));
    }
    hashes.sort();
    let joined = hashes.join("\n");
    Ok(hex::encode(Sha256::digest(joined.as_bytes())))
}

pub fn deploy_cf_pages(
    site_dir: &Path,
    target: &PublishTarget,
    state: &mut DeployState,
) -> Result<DeployReport, DeployError> {
    let account = target
        .account_id
        .clone()
        .ok_or_else(|| DeployError::Missing("cf-pages 需要 account_id".into()))?;
    let project = target
        .project
        .clone()
        .ok_or_else(|| DeployError::Missing("cf-pages 需要 project".into()))?;
    let tn = target.token_env.clone().unwrap_or_else(|| "CF_API_TOKEN".into());
    let token = crate::cred(&tn).ok_or_else(|| DeployError::Missing(format!("缺少 Cloudflare API 令牌：在 /凭证 存一条 {tn}（或设同名环境变量）")))?;

    // 整站指纹未变 → 跳过（真实 API 部署是整站的）
    let fp = site_fingerprint(site_dir)?;
    if state.files.get("::site") == Some(&fp) {
        let n = walk_files(site_dir)?.len();
        return Ok(DeployReport {
            target: target.name.clone(),
            kind: "cf-pages".into(),
            uploaded: 0,
            skipped: n,
            bytes: 0,
            detail: "站点无变更，未创建部署".into(),
            url: target.public_base_url.clone(),
        });
    }

    let files = walk_files(site_dir)?;
    let mut manifest = serde_json::Map::new();
    let mut form = reqwest::blocking::multipart::Form::new();
    let mut uploaded_bytes = 0u64;
    let mut seen_hashes: std::collections::HashMap<String, std::path::PathBuf> =
        std::collections::HashMap::new();

    for (rel, p) in &files {
        let body = std::fs::read(p)?;
        let h = hex::encode(Sha256::digest(&body));
        manifest.insert(rel.clone(), Value::String(h.clone()));
        seen_hashes.entry(h.clone()).or_insert_with(|| p.clone());
        uploaded_bytes += body.len() as u64;
    }
    // 相同内容的文件只传一次（按 hash 去重）
    for (h, p) in &seen_hashes {
        let fname = files
            .iter()
            .find(|(_, path)| path == p)
            .map(|(rel, _)| rel.clone())
            .unwrap_or_else(|| "file".into());
        let body = std::fs::read(p)?;
        form = form.part(
            h.clone(),
            reqwest::blocking::multipart::Part::bytes(body)
                .file_name(fname)
                .mime_str("application/octet-stream")
                .map_err(|e| DeployError::Io(std::io::Error::other(e.to_string())))?,
        );
    }
    form = form.text("manifest", Value::Object(manifest).to_string());

    let base = target
        .api_base
        .clone()
        .unwrap_or_else(|| "https://api.cloudflare.com".into());
    let url = format!("{base}/client/v4/accounts/{account}/pages/projects/{project}/deployments");
    let resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| DeployError::Io(std::io::Error::other(e.to_string())))?
        .post(&url)
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .map_err(|e| DeployError::Io(std::io::Error::other(e.to_string())))?;
    let status = resp.status();
    let v: Value = resp.json().unwrap_or(Value::Null);
    if !status.is_success() || v.get("success").and_then(|s| s.as_bool()) != Some(true) {
        return Err(DeployError::Git(format!(
            "Cloudflare Pages 部署失败：{status} {}",
            serde_json::to_string(&v).unwrap_or_default().chars().take(300).collect::<String>()
        )));
    }
    let deploy_url = v
        .pointer("/result/url")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .or_else(|| target.public_base_url.clone());

    state.files.insert("::site".into(), fp);
    Ok(DeployReport {
        target: target.name.clone(),
        kind: "cf-pages".into(),
        uploaded: files.len(),
        skipped: 0,
        bytes: uploaded_bytes,
        detail: format!("已创建部署（{} 个文件，同内容文件按哈希去重）", files.len()),
        url: deploy_url,
    })
}
