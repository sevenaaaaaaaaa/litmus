//! kernel-deploy::s3 — S3 兼容对象存储直传（AWS Signature V4）。
//!
//! 目标服务：AWS S3 / Cloudflare R2 / 阿里 OSS / 腾讯 COS / MinIO / Backblaze B2 等
//! 所有支持 SigV4 + path-style 的服务。
//!
//! **签名正确性由黄金向量保证**：`tests/vectors/sigv4.json` 由本机 botocore（AWS 官方
//! Python SDK）1.42.97 在固定时间与测试凭据下生成，`tests/sigv4_vectors.rs` 逐条比对
//! canonical request / string-to-sign / signature。

use hmac::{Hmac, Mac};
use kernel_store::PublishTarget;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{DeployError, DeployReport, DeployState};

type HmacSha256 = Hmac<Sha256>;

/// 需要签名的请求描述。
pub struct SignInput<'a> {
    pub method: &'a str,
    /// 已 URI 编码的绝对路径（`/` 保留）
    pub canonical_uri: &'a str,
    /// 已排序并编码的 query（无则空串）
    pub canonical_query: &'a str,
    /// 参与签名的头（名必须小写；至少含 host 与 x-amz-date）
    pub headers: BTreeMap<String, String>,
    /// hex(sha256(payload)) 或 "UNSIGNED-PAYLOAD"
    pub payload_hash: &'a str,
    pub amz_date: &'a str,
}

pub struct SigV4<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

impl<'a> SigV4<'a> {
    pub fn new(access_key: &'a str, secret_key: &'a str, region: &'a str, service: &'a str) -> Self {
        Self { access_key, secret_key, region, service }
    }

    pub fn credential_scope(&self, amz_date: &str) -> String {
        format!(
            "{}/{}/{}/aws4_request",
            &amz_date[..8],
            self.region,
            self.service
        )
    }

    /// 规范请求。头必须已小写、按名排序；值需 trim 且折叠连续空格。
    pub fn canonical_request(&self, i: &SignInput) -> String {
        let mut headers = String::new();
        for (k, v) in &i.headers {
            let collapsed = v.trim().split_whitespace().collect::<Vec<_>>().join(" ");
            headers.push_str(&format!("{}:{}\n", k.to_lowercase(), collapsed));
        }
        let signed = i
            .headers
            .keys()
            .map(|k| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            i.method, i.canonical_uri, i.canonical_query, headers, signed, i.payload_hash
        )
    }

    pub fn string_to_sign(&self, i: &SignInput, canonical_request: &str) -> String {
        let hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            i.amz_date,
            self.credential_scope(i.amz_date),
            hash
        )
    }

    pub fn signing_key(&self, amz_date: &str) -> Vec<u8> {
        let k = format!("AWS4{}", self.secret_key);
        let k_date = hmac(k.as_bytes(), amz_date[..8].as_bytes());
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, self.service.as_bytes());
        hmac(&k_service, b"aws4_request")
    }

    pub fn signature(&self, i: &SignInput, canonical_request: &str) -> String {
        let sts = self.string_to_sign(i, canonical_request);
        let key = self.signing_key(i.amz_date);
        hex::encode(hmac(&key, sts.as_bytes()))
    }

    /// 完整的 Authorization 头值。
    pub fn authorization(&self, i: &SignInput) -> String {
        let creq = self.canonical_request(i);
        let sig = self.signature(i, &creq);
        let signed = i
            .headers
            .keys()
            .map(|k| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key,
            self.credential_scope(i.amz_date),
            signed,
            sig
        )
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// 按 SigV4 规则 URI 编码（分段编码，`/` 保留；空格→%20；大写十六进制）。
pub fn uri_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' | '/' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for b in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

/// 当前 UTC 时间戳（SigV4 需要 YYYYMMDDTHHMMSSZ，这里用 UNIX 秒做民用时换算）。
pub fn amz_date_now() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    (
        format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z"),
        format!("{y:04}{mo:02}{d:02}"),
    )
}

/// UNIX 秒 → UTC 民用时间（Howard Hinnant 的 civil_from_days 算法）。
pub fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

struct Endpoint {
    scheme: String,
    host: String,
    /// 端口（含冒号），无则空
    port: String,
    base_path: String,
}

fn parse_endpoint(url: &str) -> Result<Endpoint, DeployError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| DeployError::Missing(format!("endpoint 需要 scheme：{url}")))?;
    let (authority, base_path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_string()),
        None => (rest, String::new()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), format!(":{p}")),
        _ => (authority.to_string(), String::new()),
    };
    Ok(Endpoint {
        scheme: scheme.to_string(),
        host,
        port,
        base_path,
    })
}

/// S3 兼容目标部署：逐文件 PUT，内容哈希未变则跳过。
pub fn deploy_s3(
    site_dir: &Path,
    target: &PublishTarget,
    state: &mut DeployState,
) -> Result<DeployReport, DeployError> {
    let endpoint = target
        .endpoint
        .clone()
        .ok_or_else(|| DeployError::Missing("s3 目标需要 endpoint".into()))?;
    let bucket = target
        .bucket
        .clone()
        .ok_or_else(|| DeployError::Missing("s3 目标需要 bucket".into()))?;
    let region = target.region.clone().unwrap_or_else(|| "us-east-1".into());
    let ak_name = target.access_key_env.clone().unwrap_or_else(|| "AWS_ACCESS_KEY_ID".into());
    let sk_name = target.secret_key_env.clone().unwrap_or_else(|| "AWS_SECRET_ACCESS_KEY".into());
    let access_key = crate::cred(&ak_name).ok_or_else(|| DeployError::Missing(format!("缺少访问密钥：在 /凭证 存一条 {ak_name}（或设同名环境变量）")))?;
    let secret_key = crate::cred(&sk_name).ok_or_else(|| DeployError::Missing(format!("缺少密钥：在 /凭证 存一条 {sk_name}（或设同名环境变量）")))?;
    let prefix = target.prefix.clone().unwrap_or_default();
    let prefix = prefix.trim_matches('/');

    let ep = parse_endpoint(&endpoint)?;
    let sig = SigV4::new(&access_key, &secret_key, &region, "s3");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| DeployError::Io(std::io::Error::other(e.to_string())))?;

    let files = walk_files(site_dir)?;
    let mut uploaded = 0usize;
    let mut skipped = 0usize;
    let mut bytes = 0u64;

    for (rel, abs) in files {
        let body = std::fs::read(&abs)?;
        let hash = hex::encode(Sha256::digest(&body));
        if state.files.get(&rel) == Some(&hash) {
            skipped += 1;
            continue;
        }
        let key = if prefix.is_empty() {
            rel.clone()
        } else {
            format!("{prefix}/{rel}")
        };
        let canonical_uri = format!(
            "{}/{}",
            ep.base_path,
            uri_encode_path(&format!("{bucket}/{key}"))
        );
        let (amz_date, _d) = amz_date_now();
        let host = format!("{}{}", ep.host, ep.port);
        let mut headers = BTreeMap::new();
        headers.insert("host".to_string(), host.clone());
        headers.insert("x-amz-content-sha256".to_string(), hash.clone());
        headers.insert("x-amz-date".to_string(), amz_date.clone());
        let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("");
        let mime = kernel_store::refs::mime_for_ext(ext);
        if mime != "application/octet-stream" {
            headers.insert("content-type".to_string(), mime.to_string());
        }
        let input = SignInput {
            method: "PUT",
            canonical_uri: &canonical_uri,
            canonical_query: "",
            headers: headers.clone(),
            payload_hash: &hash,
            amz_date: &amz_date,
        };
        let auth = sig.authorization(&input);
        let url = format!("{}://{}{}", ep.scheme, host, canonical_uri);
        let mut req = client
            .put(&url)
            .header("authorization", auth)
            .header("x-amz-content-sha256", hash.clone())
            .header("x-amz-date", amz_date.clone())
            .body(body.clone());
        if let Some(ct) = headers.get("content-type") {
            req = req.header("content-type", ct.clone());
        }
        let resp = req
            .send()
            .map_err(|e| DeployError::Io(std::io::Error::other(e.to_string())))?;
        if !resp.status().is_success() {
            let code = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(DeployError::Git(format!(
                "PUT {url} 失败：{code} {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        state.files.insert(rel.clone(), hash);
        uploaded += 1;
        bytes += body.len() as u64;
    }

    Ok(DeployReport {
        target: target.name.clone(),
        kind: "s3".into(),
        uploaded,
        skipped,
        bytes,
        detail: format!(
            "{} · s3://{}/{}{}",
            endpoint,
            bucket,
            if prefix.is_empty() { String::new() } else { format!("{prefix}/") },
            "（path-style，仅上传内容变化的文件）"
        ),
        url: target.public_base_url.clone(),
    })
}

/// 站点文件遍历（跳过 .git）。
pub fn walk_files(root: &Path) -> Result<Vec<(String, std::path::PathBuf)>, DeployError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, p));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
