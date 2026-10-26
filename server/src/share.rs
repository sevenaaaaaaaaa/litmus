//! 加密分享：密文 + 自带解密脚本的查看页。
//!
//! - 客户端在浏览器里用 AES-GCM 加密（随机 256 位密钥放在链接 `#k2=` 之后，或用密码经 PBKDF2 派生），
//!   服务器只收到密文、IV（和盐），存进 `.litmus/local/shares/<id>.json`。
//! - `GET /s/<id>` 免令牌：返回把密文嵌在页面里的查看页（share.html），在收件人的浏览器里解密。
//! - 配置了公开发布目标（git / s3 / cf-pages）时，同一张查看页写到站点的 `e/<id>.html` 并部署，
//!   得到别人能打开的公开地址；公开主机上只有密文。
//! - 撤回：删本地记录；公开目标上的那页用「已撤回」的空页覆盖后重新部署（部署器只上传、不删远端文件）。
//! - 兼容经典界面的旧链接：`#k=` + base64(口令)，口令补齐到 32 字节作密钥。

use super::{audit_log, check_token, err, share_target, AppState};
use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

fn shares_dir(st: &AppState) -> std::path::PathBuf {
    st.kernel.lock().unwrap().vault.sidecar().join("shares")
}

/// 查看页：tokens.css 内联（公开主机上没有 /assets），密文以 JSON 嵌入（`</` 转义，防止提前闭合 script）。
/// `rec` 为 None 时生成「不存在或已撤回」页。
pub fn page_html(rec: Option<&Value>) -> String {
    let data = match rec {
        Some(r) => json!({
            "v": r.get("v").cloned().unwrap_or(json!(2)),
            "content": r.get("content").cloned().unwrap_or(Value::Null),
            "iv": r.get("iv").cloned().unwrap_or(Value::Null),
            "salt": r.get("salt").cloned().unwrap_or(Value::Null),
            "iter": r.get("iter").cloned().unwrap_or(Value::Null),
            "created": r.get("created").cloned().unwrap_or(Value::Null),
            "expires": r.get("expires").cloned().unwrap_or(Value::Null),
        }),
        None => Value::Null,
    };
    let data = serde_json::to_string(&data).unwrap_or_else(|_| "null".into()).replace("</", "<\\/");
    // 字体走系统字体栈：公开主机上没有 /assets/fonts
    let tokens = include_str!("../web/tokens.css");
    include_str!("../web/share.html")
        .replace("<link rel=\"stylesheet\" href=\"assets/tokens.css\">", &format!("<style>{tokens}</style>"))
        .replace("__SHARE_DATA__", &data)
}

const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob: https:; \
                   font-src 'self' data:; frame-src 'self'; connect-src 'none'; form-action 'none'; base-uri 'none'";

/// POST /share {content, iv, salt?, iter?, expires_days?, path?}
pub async fn create(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let content = req.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
    if content.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 content（密文）").into_response();
    }
    if content.len() > 24 * 1024 * 1024 {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "分享内容太大（上限约 18MB）").into_response();
    }
    let iv = req.get("iv").cloned().unwrap_or(Value::Null);
    if !(iv.is_array() || iv.is_string()) {
        return err(StatusCode::BAD_REQUEST, "需要 iv").into_response();
    }
    let created = now();
    let expires = req
        .get("expires_days")
        .and_then(|d| d.as_u64())
        .filter(|d| *d > 0)
        .map(|d| created + d.min(3650) * 86_400);
    let id = kernel_core::Cas::hash_hex(content.as_bytes())[..16].to_string();
    let rec = json!({
        "v": 2,
        "content": content,
        "iv": iv,
        "salt": req.get("salt").cloned().unwrap_or(Value::Null),
        "iter": req.get("iter").cloned().unwrap_or(Value::Null),
        "created": created,
        "expires": expires,
        "path": req.get("path").and_then(|p| p.as_str()).filter(|p| kernel_core::is_safe_doc_path(p)).unwrap_or(""),
    });
    let dir = shares_dir(&st);
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(format!("{id}.json")), rec.to_string())) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let url = format!("/s/{id}");
    let (public_url, hint) = push_public(&st, &id, Some(&rec)).await;
    if let Some(u) = &public_url {
        let mut r = rec.clone();
        r["public_url"] = json!(u);
        let _ = std::fs::write(dir.join(format!("{id}.json")), r.to_string());
    }
    audit_log(&st.kernel.lock().unwrap().vault.sidecar(), "share-encrypted", &json!({ "id": id, "public": public_url.is_some() }));
    Json(json!({ "id": id, "url": url, "public_url": public_url, "hint": hint, "expires": expires })).into_response()
}

/// 有公开目标就把查看页（或撤回后的空页）写进站点并部署。返回 (公开地址, 说明)。
async fn push_public(st: &Arc<AppState>, id: &str, rec: Option<&Value>) -> (Option<String>, Option<String>) {
    let prep = {
        let k = st.kernel.lock().unwrap();
        share_target(&k.vault.config.publish).map(|(t, base)| {
            let site = k.vault.root.join(kernel_deploy::target_dir(&t, &k.vault.config.publish.site_dir));
            (t, base, site, k.vault.sidecar())
        })
    };
    let Some((target, base, site, sidecar)) = prep else {
        return (None, Some("还没有配置公开发布目标（/发布）：这个链接只能在能访问这台鹿蕊的地方打开。".into()));
    };
    let file = site.join("e").join(format!("{id}.html"));
    if let Err(e) = std::fs::create_dir_all(file.parent().unwrap()).and_then(|_| std::fs::write(&file, page_html(rec))) {
        return (None, Some(format!("写入站点目录失败：{e}")));
    }
    let t2 = target.clone();
    let r = tokio::task::spawn_blocking(move || kernel_deploy::deploy(&site, &sidecar, &t2))
        .await
        .unwrap_or_else(|j| Err(kernel_deploy::DeployError::Io(std::io::Error::other(format!("join: {j}")))));
    match r {
        Ok(report) => {
            let root = report.url.clone().filter(|u| u.starts_with("http") && base.is_none()).or(base).unwrap_or_default();
            if root.is_empty() {
                (None, Some(format!("已推到「{}」，但没配置公开地址（publish.base_url），拼不出链接。", target.name)))
            } else {
                (Some(format!("{}/e/{id}.html", root.trim_end_matches('/'))), None)
            }
        }
        Err(e) => (None, Some(format!("推送到「{}」失败：{e}", target.name))),
    }
}

/// GET /s/{id}：免令牌的查看页。密钥不在这里，在收件人链接的 # 之后。
pub async fn view(State(st): State<Arc<AppState>>, AxumPath(id): AxumPath<String>) -> impl IntoResponse {
    let rec = if valid_id(&id) {
        std::fs::read_to_string(shares_dir(&st).join(format!("{id}.json"))).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok())
    } else {
        None
    };
    let status = if rec.is_some() { StatusCode::OK } else { StatusCode::NOT_FOUND };
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        axum::response::Html(page_html(rec.as_ref())),
    )
}

/// GET /shares：本库发出的加密分享（只回元数据，不回密文）。
pub async fn list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let mut items: Vec<Value> = std::fs::read_dir(shares_dir(&st))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let id = name.strip_suffix(".json")?.to_string();
                    let v: Value = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
                    Some(json!({
                        "id": id,
                        "url": format!("/s/{id}"),
                        "path": v.get("path").cloned().unwrap_or(json!("")),
                        "created": v.get("created").cloned().unwrap_or(json!(0)),
                        "expires": v.get("expires").cloned().unwrap_or(Value::Null),
                        "public_url": v.get("public_url").cloned().unwrap_or(Value::Null),
                        "password": v.get("salt").map(|s| !s.is_null()).unwrap_or(false),
                        "bytes": v.get("content").and_then(|c| c.as_str()).map(|c| c.len()).unwrap_or(0),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| b["created"].as_u64().cmp(&a["created"].as_u64()));
    Json(json!({ "items": items })).into_response()
}

/// DELETE /share/{id}：撤回。本地记录删除；公开目标上的那页换成「已撤回」并重新部署。
pub async fn revoke(State(st): State<Arc<AppState>>, h: HeaderMap, AxumPath(id): AxumPath<String>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    if !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "bad id").into_response();
    }
    let path = shares_dir(&st).join(format!("{id}.json"));
    let had_public = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| v.get("public_url").map(|u| !u.is_null()).unwrap_or(false))
        .unwrap_or(false);
    if std::fs::remove_file(&path).is_err() {
        return err(StatusCode::NOT_FOUND, "没有这个分享").into_response();
    }
    let note = if had_public {
        let (_, hint) = push_public(&st, &id, None).await;
        hint
    } else {
        None
    };
    Json(json!({ "ok": true, "note": note })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_embeds_data_safely_and_has_no_hex() {
        let rec = json!({ "content": "abc</script><script>alert(1)</script>", "iv": [1, 2, 3], "created": 1 });
        let html = page_html(Some(&rec));
        assert!(!html.contains("abc</script>"), "密文里的 </ 必须转义");
        assert!(html.contains("abc<\\/script>"));
        assert!(!html.contains("__SHARE_DATA__"));
        assert!(html.contains("<style>"), "tokens.css 要内联");
        let gone = page_html(None);
        assert!(gone.contains(">null</script>"));
    }

    #[test]
    fn ids_are_alnum_only() {
        assert!(valid_id("0123abcdef"));
        assert!(!valid_id("../x"));
        assert!(!valid_id(""));
    }
}
