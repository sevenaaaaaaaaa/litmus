//! 知识场 F4：学习（复述与复习）、随库同步的学习进度、AI 整理建议、网页本地化。

use super::field::{read_synced, write_synced};
use super::{audit_log, check_token, err, materialize, AppState};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

fn ai_cfg(st: &AppState) -> Option<kernel_core::AiConfig> {
    st.kernel.lock().unwrap().vault.config.ai.clone().filter(|c| !c.base_url.is_empty())
}

// ───────── 学习：把一节合上，用自己的话讲一遍；模型帮你出题、看你讲得怎样 ─────────

/// POST /field/learn {task:"question"|"check", title, section, text, answer?}
/// - question：针对这一节出一道检验理解的问题（不给答案）
/// - check：对照原文看用户的复述，指出讲对的、漏掉的、讲错的
/// 没配模型返回 409：学习本身不依赖模型（自评即可），模型只是加分项。
pub async fn learn(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let task = req.get("task").and_then(|v| v.as_str()).unwrap_or("question");
    let title = req.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let section = req.get("section").and_then(|v| v.as_str()).unwrap_or("");
    let text: String = req.get("text").and_then(|v| v.as_str()).unwrap_or("").chars().take(3000).collect();
    let answer: String = req.get("answer").and_then(|v| v.as_str()).unwrap_or("").chars().take(2000).collect();
    if text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "这一节没有内容").into_response();
    }
    if task == "check" && answer.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "先用自己的话讲一遍").into_response();
    }
    let Some(cfg) = ai_cfg(&st) else {
        return err(StatusCode::CONFLICT, "还没有配置模型：自己对照原文打分也一样能复习；配上模型（/设置）后会帮你出题、看你讲得怎样").into_response();
    };
    let (system, user) = if task == "check" {
        (
            "用户在用费曼法学习：读完一节后合上原文，用自己的话复述。你对照原文看他的复述，用中文分三行输出，每行不超过 60 字：\n\
             讲对了：……\n漏掉了：……（没有就写「没有」）\n讲偏了：……（没有就写「没有」）\n\
             最后单独一行给出评分：「评分：清楚」「评分：模糊」或「评分：没讲出来」。不要寒暄。",
            format!("文档《{title}》的一节「{section}」原文：\n{text}\n\n用户的复述：\n{answer}"),
        )
    } else {
        (
            "用户要用费曼法学习下面这一节。出一道能检验他是否真正理解的问题：问「为什么」或「怎样应用」，不要问能从原文直接抄的定义。\
             只输出问题本身，一句话，中文。",
            format!("文档《{title}》的一节「{section}」：\n{text}"),
        )
    };
    match litmus_mcp::agent::complete(&cfg, system, &user).await {
        Ok(t) => {
            let t = t.trim().to_string();
            let grade = if task == "check" {
                if t.contains("评分：清楚") {
                    "good"
                } else if t.contains("评分：没讲出来") {
                    "again"
                } else {
                    "hard"
                }
            } else {
                ""
            };
            let shown = t.lines().filter(|l| !l.trim_start().starts_with("评分：")).collect::<Vec<_>>().join("\n");
            Json(json!({ "text": shown.trim(), "grade": grade })).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("这次没有连上模型：{e}")).into_response(),
    }
}

/// GET /field/learn/state：复习队列 {cards:{路径:{due, interval, ease, reps, last}}}（随库同步，放 `.litmus/learn.json`）
pub async fn learn_state_get(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    Json(learn_load(&root)).into_response()
}

/// POST /field/learn/grade {path, grade:"again"|"hard"|"good"|"easy"}：按简化的 SM-2 排下次复习时间。
/// 间隔（天）：again → 1；hard → ×1.2；good → ×ease；easy → ×ease×1.3。首次 good = 3 天、easy = 5 天。
pub async fn learn_grade(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let grade = req.get("grade").and_then(|v| v.as_str()).unwrap_or("good");
    if !kernel_core::is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let now = req.get("now").and_then(|v| v.as_u64()).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    });
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let _g = learn_lock();
    let mut state = learn_load(&root);
    let prev = state["cards"].get(&path).cloned().unwrap_or(json!({}));
    let card = schedule(&prev, grade, now);
    state["cards"][&path] = card.clone();
    if let Err(e) = learn_save(&root, &state) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "path": path, "card": card })).into_response()
}

/// learn.json 的读改写排队（复习打分、出题、AI 学习收下都会写它）
pub fn learn_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

/// 读 learn.json：{cards:{路径:…}, qs:{题号:…}}；坏了、不是对象都当空的
pub fn learn_load(root: &std::path::Path) -> Value {
    let mut v: Value = read_synced(root, "learn.json", "field-learn.json").and_then(|s| serde_json::from_str(&s).ok()).filter(|v: &Value| v.is_object()).unwrap_or(json!({}));
    for k in ["cards", "qs"] {
        if !v[k].is_object() {
            v[k] = json!({});
        }
    }
    v
}

pub fn learn_save(root: &std::path::Path, v: &Value) -> Result<(), String> {
    write_synced(root, "learn.json", &serde_json::to_vec_pretty(v).unwrap_or_default()).map_err(|e| e.to_string())
}

/// 简化的 SM-2：间隔（天）again → 1；hard → ×1.2；good → ×ease；easy → ×ease×1.3。首次 good = 3 天、easy = 5 天。
pub fn schedule(prev: &Value, grade: &str, now: u64) -> Value {
    let reps = prev.get("reps").and_then(|v| v.as_u64()).unwrap_or(0);
    let mut ease = prev.get("ease").and_then(|v| v.as_f64()).unwrap_or(2.5);
    let interval = prev.get("interval").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let lapses = prev.get("lapses").and_then(|v| v.as_u64()).unwrap_or(0) + u64::from(grade == "again" && reps > 0);
    let (next, ease2) = match grade {
        "again" => (1.0, (ease - 0.2).max(1.3)),
        "hard" => ((interval * 1.2).max(1.0), (ease - 0.15).max(1.3)),
        "easy" => (if reps == 0 { 5.0 } else { (interval * ease * 1.3).max(4.0) }, ease + 0.15),
        _ => (if reps == 0 { 3.0 } else { (interval * ease).max(2.0) }, ease),
    };
    ease = ease2;
    let next = next.min(365.0);
    json!({
        "due": now + (next * 86_400.0) as u64,
        "interval": (next * 10.0).round() / 10.0,
        "ease": (ease * 100.0).round() / 100.0,
        "reps": reps + 1,
        "lapses": lapses,
        "last": now,
        "grade": grade,
    })
}

// ───────── AI 整理：补标签、起更好的标题（只给建议，执行在壳里，可撤销） ─────────

/// 从模型回答里抠出 JSON 对象（容忍 ```json 围栏与前后废话）。
pub fn json_in(text: &str) -> Option<Value> {
    let s = text.find('{')?;
    let e = text.rfind('}')?;
    (e > s).then(|| serde_json::from_str(&text[s..=e]).ok()).flatten()
}

/// 给 /field/organize 追加 AI 建议。`docs`：(路径, 标题, 摘要, 已有标签)。返回 (tags, titles, note)。
pub async fn ai_suggestions(st: &Arc<AppState>, docs: Vec<(String, String, String, Vec<String>)>, vocab: Vec<String>) -> (Vec<Value>, Vec<Value>, Option<String>) {
    let Some(cfg) = ai_cfg(st) else {
        return (Vec::new(), Vec::new(), Some("没配模型：只给了按场结构算出的建议。配上模型（/设置）后还会建议标签和标题。".into()));
    };
    // 只看没标签的或标题过短的，最多 40 篇，控制 token
    let pick: Vec<&(String, String, String, Vec<String>)> = docs
        .iter()
        .filter(|(p, t, _, g)| p.ends_with(".md") && !p.starts_with("Notes/Memos/") && !p.starts_with("Notes/Agent/") && (g.is_empty() || t.chars().count() <= 4))
        .take(40)
        .collect();
    if pick.is_empty() {
        return (Vec::new(), Vec::new(), None);
    }
    let list: Vec<String> = pick
        .iter()
        .enumerate()
        .map(|(i, (p, t, ex, g))| format!("{}. path={p}\n   title={t}\n   tags={}\n   摘要={}", i + 1, g.join(","), ex.chars().take(160).collect::<String>()))
        .collect();
    let system = "你在帮用户整理个人知识库。对每篇文档：没有标签的，给 1 到 3 个标签，优先从「已有标签」里选，确实没有合适的才新造（新标签短、中文、不带 #）；\
        标题过短或说不清内容的，给一个 8 到 20 字、说清内容的新标题，标题本来就好就不要给。\
        只输出 JSON：{\"items\":[{\"path\":\"…\",\"tags\":[\"…\"],\"title\":\"…或省略\"}]}，不要别的文字。";
    let user = format!("已有标签：{}\n\n文档：\n{}", if vocab.is_empty() { "（还没有）".into() } else { vocab.join("、") }, list.join("\n"));
    let out = match litmus_mcp::agent::complete(&cfg, system, &user).await {
        Ok(t) => t,
        Err(e) => return (Vec::new(), Vec::new(), Some(format!("这次没有连上模型（{e}），只给了按场结构算出的建议。"))),
    };
    let Some(v) = json_in(&out) else {
        return (Vec::new(), Vec::new(), Some("模型的回答看不懂，只给了按场结构算出的建议。".into()));
    };
    let known: std::collections::HashMap<&str, &(String, String, String, Vec<String>)> = pick.iter().map(|d| (d.0.as_str(), *d)).collect();
    let mut tags = Vec::new();
    let mut titles = Vec::new();
    for it in v.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let Some(p) = it.get("path").and_then(|x| x.as_str()) else { continue };
        let Some(d) = known.get(p) else { continue };
        if d.3.is_empty() {
            let tg: Vec<String> = it
                .get("tags")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|t| t.as_str()).map(|t| t.trim().trim_start_matches('#').chars().take(16).collect::<String>()).filter(|t| !t.is_empty() && !t.contains(char::is_whitespace)).take(3).collect())
                .unwrap_or_default();
            if !tg.is_empty() {
                let fresh: Vec<&String> = tg.iter().filter(|t| !vocab.contains(t)).collect();
                tags.push(json!({ "path": p, "title": d.1, "tags": tg, "new": fresh }));
            }
        }
        if let Some(t) = it.get("title").and_then(|x| x.as_str()).map(|t| t.trim().to_string()).filter(|t| t.chars().count() >= 4 && t.chars().count() <= 40 && *t != d.1) {
            titles.push(json!({ "path": p, "from": d.1, "to": t }));
        }
    }
    (tags, titles, None)
}

// ───────── 网页本地化：正文转成结构化 Markdown，图片下载进 Assets/ ─────────

/// 取正文区：最长的 <article>，否则 <main>，否则 <body>；去掉导航、页眉页脚、侧栏、表单与脚本。
pub fn main_content(html: &str) -> String {
    let lower = html.to_lowercase();
    let pick = |tag: &str| -> Option<(usize, usize)> {
        let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
        let mut best: Option<(usize, usize)> = None;
        let mut from = 0;
        while let Some(i) = lower[from..].find(&open).map(|i| i + from) {
            let after = lower.as_bytes().get(i + open.len()).copied().unwrap_or(b' ');
            if !(after == b'>' || after.is_ascii_whitespace()) {
                from = i + open.len();
                continue;
            }
            let Some(j) = lower[i..].find(&close).map(|j| j + i + close.len()) else { break };
            if best.map(|(a, b)| j - i > b - a).unwrap_or(true) {
                best = Some((i, j));
            }
            from = j;
        }
        best
    };
    let (a, b) = pick("article").filter(|(a, b)| b - a > 400).or_else(|| pick("main")).or_else(|| pick("body")).unwrap_or((0, html.len()));
    let mut s = html[a..b].to_string();
    for tag in ["script", "style", "noscript", "nav", "header", "footer", "aside", "form", "svg", "iframe", "button"] {
        let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 400 {
                break;
            }
            let low = s.to_lowercase();
            let Some(i) = low.find(&open).filter(|&i| {
                let c = low.as_bytes().get(i + open.len()).copied().unwrap_or(b' ');
                c == b'>' || c.is_ascii_whitespace() || c == b'/'
            }) else {
                break;
            };
            match low[i..].find(&close) {
                Some(j) => s.replace_range(i..i + j + close.len(), ""),
                None => match low[i..].find('>') {
                    Some(j) => s.replace_range(i..i + j + 1, ""),
                    None => break,
                },
            }
        }
    }
    s
}

/// 相对地址 → 绝对地址。
pub fn absolutize(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    if href.starts_with("data:") || href.is_empty() {
        return None;
    }
    let scheme_end = base.find("://")? + 3;
    let host_end = base[scheme_end..].find('/').map(|i| i + scheme_end).unwrap_or(base.len());
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{}{}", &base[..scheme_end], rest));
    }
    if href.starts_with('/') {
        return Some(format!("{}{}", &base[..host_end], href));
    }
    let dir_end = base[..base.find(['?', '#']).unwrap_or(base.len())].rfind('/').filter(|&i| i >= host_end).map(|i| i + 1).unwrap_or(host_end);
    let dir = if dir_end == host_end { format!("{}/", &base[..host_end]) } else { base[..dir_end].to_string() };
    Some(format!("{dir}{href}"))
}

/// 把 Markdown 里 `![..](地址)` 的远程图片换成 `kc-img://n` 占位，返回要下载的 (占位, 绝对地址)。
pub fn collect_images(md: &str, base: &str, limit: usize) -> (String, Vec<(String, String)>) {
    let mut out = String::with_capacity(md.len());
    let mut list: Vec<(String, String)> = Vec::new();
    let mut rest = md;
    while let Some(i) = rest.find("![") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(close) = tail.find("](") else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let Some(end) = tail[close + 2..].find(')') else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let url = tail[close + 2..close + 2 + end].split_whitespace().next().unwrap_or("").to_string();
        let alt = &tail[..close + 2];
        match absolutize(base, &url).filter(|_| list.len() < limit) {
            Some(abs) => {
                let tok = list.iter().find(|(_, u)| *u == abs).map(|(t, _)| t.clone()).unwrap_or_else(|| {
                    let t = format!("kc-img://{}", list.len());
                    list.push((t.clone(), abs.clone()));
                    t
                });
                out.push_str(alt);
                out.push_str(&tok);
                out.push(')');
            }
            None => out.push_str(&tail[..close + 2 + end + 1]),
        }
        rest = &tail[close + 2 + end + 1..];
    }
    out.push_str(rest);
    (out, list)
}

fn ext_of(url: &str, mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/avif" => "avif",
        _ => {
            let u = url.split(['?', '#']).next().unwrap_or("").to_lowercase();
            ["png", "jpg", "jpeg", "gif", "webp", "svg", "avif"].into_iter().find(|e| u.ends_with(&format!(".{e}"))).map(|e| if e == "jpeg" { "jpg" } else { e }).unwrap_or("img")
        }
    }
}

/// POST /field/localize {url, html?, title?}：把网页存成库里的一篇——结构化正文 + 本地图片。
/// html 由桌面端的隐藏窗口渲染后带来（JS 渲染的站点）；没带就由服务端抓取。
pub async fn localize(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("").trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return err(StatusCode::BAD_REQUEST, "网址要以 http:// 或 https:// 开头").into_response();
    }
    let given = req.get("html").and_then(|x| x.as_str()).filter(|s| !s.trim().is_empty()).map(String::from);
    let title_hint = req.get("title").and_then(|t| t.as_str()).map(String::from);
    let url2 = url.clone();
    let work = tokio::task::spawn_blocking(move || -> Result<(kernel_convert::Converted, usize, usize), String> {
        let html = match given {
            Some(h) => h,
            None => kernel_core::fetch_html(&url2, std::time::Duration::from_secs(30)).map_err(|e| format!("抓取失败：{e}"))?,
        };
        let page_title = {
            let lower = html.to_lowercase();
            lower.find("<title").and_then(|i| html[i..].find('>').map(|g| i + g + 1)).and_then(|i| html[i..].find("</title>").map(|e| html[i..i + e].trim().to_string()))
        };
        let body = main_content(&html);
        let mut conv = kernel_convert::convert("html", body.as_bytes())?;
        if conv.title.as_deref().map(|t| t.trim().is_empty()).unwrap_or(true) {
            conv.title = title_hint.clone().or(page_title).map(|t| t.chars().take(80).collect());
        }
        if let Some(t) = &title_hint {
            conv.title = Some(t.clone());
        }
        let (md, wanted) = collect_images(&conv.markdown, &url2, 40);
        conv.markdown = md;
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("Mozilla/5.0 (Litmus web localizer)")
            .build()
            .map_err(|e| e.to_string())?;
        let (mut ok, mut failed, mut total) = (0usize, 0usize, 0usize);
        for (tok, u) in &wanted {
            let got = client.get(u).header("referer", url2.as_str()).send().ok().filter(|r| r.status().is_success()).and_then(|r| {
                let mime = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").split(';').next().unwrap_or("").trim().to_lowercase();
                if !mime.starts_with("image/") && !mime.is_empty() && mime != "application/octet-stream" {
                    return None;
                }
                let bytes = r.bytes().ok()?;
                (bytes.len() <= 10 * 1024 * 1024).then(|| (mime, bytes.to_vec()))
            });
            match got.filter(|(_, b)| total + b.len() <= 60 * 1024 * 1024) {
                Some((mime, bytes)) => {
                    total += bytes.len();
                    let name = format!("web-{}.{}", &kernel_core::Cas::hash_hex(u.as_bytes())[..10], ext_of(u, &mime));
                    conv.images.push(kernel_convert::Image { token: tok.clone(), name, bytes });
                    ok += 1;
                }
                None => {
                    // 下不来的图保留原地址，别留空链接
                    conv.markdown = conv.markdown.replace(&format!("({tok})"), &format!("({u})"));
                    failed += 1;
                }
            }
        }
        Ok((conv, ok, failed))
    })
    .await;
    let (conv, images, failed) = match work {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return err(StatusCode::BAD_GATEWAY, e).into_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    if conv.markdown.trim().len() < 20 {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "没抽出正文：这个网页可能要登录，或内容由脚本生成（桌面 App 会先渲染再收录）").into_response();
    }
    let fallback = url.clone();
    let mut k = st.kernel.lock().unwrap();
    let full = materialize(&mut k, conv, &fallback);
    let title = full.lines().next().and_then(|l| l.strip_prefix("# ")).unwrap_or(&fallback).trim().to_string();
    let body_md = full.splitn(2, '\n').nth(1).unwrap_or("").trim_start().to_string();
    let uri = format!("web://{}", &kernel_core::Cas::hash_hex(url.as_bytes())[..16]);
    match k.import_capture("web", &uri, Some(&title), &body_md, "text/markdown") {
        Ok(rel) => {
            audit_log(&k.vault.sidecar(), "localize", &json!({ "url": url, "rel": rel, "images": images, "failed": failed }));
            Json(json!({ "title": title, "rel": rel, "images": images, "failed": failed, "bytes": body_md.len() })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_handles_all_shapes() {
        let b = "https://ex.com/a/b/page.html?x=1";
        assert_eq!(absolutize(b, "https://cdn.x/i.png").unwrap(), "https://cdn.x/i.png");
        assert_eq!(absolutize(b, "//cdn.x/i.png").unwrap(), "https://cdn.x/i.png");
        assert_eq!(absolutize(b, "/img/i.png").unwrap(), "https://ex.com/img/i.png");
        assert_eq!(absolutize(b, "i.png").unwrap(), "https://ex.com/a/b/i.png");
        assert_eq!(absolutize("https://ex.com", "i.png").unwrap(), "https://ex.com/i.png");
        assert!(absolutize(b, "data:image/png;base64,xx").is_none());
    }

    #[test]
    fn images_become_placeholders_once_each() {
        let (md, list) = collect_images("看 ![a](/x.png) 和 ![b](https://c/y.jpg \"t\") 再 ![c](/x.png)", "https://ex.com/p", 10);
        assert_eq!(list.len(), 2);
        assert_eq!(md, "看 ![a](kc-img://0) 和 ![b](kc-img://1) 再 ![c](kc-img://0)");
    }

    #[test]
    fn main_content_prefers_article_and_drops_chrome() {
        let html = format!("<html><body><nav>菜单 首页</nav><header>站名</header><article><h1>正文标题</h1><p>{}</p><aside>广告</aside></article><footer>版权</footer></body></html>", "内容。".repeat(200));
        let m = main_content(&html);
        assert!(m.contains("正文标题") && !m.contains("菜单") && !m.contains("广告") && !m.contains("版权"));
    }

    #[test]
    fn json_is_found_inside_chatter() {
        let v = json_in("好的：\n```json\n{\"items\":[{\"path\":\"Notes/a.md\",\"tags\":[\"x\"]}]}\n```").unwrap();
        assert_eq!(v["items"][0]["tags"][0], "x");
        assert!(json_in("没有 JSON").is_none());
    }
}
