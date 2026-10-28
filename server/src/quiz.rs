//! 逐题复习：每篇可以拆成几道题（问一句、答一句），每道题自己排复习（简化的 SM-2）。
//!
//! - 题存在 `.litmus/learn.json` 的 `qs` 里，随库同步：`{题号: {doc, q, a, by, due, interval, ease, reps, lapses, last, grade}}`。
//! - 出题：配了模型就让模型出 4–8 道检验理解的题；没配就按规则出——加粗的词挖空、「X：Y」问 X 是什么、
//!   每个小节问一句「讲了什么」。出的题都能改、能删；复习过的题重新出题时留着。
//! - `/field/learn/due`：现在该复习的题（跨所有文档，最早到期的在前）。

use super::field_tools::{learn_load, learn_lock, learn_save, schedule};
use super::{check_token, err, AppState};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn qid(doc: &str, q: &str) -> String {
    kernel_core::Cas::hash_hex(format!("{doc}\n{q}").as_bytes())[..12].to_string()
}

fn clip(s: &str, n: usize) -> String {
    let t: String = s.trim().chars().take(n).collect();
    if s.trim().chars().count() > n { format!("{t}…") } else { t }
}

/// 去掉行内 Markdown 记号：**粗**、`码`、[文字](链接)、[[链接|别名]]
fn plain(s: &str) -> String {
    let mut out = String::new();
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '[' && cs.get(i + 1) == Some(&'[') {
            if let Some(e) = (i + 2..cs.len().saturating_sub(1)).find(|&j| cs[j] == ']' && cs[j + 1] == ']') {
                let inner: String = cs[i + 2..e].iter().collect();
                out.push_str(inner.rsplit('|').next().unwrap_or(&inner).rsplit('/').next().unwrap_or(""));
                i = e + 2;
                continue;
            }
        }
        if cs[i] == '[' {
            if let Some(m) = (i + 1..cs.len()).find(|&j| cs[j] == ']') {
                if cs.get(m + 1) == Some(&'(') {
                    if let Some(e) = (m + 2..cs.len()).find(|&j| cs[j] == ')') {
                        out.extend(&cs[i + 1..m]);
                        i = e + 1;
                        continue;
                    }
                }
            }
        }
        if matches!(cs[i], '*' | '`' | '_') && (cs[i] != '_' || cs.get(i + 1) == Some(&'_')) {
            i += 1;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out.trim().to_string()
}

/// 按规则出题（不需要模型）
pub fn rule_questions(md: &str, title: &str) -> Vec<(String, String)> {
    let mut cloze = Vec::new();
    let mut defs = Vec::new();
    let mut secs: Vec<(String, String)> = Vec::new();
    let mut heading = String::new();
    let mut body = String::new();
    let mut fence = false;
    let mut lines = md.lines().peekable();
    if md.starts_with("---") {
        lines.next();
        for l in lines.by_ref() {
            if l.starts_with("---") {
                break;
            }
        }
    }
    let flush = |heading: &str, body: &str, secs: &mut Vec<(String, String)>| {
        let b = plain(body);
        if !heading.is_empty() && b.chars().count() >= 20 {
            let first: String = b.split_inclusive(['。', '！', '？', '.', '\n']).take(2).collect();
            secs.push((format!("「{heading}」讲了什么？"), clip(&first, 160)));
        }
    };
    for line in lines {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if let Some(h) = t.strip_prefix("## ").or_else(|| t.strip_prefix("### ")) {
            flush(&heading, &body, &mut secs);
            heading = plain(h);
            body.clear();
            continue;
        }
        if t.starts_with("# ") {
            continue;
        }
        body.push_str(t);
        body.push('\n');
        let item = t.trim_start_matches(['-', '*', '+', ' ', '>']).trim();
        // 定义：「键：值」
        if let Some((k, v)) = item.split_once('：').or_else(|| item.split_once(": ")) {
            let (k, v) = (plain(k), plain(v));
            if (2..=16).contains(&k.chars().count()) && v.chars().count() >= 4 && !k.contains("http") {
                defs.push((format!("「{k}」是什么？") + &if heading.is_empty() { String::new() } else { format!("（{heading}）") }, clip(&v, 160)));
                continue;
            }
        }
        // 挖空：句子里加粗的词
        if let Some(a) = item.find("**") {
            if let Some(b) = item[a + 2..].find("**") {
                let term = &item[a + 2..a + 2 + b];
                let sentence = plain(item);
                if (1..=20).contains(&term.chars().count()) && (12..=200).contains(&sentence.chars().count()) {
                    let q = plain(&format!("{}＿＿＿＿{}", &item[..a], &item[a + 4 + b..]));
                    cloze.push((format!("填空：{q}"), term.to_string()));
                }
            }
        }
    }
    flush(&heading, &body, &mut secs);
    let mut out: Vec<(String, String)> = Vec::new();
    for x in cloze.into_iter().take(5).chain(defs.into_iter().take(5)).chain(secs.into_iter().take(6)) {
        if !out.iter().any(|y| y.0 == x.0) {
            out.push(x);
        }
    }
    if out.is_empty() {
        let b = plain(&md.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n"));
        if b.chars().count() >= 20 {
            out.push((format!("《{title}》讲了什么？"), clip(&b, 160)));
        }
    }
    out.truncate(12);
    out
}

/// 从模型回答里抠出 [{q, a}]
fn parse_ai(text: &str) -> Vec<(String, String)> {
    let (Some(s), Some(e)) = (text.find('['), text.rfind(']')) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text[s..=e]) else { return Vec::new() };
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| {
            let q = x.get("q").or_else(|| x.get("question")).and_then(|v| v.as_str())?.trim().to_string();
            let a = x.get("a").or_else(|| x.get("answer")).and_then(|v| v.as_str())?.trim().to_string();
            (!q.is_empty() && !a.is_empty()).then(|| (clip(&q, 200), clip(&a, 400)))
        })
        .take(10)
        .collect()
}

fn doc_qs(state: &Value, path: &str) -> Vec<Value> {
    let mut v: Vec<Value> = state["qs"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, x)| x["doc"] == path)
        .map(|(id, x)| {
            let mut y = x.clone();
            y["id"] = json!(id);
            y
        })
        .collect();
    v.sort_by_key(|x| x["n"].as_u64().unwrap_or(0));
    v
}

/// GET /field/learn/questions?path=
pub async fn list(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    let root = st.kernel.lock().unwrap().vault.root.clone();
    Json(json!({ "path": path, "qs": doc_qs(&learn_load(&root), &path) })).into_response()
}

/// POST /field/learn/questions {path, ai?:true}：给一篇出题。没复习过的旧题换成新的，复习过的留着。
pub async fn generate(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !super::is_safe_doc_path(&path) || !path.ends_with(".md") {
        return err(StatusCode::BAD_REQUEST, "只能给 Notes/ 下的 Markdown 文档出题").into_response();
    }
    let (root, ai) = {
        let k = st.kernel.lock().unwrap();
        (k.vault.root.clone(), k.vault.config.ai.clone().filter(|c| !c.base_url.is_empty() && !c.model.is_empty()))
    };
    let Ok(md) = std::fs::read_to_string(root.join(&path)) else { return err(StatusCode::NOT_FOUND, "这篇不在了").into_response() };
    let title = md.lines().find_map(|l| l.strip_prefix("# ")).map(|s| s.trim().to_string()).unwrap_or_else(|| path.rsplit('/').next().unwrap_or(&path).trim_end_matches(".md").to_string());
    let mut by = "rule";
    let mut qs = Vec::new();
    if req.get("ai").and_then(|v| v.as_bool()).unwrap_or(true) {
        if let Some(cfg) = ai {
            let text: String = md.chars().take(6000).collect();
            let sys = "你给用户的学习笔记出复习题。出 4 到 8 道题，检验是否真的理解、记住：问「为什么」「怎么做」「区别是什么」和关键的事实，不要问标题这种没有信息量的。\
                       答案要短（不超过 60 字），只根据原文。只输出 JSON 数组：[{\"q\":\"问题\",\"a\":\"答案\"}]，不要别的字。";
            if let Ok(t) = litmus_mcp::agent::complete(&cfg, sys, &format!("《{title}》\n\n{text}")).await {
                qs = parse_ai(&t);
                if !qs.is_empty() {
                    by = "ai";
                }
            }
        }
    }
    if qs.is_empty() {
        qs = rule_questions(&md, &title);
    }
    if qs.is_empty() {
        return err(StatusCode::CONFLICT, "这篇太短，出不了题：可以自己加一道").into_response();
    }
    let _g = learn_lock();
    let mut state = learn_load(&root);
    let obj = state["qs"].as_object_mut().unwrap();
    // 没复习过的旧题拿掉（复习过的留着，进度不丢）
    obj.retain(|_, x| x["doc"] != path || x["reps"].as_u64().unwrap_or(0) > 0);
    let base = obj.values().filter(|x| x["doc"] == path).count() as u64;
    let now = now_secs();
    for (n, (q, a)) in qs.iter().enumerate() {
        let id = qid(&path, q);
        if obj.contains_key(&id) {
            continue;
        }
        obj.insert(id, json!({ "doc": path, "q": q, "a": a, "by": by, "n": base + n as u64, "due": now, "reps": 0, "created": now }));
    }
    if let Err(e) = learn_save(&root, &state) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "path": path, "by": by, "qs": doc_qs(&state, &path) })).into_response()
}

/// POST /field/learn/question {id, q?, a?, delete?} 改一道 / 删一道；{path, q, a} 自己加一道
pub async fn edit(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let s = |k: &str| req.get(k).and_then(|v| v.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let _g = learn_lock();
    let mut state = learn_load(&root);
    let obj = state["qs"].as_object_mut().unwrap();
    let out_path;
    if let Some(id) = s("id") {
        let Some(x) = obj.get_mut(&id) else { return err(StatusCode::NOT_FOUND, "这道题不在了").into_response() };
        out_path = x["doc"].as_str().unwrap_or("").to_string();
        if req.get("delete").and_then(|v| v.as_bool()).unwrap_or(false) {
            obj.remove(&id);
        } else {
            if let Some(q) = s("q") { x["q"] = json!(clip(&q, 200)); }
            if let Some(a) = s("a") { x["a"] = json!(clip(&a, 400)); }
            x["by"] = json!("you");
        }
    } else {
        let (Some(path), Some(q), Some(a)) = (s("path"), s("q"), s("a")) else { return err(StatusCode::BAD_REQUEST, "要有问题和答案").into_response() };
        if !super::is_safe_doc_path(&path) {
            return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
        }
        let n = obj.values().filter(|x| x["doc"] == path.as_str()).count();
        obj.insert(qid(&path, &q), json!({ "doc": path, "q": clip(&q, 200), "a": clip(&a, 400), "by": "you", "n": n, "due": now_secs(), "reps": 0, "created": now_secs() }));
        out_path = path;
    }
    if let Err(e) = learn_save(&root, &state) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "path": out_path, "qs": doc_qs(&state, &out_path) })).into_response()
}

/// POST /field/learn/review {id, grade}
pub async fn review(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let grade = req.get("grade").and_then(|v| v.as_str()).filter(|g| matches!(*g, "again" | "hard" | "good" | "easy")).unwrap_or("good").to_string();
    let now = req.get("now").and_then(|v| v.as_u64()).unwrap_or_else(now_secs);
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let _g = learn_lock();
    let mut state = learn_load(&root);
    let Some(x) = state["qs"].get_mut(&id) else { return err(StatusCode::NOT_FOUND, "这道题不在了").into_response() };
    let card = schedule(x, &grade, now);
    if let (Some(o), Some(c)) = (x.as_object_mut(), card.as_object()) {
        for (k, v) in c {
            o.insert(k.clone(), v.clone());
        }
    }
    let out = x.clone();
    if let Err(e) = learn_save(&root, &state) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "id": id, "q": out })).into_response()
}

/// GET /field/learn/due：现在该复习的题（最多 200 道，最早到期的在前；文档已经不在的跳过）
pub async fn due(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let state = learn_load(&root);
    let now = now_secs();
    let mut all: Vec<Value> = state["qs"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, x)| x["doc"].as_str().map(|d| root.join(d).exists()).unwrap_or(false))
        .map(|(id, x)| {
            let mut y = x.clone();
            y["id"] = json!(id);
            y
        })
        .collect();
    let total = all.len();
    let soon = all.iter().filter(|x| x["due"].as_u64().unwrap_or(0) > now && x["due"].as_u64().unwrap_or(0) <= now + 86_400 * 7).count();
    all.retain(|x| x["due"].as_u64().unwrap_or(0) <= now);
    all.sort_by_key(|x| x["due"].as_u64().unwrap_or(0));
    let n = all.len();
    all.truncate(200);
    Json(json!({ "due": all, "due_total": n, "soon": soon, "total": total })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_make_cloze_definition_and_section_questions() {
        let md = "---\ntags: [x]\n---\n# 同步\n\n离线编辑之后用 **CRDT** 合并，冲突时两边都保留。\n\n- 端口：本机默认 7700，服务器在 /litmus 下面\n\n## 冲突策略\n\n同名不同内容另存冲突副本。绝不静默覆盖任何一侧的内容，用户自己挑。\n\n```\n**不算**：代码\n```\n";
        let qs = rule_questions(md, "同步");
        let all: String = qs.iter().map(|(q, a)| format!("{q}={a}\n")).collect();
        assert!(all.contains("填空：离线编辑之后用 ＿＿＿＿ 合并") && all.contains("=CRDT"), "{all}");
        assert!(all.contains("「端口」是什么？=本机默认 7700"), "{all}");
        assert!(all.contains("「冲突策略」讲了什么？=同名不同内容另存冲突副本。"), "{all}");
        assert!(!all.contains("不算"), "代码块里的不出题：{all}");
    }

    #[test]
    fn ai_answer_parses_with_noise() {
        let v = parse_ai("好的：\n```json\n[{\"q\":\"为什么用 CRDT？\",\"a\":\"离线也能合并\"},{\"q\":\"\",\"a\":\"x\"}]\n```");
        assert_eq!(v, vec![("为什么用 CRDT？".to_string(), "离线也能合并".to_string())]);
    }
}
