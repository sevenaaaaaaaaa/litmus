//! 知识场（Field）的 HTTP 面（docs/spec/field.md §5）。
//!
//! - `GET  /field`          新壳页面（server/web/field.html）
//! - `GET  /field/data`     场：坐标 / 区域 / 关联线 / 每篇元信息（`?rebuild=1` 整体重排）
//! - `GET  /field/react?q=` 全库对一句话的相关分（显色用，不截断到 top-5）
//! - `POST /field/place`    一段文字该落在哪（随手记落位预告）
//! - `POST /field/crystal`  依据指定来源结晶答案（带 [n] 引用；没配模型时退化为原文摘录）
//! - `POST /field/note`     划选后的「解释给我听 / 考考我」
//!
//! 坐标是派生层：缓存在 `.thirdc/field.json`，删掉即可重建；增量更新时旧坐标不动。

use super::{AppState, SseEvent, check_token, err, extract_memo_tags};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::Json;
use kernel_core::field::{self, Field};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 每篇文档在场里的元信息（与 Field 内的下标一一对应）。
#[derive(Clone)]
pub struct DocMeta {
    pub path: String,
    pub title: String,
    pub kind: &'static str,
    pub date: String,
    pub excerpt: String,
    /// 文件修改时间（unix 秒）：「最近」列表用
    pub mtime: i64,
    /// 标签（frontmatter tags 与正文 #标签，与经典壳同一规则）
    pub tags: Vec<String>,
}

pub struct Cached {
    pub epoch: u64,
    pub field: Field,
    pub meta: Vec<DocMeta>,
    pub body: Bytes,
}

/// 按库根缓存（同一进程里的测试会开多个库）。
fn cache() -> &'static Mutex<HashMap<PathBuf, Arc<Cached>>> {
    static C: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<Cached>>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}
fn build_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 62 款阅读风格（与经典壳同一份参数），按需加载。
pub async fn reading_styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=3600")],
        include_str!("../web/reading-styles.js"),
    )
}

pub async fn page() -> impl IntoResponse {
    static PAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let html = PAGE.get_or_init(|| {
        include_str!("../web/field.html").replace(
            "<link rel=\"stylesheet\" href=\"assets/tokens.css\">",
            &format!(
                "<style>{}</style>",
                include_str!("../web/tokens.css").replace("url(\"fonts/", "url(\"/assets/fonts/")
            ),
        )
    });
    ([(header::CACHE_CONTROL, "no-cache, must-revalidate")], axum::response::Html(html.as_str()))
}

fn kind_of(path: &str) -> &'static str {
    if path.starts_with("Notes/Memos/") {
        "memo"
    } else if path.starts_with("Notes/答案/") {
        "answer"
    } else if path.starts_with("Notes/Sources/") || path.starts_with("Notes/Clippings/") || path.starts_with("Notes/Web/") {
        "source"
    } else if path.ends_with(".html") || path.ends_with(".htm") {
        "html"
    } else {
        "doc"
    }
}

/// 标题：一级标题 > frontmatter title > 随手记取首句 > 文件名。
fn title_of(path: &str, text: &str) -> String {
    if path.starts_with("Notes/Memos/") && !text.lines().any(|l| l.trim_start().starts_with("# ")) {
        let first = excerpt_of(text);
        // 随手记正文以「HH:MM 」开头（经典壳与知识场同一格式），标题里去掉时间
        let b = first.as_bytes();
        let body = if b.len() > 6 && b[2] == b':' && b[5] == b' ' && b[..2].iter().chain(&b[3..5]).all(|c| c.is_ascii_digit()) { &first[6..] } else { first.as_str() };
        let t: String = body.chars().take(24).collect();
        if !t.is_empty() {
            return t;
        }
    }
    let mut in_fm = false;
    for (i, l) in text.lines().enumerate() {
        let t = l.trim();
        if i == 0 && t == "---" {
            in_fm = true;
            continue;
        }
        if in_fm {
            if t == "---" {
                in_fm = false;
            } else if let Some(v) = t.strip_prefix("title:") {
                let v = v.trim().trim_matches('"').trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
            continue;
        }
        if let Some(h) = t.strip_prefix("# ") {
            return h.trim().to_string();
        }
        if !t.is_empty() && i > 40 {
            break;
        }
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let base = name.trim_end_matches(".md").trim_end_matches(".html").trim_end_matches(".htm");
    base.to_string()
}

/// 日期：frontmatter `created:` / `date:` 的前 10 位，否则文件修改时间。
fn date_of(text: &str, mtime: i64) -> String {
    for l in text.lines().take(30) {
        let t = l.trim();
        for k in ["created:", "date:"] {
            if let Some(v) = t.strip_prefix(k) {
                let v = v.trim().trim_matches('"');
                if v.len() >= 10 && v.as_bytes()[4] == b'-' && v.as_bytes()[7] == b'-' {
                    return v[..10].to_string();
                }
            }
        }
    }
    ymd(mtime)
}

/// unix 秒 → YYYY-MM-DD（UTC+8，与用户所在时区一致的"哪一天"）。
pub fn ymd(secs: i64) -> String {
    let days = (secs + 8 * 3600).div_euclid(86400);
    // Howard Hinnant 的 civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

fn excerpt_of(text: &str) -> String {
    let mut in_fm = false;
    for (i, l) in text.lines().enumerate() {
        let t = l.trim();
        if i == 0 && t == "---" {
            in_fm = true;
            continue;
        }
        if in_fm {
            if t == "---" {
                in_fm = false;
            }
            continue;
        }
        if t.is_empty() || t.starts_with('#') || t.starts_with("```") || t.starts_with('|') || t == "---" || t.starts_with('<') {
            continue;
        }
        let s: String = t.trim_start_matches(['-', '*', '>', ' ']).replace("**", "").replace('`', "");
        return s.chars().take(120).collect();
    }
    String::new()
}

fn field_json_path(root: &std::path::Path) -> PathBuf {
    root.join(".thirdc").join("field.json")
}

fn load_prev(root: &std::path::Path) -> HashMap<String, (f32, f32)> {
    let Ok(s) = std::fs::read_to_string(field_json_path(root)) else { return HashMap::new() };
    let Ok(v) = serde_json::from_str::<Value>(&s) else { return HashMap::new() };
    v.get("pos")
        .and_then(|p| p.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| {
                    let a = v.as_array()?;
                    Some((k.clone(), (a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32)))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 取（必要时重建）当前库的场。
pub async fn current(st: &Arc<AppState>, rebuild: bool) -> Result<Arc<Cached>, String> {
    let (root, epoch) = {
        let mut k = st.kernel.lock().unwrap();
        let _ = k.sync_throttled();
        (k.vault.root.clone(), k.data_epoch())
    };
    if !rebuild {
        if let Some(c) = cache().lock().unwrap().get(&root) {
            if c.epoch == epoch {
                return Ok(c.clone());
            }
        }
    }
    let _guard = build_lock().lock().await;
    // 排队期间别人可能已经建好
    if !rebuild {
        if let Some(c) = cache().lock().unwrap().get(&root) {
            if c.epoch == epoch {
                return Ok(c.clone());
            }
        }
    }
    let (corpus, stats) = {
        let k = st.kernel.lock().unwrap();
        let (_, corpus) = k.hybrid_snapshot().map_err(|e| e.to_string())?;
        (corpus, k.doc_stats().unwrap_or_default())
    };
    let root2 = root.clone();
    let built = tokio::task::spawn_blocking(move || {
        let prev = load_prev(&root2);
        let docs: Vec<(String, String, String)> = corpus
            .into_iter()
            .map(|(p, text)| {
                let t = title_of(&p, &text);
                (p, t, text)
            })
            .collect();
        let texts: HashMap<String, (String, String)> = docs.iter().map(|(p, t, x)| (p.clone(), (t.clone(), x.clone()))).collect();
        let f = field::build(&docs, &prev, rebuild);
        let meta: Vec<DocMeta> = f
            .vectors
            .paths
            .iter()
            .map(|p| {
                let (title, text) = &texts[p];
                let mtime = stats.get(p).map(|s| s.0).unwrap_or(0);
                DocMeta { path: p.clone(), title: title.clone(), kind: kind_of(p), date: date_of(text, mtime), excerpt: excerpt_of(text), mtime, tags: extract_memo_tags(&text.chars().take(4000).collect::<String>()) }
            })
            .collect();
        // 派生层落盘：只存坐标（可删可重建）
        let pos: serde_json::Map<String, Value> = f
            .vectors
            .paths
            .iter()
            .zip(f.pos.iter())
            .map(|(p, (x, y))| (p.clone(), json!([round1(*x), round1(*y)])))
            .collect();
        let _ = std::fs::create_dir_all(root2.join(".thirdc"));
        let _ = std::fs::write(field_json_path(&root2), serde_json::to_vec(&json!({"v": 1, "pos": pos})).unwrap_or_default());
        let mut count = vec![0usize; f.region_names.len()];
        for &r in &f.region {
            count[r] += 1;
        }
        let links = links_of(&f.vectors.paths, &meta, &texts);
        let body = json!({
            "epoch": epoch,
            "links": links.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "regions": f.region_names.iter().zip(count.iter()).map(|(n, c)| json!({"name": n, "count": c})).collect::<Vec<_>>(),
            "docs": meta.iter().enumerate().map(|(i, m)| json!({
                "p": m.path, "t": m.title, "k": m.kind, "d": m.date, "ex": m.excerpt,
                "x": round1(f.pos[i].0), "y": round1(f.pos[i].1), "r": f.region[i], "m": m.mtime, "g": m.tags,
            })).collect::<Vec<_>>(),
            "edges": f.edges.iter().map(|(a, b, w)| json!([a, b, (w * 1000.0).round() / 1000.0])).collect::<Vec<_>>(),
        });
        let body = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
        Cached { epoch, field: f, meta, body }
    })
    .await
    .map_err(|e| e.to_string())?;
    let built = Arc::new(built);
    cache().lock().unwrap().insert(root, built.clone());
    Ok(built)
}

/// 文档之间真实写下的链接：`[[目标]]`（可带 |别名 #标题 ^块）与 `](Notes/...)`。
/// 目标按 精确路径 → 补 .md → 文件名 → 标题 依次解析；解析不到的不画线。返回去重后的 (源, 目标) 下标。
pub fn links_of(paths: &[String], meta: &[DocMeta], texts: &HashMap<String, (String, String)>) -> Vec<(usize, usize)> {
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (i, p) in paths.iter().enumerate() {
        by_key.entry(p.to_lowercase()).or_insert(i);
        let name = p.rsplit('/').next().unwrap_or(p);
        let stem = name.trim_end_matches(".md").trim_end_matches(".html").trim_end_matches(".htm");
        by_key.entry(stem.to_lowercase()).or_insert(i);
    }
    for (i, m) in meta.iter().enumerate() {
        by_key.entry(m.title.to_lowercase()).or_insert(i);
    }
    let resolve = |t: &str| -> Option<usize> {
        let t = t.trim().trim_start_matches("./");
        let l = t.to_lowercase();
        by_key
            .get(&l)
            .or_else(|| by_key.get(&format!("{l}.md")))
            .or_else(|| by_key.get(&format!("notes/{l}")))
            .or_else(|| by_key.get(&format!("notes/{l}.md")))
            .copied()
    };
    let mut seen = std::collections::BTreeSet::new();
    for (i, p) in paths.iter().enumerate() {
        let Some((_, text)) = texts.get(p) else { continue };
        let mut rest = text.as_str();
        while let Some(a) = rest.find("[[") {
            let after = &rest[a + 2..];
            let Some(b) = after.find("]]") else { break };
            let raw = &after[..b];
            if !raw.contains('\n') {
                let name = raw.split(['|', '#', '^']).next().unwrap_or("");
                if let Some(j) = resolve(name) {
                    if j != i {
                        seen.insert((i, j));
                    }
                }
            }
            rest = &after[b + 2..];
        }
        let mut rest = text.as_str();
        while let Some(a) = rest.find("](") {
            let after = &rest[a + 2..];
            let Some(b) = after.find(')') else { break };
            let target = after[..b].split('#').next().unwrap_or("");
            if target.starts_with("Notes/") || target.ends_with(".md") {
                let dec = target.replace("%20", " ");
                if let Some(j) = resolve(&dec) {
                    if j != i {
                        seen.insert((i, j));
                    }
                }
            }
            rest = &after[b + 1..];
        }
    }
    seen.into_iter().collect()
}

// ───────── Agent：让它去做，过程画在场上 ─────────

/// 工具参数里只回传「指向哪里」的字段，正文不回传（可能很长，也不需要画）。
fn step_args(v: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(o) = v.as_object() {
        for k in ["path", "query", "title", "name", "from", "to", "q", "topic"] {
            if let Some(x) = o.get(k) {
                if let Some(s) = x.as_str() {
                    out.insert(k.to_string(), json!(s.chars().take(200).collect::<String>()));
                }
            }
        }
    }
    Value::Object(out)
}

/// `POST /field/agent {message, context?}` → SSE：`step`（工具、指向、成败、摘要）… `reply` → `done`。
/// 配了模型走带工具的 Agent；没配走离线命令模式。对话照常记进 Agent 记忆（与经典壳一致）。
pub async fn agent(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    use axum::response::sse::Sse;
    use futures_util::stream;
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let message = req.get("message").and_then(|m| m.as_str()).unwrap_or("").trim().to_string();
    if message.is_empty() {
        return err(StatusCode::BAD_REQUEST, "说说要它做什么").into_response();
    }
    let context = req.get("context").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
    let server = st.mcp.clone();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();
    let t0 = std::time::Instant::now();
    tokio::spawn(async move {
        let ev = |name: &str, data: Value| -> SseEvent { Ok(axum::response::sse::Event::default().event(name).data(data.to_string())) };
        let full = if context.is_empty() { message.clone() } else { format!("{message}\n\n（我正在看：{context}）") };
        let ai_on = ai.as_ref().map(|c| !c.base_url.is_empty()).unwrap_or(false);
        let _ = tx.send(ev("start", json!({ "mode": if ai_on { "ai" } else { "command" } })));
        let result = match ai {
            Some(cfg) if !cfg.base_url.is_empty() => {
                let tx_step = tx.clone();
                thirdc_mcp::agent::run_ai_with(&server, &cfg, &full, move |s| {
                    let _ = tx_step.send(ev("step", json!({ "tool": s.tool, "args": step_args(&s.args), "ok": s.ok, "summary": s.summary.chars().take(300).collect::<String>() })));
                })
                .await
            }
            _ => {
                let out = thirdc_mcp::agent::command_mode(&server, &full);
                for s in &out.steps {
                    let _ = tx.send(ev("step", json!({ "tool": s.tool, "args": step_args(&s.args), "ok": s.ok, "summary": s.summary.chars().take(300).collect::<String>() })));
                }
                Ok(out)
            }
        };
        {
            let today = kernel_core::today_string();
            if let Ok(mut k) = server.kernel.lock() {
                let _ = k.append_conversation(&today, "我", &message);
                if let Ok(out) = &result {
                    let _ = k.append_conversation(&today, "studio", &out.reply);
                }
            }
        }
        match result {
            Ok(out) => {
                let _ = tx.send(ev("reply", json!({ "mode": out.mode, "text": out.reply })));
                let _ = tx.send(ev("done", json!({ "ok": true, "tokens": out.tokens, "ms": t0.elapsed().as_millis() as u64 })));
            }
            Err(e) => {
                let _ = tx.send(ev("error", json!({ "message": e.to_string() })));
                let _ = tx.send(ev("done", json!({ "ok": false, "ms": t0.elapsed().as_millis() as u64 })));
            }
        }
    });
    let out = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    Sse::new(out).into_response()
}

// ───────── 模型设置：只改 thirdc.toml 的 [ai] 表 ─────────

/// 当前模型配置（密钥只回是否已设，不回明文）。
pub async fn ai_get(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone().unwrap_or_default();
    Json(json!({
        "configured": !ai.base_url.is_empty() && !ai.model.is_empty(),
        "base_url": ai.base_url,
        "model": ai.model,
        "has_key": !ai.api_key.is_empty(),
        "key_from_env": ai.api_key.is_empty() && std::env::var("THIRDC_AI_API_KEY").map(|v| !v.is_empty()).unwrap_or(false),
    }))
    .into_response()
}

/// 把 [ai] 的 base_url / model / api_key 写回 thirdc.toml。
/// 用 toml_edit 原地改：用户手写的注释、其他表与排版都保留；api_key 传空串表示「不改」。
pub fn write_ai_config(cfg_path: &std::path::Path, base_url: &str, model: &str, api_key: Option<&str>) -> Result<(), String> {
    let raw = std::fs::read_to_string(cfg_path).map_err(|e| format!("读不到 thirdc.toml：{e}"))?;
    let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| format!("thirdc.toml 格式有误：{e}"))?;
    if !doc.contains_key("ai") {
        doc["ai"] = toml_edit::table();
    }
    let t = doc["ai"].as_table_mut().ok_or("thirdc.toml 里的 ai 不是一个表")?;
    t["base_url"] = toml_edit::value(base_url);
    t["model"] = toml_edit::value(model);
    if let Some(k) = api_key {
        t["api_key"] = toml_edit::value(k);
    } else if !t.contains_key("api_key") {
        t["api_key"] = toml_edit::value("");
    }
    let tmp = cfg_path.with_extension("tmp");
    std::fs::write(&tmp, doc.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, cfg_path).map_err(|e| e.to_string())
}

pub async fn ai_put(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let base_url = req.get("base_url").and_then(|v| v.as_str()).unwrap_or("").trim().trim_end_matches('/').to_string();
    let model = req.get("model").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let key = req.get("api_key").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return err(StatusCode::BAD_REQUEST, "接口地址要以 http:// 或 https:// 开头，例如 https://api.deepseek.com/v1").into_response();
    }
    if model.is_empty() {
        return err(StatusCode::BAD_REQUEST, "请填写模型名，例如 deepseek-chat").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    let cfg_path = k.vault.root.join("thirdc.toml");
    if let Err(e) = write_ai_config(&cfg_path, &base_url, &model, key.as_deref()) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let mut ai = k.vault.config.ai.clone().unwrap_or_default();
    ai.base_url = base_url;
    ai.model = model;
    if let Some(key) = key {
        ai.api_key = key;
    }
    k.vault.config.ai = Some(ai);
    Json(json!({ "ok": true })).into_response()
}

/// 用当前配置发一句最短的话，确认地址、模型、密钥都对。
pub async fn ai_test(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
    let Some(cfg) = ai.filter(|c| !c.base_url.is_empty()) else {
        return err(StatusCode::CONFLICT, "还没有配置模型").into_response();
    };
    let t0 = std::time::Instant::now();
    match thirdc_mcp::agent::complete(&cfg, "只回复两个字：好的", "测试").await {
        Ok(t) => Json(json!({ "ok": true, "reply": t.chars().take(40).collect::<String>(), "ms": t0.elapsed().as_millis() as u64 })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("连不上：{e}")).into_response(),
    }
}

fn round1(x: f32) -> f64 {
    (x as f64 * 10.0).round() / 10.0
}

pub async fn data(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let rebuild = q.get("rebuild").map(|v| v == "1").unwrap_or(false);
    match current(&st, rebuild).await {
        Ok(c) => ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], c.body.clone()).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 一句话对全库的相关分：语义（场向量余弦）× 0.65 + 字面（FTS 排名）× 0.35，再按最大值归一。
pub async fn react(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let text = q.get("q").cloned().unwrap_or_default();
    let c = match current(&st, false).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    // near=路径：以这篇文档自己的向量去显色（阅读时让近邻亮起来）
    if let Some(p) = q.get("near") {
        return match c.meta.iter().position(|m| &m.path == p) {
            Some(i) => Json(score_vec(&c, &c.field.vectors.vecs[i].clone(), Some(i))).into_response(),
            None => err(StatusCode::NOT_FOUND, "场里没有这篇文档").into_response(),
        };
    }
    let fts: Vec<(String, f64)> = if text.chars().count() >= 2 {
        st.kernel.lock().unwrap().search(&text).unwrap_or_default()
    } else {
        Vec::new()
    };
    Json(score(&c, &text, &fts)).into_response()
}

/// 用一篇文档的向量给全库打分（不含它自己）。
pub fn score_vec(c: &Cached, qv: &field::SparseVec, skip: Option<usize>) -> Value {
    let mut hits: Vec<(usize, f32)> = c.field.vectors.vecs.iter().enumerate().filter(|(i, _)| Some(*i) != skip).map(|(i, v)| (i, qv.dot(v))).filter(|x| x.1 > 0.0).collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    hits.truncate(40);
    let mx = hits.first().map(|h| h.1).unwrap_or(1.0).max(1e-6);
    json!({
        "epoch": c.epoch,
        "strength": (mx * 100.0).round() / 100.0,
        "hits": hits.iter().map(|(i, v)| json!([i, ((v / mx) * 1000.0).round() / 1000.0])).collect::<Vec<_>>(),
    })
}

pub fn score(c: &Cached, text: &str, fts: &[(String, f64)]) -> Value {
    let qv = c.field.vectors.embed(text);
    let n = c.meta.len();
    let mut s = vec![0.0f32; n];
    let mut cos_max = 0.0f32;
    if !qv.0.is_empty() {
        for (i, v) in c.field.vectors.vecs.iter().enumerate() {
            let d = qv.dot(v);
            s[i] = d;
            cos_max = cos_max.max(d);
        }
        if cos_max > 0.0 {
            for x in s.iter_mut() {
                *x = *x / cos_max * 0.65;
            }
        }
    }
    let idx: HashMap<&str, usize> = c.meta.iter().enumerate().map(|(i, m)| (m.path.as_str(), i)).collect();
    for (rank, (p, _)) in fts.iter().take(200).enumerate() {
        if let Some(&i) = idx.get(p.as_str()) {
            s[i] += 0.35 / (1.0 + rank as f32 * 0.12);
        }
    }
    // 标题直接包含整句：再加一把（用户常常就是在找那篇）
    let tl = text.trim().to_lowercase();
    if tl.chars().count() >= 2 {
        for (i, m) in c.meta.iter().enumerate() {
            if m.title.to_lowercase().contains(&tl) {
                s[i] += 0.3;
            }
        }
    }
    let mx = s.iter().cloned().fold(0.0f32, f32::max);
    let mut hits: Vec<(usize, f32)> = s.iter().enumerate().filter(|(_, &v)| v > 0.0).map(|(i, &v)| (i, v / mx.max(1e-6))).filter(|x| x.1 >= 0.05).collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    hits.truncate(400);
    json!({
        "epoch": c.epoch,
        "strength": (cos_max * 100.0).round() / 100.0,
        "hits": hits.iter().map(|(i, v)| json!([i, (v * 1000.0).round() / 1000.0])).collect::<Vec<_>>(),
    })
}

pub async fn place(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let text = req.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let c = match current(&st, false).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match field::place(&c.field, &text) {
        Some((x, y, nb)) => Json(json!({
            "x": round1(x), "y": round1(y),
            "r": c.field.region[nb[0].0],
            "near": nb.iter().map(|(i, s)| json!({"i": i, "s": (s * 1000.0).round() / 1000.0})).collect::<Vec<_>>(),
        }))
        .into_response(),
        None => Json(json!({ "x": null, "y": null, "r": null, "near": [] })).into_response(),
    }
}

/// 从一篇正文里挑出和问题最相关的段落（保持原顺序），供模型引用。
pub fn passages(text: &str, q: &str, max: usize) -> String {
    let qt: Vec<String> = field::tokens(q);
    let paras: Vec<&str> = text.split("\n\n").map(|p| p.trim()).filter(|p| p.chars().count() > 12 && !p.starts_with("```") && !p.starts_with("---")).collect();
    let mut scored: Vec<(usize, usize, &str)> = paras
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let l = p.to_lowercase();
            (i, qt.iter().filter(|t| l.contains(t.as_str())).count(), *p)
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut top: Vec<(usize, usize, &str)> = scored.into_iter().filter(|x| x.1 > 0).take(3).collect();
    if top.is_empty() {
        top = paras.iter().enumerate().take(2).map(|(i, p)| (i, 0, *p)).collect();
    }
    top.sort_by_key(|x| x.0);
    let joined = top.iter().map(|x| x.2.split_whitespace().collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n");
    joined.chars().take(max).collect()
}

/// 没有模型时的退路：每篇来源里与问题重合最多的一句。
pub fn best_sentence(text: &str, q: &str) -> String {
    let qt = field::tokens(q);
    let body: String = text
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with('#') && !t.starts_with("```") && !t.starts_with('|') && !t.starts_with("---") && !t.starts_with("created:") && !t.starts_with("tags:")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut best = (0usize, String::new());
    for s in body.split(['。', '！', '？', '\n', '；']) {
        let s = s.trim().trim_start_matches(['-', '*', '>', ' ']).replace("**", "").replace('`', "");
        // 表格线、框图字符、链接残片不算句子
        if s.chars().any(|c| matches!(c, '│' | '┌' | '└' | '├' | '─' | '┐' | '┘' | '┤' | '|')) || s.starts_with("http") {
            continue;
        }
        let n = s.chars().count();
        if !(8..=160).contains(&n) {
            continue;
        }
        let l = s.to_lowercase();
        let hit = qt.iter().filter(|t| l.contains(t.as_str())).map(|t| t.chars().count()).sum::<usize>();
        if hit > best.0 || best.1.is_empty() {
            best = (hit, s.clone());
        }
    }
    best.1
}

pub async fn crystal(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let q = req.get("q").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let paths: Vec<String> = req
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|p| kernel_core::is_safe_doc_path(p)).take(6).collect())
        .unwrap_or_default();
    if q.is_empty() || paths.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要问题 q 和至少一篇来源 paths").into_response();
    }
    let (root, ai) = {
        let k = st.kernel.lock().unwrap();
        (k.vault.root.clone(), k.vault.config.ai.clone())
    };
    let sources: Vec<(String, String, String)> = paths
        .iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(root.join(p)).ok()?;
            Some((p.clone(), title_of(p, &text), text))
        })
        .collect();
    if sources.is_empty() {
        return err(StatusCode::NOT_FOUND, "来源文档都读不到了，刷新一下场再试").into_response();
    }
    let refs: Vec<Value> = sources.iter().map(|(p, t, _)| json!({"path": p, "title": t})).collect();
    match ai {
        Some(cfg) if !cfg.base_url.is_empty() => {
            let ctx: Vec<String> = sources
                .iter()
                .enumerate()
                .map(|(i, (p, t, text))| format!("[{}] 《{}》（{}）\n{}", i + 1, t, p, passages(text, &q, 900)))
                .collect();
            let system = "你在回答用户个人知识库里的一个问题。只依据用户给出的编号资料回答，用中文，120 到 220 字，可以分 2 到 4 个短段或要点。\
                每个论断后用 [编号] 标出处，例如 [2]。资料不够回答时，直说缺什么。不要写开场白和总结套话。";
            let user = format!("问题：{q}\n\n资料：\n{}", ctx.join("\n\n"));
            match thirdc_mcp::agent::complete(&cfg, system, &user).await {
                Ok(text) if !text.trim().is_empty() => Json(json!({ "mode": "ai", "text": text.trim(), "sources": refs })).into_response(),
                Ok(_) => Json(json!({ "mode": "extract", "note": "模型返回了空答案，下面是原文里最相关的句子。", "text": extract(&sources, &q), "sources": refs })).into_response(),
                Err(e) => Json(json!({ "mode": "extract", "note": format!("这次没有连上模型（{e}），下面是原文里最相关的句子。"), "text": extract(&sources, &q), "sources": refs })).into_response(),
            }
        }
        _ => Json(json!({
            "mode": "extract",
            "note": "还没有配置模型，下面是原文里最相关的句子。在设置里填上模型后，这里会写出完整答案。",
            "text": extract(&sources, &q),
            "sources": refs,
        }))
        .into_response(),
    }
}

fn extract(sources: &[(String, String, String)], q: &str) -> String {
    sources
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, (_, t, text))| format!("「{t}」里写：{} [{}]", best_sentence(text, q), i + 1))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub async fn note(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let task = req.get("task").and_then(|v| v.as_str()).unwrap_or("explain");
    let text: String = req.get("text").and_then(|v| v.as_str()).unwrap_or("").chars().take(1500).collect();
    let title = req.get("title").and_then(|v| v.as_str()).unwrap_or("");
    if text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "先划选一段文字").into_response();
    }
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
    let Some(cfg) = ai.filter(|c| !c.base_url.is_empty()) else {
        return err(StatusCode::CONFLICT, "还没有配置模型：在设置 → AI 里填上接口地址与模型后就能用").into_response();
    };
    let (system, user) = if task == "quiz" {
        (
            "你帮用户检验自己是否真正读懂了一段文字。出一道能检验理解的问题，不给答案，再给一句提示。只输出两行：\n问题：……\n提示：……",
            format!("出自文档《{title}》：\n{text}"),
        )
    } else {
        (
            "用三到五句中文，向一个没读过这篇文档的人解释用户划选的这段话在说什么、为什么重要。只输出解释本身。",
            format!("文档《{title}》里的段落：\n{text}"),
        )
    };
    match thirdc_mcp::agent::complete(&cfg, system, &user).await {
        Ok(t) => Json(json!({ "text": t.trim() })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("这次没有连上模型：{e}")).into_response(),
    }
}

#[cfg(test)]
mod api_tests {
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    fn app() -> (axum::Router, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = kernel_core::Vault::init(dir.path(), "field-test").unwrap();
        let state = crate::build_state(vault).unwrap();
        let token = state.token.clone();
        (crate::router(state), token, dir)
    }

    async fn call(app: &axum::Router, token: &str, method: &str, uri: &str, body: &str) -> (u16, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", if method == "PUT" { "text/markdown" } else { "application/json" })
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn seed(app: &axum::Router, token: &str) {
        let docs = [
            ("Notes/同步/协议.md", "# 同步协议\n\n离线 同步 冲突 合并 CRDT 设备 并发 op-log。冲突时两边都保留。"),
            ("Notes/同步/冲突.md", "# 冲突处理\n\n同步 冲突 合并 两边保留 不静默覆盖 CRDT。"),
            ("Notes/同步/设备.md", "# 多设备\n\n设备 同步 离线 合并 CRDT 冲突 op-log。"),
            ("Notes/发布/站点.md", "# 发布站点\n\n发布 站点 部署 线上 构建 静态 域名。"),
            ("Notes/发布/部署.md", "# 部署流程\n\n部署 发布 线上 服务器 构建 静态 上传。"),
            ("Notes/发布/域名.md", "# 域名与证书\n\n域名 发布 线上 部署 证书 站点。"),
        ];
        for (p, t) in docs {
            let (st, _) = call(app, token, "PUT", &format!("/doc?path={}", urlenc(p)), t).await;
            assert_eq!(st, 200, "写入 {p} 失败");
        }
    }

    fn urlenc(s: &str) -> String {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/._-".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    }

    #[tokio::test]
    async fn field_page_is_served_without_token() {
        let (app, _t, _d) = app();
        let resp = app.oneshot(Request::builder().uri("/field").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("知识场"));
        assert!(!html.contains("href=\"assets/tokens.css\""), "tokens.css 应内联");
    }

    #[tokio::test]
    async fn data_react_place_and_crystal_work_end_to_end() {
        let (app, token, dir) = app();
        seed(&app, &token).await;
        let (st, data) = call(&app, &token, "GET", "/field/data", "").await;
        assert_eq!(st, 200);
        let docs = data["docs"].as_array().unwrap();
        assert_eq!(docs.len(), 6);
        assert!(!data["regions"].as_array().unwrap().is_empty());
        assert!(dir.path().join(".thirdc/field.json").exists(), "坐标应落到派生层文件");
        let idx = |p: &str| docs.iter().position(|d| d["p"] == p).unwrap();

        // 显色：问同步，同步三篇排在发布三篇前面
        let (st, r) = call(&app, &token, "GET", &format!("/field/react?q={}", urlenc("同步冲突怎么合并")), "").await;
        assert_eq!(st, 200);
        let hits: Vec<usize> = r["hits"].as_array().unwrap().iter().map(|h| h[0].as_u64().unwrap() as usize).collect();
        assert!(hits.len() >= 3);
        for top in &hits[..3] {
            assert!(docs[*top]["p"].as_str().unwrap().starts_with("Notes/同步/"), "前三应是同步：{:?}", docs[*top]["p"]);
        }

        // 近邻：以一篇文档自己的向量显色，不含它自己
        let (_, r) = call(&app, &token, "GET", &format!("/field/react?near={}", urlenc("Notes/发布/站点.md")), "").await;
        let first = r["hits"][0][0].as_u64().unwrap() as usize;
        assert_ne!(first, idx("Notes/发布/站点.md"));
        assert!(docs[first]["p"].as_str().unwrap().starts_with("Notes/发布/"));

        // 落位预告：一句关于部署的话应落在发布一侧
        let (_, p) = call(&app, &token, "POST", "/field/place", r#"{"text":"线上部署又失败了"}"#).await;
        let near = p["near"][0]["i"].as_u64().unwrap() as usize;
        assert!(docs[near]["p"].as_str().unwrap().starts_with("Notes/发布/"));

        // 结晶：没配模型时退化为原文摘录，并带 [n] 引用
        let (st, c) = call(&app, &token, "POST", "/field/crystal", r#"{"q":"冲突怎么处理","paths":["Notes/同步/冲突.md","Notes/同步/协议.md"]}"#).await;
        assert_eq!(st, 200);
        assert_eq!(c["mode"], "extract");
        assert!(c["text"].as_str().unwrap().contains("[1]"));

        // 解释：没配模型 → 409 并说明怎么办
        let (st, n) = call(&app, &token, "POST", "/field/note", r#"{"task":"explain","text":"冲突时两边都保留","title":"冲突处理"}"#).await;
        assert_eq!(st, 409);
        assert!(n["error"].as_str().unwrap().contains("模型"));
    }

    #[tokio::test]
    async fn new_document_keeps_everyone_elses_coordinates() {
        let (app, token, _dir) = app();
        seed(&app, &token).await;
        let (_, a) = call(&app, &token, "GET", "/field/data", "").await;
        let (st, _) = call(&app, &token, "PUT", &format!("/doc?path={}", urlenc("Notes/同步/新.md")), "# 新想法\n\n同步 冲突 合并").await;
        assert_eq!(st, 200);
        let (_, b) = call(&app, &token, "GET", "/field/data", "").await;
        let pos = |v: &Value| -> std::collections::HashMap<String, (f64, f64)> {
            v["docs"].as_array().unwrap().iter().map(|d| (d["p"].as_str().unwrap().to_string(), (d["x"].as_f64().unwrap(), d["y"].as_f64().unwrap()))).collect()
        };
        let (pa, pb) = (pos(&a), pos(&b));
        assert_eq!(pb.len(), 7);
        for (p, xy) in &pa {
            assert_eq!(pb[p], *xy, "{p} 被挪动了");
        }
    }

    #[tokio::test]
    async fn links_and_ai_settings() {
        let (app, token, dir) = app();
        seed(&app, &token).await;
        let (st, _) = call(&app, &token, "PUT", &format!("/doc?path={}", urlenc("Notes/索引.md")), "# 索引\n\n见 [[冲突处理]] 与 [[Notes/发布/部署.md|部署]]，还有 [站点](Notes/发布/站点.md)。").await;
        assert_eq!(st, 200);
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let docs = d["docs"].as_array().unwrap();
        let idx = |p: &str| docs.iter().position(|x| x["p"] == p).unwrap() as u64;
        let links: Vec<(u64, u64)> = d["links"].as_array().unwrap().iter().map(|l| (l[0].as_u64().unwrap(), l[1].as_u64().unwrap())).collect();
        let src = idx("Notes/索引.md");
        for t in ["Notes/同步/冲突.md", "Notes/发布/部署.md", "Notes/发布/站点.md"] {
            assert!(links.contains(&(src, idx(t))), "缺链接 → {t}：{links:?}");
        }
        assert!(docs[0]["m"].as_i64().unwrap() > 0);

        // 模型设置：写回 thirdc.toml 只动 [ai]，原有内容与注释保留
        let cfg = dir.path().join("thirdc.toml");
        let before = std::fs::read_to_string(&cfg).unwrap();
        std::fs::write(&cfg, format!("# 我的注释\n{before}")).unwrap();
        let (st, _) = call(&app, &token, "POST", "/field/ai", r#"{"base_url":"ftp://x","model":"m"}"#).await;
        assert_eq!(st, 400);
        let (st, _) = call(&app, &token, "POST", "/field/ai", r#"{"base_url":"https://api.example.com/v1/","model":"demo-chat","api_key":"sk-test"}"#).await;
        assert_eq!(st, 200);
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.starts_with("# 我的注释"), "注释被抹掉了");
        assert!(after.contains("[ai]") && after.contains("demo-chat") && after.contains("https://api.example.com/v1\""));
        let (_, g) = call(&app, &token, "GET", "/field/ai", "").await;
        assert_eq!(g["configured"], true);
        assert_eq!(g["has_key"], true);
        assert!(g.get("api_key").is_none(), "不回明文密钥");
        // 再存一次不带密钥：原密钥保留
        let (_, _) = call(&app, &token, "POST", "/field/ai", r#"{"base_url":"https://api.example.com/v1","model":"demo-2"}"#).await;
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("sk-test"));
        // 重新打开库：配置能被内核读回
        let v = kernel_core::Vault::open(dir.path()).unwrap();
        assert_eq!(v.config.ai.unwrap().model, "demo-2");
    }

    #[tokio::test]
    async fn agent_streams_steps_in_command_mode_and_tags_come_through() {
        let (app, token, _dir) = app();
        seed(&app, &token).await;
        let (st, _) = call(&app, &token, "PUT", &format!("/doc?path={}", urlenc("Notes/带标签.md")), "---\ntags: [周报]\n---\n# 带标签\n\n正文 #复盘").await;
        assert_eq!(st, 200);
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let doc = d["docs"].as_array().unwrap().iter().find(|x| x["p"] == "Notes/带标签.md").unwrap().clone();
        let tags: Vec<String> = doc["g"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
        assert!(tags.contains(&"周报".to_string()) && tags.contains(&"复盘".to_string()), "{tags:?}");

        let req = Request::builder().method("POST").uri("/field/agent")
            .header("authorization", format!("Bearer {token}")).header("content-type", "application/json")
            .body(Body::from(r#"{"message":"搜索 同步","context":"Notes/同步/协议.md"}"#)).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: start") && text.contains("event: reply") && text.contains("event: done"), "{text}");
        let (st, _) = call(&app, &token, "POST", "/field/agent", r#"{"message":"  "}"#).await;
        assert_eq!(st, 400);
    }

    #[tokio::test]
    async fn field_api_requires_token() {
        let (app, _t, _d) = app();
        let resp = app.oneshot(Request::builder().uri("/field/data").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status().as_u16(), 401);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ymd_is_beijing_calendar_day() {
        assert_eq!(ymd(0), "1970-01-01");
        // 2026-10-24 16:30 UTC = 2026-10-25 00:30 北京
        assert_eq!(ymd(1_792_859_400), "2026-10-25");
    }

    #[test]
    fn memo_title_is_its_first_sentence_without_time() {
        assert_eq!(title_of("Notes/Memos/2026-10/20T091200.md", "---\ncreated: 2026-10-20T09:12:00\n---\n\n09:12 同步冲突时两边都保留\n"), "同步冲突时两边都保留");
    }

    #[test]
    fn title_prefers_heading_then_frontmatter_then_filename() {
        assert_eq!(title_of("Notes/a.md", "---\ntitle: 甲\n---\n\n正文"), "甲");
        assert_eq!(title_of("Notes/a.md", "# 乙\n\n正文"), "乙");
        assert_eq!(title_of("Notes/x/丙.md", "正文"), "丙");
    }

    #[test]
    fn date_reads_created_frontmatter() {
        assert_eq!(date_of("---\ncreated: 2026-10-03T09:12:00\n---\n", 0), "2026-10-03");
        assert_eq!(date_of("没有日期", 0), "1970-01-01");
    }

    #[test]
    fn passages_keep_relevant_paragraphs_in_order() {
        let t = "第一段讲同步协议的基本设计。\n\n第二段讲配色与字体。\n\n第三段讲同步冲突如何合并。";
        let p = passages(t, "同步冲突", 900);
        assert!(p.contains("第一段") && p.contains("第三段"));
        assert!(p.find("第一段").unwrap() < p.find("第三段").unwrap());
    }

    #[test]
    fn best_sentence_skips_headings_and_frontmatter() {
        let t = "---\ncreated: 2026-01-01\n---\n# 标题里也有同步\n\n这一句和配色有关。\n这一句讲同步冲突怎样合并。";
        assert_eq!(best_sentence(t, "同步冲突"), "这一句讲同步冲突怎样合并");
    }
}
