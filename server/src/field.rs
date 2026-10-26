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
    /// 正文字数（不含 frontmatter 与标题）：判断碎片
    pub chars: usize,
    /// 过时信号：未完成的 TODO / 提到的计划日期（整理时与今天比较）
    pub todo: bool,
    pub plans: Vec<String>,
}

pub struct Cached {
    pub epoch: u64,
    pub field: Field,
    pub meta: Vec<DocMeta>,
    pub body: Bytes,
    /// 每篇前 6 个近邻：整理（碎片、取代关系）时才算，算一次缓存到库变
    pub knn: std::sync::OnceLock<Vec<Vec<(usize, f32)>>>,
}

impl Cached {
    pub fn knn(&self) -> &Vec<Vec<(usize, f32)>> {
        self.knn.get_or_init(|| self.field.vectors.knn(6))
    }
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
pub fn title_of(path: &str, text: &str) -> String {
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
        return s.chars().take(64).collect();
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
    let trace = std::env::var("THIRDC_FIELD_TRACE").is_ok();
    let t0 = std::time::Instant::now();
    let (corpus, stats) = {
        let k = st.kernel.lock().unwrap();
        let (_, corpus) = k.hybrid_snapshot().map_err(|e| e.to_string())?;
        (corpus, k.doc_stats().unwrap_or_default())
    };
    if trace {
        eprintln!("[field] snapshot {:?}", t0.elapsed());
    }
    let root2 = root.clone();
    let built = tokio::task::spawn_blocking(move || {
        let lap = std::time::Instant::now();
        let mark = |what: &str| {
            if trace {
                eprintln!("[field] {what} {:?}", lap.elapsed());
            }
        };
        let prev = load_prev(&root2);
        let docs: Vec<(String, String, String)> = corpus
            .into_iter()
            .map(|(p, text)| {
                let t = title_of(&p, &text);
                (p, t, text)
            })
            .collect();
        let texts: HashMap<String, (String, String)> = docs.iter().map(|(p, t, x)| (p.clone(), (t.clone(), x.clone()))).collect();
        mark("docs");
        let f = field::build(&docs, &prev, rebuild);
        mark("build");
        let meta: Vec<DocMeta> = f
            .vectors
            .paths
            .iter()
            .map(|p| {
                let (title, text) = &texts[p];
                let mtime = stats.get(p).map(|s| s.0).unwrap_or(0);
                let (chars, todo, plans) = super::organize::signals(text);
                DocMeta { path: p.clone(), title: title.clone(), kind: kind_of(p), date: date_of(text, mtime), excerpt: excerpt_of(text), mtime, tags: extract_memo_tags(&text.chars().take(4000).collect::<String>()), chars, todo, plans }
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
        mark("meta+pos");
        let links = links_of(&f.vectors.paths, &meta, &texts);
        mark("links");
        let names = load_region_names(&root2);
        let body = json!({
            "epoch": epoch,
            "links": links.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "regions": f.region_names.iter().zip(count.iter()).map(|(n, c)| json!({"name": names.get(n).cloned().unwrap_or_else(|| n.clone()), "auto": n, "count": c})).collect::<Vec<_>>(),
            "docs": meta.iter().enumerate().map(|(i, m)| json!({
                "p": m.path, "t": m.title, "k": m.kind, "d": m.date, "ex": m.excerpt,
                "x": round1(f.pos[i].0), "y": round1(f.pos[i].1), "r": f.region[i], "m": m.mtime, "g": m.tags,
            })).collect::<Vec<_>>(),
            "edges": f.edges.iter().map(|(a, b, w)| json!([a, b, (w * 1000.0).round() / 1000.0])).collect::<Vec<_>>(),
        });
        let body = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
        mark("json");
        Cached { epoch, field: f, meta, body, knn: std::sync::OnceLock::new() }
    })
    .await
    .map_err(|e| e.to_string())?;
    let built = Arc::new(built);
    cache().lock().unwrap().insert(root, built.clone());
    Ok(built)
}

// ───────── 区域改名：用户起的名字按自动名记住（自动名 = 区域中心文档的短标题） ─────────

/// 随库同步的界面状态放在库根的 `.litmus/`：git 同步会带上它（只排除 `.thirdc/`），
/// 又不在 `Notes/` 下，所以不会被当成文档、不进搜索与文档列表。
pub fn synced_path(root: &std::path::Path, name: &str) -> PathBuf {
    root.join(".litmus").join(name)
}

/// 读随库状态；新位置没有时读旧位置（0.12 写在 `.thirdc/`，只在本机）。
pub fn read_synced(root: &std::path::Path, name: &str, legacy: &str) -> Option<String> {
    std::fs::read_to_string(synced_path(root, name)).ok().or_else(|| std::fs::read_to_string(root.join(".thirdc").join(legacy)).ok())
}

pub fn write_synced(root: &std::path::Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let p = synced_path(root, name);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &p)
}

pub fn load_region_names(root: &std::path::Path) -> HashMap<String, String> {
    read_synced(root, "regions.json", "field-names.json")
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        .unwrap_or_default()
}

/// POST /field/region {auto, name}：name 为空表示恢复自动名。区域的中心文档变了时，名字回到自动名。
pub async fn region_rename(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let auto = req.get("auto").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let name: String = req.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().chars().take(24).collect();
    if auto.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 auto（区域的自动名）").into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let mut names = load_region_names(&root);
    if name.is_empty() || name == auto {
        names.remove(&auto);
    } else {
        names.insert(auto.clone(), name.clone());
    }
    if let Err(e) = write_synced(&root, "regions.json", &serde_json::to_vec_pretty(&names).unwrap_or_default()) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    cache().lock().unwrap().remove(&root);
    Json(json!({ "auto": auto, "name": if name.is_empty() { auto.clone() } else { name } })).into_response()
}

// ───────── 视角：一组文档 + 镜头 + 排布，可命名、切换（替代经典壳的多画布） ─────────

pub async fn views_get(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let v: Value = read_synced(&root, "views.json", "field-views.json").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({ "views": [] }));
    Json(v).into_response()
}

/// PUT /field/views {views:[{name, layout, cam:{x,y,s}, paths:[...], q?}]}：整表替换（最多 60 个，每个最多 400 篇）。
pub async fn views_put(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let Some(list) = req.get("views").and_then(|v| v.as_array()) else {
        return err(StatusCode::BAD_REQUEST, "需要 views 数组").into_response();
    };
    let clean: Vec<Value> = list
        .iter()
        .take(60)
        .filter_map(|v| {
            let name: String = v.get("name")?.as_str()?.trim().chars().take(40).collect();
            if name.is_empty() {
                return None;
            }
            let paths: Vec<Value> = v
                .get("paths")
                .and_then(|p| p.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).filter(|p| kernel_core::is_safe_doc_path(p)).take(400).map(|p| json!(p)).collect())
                .unwrap_or_default();
            let num = |k: &str| v.get("cam").and_then(|c| c.get(k)).and_then(|x| x.as_f64()).filter(|x| x.is_finite()).unwrap_or(0.0);
            Some(json!({
                "name": name,
                "layout": if v.get("layout").and_then(|l| l.as_str()) == Some("time") { "time" } else { "sem" },
                "cam": { "x": num("x"), "y": num("y"), "s": num("s").clamp(0.02, 4.0) },
                "paths": paths,
                "q": v.get("q").and_then(|q| q.as_str()).map(|q| q.chars().take(200).collect::<String>()).unwrap_or_default(),
                "at": v.get("at").and_then(|x| x.as_u64()).unwrap_or(0),
            }))
        })
        .collect();
    let root = st.kernel.lock().unwrap().vault.root.clone();
    match write_synced(&root, "views.json", &serde_json::to_vec_pretty(&json!({ "views": clean })).unwrap_or_default()) {
        Ok(()) => Json(json!({ "views": clean })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ───────── 批量整理：用场本身给建议（放回意思所在的文件夹、占位名改成标题、可能重复） ─────────

/// 一篇文档所在的文件夹（`Notes/a/b.md` → `Notes/a`；`Notes/b.md` → `Notes`）。
fn folder_of(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// POST /field/organize：只给建议，不改文件。执行由壳逐条调用 /doc/move 与 /doc/autoname（都会改好链接）。
pub async fn organize_plan(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let c = match current(&st, false).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let names = load_region_names(&root);
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let scope = super::organize::Scope::from_json(&req);
    let keep = scope.keep(&c);
    let f = &c.field;
    let n = c.meta.len();
    let region_name = |r: usize| {
        let a = f.region_names.get(r).cloned().unwrap_or_default();
        names.get(&a).cloned().unwrap_or(a)
    };
    // 每个区域里，成员最多的「真」文件夹（不算 Notes 根、随手记、答案、采集原文）
    let skip_folder = |d: &str| d == "Notes" || d.starts_with("Notes/Memos") || d.starts_with("Notes/答案") || d.starts_with("Notes/Agent");
    let k = f.region_names.len();
    let mut tally: Vec<HashMap<String, usize>> = vec![HashMap::new(); k];
    let mut members = vec![0usize; k];
    for (i, m) in c.meta.iter().enumerate() {
        let r = f.region[i];
        members[r] += 1;
        let d = folder_of(&m.path);
        if !skip_folder(d) && m.kind != "source" {
            *tally[r].entry(d.to_string()).or_insert(0) += 1;
        }
    }
    let home: Vec<Option<(String, usize)>> = tally
        .iter()
        .enumerate()
        .map(|(r, t)| {
            let mut v: Vec<(&String, &usize)> = t.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            v.first().filter(|(_, c)| **c >= 3 && **c * 3 >= members[r]).map(|(d, c)| ((*d).clone(), **c))
        })
        .collect();
    // 占位文件名 → 正文标题（先算：同一篇既要挪又要改名时，合成一次移动）
    let safe_name = |t: &str| -> String {
        t.chars().filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '\n' | '\r' | '\t')).collect::<String>().trim().chars().take(60).collect()
    };
    let mut retitle: HashMap<String, String> = HashMap::new();
    for (i, m) in c.meta.iter().enumerate() {
        if !keep[i] || !m.path.ends_with(".md") || !super::is_placeholder_name(&super::stem_of(&m.path)) {
            continue;
        }
        let md = std::fs::read_to_string(root.join(&m.path)).unwrap_or_default();
        if let Some(h1) = super::doc_h1(&md).filter(|t| !super::is_placeholder_name(t)) {
            let n = safe_name(&h1);
            if !n.is_empty() {
                retitle.insert(m.path.clone(), n);
            }
        }
    }
    let mut moves = Vec::new();
    for (i, m) in c.meta.iter().enumerate() {
        if !keep[i] || folder_of(&m.path) != "Notes" || m.kind != "doc" {
            continue;
        }
        let r = f.region[i];
        if let Some((dir, cnt)) = &home[r] {
            let name = match retitle.remove(&m.path) {
                Some(t) => format!("{t}.md"),
                None => m.path.rsplit('/').next().unwrap_or(&m.path).to_string(),
            };
            let to = format!("{dir}/{name}");
            if !root.join(&to).exists() {
                let renamed = !m.path.ends_with(&format!("/{name}"));
                moves.push(json!({
                    "from": m.path, "to": to, "title": m.title, "region": region_name(r),
                    "reason": format!("散放在 Notes 根目录；意思上属于「{}」，那里有 {cnt} 篇放在 {dir}/{}", region_name(r), if renamed { "；占位文件名顺便换成标题" } else { "" }),
                }));
            }
        }
    }
    let mut renames: Vec<Value> = retitle
        .into_iter()
        .filter(|(p, t)| {
            let d = folder_of(p);
            !root.join(format!("{d}/{t}.md")).exists()
        })
        .map(|(p, t)| json!({ "path": p, "from": super::stem_of(&p), "to": t, "reason": "文件名是占位名，正文已有标题" }))
        .collect();
    renames.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let mut dups = Vec::new();
    for &(a, b, w) in &f.edges {
        if w >= 0.86 && a < n && b < n && (keep[a] || keep[b]) {
            dups.push(json!({ "a": c.meta[a].path, "b": c.meta[b].path, "ta": c.meta[a].title, "tb": c.meta[b].title, "sim": (w * 100.0).round() / 100.0 }));
        }
    }
    dups.sort_by(|x, y| y["sim"].as_f64().partial_cmp(&x["sim"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    dups.truncate(30);
    let want_ai = req.get("ai").and_then(|a| a.as_bool()).unwrap_or(false);
    let (tags, titles, ai_note) = if want_ai {
        let mut vocab: HashMap<String, usize> = HashMap::new();
        for m in &c.meta {
            for t in &m.tags {
                *vocab.entry(t.clone()).or_insert(0) += 1;
            }
        }
        let mut vocab: Vec<(String, usize)> = vocab.into_iter().collect();
        vocab.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let docs = c.meta.iter().map(|m| (m.path.clone(), m.title.clone(), m.excerpt.clone(), m.tags.clone())).collect();
        super::field_tools::ai_suggestions(&st, docs, vocab.into_iter().take(80).map(|(t, _)| t).collect()).await
    } else {
        (Vec::new(), Vec::new(), None)
    };
    let mut out = json!({ "moves": moves, "renames": renames, "dups": dups, "tags": tags, "titles": titles, "ai": want_ai, "ai_note": ai_note, "considered": n });
    if let (Some(o), Value::Object(x)) = (out.as_object_mut(), super::organize::plan_extras(&c, &scope)) {
        o.extend(x);
    }
    out["options"] = super::organize::scope_options(&c, &root);
    Json(out).into_response()
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
                let tx_delta = tx.clone();
                let mut on_step = move |s: &thirdc_mcp::agent::Step| {
                    let _ = tx_step.send(ev("step", json!({ "tool": s.tool, "args": step_args(&s.args), "ok": s.ok, "summary": s.summary.chars().take(300).collect::<String>() })));
                };
                let mut on_delta = move |t: &str| {
                    let _ = tx_delta.send(ev("delta", json!({ "t": t })));
                };
                thirdc_mcp::agent::run_ai_streaming(&server, &cfg, &full, &mut on_step, &mut on_delta).await
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
            if req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false) {
                return crystal_stream(cfg, system, user, q, sources, refs).into_response();
            }
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

/// 结晶答案的流式版：先回 start（来源），正文逐段 delta，最后 done（与非流式 JSON 同形）。
/// 模型中途失败或返回空答案时，done 退化为原文摘句并带 note。
fn crystal_stream(
    cfg: kernel_core::AiConfig,
    system: &'static str,
    user: String,
    q: String,
    sources: Vec<(String, String, String)>,
    refs: Vec<Value>,
) -> impl IntoResponse {
    use axum::response::sse::Sse;
    use futures_util::stream;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();
    tokio::spawn(async move {
        let ev = |name: &str, data: Value| -> SseEvent { Ok(axum::response::sse::Event::default().event(name).data(data.to_string())) };
        let _ = tx.send(ev("start", json!({ "sources": refs })));
        let txd = tx.clone();
        let mut on_delta = move |t: &str| {
            let _ = txd.send(ev("delta", json!({ "t": t })));
        };
        let done = match thirdc_mcp::agent::complete_stream(&cfg, system, &user, &mut on_delta).await {
            Ok(text) if !text.trim().is_empty() => json!({ "mode": "ai", "text": text.trim(), "sources": refs }),
            Ok(_) => json!({ "mode": "extract", "note": "模型返回了空答案，下面是原文里最相关的句子。", "text": extract(&sources, &q), "sources": refs }),
            Err(e) => json!({ "mode": "extract", "note": format!("这次没有连上模型（{e}），下面是原文里最相关的句子。"), "text": extract(&sources, &q), "sources": refs }),
        };
        let _ = tx.send(ev("done", done));
    });
    Sse::new(stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }))
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

    async fn raw(app: &axum::Router, token: Option<&str>, method: &str, uri: &str, body: &str) -> (u16, String) {
        let mut b = Request::builder().method(method).uri(uri).header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let st = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        (st, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn regions_rename_and_views_round_trip() {
        let (app, token, _dir) = app();
        seed(&app, &token).await;
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let auto = d["regions"][0]["auto"].as_str().unwrap().to_string();
        assert_eq!(d["regions"][0]["name"], auto.as_str());
        let (st, _) = call(&app, &token, "POST", "/field/region", &serde_json::json!({"auto": auto, "name": "我的区域"}).to_string()).await;
        assert_eq!(st, 200);
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        assert!(d["regions"].as_array().unwrap().iter().any(|r| r["name"] == "我的区域" && r["auto"] == auto.as_str()));
        let (_, _) = call(&app, &token, "POST", "/field/region", &serde_json::json!({"auto": auto, "name": ""}).to_string()).await;
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        assert!(!d["regions"].as_array().unwrap().iter().any(|r| r["name"] == "我的区域"));

        let body = r#"{"views":[{"name":"写作","layout":"time","cam":{"x":10,"y":-5,"s":9},"paths":["Notes/同步/协议.md","../etc/passwd"]},{"name":"  "}]}"#;
        let (st, v) = call(&app, &token, "PUT", "/field/views", body).await;
        assert_eq!(st, 200, "{v}");
        let (_, g) = call(&app, &token, "GET", "/field/views", "").await;
        let vs = g["views"].as_array().unwrap();
        assert_eq!(vs.len(), 1, "空名字的视角应被丢掉");
        assert_eq!(vs[0]["layout"], "time");
        assert_eq!(vs[0]["cam"]["s"], 4.0, "缩放要夹在范围内");
        assert_eq!(vs[0]["paths"].as_array().unwrap().len(), 1, "非法路径要被滤掉");
    }

    #[tokio::test]
    async fn organize_suggests_and_move_relinks() {
        let (app, token, dir) = app();
        seed(&app, &token).await;
        let (st, _) = call(&app, &token, "PUT", &format!("/doc?path={}", urlenc("Notes/未命名.md")), "# 冲突的三种处理\n\n同步 冲突 合并 两边保留 CRDT 设备 离线 op-log。").await;
        assert_eq!(st, 200);
        let (st, _) = call(&app, &token, "PUT", &format!("/doc?path={}", urlenc("Notes/同步/目录.md")), "# 目录\n\n见 [[Notes/未命名.md]] 和 [[未命名|那篇]]，以及 [原文](Notes/未命名.md)。同步 冲突").await;
        assert_eq!(st, 200);
        let (st, _) = call(&app, &token, "PUT", &format!("/doc?path={}", urlenc("Notes/发布/未命名.md")), "# 证书续期\n\n域名 证书 发布 部署 线上。").await;
        assert_eq!(st, 200);
        let (st, plan) = call(&app, &token, "POST", "/field/organize", "{}").await;
        assert_eq!(st, 200, "{plan}");
        let mv = plan["moves"].as_array().unwrap();
        assert!(mv.iter().any(|m| m["from"] == "Notes/未命名.md" && m["to"] == "Notes/同步/冲突的三种处理.md"), "挪动与改名合成一次：{plan}");
        let rn = plan["renames"].as_array().unwrap();
        assert!(!rn.iter().any(|r| r["path"] == "Notes/未命名.md"), "已经合进移动的不再单列：{plan}");
        assert!(rn.iter().any(|r| r["path"] == "Notes/发布/未命名.md" && r["to"] == "证书续期"), "{plan}");
        // 执行移动：链接一起改
        let (st, r) = call(&app, &token, "POST", "/doc/move", r#"{"from":"Notes/未命名.md","to":"Notes/同步/冲突三种.md"}"#).await;
        assert_eq!(st, 200, "{r}");
        assert!(r["relinked"].as_u64().unwrap() >= 1);
        let idx = std::fs::read_to_string(dir.path().join("Notes/同步/目录.md")).unwrap();
        assert!(idx.contains("[[Notes/同步/冲突三种.md]]") && idx.contains("[[冲突三种|那篇]]") && idx.contains("](Notes/同步/冲突三种.md)"), "{idx}");
    }

    #[tokio::test]
    async fn encrypted_share_round_trip() {
        let (app, token, _dir) = app();
        let (st, r) = call(&app, &token, "POST", "/share", r#"{"content":"Q0lQSEVSVEVYVA","iv":"aXZpdml2aXZpdml2","expires_days":7,"path":"Notes/x.md"}"#).await;
        assert_eq!(st, 200, "{r}");
        let id = r["id"].as_str().unwrap().to_string();
        assert_eq!(r["url"], format!("/s/{id}"));
        assert!(r["public_url"].is_null(), "没配公开目标就没有公开地址");
        // 查看页免令牌，密文嵌在页里
        let (st, html) = raw(&app, None, "GET", &format!("/s/{id}"), "").await;
        assert_eq!(st, 200);
        assert!(html.contains("Q0lQSEVSVEVYVA") && html.contains("\"expires\":"));
        assert!(!html.contains("__SHARE_DATA__"));
        let (_, l) = call(&app, &token, "GET", "/shares", "").await;
        assert_eq!(l["items"][0]["id"], id.as_str());
        assert!(l["items"][0].get("content").is_none(), "列表不回密文");
        let (st, _) = raw(&app, None, "GET", "/shares", "").await;
        assert_eq!(st, 401);
        let (st, _) = call(&app, &token, "DELETE", &format!("/share/{id}"), "").await;
        assert_eq!(st, 200);
        let (st, html) = raw(&app, None, "GET", &format!("/s/{id}"), "").await;
        assert_eq!(st, 404);
        assert!(html.contains(">null</script>"));
        let (st, _) = raw(&app, None, "GET", "/s/..%2Fx", "").await;
        assert_eq!(st, 404);
    }

    /// 假的 OpenAI 兼容服务：要工具时先流式吐一个分成几段的工具调用，之后流式吐正文。
    async fn mock_llm() -> String {
        use axum::{routing::post, Router};
        async fn h(body: axum::body::Bytes) -> axum::response::Response {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let msgs = v["messages"].as_array().cloned().unwrap_or_default();
            let tool_turn = v.get("tools").is_some() && !msgs.iter().any(|m| m["role"] == "tool");
            let chunks: Vec<String> = if tool_turn {
                vec![
                    r#"{"choices":[{"delta":{"content":"我先查一下"}}]}"#.into(),
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"search_","arguments":"{\"query\":"}}]}}]}"#.into(),
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"vault","arguments":"\"同步\"}"}}]}}]}"#.into(),
                ]
            } else {
                vec![
                    r#"{"choices":[{"delta":{"content":"冲突时两边"}}]}"#.into(),
                    r#"{"choices":[{"delta":{"content":"都保留 [1]。"}}]}"#.into(),
                    r#"{"choices":[],"usage":{"total_tokens":42}}"#.into(),
                ]
            };
            if v["stream"] != true {
                let text = if tool_turn { String::new() } else { "冲突时两边都保留 [1]。".to_string() };
                return axum::response::Response::builder().header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::json!({"choices":[{"message":{"role":"assistant","content":text}}]}).to_string())).unwrap();
            }
            let mut out = String::new();
            for c in chunks {
                out.push_str(&format!("data: {c}\n\n"));
            }
            out.push_str("data: [DONE]\n\n");
            axum::response::Response::builder().header("content-type", "text/event-stream").body(axum::body::Body::from(out)).unwrap()
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, Router::new().route("/v1/chat/completions", post(h))).await.unwrap() });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn crystal_and_agent_stream_from_the_model() {
        let (app, token, _dir) = app();
        seed(&app, &token).await;
        let base = mock_llm().await;
        let (st, _) = call(&app, &token, "POST", "/field/ai", &serde_json::json!({"base_url": base, "model": "mock"}).to_string()).await;
        assert_eq!(st, 200);
        let (st, text) = raw(&app, Some(&token), "POST", "/field/crystal", r#"{"q":"冲突怎么办","paths":["Notes/同步/冲突.md"],"stream":true}"#).await;
        assert_eq!(st, 200);
        assert!(text.contains("event: start") && text.matches("event: delta").count() == 2, "{text}");
        assert!(text.contains(r#""text":"冲突时两边都保留 [1]。""#) && text.contains(r#""mode":"ai""#), "{text}");
        // 不要流式时照旧回 JSON
        let (_, j) = call(&app, &token, "POST", "/field/crystal", r#"{"q":"冲突怎么办","paths":["Notes/同步/冲突.md"]}"#).await;
        assert_eq!(j["text"], "冲突时两边都保留 [1]。");
        // Agent：工具调用分段到达也能拼回来；过渡话、步骤、正文增量依次推出
        let (st, text) = raw(&app, Some(&token), "POST", "/field/agent", r#"{"message":"同步冲突怎么处理"}"#).await;
        assert_eq!(st, 200);
        let i_pre = text.find("我先查一下").expect("过渡话");
        let i_step = text.find("event: step").expect("步骤");
        let i_ans = text.find("冲突时两边").unwrap_or_else(|| panic!("正文增量：{text}"));
        assert!(i_pre < i_step && i_step < i_ans, "{text}");
        assert!(text.contains(r#""tool":"search_vault""#) && text.contains(r#""query":"同步""#), "{text}");
        assert!(text.contains("event: reply") && text.contains(r#""tokens":42"#), "{text}");
    }

    /// 非流式的假模型：按 system 提示词决定回答。
    async fn mock_json_llm(reply: fn(&str, &str) -> String) -> String {
        use axum::{routing::post, Router};
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: axum::body::Bytes| async move {
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let sys = v["messages"][0]["content"].as_str().unwrap_or("").to_string();
                let user = v["messages"][1]["content"].as_str().unwrap_or("").to_string();
                axum::Json(serde_json::json!({"choices":[{"message":{"role":"assistant","content": reply(&sys, &user)}}]}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn learning_schedules_reviews_in_synced_state() {
        let (app, token, dir) = app();
        seed(&app, &token).await;
        // 没配模型：学习接口给 409 与可执行的提示
        let (st, e) = call(&app, &token, "POST", "/field/learn", r#"{"task":"question","title":"t","section":"s","text":"同步冲突两边都保留"}"#).await;
        assert_eq!(st, 409, "{e}");
        let now = 1_800_000_000u64;
        let (st, r) = call(&app, &token, "POST", "/field/learn/grade", &format!(r#"{{"path":"Notes/同步/协议.md","grade":"good","now":{now}}}"#)).await;
        assert_eq!(st, 200, "{r}");
        assert_eq!(r["card"]["due"], now + 3 * 86_400, "第一次讲清楚：3 天后");
        let (_, r) = call(&app, &token, "POST", "/field/learn/grade", &format!(r#"{{"path":"Notes/同步/协议.md","grade":"good","now":{}}}"#, now + 3 * 86_400)).await;
        assert_eq!(r["card"]["interval"], 7.5, "第二次：3 × 2.5");
        let (_, r) = call(&app, &token, "POST", "/field/learn/grade", &format!(r#"{{"path":"Notes/同步/协议.md","grade":"again","now":{now}}}"#)).await;
        assert_eq!(r["card"]["due"], now + 86_400, "没讲出来：明天再来");
        assert!(r["card"]["ease"].as_f64().unwrap() < 2.5);
        let (st, _) = call(&app, &token, "POST", "/field/learn/grade", r#"{"path":"../x","grade":"good"}"#).await;
        assert_eq!(st, 400);
        // 状态在库根 .litmus/（随 git 同步），不在 Notes/ 下、不会变成文档
        assert!(dir.path().join(".litmus/learn.json").is_file());
        let (_, g) = call(&app, &token, "GET", "/field/learn/state", "").await;
        assert!(g["cards"]["Notes/同步/协议.md"]["reps"].as_u64().unwrap() == 3);
        let (_, docs) = call(&app, &token, "GET", "/field/data", "").await;
        assert!(!docs["docs"].as_array().unwrap().iter().any(|d| d["p"].as_str().unwrap().contains("learn")));
        // 视角也搬到 .litmus/；旧位置（.thirdc/field-views.json）能读回
        std::fs::write(dir.path().join(".thirdc/field-views.json"), r#"{"views":[{"name":"旧的","layout":"sem","cam":{"x":0,"y":0,"s":1},"paths":[]}]}"#).unwrap();
        let (_, v) = call(&app, &token, "GET", "/field/views", "").await;
        assert_eq!(v["views"][0]["name"], "旧的");
        let (_, _) = call(&app, &token, "PUT", "/field/views", r#"{"views":[{"name":"新的","layout":"sem","cam":{"x":0,"y":0,"s":1},"paths":[]}]}"#).await;
        assert!(std::fs::read_to_string(dir.path().join(".litmus/views.json")).unwrap().contains("新的"));
    }

    #[tokio::test]
    async fn learn_and_ai_organize_use_the_model() {
        let (app, token, _dir) = app();
        seed(&app, &token).await;
        let (st, _) = call(&app, &token, "PUT", &format!("/doc?path={}", urlenc("Notes/发布/x.md")), "---\ntags: [发布]\n---\n# 发布\n\n发布 站点").await;
        assert_eq!(st, 200);
        let base = mock_json_llm(|sys, user| {
            if sys.contains("对照原文") {
                "讲对了：两边保留\n漏掉了：没有\n讲偏了：没有\n评分：清楚".into()
            } else if sys.contains("出一道") {
                "为什么冲突时不直接用新的覆盖旧的？".into()
            } else if sys.contains("整理个人知识库") {
                assert!(user.contains("已有标签：发布"), "{user}");
                r##"好的 ```json {"items":[{"path":"Notes/同步/协议.md","tags":["同步","#新词 有空格","CRDT"],"title":"同步协议：冲突两边保留"},{"path":"Notes/不存在.md","tags":["x"]}]} ```"##.into()
            } else {
                "?".into()
            }
        })
        .await;
        let (st, _) = call(&app, &token, "POST", "/field/ai", &serde_json::json!({"base_url": base, "model": "mock"}).to_string()).await;
        assert_eq!(st, 200);
        let (st, q) = call(&app, &token, "POST", "/field/learn", r#"{"task":"question","title":"同步协议","section":"冲突","text":"冲突时两边都保留"}"#).await;
        assert_eq!(st, 200);
        assert!(q["text"].as_str().unwrap().starts_with("为什么"));
        let (_, c) = call(&app, &token, "POST", "/field/learn", r#"{"task":"check","title":"同步协议","section":"冲突","text":"冲突时两边都保留","answer":"两边都留着"}"#).await;
        assert_eq!(c["grade"], "good");
        assert!(!c["text"].as_str().unwrap().contains("评分"), "评分行不给用户看，换成按钮提示");
        let (st, plan) = call(&app, &token, "POST", "/field/organize", r#"{"ai":true}"#).await;
        assert_eq!(st, 200, "{plan}");
        let tags = plan["tags"].as_array().unwrap();
        let t = tags.iter().find(|t| t["path"] == "Notes/同步/协议.md").expect("补标签建议");
        assert_eq!(t["tags"], serde_json::json!(["同步", "CRDT"]), "带空格的新词要丢掉：{plan}");
        assert!(!tags.iter().any(|t| t["path"] == "Notes/不存在.md"), "库里没有的路径不收");
        assert!(plan["titles"].as_array().unwrap().iter().any(|t| t["to"] == "同步协议：冲突两边保留"));
        let (_, plain) = call(&app, &token, "POST", "/field/organize", "{}").await;
        assert!(plain["tags"].as_array().unwrap().is_empty() && plain["ai"] == false);
    }

    #[tokio::test]
    async fn localize_saves_body_and_images() {
        use axum::{routing::get, Router};
        let png: Vec<u8> = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4, 0, 9, 251, 3, 253, 167, 98, 97, 21, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130];
        let body = format!(
            "<html><head><title>网页标题</title></head><body><nav>导航 首页 关于</nav><article><h1>一篇好文章</h1><h2>第一节</h2><p>{}</p><p><img src=\"/img/a.png\" alt=\"图一\"></p><p><img src=\"missing.png\" alt=\"坏图\"></p><ul><li>要点甲</li><li>要点乙</li></ul></article><footer>版权所有</footer></body></html>",
            "这是正文，讲本地化。".repeat(30)
        );
        let site = Router::new()
            .route("/post/1", get(move || { let b = body.clone(); async move { axum::response::Html(b) } }))
            .route("/img/a.png", get(move || { let p = png.clone(); async move { ([("content-type", "image/png")], p) } }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, site).await.unwrap() });
        let (app, token, dir) = app();
        let (st, r) = call(&app, &token, "POST", "/field/localize", &serde_json::json!({"url": format!("http://{addr}/post/1")}).to_string()).await;
        assert_eq!(st, 200, "{r}");
        assert_eq!(r["images"], 1);
        assert_eq!(r["failed"], 1);
        let rel = r["rel"].as_str().unwrap();
        let md = std::fs::read_to_string(dir.path().join(rel)).unwrap();
        assert!(md.contains("一篇好文章") && md.contains("## 第一节") && md.contains("- 要点甲"), "{md}");
        assert!(!md.contains("导航") && !md.contains("版权所有"), "页面框架要去掉：{md}");
        assert!(md.contains("](Assets/"), "图片要换成库内地址：{md}");
        assert!(md.contains(&format!("](http://{addr}/post/missing.png)")), "下不来的图保留原地址：{md}");
        let (st, _) = call(&app, &token, "POST", "/field/localize", r#"{"url":"ftp://x"}"#).await;
        assert_eq!(st, 400);
    }

    fn write_aged(root: &std::path::Path, rel: &str, text: &str, days_ago: u64) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(days_ago * 86_400);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
    }

    #[tokio::test]
    async fn whole_vault_organize_finds_old_and_new_fragments_and_stale_docs() {
        let (app, token, dir) = app();
        let root = dir.path();
        // 跨两年的同主题碎片：长期的和近期的一起
        let frags = [
            ("Notes/Memos/2024-01/a.md", "备份 3-2-1 本地 云端 异地 三份", 700),
            ("Notes/Memos/2024-09/b.md", "备份 云端 异地 rclone 加密 三份", 400),
            ("Notes/Memos/2026-10/c.md", "备份 本地 异地 恢复演练 三份 加密", 3),
            ("Notes/Memos/2026-10/d.md", "备份 rclone 云端 恢复 本地 三份", 1),
        ];
        for (p, t, d) in frags {
            write_aged(root, p, t, d);
        }
        // 过时：旧方案被新方案取代；计划日期已过；TODO 放太久
        write_aged(root, "Notes/方案/发布流程.md", &format!("# 发布流程\n\n{}", "发布 构建 签名 公证 上传 DMG 手工 流程 步骤。".repeat(12)), 400);
        write_aged(root, "Notes/方案/发布流程新版.md", &format!("# 发布流程（新版）\n\n{}", "发布 构建 签名 公证 上传 DMG 自动 流程 步骤 脚本。".repeat(14)), 2);
        write_aged(root, "Notes/方案/路线.md", "# 路线\n\n移动端 预计 2025-03 上线，之后做协作。", 300);
        write_aged(root, "Notes/方案/待办.md", "# 待办\n\n- [ ] 写测试\n- [ ] 补文档 TODO", 120);
        let (st, _) = call(&app, &token, "POST", "/sync", "{}").await;
        assert_eq!(st, 200);
        let (st, plan) = call(&app, &token, "POST", "/field/organize", "{}").await;
        assert_eq!(st, 200, "{plan}");
        let fr = plan["fragments"].as_array().unwrap();
        let g = fr.iter().find(|g| g["paths"].as_array().unwrap().len() >= 3).unwrap_or_else(|| panic!("要有一组碎片：{plan}"));
        assert!(g["span_days"].as_i64().unwrap() > 600, "长期和近期的碎片在同一组：{g}");
        let stale = plan["stale"].as_array().unwrap();
        let kind = |p: &str| stale.iter().find(|x| x["path"] == p).map(|x| x["kind"].as_str().unwrap().to_string());
        assert_eq!(kind("Notes/方案/发布流程.md").as_deref(), Some("superseded"), "{plan}");
        assert_eq!(kind("Notes/方案/路线.md").as_deref(), Some("expired"), "{plan}");
        assert_eq!(kind("Notes/方案/待办.md").as_deref(), Some("todo"), "{plan}");
        assert!(kind("Notes/方案/发布流程新版.md").is_none(), "新版本身不算过时");
        // 范围：只看最近 30 天，旧文档不进来
        let recent = super::ymd(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 - 30 * 86_400);
        let (_, plan2) = call(&app, &token, "POST", "/field/organize", &serde_json::json!({"scope": {"from": recent}}).to_string()).await;
        assert!(plan2["in_scope"].as_u64().unwrap() < plan["in_scope"].as_u64().unwrap());
        assert!(plan2["stale"].as_array().unwrap().iter().all(|x| x["path"] != "Notes/方案/路线.md"));
        assert!(plan2["options"]["regions"].as_array().is_some());
        // 合成：原文留着并注明「已并入」；撤销说明齐全
        let paths: Vec<Value> = g["paths"].as_array().unwrap().clone();
        let (st, m) = call(&app, &token, "POST", "/field/merge", &serde_json::json!({"paths": paths, "title": "备份要点", "folder": "Notes/方案", "mode": "plain", "originals": "keep"}).to_string()).await;
        assert_eq!(st, 200, "{m}");
        let merged = std::fs::read_to_string(root.join(m["path"].as_str().unwrap())).unwrap();
        assert!(merged.starts_with("# 备份要点") && merged.contains("## 来源") && merged.contains("恢复演练"), "{merged}");
        let first = paths[0].as_str().unwrap();
        assert!(std::fs::read_to_string(root.join(first)).unwrap().contains("已并入 [[备份要点]]"));
        assert_eq!(m["undo"]["restore"].as_array().unwrap().len(), paths.len());
        assert_eq!(m["undo"]["delete"][0], m["path"]);
        // 归档方式：原文挪走，撤销给出挪回的路径
        let (_, m2) = call(&app, &token, "POST", "/field/merge", r#"{"paths":["Notes/方案/路线.md","Notes/方案/待办.md"],"title":"路线与待办","mode":"plain","originals":"archive"}"#).await;
        assert!(root.join("Notes/归档/碎片/路线.md").is_file());
        assert_eq!(m2["undo"]["moves"].as_array().unwrap().len(), 2);
        // 没配模型：更新稿给 409；巡检估算不调模型
        let (st, _) = call(&app, &token, "POST", "/field/refresh", r#"{"path":"Notes/方案/发布流程.md"}"#).await;
        assert_eq!(st, 409);
        let (st, est) = call(&app, &token, "POST", "/field/patrol", r#"{"dry":true}"#).await;
        assert_eq!(st, 200);
        assert!(est["docs"].as_u64().unwrap() > 0 && est["calls"].as_u64().unwrap() > 0, "{est}");
        let (st, _) = call(&app, &token, "POST", "/field/patrol", "{}").await;
        assert_eq!(st, 409, "没配模型不能巡检");
    }

    #[tokio::test]
    async fn patrol_runs_in_batches_and_refresh_drafts_an_update() {
        let (app, token, dir) = app();
        let root = dir.path();
        for i in 0..70 {
            write_aged(root, &format!("Notes/资料/第{i}篇.md"), &format!("# 资料{i}\n\n同步 冲突 合并 设备 第{i}条记录 离线 op-log。"), 5);
        }
        write_aged(root, "Notes/资料/旧计划.md", "# 旧计划\n\n预计 2025-02 发布同步。", 200);
        let (_, _) = call(&app, &token, "POST", "/sync", "{}").await;
        let base = mock_json_llm(|sys, user| {
            if sys.contains("整理个人知识库") {
                let paths: Vec<&str> = user.lines().filter_map(|l| l.trim().split_once("path=").map(|x| x.1)).collect();
                serde_json::json!({ "items": paths.iter().map(|p| serde_json::json!({"path": p, "tags": ["同步"]})).collect::<Vec<_>>() }).to_string()
            } else if sys.contains("是否真的过时") {
                let p = user.lines().find_map(|l| l.trim().split_once("path=").map(|x| x.1.to_string())).unwrap_or_default();
                serde_json::json!({"items":[{"path": p, "outdated": true, "what": "2025-02 的发布计划已过"}]}).to_string()
            } else if sys.contains("可能过时的文档") {
                "---\nreviewed: x\n---\n# 旧计划\n\n同步已在 2025-04 发布。\n===CHANGES===\n- 发布日期改成实际的 2025-04".into()
            } else {
                "?".into()
            }
        })
        .await;
        let (_, _) = call(&app, &token, "POST", "/field/ai", &serde_json::json!({"base_url": base, "model": "mock"}).to_string()).await;
        let (st, r) = call(&app, &token, "POST", "/field/patrol", "{}").await;
        assert_eq!(st, 200, "{r}");
        assert!(r["calls"].as_u64().unwrap() >= 3, "71 篇缺标签：至少 3 批（每批 30）：{r}");
        let mut p = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            p = call(&app, &token, "GET", "/field/patrol", "").await.1;
            if p["running"] == false {
                break;
            }
        }
        assert_eq!(p["running"], false, "{p}");
        assert_eq!(p["done"], p["total"]);
        assert!(p["tags"].as_array().unwrap().len() >= 70, "全部范围都看到了，不再只看前 40 篇：{}", p["tags"].as_array().unwrap().len());
        assert!(p["verified"].as_array().unwrap().iter().any(|v| v["path"] == "Notes/资料/旧计划.md" && v["outdated"] == true), "{p}");
        assert!(root.join(".thirdc/patrol.json").is_file());
        let (st, rf) = call(&app, &token, "POST", "/field/refresh", r#"{"path":"Notes/资料/旧计划.md"}"#).await;
        assert_eq!(st, 200, "{rf}");
        assert!(rf["markdown"].as_str().unwrap().contains("2025-04") && rf["changes"].as_str().unwrap().contains("发布日期"));
        assert!(rf["before"].as_str().unwrap().contains("预计 2025-02"), "不写盘，原文还在");
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
