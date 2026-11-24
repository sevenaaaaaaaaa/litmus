//! 三入口的 HTTP 面（P1-13）：访谈 / 提炼 / 粘贴。
//!
//! 两个纯端点，都不写盘：
//! - POST /field/soul/entrance  给输入，返回候选（逐字带出处）；
//! - POST /field/soul/draft     给输入 + 决定，返回组装好的卡与旁挂。
//!
//! 落盘只有一条 PUT /field/soul/card。三个入口共用它，不各写一套。

use super::{check_token, err, AppState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use kernel_soul::entrances::{self as soul_in, Candidate, Decision, NoteFile, Review, Rules};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// 规则从库里读（.litmus/soul-rules.json），没有就用编译进来的默认。
/// 库里有但不合法：报错，不静默退回默认——那会让用户以为自己加的线索词生效了。
pub(crate) fn load_rules(root: &Path) -> Result<(Rules, &'static str), String> {
    let p = root.join(".litmus/soul-rules.json");
    if p.exists() {
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("读不了 {}：{e}", p.display()))?;
        let rules: Rules = serde_json::from_str(&raw).map_err(|e| format!("库里的 soul-rules.json 不合法：{e}"))?;
        soul_in::validate_rules(&rules).map_err(|e| format!("{}: {}", e.code, e.message))?;
        return Ok((rules, "vault"));
    }
    Ok((soul_in::default_rules(), "default"))
}

/// 三种输入 → 同一份候选。提炼要读盘（只读 Notes/ 下的 .md），访谈与粘贴不读盘。
fn derive(req: &Value, root: &Path, rules: &Rules) -> Result<(String, Vec<Candidate>), (StatusCode, String)> {
    let kind = req["kind"].as_str().unwrap_or("").to_string();
    match kind.as_str() {
        "interview" => {
            let obj = req["answers"]
                .as_object()
                .ok_or((StatusCode::BAD_REQUEST, "interview 要 answers 对象".to_string()))?;
            let pairs: Vec<(String, String)> = obj
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect();
            if pairs.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "answers 是空的".to_string()));
            }
            let cands = soul_in::from_interview(&pairs).map_err(|e| (StatusCode::BAD_REQUEST, format!("{}: {}", e.code, e.message)))?;
            Ok((kind, cands))
        }
        "extract" => {
            let paths = req["paths"]
                .as_array()
                .ok_or((StatusCode::BAD_REQUEST, "extract 要 paths 数组".to_string()))?;
            if paths.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "paths 是空的".to_string()));
            }
            let per = req["perBlock"].as_u64().unwrap_or(3).clamp(1, 9) as usize;
            let mut owned: Vec<(String, String)> = Vec::new();
            for p in paths {
                let rel = p.as_str().unwrap_or("");
                let abs = super::soul::card_path_in(root, rel)
                    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
                let text = std::fs::read_to_string(&abs)
                    .map_err(|_| (StatusCode::NOT_FOUND, format!("没有这篇：{rel}")))?;
                owned.push((rel.to_string(), text));
            }
            if owned.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "没有可提炼的笔记".to_string()));
            }
            let files: Vec<NoteFile<'_>> = owned
                .iter()
                .map(|(path, text)| NoteFile { path, text })
                .collect();
            Ok((kind, soul_in::from_notes(&files, rules, per)))
        }
        "paste" => {
            let text = req["text"].as_str().unwrap_or("");
            if text.trim().is_empty() {
                return Err((StatusCode::BAD_REQUEST, "text 是空的".to_string()));
            }
            let block = req["block"].as_str().unwrap_or("voice");
            let cands = soul_in::from_paste(text, block)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("{}: {}", e.code, e.message)))?;
            Ok((kind, cands))
        }
        other => Err((
            StatusCode::BAD_REQUEST,
            format!("未知入口：{other}（只认 interview / extract / paste）"),
        )),
    }
}

/// 候选端点：只读、只算，不写盘。
pub async fn entrance(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let (rules, rules_source) = match load_rules(&root) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let (kind, cands) = match derive(&req, &root, &rules) {
        Ok(v) => v,
        Err((s, e)) => return err(s, e).into_response(),
    };
    for c in &cands {
        if let Err(i) = soul_in::assert_grounded(c) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{}: {}", i.code, i.message)).into_response();
        }
    }
    Json(json!({ "kind": kind, "candidates": cands, "rulesSource": rules_source })).into_response()
}

/// 组装端点：同一个输入 + 用户决定 → 卡与旁挂。仍然不写盘。
/// 服务端重新推导一遍候选，客户端只送「第几条、接受/改写/删除」——正文不能由客户端凭空给。
pub async fn draft(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let (rules, _) = match load_rules(&root) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let (kind, cands) = match derive(&req, &root, &rules) {
        Ok(v) => v,
        Err((s, e)) => return err(s, e).into_response(),
    };
    let mut review = Review::new(cands);
    let decisions = req["decisions"].as_array().cloned().unwrap_or_default();
    if decisions.is_empty() {
        return err(StatusCode::BAD_REQUEST, "一条决定都没有：接受、改写或删除至少要动一次".to_string()).into_response();
    }
    for d in decisions {
        let index = match d["index"].as_u64() {
            Some(i) => i as usize,
            None => return err(StatusCode::BAD_REQUEST, "决定缺 index".to_string()).into_response(),
        };
        let decision = match d["decision"].as_str().unwrap_or("") {
            "accepted" => Decision::Accepted,
            "edited" => Decision::Edited,
            "dropped" => Decision::Dropped,
            other => return err(StatusCode::BAD_REQUEST, format!("未知决定：{other}")).into_response(),
        };
        if let Err(e) = review.decide(index, decision, d["edited"].as_str()) {
            return err(StatusCode::BAD_REQUEST, format!("{}: {}", e.code, e.message)).into_response();
        }
    }
    let kept = review.kept();
    if kept.is_empty() {
        return err(StatusCode::BAD_REQUEST, "一条都没留：至少接受或改写一条".to_string()).into_response();
    }
    let confirmed = req["confirmed"] == true;
    let version = req["version"].as_u64().unwrap_or(0) as u32;
    let date = kernel_core::today_string();
    let draft = match soul_in::assemble(&kept, confirmed, version, &date) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{}: {}", e.code, e.message)).into_response(),
    };
    if let Some(first) = kernel_soul::parse(&draft.markdown).errors.first() {
        return err(StatusCode::BAD_REQUEST, format!("组装出来的卡不合法：{}", first.message)).into_response();
    }
    Json(json!({
        "kind": kind,
        "markdown": draft.markdown,
        "sources": { "version": 1, "anchors": draft.sources },
        "itemCount": kept.len(),
        "anchorCount": draft.sources.len(),
    }))
    .into_response()
}
#[cfg(test)]
mod tests {
    use crate::{build_state, router as build_router};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use kernel_core::Vault;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn app() -> (axum::Router, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path(), "soul-entrance").unwrap();
        let state = build_state(vault).unwrap();
        let token = state.token.clone();
        (build_router(state), token, dir)
    }

    fn req(method: &str, uri: &str, token: &str, body: Option<String>) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        b.body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap()
    }

    async fn json_of(resp: axum::response::Response) -> Value {
        let raw = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&raw).unwrap_or(Value::Null)
    }

    const NOTE: &str = "# 判断与证据\n\n判断不是结论，是被反复使用过的取舍。\n\n先看反例：没有反例说明还没想清楚。\n\n不写没有来源的结论。\n";

    #[tokio::test]
    async fn interview_entrance_makes_a_temporary_card_with_sources() {
        let (app, token, dir) = app();
        let answers = json!({ "belief": "知识不是收藏，是被用过的判断。好作品先有观点。" });

        let resp = app
            .clone()
            .oneshot(req("POST", "/field/soul/entrance", &token, Some(json!({"kind":"interview","answers":answers}).to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_of(resp).await;
        assert_eq!(v["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(v["rulesSource"], "default");

        let draft_body = json!({
            "kind": "interview",
            "answers": answers,
            "decisions": [
                {"index": 0, "decision": "accepted"},
                {"index": 1, "decision": "dropped"}
            ]
        })
        .to_string();
        let resp = app.clone().oneshot(req("POST", "/field/soul/draft", &token, Some(draft_body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let d = json_of(resp).await;
        assert!(d["markdown"].as_str().unwrap().contains("^belief-1"));
        assert_eq!(d["anchorCount"], 1);

        let put = json!({ "path": "Notes/灵魂卡.md", "markdown": d["markdown"], "sources": d["sources"] }).to_string();
        let resp = app.clone().oneshot(req("PUT", "/field/soul/card", &token, Some(put))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app.oneshot(req("GET", "/field/soul/card?path=Notes%2F灵魂卡.md", &token, None)).await.unwrap();
        let v = json_of(resp).await;
        assert_eq!(v["temporary"], true);
        assert_eq!(v["missingSources"], 0);
        assert_eq!(v["sections"][0]["items"][0]["source"]["ref"], "answer:belief");
        assert!(dir.path().join("Notes/灵魂卡.md").exists());
    }

    #[tokio::test]
    async fn extract_reads_notes_and_keeps_line_refs() {
        let (app, token, dir) = app();
        let notes = dir.path().join("Notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("note-1.md"), NOTE).unwrap();

        let resp = app
            .clone()
            .oneshot(req("POST", "/field/soul/entrance", &token, Some(json!({"kind":"extract","paths":["Notes/note-1.md"]}).to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_of(resp).await;
        let cands = v["candidates"].as_array().unwrap();
        assert!(cands.len() >= 3, "至少要三条，实际 {}", cands.len());
        for c in cands {
            assert!(c["ref"].as_str().unwrap().starts_with("Notes/note-1.md:"));
        }

        let decisions: Vec<Value> = (0..cands.len()).map(|i| json!({"index": i, "decision": "accepted"})).collect();
        let draft = json!({"kind":"extract","paths":["Notes/note-1.md"],"decisions":decisions}).to_string();
        let resp = app.clone().oneshot(req("POST", "/field/soul/draft", &token, Some(draft))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let d = json_of(resp).await;

        let put = json!({ "path": "Notes/灵魂卡.md", "markdown": d["markdown"], "sources": d["sources"] }).to_string();
        let resp = app.clone().oneshot(req("PUT", "/field/soul/card", &token, Some(put))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app.oneshot(req("GET", "/field/soul/card?path=Notes%2F灵魂卡.md", &token, None)).await.unwrap();
        let v = json_of(resp).await;
        assert_eq!(v["missingSources"], 0);
        let refs: Vec<String> = v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s["items"].as_array().unwrap().iter())
            .map(|i| i["source"]["ref"].as_str().unwrap().to_string())
            .collect();
        assert!(refs.iter().all(|r| r.starts_with("Notes/note-1.md:")));
    }

    #[tokio::test]
    async fn paste_edit_keeps_the_original_quote_and_is_not_flagged() {
        let (app, token, _dir) = app();
        let text = "我写东西先给结论。然后再补理由。";
        let resp = app
            .clone()
            .oneshot(req("POST", "/field/soul/entrance", &token, Some(json!({"kind":"paste","text":text,"block":"voice"}).to_string())))
            .await
            .unwrap();
        let v = json_of(resp).await;
        assert_eq!(v["candidates"].as_array().unwrap().len(), 2);

        let draft = json!({
            "kind": "paste", "text": text, "block": "voice",
            "decisions": [
                {"index": 0, "decision": "edited", "edited": "先给结论，再补理由。"},
                {"index": 1, "decision": "dropped"}
            ]
        })
        .to_string();
        let resp = app.clone().oneshot(req("POST", "/field/soul/draft", &token, Some(draft))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let d = json_of(resp).await;
        assert!(d["markdown"].as_str().unwrap().contains("先给结论，再补理由。"));
        assert_eq!(d["sources"]["anchors"]["voice-1"]["quote"], "我写东西先给结论");
        assert_eq!(d["sources"]["anchors"]["voice-1"]["edited"], true);

        let put = json!({ "path": "Notes/灵魂卡.md", "markdown": d["markdown"], "sources": d["sources"] }).to_string();
        let resp = app.clone().oneshot(req("PUT", "/field/soul/card", &token, Some(put))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app.oneshot(req("GET", "/field/soul/card?path=Notes%2F灵魂卡.md", &token, None)).await.unwrap();
        let v = json_of(resp).await;
        assert_eq!(v["sourceIssues"].as_array().unwrap().len(), 0, "改写过的条目不报 unquoted-source");
        assert_eq!(v["missingSources"], 0);
    }

    #[tokio::test]
    async fn draft_refuses_bad_decisions_and_unknown_entrances() {
        let (app, token, _dir) = app();
        let bad_index = json!({
            "kind": "paste", "text": "一句够长的原话在这里。",
            "decisions": [{"index": 99, "decision": "accepted"}]
        })
        .to_string();
        let resp = app.clone().oneshot(req("POST", "/field/soul/draft", &token, Some(bad_index))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let empty = json!({"kind":"paste","text":"一句够长的原话在这里。","decisions":[]}).to_string();
        let resp = app.clone().oneshot(req("POST", "/field/soul/draft", &token, Some(empty))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let unknown = json!({"kind":"telepathy","text":"x"}).to_string();
        let resp = app.oneshot(req("POST", "/field/soul/entrance", &token, Some(unknown))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn vault_rules_override_the_defaults_and_bad_rules_are_refused() {
        let (app, token, dir) = app();
        let notes = dir.path().join("Notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(notes.join("n.md"), "这句话只该命中「我相信」。\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".litmus")).unwrap();
        std::fs::write(
            dir.path().join(".litmus/soul-rules.json"),
            json!({
                "version": 1,
                "cues": {
                    "belief": ["这句话"], "criteria": ["标准"], "refuse": ["拒绝"],
                    "voice": ["短句"], "question": ["如何"], "reader": ["读者"]
                }
            })
            .to_string(),
        )
        .unwrap();

        let resp = app
            .clone()
            .oneshot(req("POST", "/field/soul/entrance", &token, Some(json!({"kind":"extract","paths":["Notes/n.md"]}).to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_of(resp).await;
        assert_eq!(v["rulesSource"], "vault");
        assert_eq!(v["candidates"][0]["block"], "belief");

        std::fs::write(dir.path().join(".litmus/soul-rules.json"), json!({"version":1,"cues":{"belief":["x"]}}).to_string()).unwrap();
        let resp = app
            .oneshot(req("POST", "/field/soul/entrance", &token, Some(json!({"kind":"extract","paths":["Notes/n.md"]}).to_string())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "规则缺区块要报错，不退回默认");
    }
}
