//! 灵魂卡的 HTTP 面（P1-10b 的 API 一半）。
//!
//! - `GET  /field/soul/card?path=` 读一张卡：区块、锚点、出处、问题
//! - `PUT  /field/soul/card`       写一张卡（带可选出处），拒绝不合法与覆盖真卡
//! - `POST /field/soul/upgrade`    临时卡 → 真卡：只翻 frontmatter 那一行
//!
//! 四条纪律：
//! 1. 升级后锚点集合必须是升级前的超集；不是就 409，且一个字节都不写。
//! 2. 写盘先 tmp 再 rename：半截文件比没有文件更坏。
//! 3. 出处缺失要报出来（`missingSources`），不静默补。
//! 4. 路径只接受 `Notes/` 下的 `.md`，拒绝绝对路径与 `..`——这条不是洁癖，
//!    这三个接口都会写盘。

use super::{check_token, err, AppState};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use kernel_soul::sources::{check_sources, parse_sources, sources_to_json, SOURCES_DIR, SOURCES_VERSION};
use kernel_soul::{parse, upgrade};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// 把用户给的相对路径变成「Notes/ 下的 .md」，否则给出人话理由。
pub fn card_path_in(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("path 不能为空".to_string());
    }
    if !rel.ends_with(".md") {
        return Err(format!("只接受 .md：{rel}"));
    }
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))) {
        return Err(format!("不允许绝对路径或 ..：{rel}"));
    }
    let mut it = p.iter();
    if it.next().map(|s| s.to_string_lossy().to_string()).as_deref() != Some("Notes") {
        return Err(format!("只接受 Notes/ 下的卡：{rel}"));
    }
    Ok(root.join(p))
}

/// 卡路径 → 旁挂路径。与 Node 参考实现 sourcesPath 同一规则：只转义 % 和 /。
pub fn sources_rel(rel: &str) -> String {
    let stem = rel.strip_suffix(".md").or_else(|| rel.strip_suffix(".MD")).unwrap_or(rel);
    let safe = stem.replace('%', "%25").replace('/', "%2F");
    format!("{SOURCES_DIR}/{safe}.json")
}

fn write_atomic(path: &Path, data: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp-soul");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// 读一张卡。卡不存在是 404，不是空卡——空卡会让人以为卡还在、只是没内容。
pub async fn card_get(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let rel = q.get("path").cloned().unwrap_or_default();
    let path = match card_path_in(&root, &rel) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return err(StatusCode::NOT_FOUND, format!("没有这张卡：{rel}")).into_response(),
    };
    let doc = parse(&text);

    let sources_path = root.join(sources_rel(&rel));
    let (sources, sources_error) = match std::fs::read_to_string(&sources_path) {
        Err(_) => (None, Some("no-sources".to_string())),
        Ok(raw) => match parse_sources(&raw) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(format!("{}: {}", e.code, e.message))),
        },
    };
    let issues = sources.as_ref().map(|s| check_sources(&doc, s)).unwrap_or_default();

    Json(json!({
        "path": rel,
        "markdown": text,
        "temporary": doc.temporary,
        "version": doc.version,
        "sections": doc.sections.iter().map(|s| json!({
            "name": s.name,
            "key": s.key,
            "items": s.items.iter().map(|i| json!({
                "anchor": i.anchor,
                "text": i.text,
                "line": i.line,
                "source": i.anchor.as_ref().and_then(|a| sources.as_ref().and_then(|src| src.anchors.get(a))),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "errors": doc.errors,
        "warnings": doc.warnings,
        "anchorCount": doc.anchor_count,
        "sourcesPath": sources_rel(&rel),
        "sourcesError": sources_error,
        "sourceIssues": issues,
    }))
    .into_response()
}

/// 写一张卡。默认不覆盖已有真卡：`force` 才覆盖，且覆盖前要求新卡本身合法。
pub async fn card_put(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let rel = req["path"].as_str().unwrap_or_default().to_string();
    let markdown = req["markdown"].as_str().unwrap_or_default().to_string();
    let force = req["force"] == true;
    let root = st.kernel.lock().unwrap().vault.root.clone();

    let path = match card_path_in(&root, &rel) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    if markdown.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "markdown 不能为空".to_string()).into_response();
    }
    let doc = parse(&markdown);
    if !doc.errors.is_empty() {
        return err(StatusCode::BAD_REQUEST, format!("卡不合法：{}", doc.errors[0].message)).into_response();
    }
    if path.exists() && !force {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if !parse(&existing).temporary {
            return err(StatusCode::CONFLICT, format!("已经有真卡了：{rel}（要覆盖请显式 force）")).into_response();
        }
    }
    if let Err(e) = write_atomic(&path, &markdown) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    // 出处：给了就一起落盘，并当场对一遍；缺的报出来，不补。
    let mut sources_path: Option<String> = None;
    let mut source_issues: Vec<Value> = Vec::new();
    if let Some(obj) = req.get("sources").filter(|v| v.is_object()) {
        let mut s = kernel_soul::sources::Sources::new(rel.clone());
        let mut anchors = std::collections::BTreeMap::new();
        if let Some(map) = obj.get("anchors").and_then(|v| v.as_object()) {
            for (k, v) in map {
                let entry = serde_json::from_value::<kernel_soul::sources::SourceEntry>(v.clone());
                match entry {
                    Ok(e) => { anchors.insert(k.clone(), e); }
                    Err(e) => return err(StatusCode::BAD_REQUEST, format!("出处 {k} 格式不对：{e}")).into_response(),
                }
            }
        }
        s.anchors = anchors;
        s.version = SOURCES_VERSION;
        let sp = root.join(sources_rel(&rel));
        if let Err(e) = write_atomic(&sp, &sources_to_json(&s)) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
        sources_path = Some(sources_rel(&rel));
        source_issues = check_sources(&doc, &s).into_iter().map(|i| json!(i)).collect();
    }

    Json(json!({
        "ok": true,
        "path": rel,
        "temporary": doc.temporary,
        "anchorCount": doc.anchor_count,
        "sourcesPath": sources_path,
        "sourceIssues": source_issues,
    }))
    .into_response()
}

/// 临时卡 → 真卡。只翻 frontmatter 那一行；锚点丢了就 409，一个字节都不写。
pub async fn upgrade_post(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let rel = req["path"].as_str().unwrap_or_default().to_string();
    let root = st.kernel.lock().unwrap().vault.root.clone();

    let path = match card_path_in(&root, &rel) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return err(StatusCode::NOT_FOUND, format!("没有这张卡：{rel}")).into_response(),
    };

    let up = match upgrade(&text) {
        Ok(u) => u,
        Err(e) => {
            let status = match e.code.as_str() {
                "upgrade-already-confirmed" => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            return err(status, format!("{}: {}", e.code, e.message)).into_response();
        }
    };

    if let Err(e) = write_atomic(&path, &up.text) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    // 升级不动锚点，所以升级前建的旁挂应当逐条仍然对得上；对不上就报出来。
    let doc = parse(&up.text);
    let sp = root.join(sources_rel(&rel));
    let mut source_issues: Vec<Value> = Vec::new();
    let mut missing = 0usize;
    if let Ok(raw) = std::fs::read_to_string(&sp) {
        if let Ok(s) = parse_sources(&raw) {
            let issues = check_sources(&doc, &s);
            missing = issues.iter().filter(|i| i.code == "missing-source").count();
            source_issues = issues.into_iter().map(|i| json!(i)).collect();
        }
    }

    Json(json!({
        "ok": true,
        "path": rel,
        "temporary": false,
        "anchorsBefore": up.anchors_before,
        "anchorsAfter": up.anchors_after,
        "superset": true,
        "missingSources": missing,
        "sourceIssues": source_issues,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_path_rejects_anything_outside_notes_md() {
        let root = Path::new("/vault");
        for bad in [
            "",
            "Notes/../etc/passwd.md",
            "/etc/x.md",
            "Notes/a.txt",
            "Assets/a.md",
            "Notes",
            "../Notes/a.md",
        ] {
            assert!(card_path_in(root, bad).is_err(), "应当拒绝：{bad:?}");
        }
    }

    #[test]
    fn card_path_accepts_a_notes_markdown() {
        let root = Path::new("/vault");
        let p = card_path_in(root, "Notes/灵魂卡.md").expect("合法路径");
        assert_eq!(p, root.join("Notes/灵魂卡.md"));
    }

    #[test]
    fn sources_rel_escapes_only_percent_and_slash() {
        assert_eq!(sources_rel("Notes/灵魂卡.md"), ".litmus/soul-sources/Notes%2F灵魂卡.json");
        assert_eq!(sources_rel("Notes/a%b.md"), ".litmus/soul-sources/Notes%2Fa%25b.json");
        assert_eq!(SOURCES_DIR, ".litmus/soul-sources");
    }

    #[test]
    fn two_different_cards_never_share_a_sources_file() {
        // 转义必须单射：撞名等于两条判断共用一份出处。
        let a = sources_rel("Notes/a/b.md");
        let b = sources_rel("Notes/a__b.md");
        let c = sources_rel("Notes/a%2Fb.md");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("litmus-soul-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("Notes/card.md");
        write_atomic(&p, "第一次\n").expect("写第一版");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "第一次\n");
        write_atomic(&p, "第二次\n").expect("覆盖");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "第二次\n");
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp-soul"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下临时文件：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── HTTP 面：真起 router，真写盘 ────────────────────────────────────────
    // 这一层不测「函数返回了什么」，测「文件有没有被改」——因为这三个端点都会写盘。

    use crate::{build_state, router as build_router};
    use axum::body::Body;
    use axum::http::Request;
    use kernel_core::Vault;
    use tower::ServiceExt;

    const TEMP_CARD: &str = "---
type: soul-card
version: 0
confirmed: false
---

## 我相信
- 知识不是收藏，是被用过的判断。 ^belief-1

## 我如何判断
- 先看反例：没有反例说明还没想清楚。 ^criteria-1

## 我拒绝
- 不做没有出处的断言。 ^refuse-1

## 我的声音
- 短句、具体、少形容词。 ^voice-1

## 我在追的问题
- 什么事值得反复做。 ^question-1

## 我的读者 / 作品
- 读者：正在做产品的人。 ^reader-1
";

    fn app() -> (axum::Router, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path(), "soul-api").unwrap();
        let state = build_state(vault).unwrap();
        let token = state.token.clone();
        (build_router(state), token, dir)
    }

    fn auth_req(method: &str, uri: &str, token: &str, body: Option<String>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        b.body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap()
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let raw = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn upgrade_endpoint_flips_confirmed_and_keeps_every_anchor() {
        let (app, token, dir) = app();
        let p = dir.path().join("Notes");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("temp.md"), TEMP_CARD).unwrap();

        let resp = app
            .oneshot(auth_req("POST", "/field/soul/upgrade", &token, Some(r#"{"path":"Notes/temp.md"}"#.to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["superset"], true);
        assert_eq!(v["anchorsBefore"], v["anchorsAfter"], "升级不该动锚点");

        let after = std::fs::read_to_string(p.join("temp.md")).unwrap();
        assert!(after.contains("confirmed: true"), "frontmatter 必须翻过来");
        assert!(!after.contains("confirmed: false"));
        assert!(after.contains("^belief-1"), "锚点必须还在");
    }

    #[tokio::test]
    async fn upgrade_endpoint_refuses_a_confirmed_card() {
        let (app, token, dir) = app();
        let p = dir.path().join("Notes");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("real.md"), TEMP_CARD.replace("confirmed: false", "confirmed: true")).unwrap();

        let resp = app
            .oneshot(auth_req("POST", "/field/soul/upgrade", &token, Some(r#"{"path":"Notes/real.md"}"#.to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "真卡不该被再升级一次");
    }

    #[tokio::test]
    async fn card_put_writes_provenance_and_get_reports_the_gaps() {
        let (app, token, dir) = app();
        let body = serde_json::json!({
            "path": "Notes/temp.md",
            "markdown": TEMP_CARD,
            "sources": { "anchors": {
                "belief-1": { "source": "interview", "ref": "answer:belief", "quote": "知识不是收藏，是被用过的判断。" }
            }}
        })
        .to_string();
        let resp = app
            .clone()
            .oneshot(auth_req("PUT", "/field/soul/card", &token, Some(body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 旁挂只写了 1 条，其余 5 条应当被显式报缺——而不是被补成空出处。
        let resp = app
            .oneshot(auth_req("GET", "/field/soul/card?path=Notes%2Ftemp.md", &token, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let issues = v["sourceIssues"].as_array().expect("应当有逐条判定");
        let missing = issues.iter().filter(|i| i["code"] == "missing-source").count();
        assert_eq!(missing, 5, "6 条判断只给了 1 条出处，应当报 5 条缺");

        let sidecar = dir.path().join(".litmus/soul-sources/Notes%2Ftemp.json");
        let raw = std::fs::read_to_string(&sidecar).expect("旁挂必须落盘");
        assert!(raw.contains("belief-1"));
        assert!(!raw.contains("criteria-1"), "缺的出处不许被凭空补上");
    }

    #[tokio::test]
    async fn card_put_rejects_path_that_leaves_notes() {
        let (app, token, _dir) = app();
        for bad in ["Notes/../secret.md", "/etc/passwd.md", "Assets/a.md"] {
            let body = serde_json::json!({ "path": bad, "markdown": TEMP_CARD }).to_string();
            let resp = app
                .clone()
                .oneshot(auth_req("PUT", "/field/soul/card", &token, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "应当拒绝：{bad}");
        }
    }
}

