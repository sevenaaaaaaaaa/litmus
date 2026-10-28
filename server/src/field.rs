//! 知识场（Field）的 HTTP 面（docs/spec/field.md §5）。
//!
//! - `GET  /field`          新壳页面（server/web/field.html）
//! - `GET  /field/data`     场：坐标 / 区域 / 关联线 / 每篇元信息（`?rebuild=1` 整体重排）
//! - `GET  /field/react?q=` 全库对一句话的相关分（显色用，不截断到 top-5）
//! - `POST /field/place`    一段文字该落在哪（随手记落位预告）
//! - `POST /field/crystal`  依据指定来源结晶答案（带 [n] 引用；没配模型时退化为原文摘录）
//! - `POST /field/note`     划选后的「解释给我听 / 考考我」
//!
//! 坐标是派生层：缓存在 `.litmus/local/field.json`，删掉即可重建；增量更新时旧坐标不动。

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
    /// 库外来源名（库里的文档为 None）
    pub src: Option<String>,
    /// 写下的事实「键：值」：跨文档冲突比对用（库外的不抽）
    pub facts: Vec<super::organize::Fact>,
}

pub struct Cached {
    pub epoch: u64,
    /// 库外来源的版本（扫描完成或来源变了就变）
    pub ext_rev: u64,
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
    root.join(".litmus/local").join("field.json")
}

/// 一篇换了路径（库外条目入库）：把它在场上的位置交给新路径，下次铺场时新的一篇原地出现。
pub fn carry_position(root: &std::path::Path, from: &str, to: &str) {
    let p = field_json_path(root);
    let Ok(raw) = std::fs::read_to_string(&p) else { return };
    let Ok(mut v) = serde_json::from_str::<Value>(&raw) else { return };
    if let Some(pos) = v.get("pos").and_then(|x| x.get(from)).cloned() {
        v["pos"][to] = pos;
        let _ = std::fs::write(&p, serde_json::to_vec(&v).unwrap_or_default());
    }
}

/// 整个文件夹换了位置：底下每篇在场上的位置都跟过去，不让它们在场上跳一下。
pub fn carry_prefix(root: &std::path::Path, from: &str, to: &str) {
    let p = field_json_path(root);
    let Ok(raw) = std::fs::read_to_string(&p) else { return };
    let Ok(mut v) = serde_json::from_str::<Value>(&raw) else { return };
    let pre = format!("{from}/");
    let moved: Vec<(String, Value)> = match v.get("pos").and_then(|x| x.as_object()) {
        Some(m) => m.iter().filter(|(k, _)| k.starts_with(&pre)).map(|(k, x)| (format!("{to}/{}", &k[pre.len()..]), x.clone())).collect(),
        None => return,
    };
    if moved.is_empty() {
        return;
    }
    for (k, x) in moved {
        v["pos"][k] = x;
    }
    let _ = std::fs::write(&p, serde_json::to_vec(&v).unwrap_or_default());
}

/// 库里的文档开头「> 导入自 `原处`」：原处已经入了库，库外那条就不再上场
/// （兼容 0.18 之前导入、没记在 imported 里的）。
fn imported_origins(docs: &[(String, String, String)]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for (_, _, text) in docs {
        // 搬家向导记在 frontmatter 里：imported_from: "/原来的/路径.md"
        if text.starts_with("---") {
            for l in text.lines().skip(1).take(30) {
                if l.starts_with("---") { break; }
                if let Some(v) = l.strip_prefix("imported_from:") {
                    let v = v.trim().trim_matches('"');
                    if v.starts_with('/') { out.insert(v.replace("\\\\", "\\").replace("\\\"", "\"")); }
                }
            }
        }
        for l in text.lines().take(8) {
            let Some(rest) = l.strip_prefix("> 导入自 ") else { continue };
            // 只认本地路径：网址可能是一整个接口共用的，按它去重会误伤同源的其他条目
            if let Some(a) = rest.strip_prefix('`') {
                if let Some(b) = a.find('`') {
                    out.insert(a[..b].to_string());
                }
            }
        }
    }
    out
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
    let ext_rev = super::sources::REV.load(std::sync::atomic::Ordering::SeqCst);
    if !rebuild {
        if let Some(c) = cache().lock().unwrap().get(&root) {
            if c.epoch == epoch && c.ext_rev == ext_rev {
                return Ok(c.clone());
            }
        }
    }
    let _guard = build_lock().lock().await;
    // 排队期间别人可能已经建好
    if !rebuild {
        if let Some(c) = cache().lock().unwrap().get(&root) {
            if c.epoch == epoch && c.ext_rev == ext_rev {
                return Ok(c.clone());
            }
        }
    }
    let trace = std::env::var("LITMUS_FIELD_TRACE").or_else(|_| std::env::var("THIRDC_FIELD_TRACE")).is_ok();
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
        let mut docs: Vec<(String, String, String)> = corpus
            .into_iter()
            .map(|(p, text)| {
                let t = title_of(&p, &text);
                (p, t, text)
            })
            .collect();
        // 库外来源：原处索引的文字一起铺进场里（path 以 ext: 开头，不可写）
        let ext = super::sources::corpus(&root2);
        let origins = imported_origins(&docs);
        let mut ext_meta: HashMap<String, (String, String, i64, String)> = HashMap::new();
        for (it, src_name, text) in ext {
            if origins.contains(&it.loc) {
                continue;
            }
            ext_meta.insert(it.key.clone(), (src_name, it.date.clone(), it.mtime, it.kind.clone()));
            docs.push((it.key, it.title, text));
        }
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
                if let Some((src, date, mtime, _)) = ext_meta.get(p) {
                    let (chars, todo, plans) = super::organize::signals(text);
                    return DocMeta { path: p.clone(), title: title.clone(), kind: "ext", date: date.clone(), excerpt: excerpt_of(text), mtime: *mtime, tags: Vec::new(), chars, todo, plans, src: Some(src.clone()), facts: Vec::new() };
                }
                let mtime = stats.get(p).map(|s| s.0).unwrap_or(0);
                let (chars, todo, plans) = super::organize::signals(text);
                DocMeta { path: p.clone(), title: title.clone(), kind: kind_of(p), date: date_of(text, mtime), excerpt: excerpt_of(text), mtime, tags: extract_memo_tags(&text.chars().take(4000).collect::<String>()), chars, todo, plans, src: None, facts: super::organize::facts_of(text) }
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
        let _ = std::fs::create_dir_all(root2.join(".litmus/local"));
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
                "x": round1(f.pos[i].0), "y": round1(f.pos[i].1), "r": f.region[i], "m": m.mtime, "g": m.tags, "s": m.src,
            })).collect::<Vec<_>>(),
            "edges": f.edges.iter().map(|(a, b, w)| json!([a, b, (w * 1000.0).round() / 1000.0])).collect::<Vec<_>>(),
        });
        let body = Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
        mark("json");
        Cached { epoch, ext_rev, field: f, meta, body, knn: std::sync::OnceLock::new() }
    })
    .await
    .map_err(|e| e.to_string())?;
    let built = Arc::new(built);
    cache().lock().unwrap().insert(root, built.clone());
    Ok(built)
}

// ───────── 区域改名：用户起的名字按自动名记住（自动名 = 区域中心文档的短标题） ─────────

/// 随库同步的界面状态放在库根的 `.litmus/`：git 同步会带上它（只排除 `.litmus/local/`），
/// 又不在 `Notes/` 下，所以不会被当成文档、不进搜索与文档列表。
pub fn synced_path(root: &std::path::Path, name: &str) -> PathBuf {
    root.join(".litmus").join(name)
}

/// 读随库状态；新位置没有时读旧位置（0.12 写在 `.litmus/local/`，只在本机）。
pub fn read_synced(root: &std::path::Path, name: &str, legacy: &str) -> Option<String> {
    std::fs::read_to_string(synced_path(root, name)).ok().or_else(|| std::fs::read_to_string(root.join(".litmus/local").join(legacy)).ok())
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
        if w >= 0.86 && a < n && b < n && keep[a] && keep[b] {
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
    if let (Some(o), Value::Object(x)) = (out.as_object_mut(), super::organize::plan_extras(&c, &scope, &root)) {
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
                let mut on_step = move |s: &litmus_mcp::agent::Step| {
                    let _ = tx_step.send(ev("step", json!({ "tool": s.tool, "args": step_args(&s.args), "ok": s.ok, "summary": s.summary.chars().take(300).collect::<String>() })));
                };
                let mut on_delta = move |t: &str| {
                    let _ = tx_delta.send(ev("delta", json!({ "t": t })));
                };
                litmus_mcp::agent::run_ai_streaming(&server, &cfg, &full, &mut on_step, &mut on_delta).await
            }
            _ => {
                let out = litmus_mcp::agent::command_mode(&server, &full);
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

// ───────── 模型设置：只改 litmus.toml 的 [ai] 表 ─────────

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
        // 密钥还写在随库同步的 litmus.toml 里（0.21 及以前的做法）：界面上提示挪进凭证库
        "key_in_toml": toml_has_key(&st.kernel.lock().unwrap().vault.root.join("litmus.toml")),
        "key_from_env": ai.api_key.is_empty() && std::env::var("LITMUS_AI_API_KEY").or_else(|_| std::env::var("THIRDC_AI_API_KEY")).map(|v| !v.is_empty()).unwrap_or(false),
    }))
    .into_response()
}

/// 把 [ai] 的 base_url / model / api_key 写回 litmus.toml。
/// 用 toml_edit 原地改：用户手写的注释、其他表与排版都保留；api_key 传空串表示「不改」。
pub fn write_ai_config(cfg_path: &std::path::Path, base_url: &str, model: &str, api_key: Option<&str>) -> Result<(), String> {
    let raw = std::fs::read_to_string(cfg_path).map_err(|e| format!("读不到 litmus.toml：{e}"))?;
    let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| format!("litmus.toml 格式有误：{e}"))?;
    if !doc.contains_key("ai") {
        doc["ai"] = toml_edit::table();
    }
    let t = doc["ai"].as_table_mut().ok_or("litmus.toml 里的 ai 不是一个表")?;
    t["base_url"] = toml_edit::value(base_url);
    t["model"] = toml_edit::value(model);
    // Some("") = 清掉（密钥在凭证库里）；None = 不动
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
    let cfg_path = k.vault.root.join("litmus.toml");
    // 新填的密钥进加密的凭证库，只在这台设备；litmus.toml 里的旧密钥一并拿掉（它会随库同步）
    if let Some(key) = key.as_deref() {
        if let Err(e) = kernel_core::secrets::put(&k.vault.root, "ai_api_key", key, "模型接口密钥（/设置）") {
            return err(StatusCode::CONFLICT, e).into_response();
        }
    }
    if let Err(e) = write_ai_config(&cfg_path, &base_url, &model, key.as_ref().map(|_| "")) {
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

fn toml_has_key(cfg_path: &std::path::Path) -> bool {
    std::fs::read_to_string(cfg_path).ok().and_then(|raw| raw.parse::<toml_edit::DocumentMut>().ok()).and_then(|d| d.get("ai").and_then(|a| a.get("api_key")).and_then(|k| k.as_str()).map(|k| !k.is_empty())).unwrap_or(false)
}

/// POST /field/ai/move-key：把 litmus.toml 里的模型密钥挪进凭证库
pub async fn ai_move_key(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let cfg_path = k.vault.root.join("litmus.toml");
    let key = std::fs::read_to_string(&cfg_path).ok().and_then(|raw| raw.parse::<toml_edit::DocumentMut>().ok()).and_then(|d| d.get("ai").and_then(|a| a.get("api_key")).and_then(|v| v.as_str()).map(String::from)).filter(|v| !v.is_empty());
    let Some(key) = key else { return err(StatusCode::CONFLICT, "litmus.toml 里没有密钥").into_response() };
    let ai = k.vault.config.ai.clone().unwrap_or_default();
    if let Err(e) = kernel_core::secrets::put(&k.vault.root, "ai_api_key", &key, "模型接口密钥（/设置）") {
        return err(StatusCode::CONFLICT, e).into_response();
    }
    if let Err(e) = write_ai_config(&cfg_path, &ai.base_url, &ai.model, Some("")) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
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
    match litmus_mcp::agent::complete(&cfg, "只回复两个字：好的", "测试").await {
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
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|p| kernel_core::is_safe_doc_path(p) || p.starts_with("ext:")).take(6).collect())
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
            // 库外的一篇：读索引里抽好的文字
            if p.starts_with("ext:") {
                let (it, _) = super::sources::find_item(&root, p)?;
                return Some((p.clone(), it.title.clone(), super::sources::read_text(&root, &it.hash)));
            }
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
            match litmus_mcp::agent::complete(&cfg, system, &user).await {
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
        let done = match litmus_mcp::agent::complete_stream(&cfg, system, &user, &mut on_delta).await {
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
    match litmus_mcp::agent::complete(&cfg, system, &user).await {
        Ok(t) => Json(json!({ "text": t.trim() })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("这次没有连上模型：{e}")).into_response(),
    }
}

#[cfg(test)]
mod api_tests {
    use axum::body::Body;
    use axum::http::HeaderMap;
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
        assert!(dir.path().join(".litmus/local/field.json").exists(), "坐标应落到派生层文件");
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

        // 模型设置：写回 litmus.toml 只动 [ai]，原有内容与注释保留
        let cfg = dir.path().join("litmus.toml");
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
        // 密钥进加密的凭证库，不写进随库同步的 litmus.toml
        assert!(!after.contains("sk-test"));
        // 再存一次不带密钥：原密钥保留
        let (_, _) = call(&app, &token, "POST", "/field/ai", r#"{"base_url":"https://api.example.com/v1","model":"demo-2"}"#).await;
        assert_eq!(kernel_core::secrets::get(dir.path(), "ai_api_key").as_deref(), Some("sk-test"));
        // 重新打开库：配置能被内核读回，密钥从凭证库补上
        let v = kernel_core::Vault::open(dir.path()).unwrap();
        let ai = v.config.ai.unwrap();
        assert_eq!((ai.model.as_str(), ai.api_key.as_str()), ("demo-2", "sk-test"));
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
        // 视角也搬到 .litmus/；旧位置（.litmus/local/field-views.json）能读回
        std::fs::write(dir.path().join(".litmus/local/field-views.json"), r#"{"views":[{"name":"旧的","layout":"sem","cam":{"x":0,"y":0,"s":1},"paths":[]}]}"#).unwrap();
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
        // 巡检要把内容发给模型：没看隐私告知、没同意不开始
        let (st, r) = call(&app, &token, "POST", "/field/patrol", "{}").await;
        assert_eq!(st, 428, "{r}");
        assert_eq!(r["needs_consent"], true);
        let (st, _) = call(&app, &token, "POST", "/field/consent", r#"{"agree":true}"#).await;
        assert_eq!(st, 200);
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
        assert!(root.join(".litmus/local/patrol.json").is_file());
        let (st, rf) = call(&app, &token, "POST", "/field/refresh", r#"{"path":"Notes/资料/旧计划.md"}"#).await;
        assert_eq!(st, 200, "{rf}");
        assert!(rf["markdown"].as_str().unwrap().contains("2025-04") && rf["changes"].as_str().unwrap().contains("发布日期"));
        assert!(rf["before"].as_str().unwrap().contains("预计 2025-02"), "不写盘，原文还在");
    }

    #[tokio::test]
    async fn onboarding_consent_background_scan_and_ai_digest() {
        let (app, token, dir) = app();
        // 新库：第一次用 → 边用边引导
        let (_, o) = call(&app, &token, "GET", "/field/onboarding", "").await;
        assert_eq!(o["mode"], "first", "{o}");
        assert_eq!(o["existing"], false);
        let (_, o) = call(&app, &token, "POST", "/field/onboarding", r#"{"step":"note"}"#).await;
        assert!(o["state"]["steps"]["note"].as_u64().is_some());
        assert!(dir.path().join(".litmus/onboarding.json").is_file(), "引导进度随库同步");
        let (_, _) = call(&app, &token, "POST", "/field/onboarding", r#"{"dismiss":true,"seen":true}"#).await;
        let (_, o) = call(&app, &token, "GET", "/field/onboarding", "").await;
        assert_eq!(o["mode"], "none", "{o}");

        // 数据源：只接最近的 2 个；后台扫描，进度在 /jobs
        let outside = tempfile::tempdir().unwrap();
        let o = outside.path();
        let long = "这是一份足够长的资料，讲的是同步冲突怎么处理：两边都保留，同名不同内容另存冲突副本，绝不静默覆盖。设备离线也能继续写，回来再合并。";
        for (i, name) in ["旧的.md", "中间.md", "最新.md"].iter().enumerate() {
            let f = o.join(name);
            std::fs::write(&f, format!("# {name}\n\n{long}")).unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400 * (10 - i as u64 * 3));
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
        }
        let (_, r) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": {"kind": "folder", "name": "下载", "path": o.to_string_lossy(), "limit": 2}}).to_string()).await;
        let id = r["source"]["id"].as_str().unwrap().to_string();
        let (st, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
        assert_eq!(st, 200);
        let mut j = Value::Null;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            j = call(&app, &token, "GET", "/jobs", "").await.1;
            if j["busy"] == false && j["jobs"].as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                break;
            }
        }
        let scan = &j["jobs"][0];
        assert_eq!(scan["kind"], "scan", "{j}");
        assert_eq!(scan["state"], "done", "{j}");
        assert_eq!(scan["total"], 0, "完成后 total 归零，看 items");
        assert_eq!(scan["items"], 2, "只接最近的 2 个：{j}");
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let titles: Vec<&str> = d["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").map(|x| x["t"].as_str().unwrap()).collect();
        assert!(titles.contains(&"最新.md") && titles.contains(&"中间.md") && !titles.contains(&"旧的.md"), "{titles:?}");

        // AI 学习：没配模型 → 估算里说没配；配了但没同意 → 428；同意后后台学，学成的等确认
        let (_, e) = call(&app, &token, "POST", "/field/digest", r#"{"dry":true}"#).await;
        assert_eq!(e["configured"], false);
        assert_eq!(e["count"], 2);
        let base = mock_json_llm(|sys, user| {
            if sys.contains("学成一篇学习笔记") {
                let t = user.split('《').nth(1).and_then(|x| x.split('》').next()).unwrap_or("?");
                serde_json::json!({"title": format!("{t} 的要点"), "summary": "冲突时两边都保留。", "points": ["同名不同内容另存冲突副本", "离线也能写"], "use": "设计同步时参考", "tags": ["同步"]}).to_string()
            } else {
                "?".into()
            }
        })
        .await;
        let (_, _) = call(&app, &token, "POST", "/field/ai", &serde_json::json!({"base_url": base, "model": "mock"}).to_string()).await;
        let (_, e) = call(&app, &token, "POST", "/field/digest", r#"{"dry":true}"#).await;
        assert_eq!(e["configured"], true);
        assert_eq!(e["local"], true, "127.0.0.1 上的模型算本机");
        assert_eq!(e["agreed"], false);
        assert!(e["sent_chars"].as_u64().unwrap() > 100);
        let (st, r) = call(&app, &token, "POST", "/field/digest", "{}").await;
        assert_eq!(st, 428, "{r}");
        let (_, c) = call(&app, &token, "POST", "/field/consent", r#"{"agree":true}"#).await;
        assert_eq!(c["agreed"], true);
        let (st, r) = call(&app, &token, "POST", "/field/digest", r#"{"limit":5}"#).await;
        assert_eq!(st, 200, "{r}");
        assert_eq!(r["total"], 2);
        let mut g = Value::Null;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            g = call(&app, &token, "GET", "/field/digest", "").await.1;
            if g["job"]["running"] == false {
                break;
            }
        }
        let drafts = g["drafts"].as_array().unwrap().clone();
        assert_eq!(drafts.len(), 2, "{g}");
        assert!(!dir.path().join("Notes/学习").exists(), "确认之前不进库");
        let (_, j) = call(&app, &token, "GET", "/jobs", "").await;
        assert_eq!(j["pending"], 2);
        assert!(j["jobs"].as_array().unwrap().iter().any(|x| x["kind"] == "digest" && x["items"] == 2), "{j}");
        // 收下一篇：进 Notes/学习/，带出处，排进复习；另一篇不要
        let (st, a) = call(&app, &token, "POST", "/field/digest/accept", &serde_json::json!({"ids": [drafts[0]["id"]]}).to_string()).await;
        assert_eq!(st, 200, "{a}");
        let path = a["paths"][0].as_str().unwrap().to_string();
        assert!(path.starts_with("Notes/学习/") && path.ends_with(" 的要点.md"), "{path}");
        let md = std::fs::read_to_string(dir.path().join(&path)).unwrap();
        assert!(md.contains("tags: [学习, 同步]") && md.contains("origin: /") && md.contains("最新.md") && md.contains("## 要点"), "{md}");
        let learn = std::fs::read_to_string(dir.path().join(".litmus/learn.json")).unwrap();
        assert!(learn.contains(&path));
        let (_, _) = call(&app, &token, "POST", "/field/digest/reject", &serde_json::json!({"ids": [drafts[1]["id"]]}).to_string()).await;
        let (_, g) = call(&app, &token, "GET", "/field/digest", "").await;
        assert_eq!(g["drafts"].as_array().unwrap().len(), 0);
        assert_eq!(g["learned"], 2);
        // 都学过了：不会再学同一版原文
        let (st, _) = call(&app, &token, "POST", "/field/digest", "{}").await;
        assert_eq!(st, 400);
        // 换了服务商：同意作废，要重新问
        let (_, _) = call(&app, &token, "POST", "/field/ai", r#"{"base_url":"https://api.example.com/v1","model":"m"}"#).await;
        let (_, c) = call(&app, &token, "GET", "/field/consent", "").await;
        assert_eq!(c["agreed"], false);
        assert_eq!(c["host"], "api.example.com");
        // 撤回
        let (_, c) = call(&app, &token, "POST", "/field/consent", r#"{"agree":false}"#).await;
        assert_eq!(c["agreed"], false);
    }

    /// 一个假的「外面」：GitHub / Sanity / MFlow 的 REST，加 OpenFlow 风格的 HTTP MCP（回 SSE）。
    async fn mock_outside() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::{extract::Path as P, extract::Query as Q, http::HeaderMap as H, routing::{get, post}, Router};
        use base64::Engine;
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let g1 = got.clone();
        let g2 = got.clone();
        let g3 = got.clone();
        let g4 = got.clone();
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        let (r1, r2) = (b64("# 仓库说明\n\n这是 README。"), b64("# 设计规范\n\n全 oklch。"));
        let app = Router::new()
            .route("/gh/user", get(|| async { axum::Json(serde_json::json!({"login": "me"})) }))
            .route("/gh/user/repos", get(|| async { axum::Json(serde_json::json!([{"full_name": "me/proj", "fork": false, "archived": false}, {"full_name": "me/fork", "fork": true}])) }))
            .route("/gh/repos/me/proj", get(|| async { axum::Json(serde_json::json!({"default_branch": "main", "pushed_at": "2026-10-20T08:00:00Z"})) }))
            .route("/gh/repos/me/proj/git/trees/main", get(|| async { axum::Json(serde_json::json!({"tree": [
                {"path": "README.md", "type": "blob", "sha": "s1", "size": 20}, {"path": "docs/design.md", "type": "blob", "sha": "s2", "size": 20},
                {"path": "src/main.rs", "type": "blob", "sha": "s3", "size": 20}, {"path": "src/deep/notes.md", "type": "blob", "sha": "s4", "size": 20}]})) }))
            .route("/gh/repos/me/proj/git/blobs/{sha}", get(move |P(sha): P<String>| { let (a, b) = (r1.clone(), r2.clone()); async move { axum::Json(serde_json::json!({"content": if sha == "s1" { a } else { b }})) } }))
            .route("/gh/repos/me/proj/contents/{*path}", get(|| async { (axum::http::StatusCode::NOT_FOUND, "{}") }).put(move |P(path): P<String>, h: H, body: axum::body::Bytes| { let g = g1.clone(); async move {
                let v: Value = serde_json::from_slice(&body).unwrap();
                g.lock().unwrap().push(serde_json::json!({"to": "github", "path": path, "auth": h.get("authorization").map(|x| x.to_str().unwrap().to_string()), "body": v}));
                axum::Json(serde_json::json!({"content": {"html_url": format!("https://github.com/me/proj/blob/main/{path}")}})) } }))
            .route("/sanity/v2024-01-01/data/query/production", get(|Q(q): Q<std::collections::HashMap<String, String>>| async move {
                if q.get("query").map(|x| x.starts_with("count(")).unwrap_or(false) { return axum::Json(serde_json::json!({"result": 1})); }
                axum::Json(serde_json::json!({"result": [{"_id": "page-1", "_type": "compositePage", "_rev": "r1", "_updatedAt": "2026-10-21T00:00:00Z", "title": "AI 绘图落地页",
                    "sections": [{"_type": "hero", "body": [{"_type": "block", "style": "normal", "markDefs": [], "children": [{"_type": "span", "text": "一句话生成海报", "marks": []}]}]}]}]})) }))
            .route("/sanity/v2024-01-01/data/mutate/production", post(move |body: axum::body::Bytes| { let g = g2.clone(); async move {
                g.lock().unwrap().push(serde_json::json!({"to": "sanity", "body": serde_json::from_slice::<Value>(&body).unwrap()}));
                axum::Json(serde_json::json!({"results": [{"id": "x"}]})) } }))
            .route("/mflow/api/kb/tree", get(|h: H| async move {
                if h.get("x-mflow-token").map(|x| x.to_str().unwrap()) != Some("mf-tok") { return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": "需要 X-MFlow-Token"}))); }
                (axum::http::StatusCode::OK, axum::Json(serde_json::json!([{"name": "产品知识库", "files": 1}, {"name": "空的", "files": 0}]))) }))
            .route("/mflow/api/kb/list", get(|| async { axum::Json(serde_json::json!([{"path": "insight-data/Knowledge Base/产品介绍.md", "name": "产品介绍.md", "date": "2026-10-01"}])) }))
            .route("/mflow/api/reports", get(|| async { axum::Json(serde_json::json!({"SEO 周报": [{"path": "insight-data/reports/weekly/w43.md", "name": "w43.md", "date": "2026-10-25"}], "会话日志": [{"path": "harness/sessions/s.md", "date": "2026-10-26"}]})) }))
            .route("/mflow/api/content/raw", get(|Q(q): Q<std::collections::HashMap<String, String>>| async move { let p = q.get("path").cloned().unwrap_or_default(); axum::Json(serde_json::json!({"text": if p.contains("w43") { "# 第 43 周 SEO 周报\n\n点击 +12%。" } else { "# 产品介绍\n\nMFlow 是 GEO Agent。" }})) }))
            .route("/mflow/api/item/upsert", post(move |body: axum::body::Bytes| { let g = g3.clone(); async move {
                g.lock().unwrap().push(serde_json::json!({"to": "mflow", "body": serde_json::from_slice::<Value>(&body).unwrap()}));
                axum::Json(serde_json::json!({"ok": true})) } }))
            .route("/of/mcp", post(move |h: H, body: axum::body::Bytes| { let g = g4.clone(); async move {
                if h.get("x-api-key").map(|x| x.to_str().unwrap()) != Some("of-key") {
                    return (axum::http::StatusCode::UNAUTHORIZED, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"API Key 无效"}}"#.to_string());
                }
                let m: Value = serde_json::from_slice(&body).unwrap();
                let id = m["id"].clone();
                let text = |v: Value| serde_json::json!({"content": [{"type": "text", "text": v.to_string()}]});
                let res = match m["method"].as_str().unwrap_or("") {
                    "initialize" => serde_json::json!({"protocolVersion": "2024-11-05", "serverInfo": {"name": "openflow"}, "capabilities": {"tools": {}}}),
                    "tools/list" => serde_json::json!({"tools": [{"name": "articles_list"}, {"name": "article_get"}, {"name": "skills_list"}, {"name": "article_create"}, {"name": "article_publish"}]}),
                    "tools/call" => {
                        let a = &m["params"]["arguments"];
                        match m["params"]["name"].as_str().unwrap_or("") {
                            "articles_list" => text(if a["status"] == "draft" { serde_json::json!([]) } else { serde_json::json!([{"id": "art_1", "title": "增长复盘方法", "slug": "growth-review", "category": "insight", "status": "published", "updated_at": "2026-10-22 10:00:00"}]) }),
                            "article_get" => text(serde_json::json!({"id": "art_1", "title": "增长复盘方法", "slug": "growth-review", "content": "<h2>先看目标</h2><p>每周对比目标与实际。</p>", "category": "insight", "status": "published", "updated_at": "2026-10-22 10:00:00"})),
                            "skills_list" => text(serde_json::json!([{"id": "sk1", "title": "周报生成", "type": "prompt", "description": "按模板写周报"}])),
                            "article_create" => { g.lock().unwrap().push(serde_json::json!({"to": "openflow", "args": a})); text(serde_json::json!({"ok": true, "id": "article_new"})) }
                            _ => text(serde_json::json!({"error": "unknown"})),
                        }
                    }
                    _ => return (axum::http::StatusCode::ACCEPTED, String::new()),
                };
                (axum::http::StatusCode::OK, format!("data: {}\n\n", serde_json::json!({"jsonrpc": "2.0", "id": id, "result": res})))
            } }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), got)
    }

    const NOTION_MOCK: &str = r#"
import json, sys
def out(m): sys.stdout.write(json.dumps(m, ensure_ascii=False) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    mid, meth = m.get("id"), m.get("method")
    if meth == "initialize": out({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2025-06-18", "serverInfo": {"name": "notion-mock"}, "capabilities": {}}})
    elif meth == "tools/list": out({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{"name": "API-post-search"}, {"name": "API-get-block-children"}, {"name": "API-post-page"}, {"name": "API-patch-block-children"}]}})
    elif meth == "tools/call":
        n, a = m["params"]["name"], m["params"].get("arguments", {})
        if n == "API-post-search": r = {"results": [{"object": "page", "id": "11111111-2222-3333-4444-555555555555", "url": "https://www.notion.so/p1", "last_edited_time": "2026-10-23T00:00:00.000Z", "properties": {"Name": {"type": "title", "title": [{"plain_text": "读书笔记：增长黑客"}]}}}], "has_more": False}
        elif n == "API-get-block-children": r = {"results": [{"type": "heading_2", "heading_2": {"rich_text": [{"plain_text": "要点"}]}}, {"type": "bulleted_list_item", "bulleted_list_item": {"rich_text": [{"plain_text": "北极星指标"}]}}]}
        elif n == "API-post-page":
            sys.stderr.write(json.dumps(a, ensure_ascii=False))
            open(__file__ + ".got", "w").write(json.dumps(a, ensure_ascii=False))
            r = {"object": "page", "id": "new-page-id", "url": "https://www.notion.so/new"}
        else: r = {}
        out({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": json.dumps(r, ensure_ascii=False)}]}})
"#;

    #[tokio::test]
    async fn connectors_read_in_and_publish_back() {
        let (app, token, dir) = app();
        let root = dir.path();
        let (base, got) = mock_outside().await;
        for (n, v) in [("github_token", "gh-tok"), ("sanity_token", "sa-tok"), ("mflow_token", "mf-tok"), ("openflow_api_key", "of-key"), ("notion_token", "nt-tok")] {
            let (st, _) = call(&app, &token, "POST", "/secrets", &serde_json::json!({"name": n, "value": v}).to_string()).await;
            assert_eq!(st, 200);
        }
        let script = root.join("notion_mock.py");
        std::fs::write(&script, NOTION_MOCK).unwrap();
        let (_, pres) = call(&app, &token, "GET", "/connectors", "").await;
        assert_eq!(pres["presets"].as_array().unwrap().len(), 5);
        let srcs = [
            serde_json::json!({"kind": "github", "name": "GitHub", "conf": {"api_base": format!("{base}/gh"), "write": {"repo": "me/proj", "dir": "docs/litmus"}}}),
            serde_json::json!({"kind": "sanity", "name": "Sanity", "conf": {"project": "p1", "api_base": format!("{base}/sanity"), "write": {"type": "post"}}}),
            serde_json::json!({"kind": "mflow", "name": "MFlow", "conf": {"url": format!("{base}/mflow")}}),
            serde_json::json!({"kind": "openflow", "name": "OpenFlow", "conf": {"url": format!("{base}/of/mcp"), "site": "https://nownexts.com"}}),
            serde_json::json!({"kind": "notion", "name": "Notion", "conf": {"command": "python3", "args": [script.to_string_lossy()], "write": {"parent": "https://www.notion.so/x/父页面-aaaaaaaabbbbccccddddeeeeeeeeeeee"}}}),
        ];
        let mut ids = Vec::new();
        for s in &srcs {
            let (st, r) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": s}).to_string()).await;
            assert_eq!(st, 200, "{r}");
            let id = r["source"]["id"].as_str().unwrap().to_string();
            let (_, t) = call(&app, &token, "POST", "/connectors/test", &serde_json::json!({"id": id}).to_string()).await;
            assert_eq!(t["ok"], true, "{s} → {t}");
            let (st, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
            assert_eq!(st, 200);
            ids.push(id);
        }
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if call(&app, &token, "GET", "/jobs", "").await.1["busy"] == false {
                break;
            }
        }
        let (_, list) = call(&app, &token, "GET", "/sources", "").await;
        for s in list["sources"].as_array().unwrap() {
            assert!(s["scan"]["error"].is_null(), "{}：{}", s["name"], s["scan"]);
        }
        let n = |name: &str| list["sources"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap()["items"].as_u64().unwrap();
        assert_eq!(n("GitHub"), 2, "README + docs/design.md，跳过 fork、代码和深处的 md");
        assert_eq!(n("Sanity"), 1);
        assert_eq!(n("MFlow"), 2, "知识库 1 + 周报 1，会话日志不要");
        assert_eq!(n("OpenFlow"), 2, "文章 1 + 技能 1");
        assert_eq!(n("Notion"), 1);
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let titles: Vec<&str> = d["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").map(|x| x["t"].as_str().unwrap()).collect();
        for t in ["仓库说明", "设计规范", "AI 绘图落地页", "产品介绍", "第 43 周 SEO 周报", "增长复盘方法", "周报生成", "读书笔记：增长黑客"] {
            assert!(titles.contains(&t), "{t} 不在场上：{titles:?}");
        }
        // 正文转好了
        let key = d["docs"].as_array().unwrap().iter().find(|x| x["t"] == "增长复盘方法").unwrap()["p"].as_str().unwrap().to_string();
        let (_, e) = call(&app, &token, "GET", &format!("/field/ext?p={}", urlenc(&key)), "").await;
        assert!(e["source"].as_str().unwrap().contains("每周对比目标与实际"), "{e}");
        let key = d["docs"].as_array().unwrap().iter().find(|x| x["t"] == "AI 绘图落地页").unwrap()["p"].as_str().unwrap().to_string();
        let (_, e) = call(&app, &token, "GET", &format!("/field/ext?p={}", urlenc(&key)), "").await;
        assert!(e["source"].as_str().unwrap().contains("一句话生成海报"), "{e}");
        // 发回去：没确认不发
        std::fs::create_dir_all(root.join("Notes/方案")).unwrap();
        std::fs::write(root.join("Notes/方案/同步方案.md"), "# 同步方案\n\n冲突时 **两边都保留**。\n\n- 离线可写\n- 回来再合").unwrap();
        let (_, _) = call(&app, &token, "POST", "/sync", "{}").await;
        let (st, _) = call(&app, &token, "POST", "/connectors/publish", &serde_json::json!({"id": ids[0], "path": "Notes/方案/同步方案.md"}).to_string()).await;
        assert_eq!(st, 400);
        for id in &ids {
            let (st, r) = call(&app, &token, "POST", "/connectors/publish", &serde_json::json!({"id": id, "path": "Notes/方案/同步方案.md", "confirm": true}).to_string()).await;
            assert_eq!(st, 200, "{id}：{r}");
        }
        let got = got.lock().unwrap().clone();
        let gh = got.iter().find(|g| g["to"] == "github").unwrap();
        assert_eq!(gh["path"], "docs/litmus/同步方案.md");
        assert_eq!(gh["auth"], "Bearer gh-tok");
        assert!(gh["body"]["content"].as_str().is_some());
        let sa = got.iter().find(|g| g["to"] == "sanity").unwrap();
        let doc = &sa["body"]["mutations"][0]["createOrReplace"];
        assert!(doc["_id"].as_str().unwrap().starts_with("drafts.litmus-"), "Sanity 只写草稿：{doc}");
        assert_eq!(doc["_type"], "post");
        assert_eq!(doc["title"], "同步方案");
        assert_eq!(doc["body"][0]["children"][1]["marks"][0], "strong", "正文不重复标题：{doc}");
        let of = got.iter().find(|g| g["to"] == "openflow").unwrap();
        assert_eq!(of["args"]["status"], "draft");
        assert!(of["args"]["content"].as_str().unwrap().contains("<strong>两边都保留</strong>"), "{of}");
        let mf = got.iter().find(|g| g["to"] == "mflow").unwrap();
        assert_eq!(mf["body"]["topic"], "同步方案");
        let np: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}.got", script.display())).unwrap()).unwrap();
        assert_eq!(np["parent"]["page_id"], "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(np["properties"]["title"]["title"][0]["text"]["content"], "同步方案");
        assert_eq!(np["children"][1]["type"], "bulleted_list_item");
        // 发过的记下来
        let ob: Value = serde_json::from_str(&std::fs::read_to_string(root.join(".litmus/local/outbound.json")).unwrap()).unwrap();
        assert_eq!(ob["Notes/方案/同步方案.md"].as_object().unwrap().len(), 5);
        // 换错的 OpenFlow 密钥：测连接说清楚是鉴权
        let (_, _) = call(&app, &token, "POST", "/secrets", r#"{"name":"openflow_api_key","value":"wrong"}"#).await;
        let (_, t) = call(&app, &token, "POST", "/connectors/test", &serde_json::json!({"id": ids[3]}).to_string()).await;
        assert_eq!(t["ok"], false);
        assert!(t["message"].as_str().unwrap().contains("鉴权") && t["message"].as_str().unwrap().contains("API Key 无效"), "{t}");
    }

    #[tokio::test]
    async fn curate_asks_the_model_only_about_unsure_docs() {
        let (app, token, dir) = app();
        let f = dir.path().join("Notes/Sources/ops/Q3 数据报告.md");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "# Q3 数据报告\n\n留存 41%。").unwrap();
        let g = dir.path().join("Notes/资料/方案.md");
        std::fs::create_dir_all(g.parent().unwrap()).unwrap();
        std::fs::write(&g, "# 同步方案\n\n两边保留。").unwrap();
        let (_, _) = call(&app, &token, "POST", "/sync", "{}").await;
        let base = mock_json_llm(|sys, user| {
            if sys.contains("清理知识库") {
                assert!(user.contains("Q3 数据报告") && !user.contains("同步方案"), "只问拿不准的：{user}");
                serde_json::json!({"items":[{"i":1,"cat":"report","why":"有数据的季度报告"}]}).to_string()
            } else {
                "?".into()
            }
        })
        .await;
        let (_, _) = call(&app, &token, "POST", "/field/ai", &serde_json::json!({"base_url": base, "model": "mock"}).to_string()).await;
        let (st, _) = call(&app, &token, "POST", "/field/curate", "{}").await;
        assert_eq!(st, 428, "要先同意隐私告知");
        let (_, _) = call(&app, &token, "POST", "/field/consent", r#"{"agree":true}"#).await;
        let (st, _) = call(&app, &token, "POST", "/field/curate", "{}").await;
        assert_eq!(st, 200);
        let mut c = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            c = call(&app, &token, "GET", "/field/curate", "").await.1;
            if c["job"]["running"] == false {
                break;
            }
        }
        let r = c["rows"].as_array().unwrap().iter().find(|r| r["title"] == "Q3 数据报告").unwrap().clone();
        assert_eq!((r["by"].as_str(), r["cat"].as_str(), r["keep"].as_bool()), (Some("ai"), Some("report"), Some(true)), "{c}");
        assert_eq!(c["job"]["ai_calls"], 1);
    }

    #[tokio::test]
    async fn curate_keeps_knowledge_and_trashes_process_docs_with_undo() {
        let (app, token, dir) = app();
        let root = dir.path();
        let put = |p: &str, t: &str| {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        put("Notes/资料/Q3 增长分析报告.md", "# Q3 增长分析报告\n\n| 指标 | 数值 |\n|---|---|\n| 留存 | 41% |");
        put("Notes/资料/部署指南.md", "# 部署指南\n\n1. 打包\n2. 上传\n3. 重启");
        put("Notes/Sources/会话/修登录.md", "# 修登录\n\n> 导入自 `/Users/x/.claude/projects/a.jsonl`（2026-10-01）\n\n## 结论\n\n好了\n\n## 过程\n\n**问**：修一下\n\n**答**：好");
        put("Notes/Sources/ops/交接.md", "# 交接\n\n> 导入自 `/Users/x/proj/ops/2026-10-20-handoff.md`（2026-10-20）\n\n今天做到这里。");
        put("Notes/资料/随想.md", "# 随想\n\n" );
        let (_, _) = call(&app, &token, "POST", "/sync", "{}").await;
        // 库外：一个 ops 文件夹
        let outside = tempfile::tempdir().unwrap();
        let ops = outside.path().join("ops");
        std::fs::create_dir_all(&ops).unwrap();
        std::fs::write(ops.join("progress.md"), "# 进度\n\n今天改了同步。").unwrap();
        std::fs::write(ops.join("课程大纲.md"), "# 课程大纲\n\n第一课 所有权").unwrap();
        let (_, r) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": {"kind": "folder", "name": "ops", "path": ops.to_string_lossy()}}).to_string()).await;
        let id = r["source"]["id"].as_str().unwrap().to_string();
        let (_, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if call(&app, &token, "GET", "/jobs", "").await.1["busy"] == false {
                break;
            }
        }
        // 扫描时就按规则扣下了 progress.md（ops/ 里、像过程文档），课程大纲照常上场
        let (_, j) = call(&app, &token, "GET", "/jobs", "").await;
        assert_eq!(j["held"], 1, "{j}");
        // 全库筛一遍（没配模型：只用规则）
        let (st, r) = call(&app, &token, "POST", "/field/curate", r#"{"scope":"all"}"#).await;
        assert_eq!(st, 200, "{r}");
        let mut c = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            c = call(&app, &token, "GET", "/field/curate", "").await.1;
            if c["job"]["running"] == false {
                break;
            }
        }
        let rows = c["rows"].as_array().unwrap();
        let row = |t: &str| rows.iter().find(|r| r["title"] == t).unwrap_or_else(|| panic!("{t} 不在清单里：{c}")).clone();
        assert_eq!(row("Q3 增长分析报告")["cat"], "report");
        assert_eq!(row("Q3 增长分析报告")["keep"], true);
        assert_eq!(row("部署指南")["cat"], "tutorial");
        assert_eq!(row("修登录")["cat"], "session");
        assert_eq!(row("修登录")["keep"], false);
        assert_eq!(row("交接")["cat"], "process", "导入自 ops/ 的交接：{c}");
        assert_eq!(row("课程大纲")["cat"], "course");
        // 执行：剔除清单里判为剔除的 + 等确认的
        let mut drop: Vec<String> = rows.iter().filter(|r| r["keep"] == false).map(|r| r["key"].as_str().unwrap().to_string()).collect();
        drop.extend(c["held"].as_array().unwrap().iter().map(|r| r["key"].as_str().unwrap().to_string()));
        let (st, a) = call(&app, &token, "POST", "/field/curate/apply", &serde_json::json!({"drop": drop}).to_string()).await;
        assert_eq!(st, 200, "{a}");
        assert_eq!(a["trashed"], 2, "{a} {drop:?}");
        assert_eq!(a["hidden"], 1, "{a}");
        assert!(!root.join("Notes/Sources/会话/修登录.md").exists());
        assert!(root.join(".litmus/local/trash/Notes/Sources/会话/修登录.md").exists(), "进了回收站");
        assert!(ops.join("progress.md").exists(), "库外原件不动");
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let titles: Vec<&str> = d["docs"].as_array().unwrap().iter().map(|x| x["t"].as_str().unwrap()).collect();
        assert!(titles.contains(&"课程大纲") && titles.contains(&"部署指南") && !titles.contains(&"进度") && !titles.contains(&"修登录"), "{titles:?}");
        // 重新扫描：剔掉的不会回来
        let (_, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if call(&app, &token, "GET", "/jobs", "").await.1["busy"] == false {
                break;
            }
        }
        let (_, j) = call(&app, &token, "GET", "/jobs", "").await;
        assert_eq!(j["held"], 0, "{j}");
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        assert!(!d["docs"].as_array().unwrap().iter().any(|x| x["t"] == "进度"));
        // 整批撤销：库内的回到原处，库外的回到场上
        let (st, u) = call(&app, &token, "POST", "/field/curate/undo", &serde_json::json!({"batch": a["batch"]}).to_string()).await;
        assert_eq!(st, 200, "{u}");
        assert_eq!(u["restored"], 3, "{u}");
        assert!(root.join("Notes/Sources/会话/修登录.md").exists());
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        assert!(d["docs"].as_array().unwrap().iter().any(|x| x["t"] == "进度"));
        // 放回来的：重新筛一遍也照你说的留着
        let (_, _) = call(&app, &token, "POST", "/field/curate", r#"{"scope":"all"}"#).await;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if call(&app, &token, "GET", "/field/curate", "").await.1["job"]["running"] == false {
                break;
            }
        }
        let (_, c2) = call(&app, &token, "GET", "/field/curate", "").await;
        let r = c2["rows"].as_array().unwrap().iter().find(|r| r["title"] == "修登录").unwrap().clone();
        assert_eq!((r["keep"].as_bool(), r["by"].as_str()), (Some(true), Some("you")), "{r}");
        // 设置：把「会话」改成保留，清单跟着变
        let (_, cfg) = call(&app, &token, "GET", "/field/curate", "").await;
        let mut conf = cfg["config"].clone();
        conf["drop"] = serde_json::json!(["process", "generated"]);
        conf["keep"].as_array_mut().unwrap().push(serde_json::json!("session"));
        let (st, _) = call(&app, &token, "POST", "/field/curate/config", &conf.to_string()).await;
        assert_eq!(st, 200);
        assert!(root.join(".litmus/curate.json").is_file(), "设置随库同步");
    }

    #[tokio::test]
    async fn batch_import_copies_a_whole_source_in_the_background() {
        let (app, token, dir) = app();
        let outside = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(outside.path().join(format!("资料{i}.md")), format!("# 资料{i}\n\n第 {i} 份资料的正文，讲同步与冲突。")).unwrap();
        }
        let (_, r) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": {"kind": "folder", "name": "下载", "path": outside.path().to_string_lossy()}}).to_string()).await;
        let id = r["source"]["id"].as_str().unwrap().to_string();
        let (_, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if call(&app, &token, "GET", "/jobs", "").await.1["busy"] == false {
                break;
            }
        }
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let keys: Vec<String> = d["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").map(|x| x["p"].as_str().unwrap().to_string()).collect();
        assert_eq!(keys.len(), 5);
        // 选中两篇入库（默认复制）
        let (st, r) = call(&app, &token, "POST", "/field/ext/import-batch", &serde_json::json!({"keys": [keys[0], keys[1], "Notes/不是库外的.md"]}).to_string()).await;
        assert_eq!(st, 200, "{r}");
        assert_eq!(r["total"], 2);
        let mut j = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            j = call(&app, &token, "GET", "/jobs", "").await.1;
            if j["busy"] == false {
                break;
            }
        }
        let imp = j["jobs"].as_array().unwrap().iter().find(|x| x["kind"] == "import").unwrap().clone();
        assert_eq!((imp["state"].as_str(), imp["items"].as_u64()), (Some("done"), Some(2)), "{j}");
        // 整个来源剩下的：估算 3 篇，入库后场上没有库外的了，原件都还在
        let (_, e) = call(&app, &token, "POST", "/field/ext/import-batch", &serde_json::json!({"source": id, "dry": true}).to_string()).await;
        assert_eq!(e["count"], 3);
        let (_, _) = call(&app, &token, "POST", "/field/ext/import-batch", &serde_json::json!({"source": id}).to_string()).await;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if call(&app, &token, "GET", "/jobs", "").await.1["busy"] == false {
                break;
            }
        }
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        assert_eq!(d["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").count(), 0);
        assert_eq!(std::fs::read_dir(dir.path().join("Notes/Sources/下载")).unwrap().count(), 5);
        assert!((0..5).all(|i| outside.path().join(format!("资料{i}.md")).exists()), "复制进库：原件留着");
        let (st, _) = call(&app, &token, "POST", "/field/ext/import-batch", &serde_json::json!({"source": id}).to_string()).await;
        assert_eq!(st, 400, "都入库了：没东西可入");
    }

    #[tokio::test]
    async fn outside_sources_are_indexed_in_place_and_imported_on_demand() {
        let (app, token, dir) = app();
        seed(&app, &token).await;
        let outside = tempfile::tempdir().unwrap();
        let o = outside.path();
        std::fs::write(o.join("同步笔记.md"), "# 外面的同步笔记\n\n同步 冲突 合并 两边保留 CRDT 设备 离线。").unwrap();
        std::fs::write(o.join("发布.txt"), "发布 站点 部署 线上 构建 静态 域名 证书。").unwrap();
        std::fs::write(o.join("页面.html"), "<html><head><title>网页标题</title></head><body><h1>部署手册</h1><p>部署 发布 线上 服务器。</p></body></html>").unwrap();
        std::fs::write(o.join("忽略.bin"), "x").unwrap();
        std::fs::create_dir_all(o.join(".hidden")).unwrap();
        std::fs::write(o.join(".hidden/秘密.md"), "# 不该收").unwrap();
        // 不能把库自己当来源
        let (st, _) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": {"kind": "folder", "path": dir.path().join("Notes").to_string_lossy()}}).to_string()).await;
        assert_eq!(st, 400);
        let (st, r) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": {"kind": "folder", "name": "下载", "path": o.to_string_lossy()}}).to_string()).await;
        assert_eq!(st, 200, "{r}");
        let id = r["source"]["id"].as_str().unwrap().to_string();
        let (st, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
        assert_eq!(st, 200);
        let mut list = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            list = call(&app, &token, "GET", "/sources", "").await.1;
            if list["sources"][0]["scan"]["running"] == false {
                break;
            }
        }
        assert_eq!(list["sources"][0]["items"], 3, "md / txt / html 三篇，隐藏目录与不认识的扩展名不收：{list}");
        // 场里出现库外节点：kind=ext，带来源名；原文件没被复制进库
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let ext: Vec<&Value> = d["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").collect();
        assert_eq!(ext.len(), 3, "{d}");
        assert!(ext.iter().all(|x| x["s"] == "下载" && x["p"].as_str().unwrap().starts_with(&format!("ext:{id}/"))));
        assert!(ext.iter().any(|x| x["t"] == "部署手册"), "html 取正文标题");
        assert!(!dir.path().join("Notes/同步笔记.md").exists());
        // 显色能找到库外的；结晶能读库外的原文
        let (_, react) = call(&app, &token, "GET", "/field/react?q=%E5%90%8C%E6%AD%A5%E5%86%B2%E7%AA%81", "").await;
        let docs = d["docs"].as_array().unwrap();
        let key = ext.iter().find(|x| x["t"] == "外面的同步笔记").unwrap()["p"].as_str().unwrap().to_string();
        let idx = docs.iter().position(|x| x["p"] == key.as_str()).unwrap() as u64;
        assert!(react["hits"].as_array().unwrap().iter().any(|h| h[0] == idx), "{react}");
        let (_, c) = call(&app, &token, "POST", "/field/crystal", &serde_json::json!({"q": "同步冲突怎么办", "paths": [key]}).to_string()).await;
        assert!(c["text"].as_str().unwrap().contains("外面的同步笔记"), "{c}");
        // 整理不碰库外的
        let (_, plan) = call(&app, &token, "POST", "/field/organize", "{}").await;
        assert!(!plan.to_string().contains("ext:"), "{plan}");
        // 打开：只读正文
        let (st, e) = call(&app, &token, "GET", &format!("/field/ext?p={}", urlenc(&key)), "").await;
        assert_eq!(st, 200);
        assert_eq!(e["src"], "下载");
        assert!(e["html"].as_str().unwrap().contains("两边保留") && e["exists"] == true);
        // 复制进库：原件留着
        let (st, imp) = call(&app, &token, "POST", "/field/ext/import", &serde_json::json!({"p": key, "mode": "copy"}).to_string()).await;
        assert_eq!(st, 200, "{imp}");
        let md = std::fs::read_to_string(dir.path().join(imp["path"].as_str().unwrap())).unwrap();
        assert!(md.starts_with("# 外面的同步笔记") && md.contains("导入自") && md.contains("两边保留"), "{md}");
        assert!(o.join("同步笔记.md").exists());
        // 移进库：原件进 Assets，原处没了，索引里也去掉
        let txt_key = ext.iter().find(|x| x["p"].as_str().unwrap().ends_with("发布.txt")).unwrap()["p"].as_str().unwrap().to_string();
        let (st, mv) = call(&app, &token, "POST", "/field/ext/import", &serde_json::json!({"p": txt_key, "mode": "move"}).to_string()).await;
        assert_eq!(st, 200, "{mv}");
        assert_eq!(mv["moved"], true);
        assert!(!o.join("发布.txt").exists());
        assert!(mv["asset"].is_null(), "纯文本移进库不另存附件：正文就是原件");
        assert!(std::fs::read_to_string(dir.path().join(mv["path"].as_str().unwrap())).unwrap().contains("证书"));
        // 网页之类：原件进 Assets/imported/，正文链过去
        let html_key = ext.iter().find(|x| x["p"].as_str().unwrap().ends_with("页面.html")).unwrap()["p"].as_str().unwrap().to_string();
        let (_, hv) = call(&app, &token, "POST", "/field/ext/import", &serde_json::json!({"p": html_key, "mode": "move"}).to_string()).await;
        assert_eq!(hv["asset"], "Assets/imported/页面.html", "{hv}");
        assert!(dir.path().join("Assets/imported/页面.html").is_file() && !o.join("页面.html").exists());
        assert!(std::fs::read_to_string(dir.path().join(hv["path"].as_str().unwrap())).unwrap().contains("](Assets/imported/页面.html)"));
        let (_, d2) = call(&app, &token, "GET", "/field/data", "").await;
        // 入了库就以库里那篇为准：复制进库的原件也不再以库外虚线圈出现，场上只有库里那三篇
        assert_eq!(d2["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").count(), 0, "{d2}");
        let was = d["docs"].as_array().unwrap().iter().find(|x| x["p"] == key.as_str()).unwrap().clone();
        let now = d2["docs"].as_array().unwrap().iter().find(|x| x["p"] == imp["path"]).unwrap().clone();
        assert_eq!((now["x"].clone(), now["y"].clone()), (was["x"].clone(), was["y"].clone()), "入库的那篇原地出现，接替虚线圈的位置");
        // 重新扫描也不回来（原件还在原处）
        let (_, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if call(&app, &token, "GET", "/sources", "").await.1["sources"][0]["scan"]["running"] == false {
                break;
            }
        }
        let (_, d2) = call(&app, &token, "GET", "/field/data", "").await;
        assert_eq!(d2["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").count(), 0, "{d2}");
        // 停用来源：场里的库外节点消失；移除：索引清掉，原文件不动
        let mut src: Value = list["sources"][0].clone();
        src["enabled"] = serde_json::json!(false);
        let (_, _) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": src}).to_string()).await;
        let (_, d3) = call(&app, &token, "GET", "/field/data", "").await;
        assert_eq!(d3["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").count(), 0);
        let (st, _) = call(&app, &token, "DELETE", &format!("/sources/{id}"), "").await;
        assert_eq!(st, 200);
        assert!(o.join("同步笔记.md").exists());
    }

    #[tokio::test]
    async fn agent_sessions_rest_and_google_sources() {
        let (app, token, _dir) = app();
        // Claude Code 会话目录
        let sess = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(sess.path().join("-Users-x-InputFlow")).unwrap();
        std::fs::write(sess.path().join("-Users-x-InputFlow/a.jsonl"), r#"{"type":"user","cwd":"/Users/x/InputFlow","message":{"role":"user","content":"候选词排序怎么改"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"结论：按频率降序，同频按最近使用。"}]}}"#).unwrap();
        let (_, r) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": {"kind": "agent", "agent": "claude", "name": "Claude Code", "path": sess.path().to_string_lossy()}}).to_string()).await;
        let aid = r["source"]["id"].as_str().unwrap().to_string();
        // 一个假的 JSON 接口 + 假的 Google
        use axum::{routing::{get, post}, Router};
        let api = Router::new()
            .route("/notes", get(|h: HeaderMap| async move {
                let ok = h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer tk-123");
                axum::Json(if ok { serde_json::json!({"data": {"items": [{"id": 1, "name": "接口里的一条", "content": "同步 冲突 的说明", "updated_at": "2026-01-02T00:00:00Z"}]}}) } else { serde_json::json!({"data": {"items": []}}) })
            }))
            .route("/token", post(|| async { axum::Json(serde_json::json!({"access_token": "ya29"})) }))
            .route("/files", get(|| async { axum::Json(serde_json::json!({"files": [{"id": "f1", "name": "云端方案", "modifiedTime": "2026-02-03T01:02:03Z", "webViewLink": "https://docs/f1"}]})) }))
            .route("/files/f1/export", get(|| async { "# 云端方案\n\n发布 站点 部署".to_string() }))
            .route("/users/me/messages", get(|| async { axum::Json(serde_json::json!({"messages": [{"id": "m1"}]})) }))
            .route("/users/me/messages/m1", get(|| async { axum::Json(serde_json::json!({"internalDate": "1767225600000", "payload": {"headers": [{"name": "Subject", "value": "周会纪要"}, {"name": "From", "value": "a@x"}], "mimeType": "text/plain", "body": {"data": "5L2g5aW977yM6L-Z5piv5q2j5paH"}}})) }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, api).await.unwrap() });
        let base = format!("http://{addr}");
        let (_, _) = call(&app, &token, "POST", "/secrets", r#"{"name":"demo_token","value":"tk-123"}"#).await;
        let (_, r) = call(&app, &token, "POST", "/sources", &serde_json::json!({"source": {"kind": "rest", "name": "我的接口", "rest": {"url": format!("{base}/notes"), "headers": {"Authorization": "Bearer {{secret:demo_token}}"}, "items": "/data/items", "title": "name", "body": "content", "date": "updated_at"}}}).to_string()).await;
        let rid = r["source"]["id"].as_str().unwrap().to_string();
        std::env::set_var("LITMUS_GOOGLE_OAUTH", &base);
        std::env::set_var("LITMUS_GOOGLE_DRIVE", &base);
        std::env::set_var("LITMUS_GOOGLE_GMAIL", &base);
        let (_, r) = call(&app, &token, "POST", "/sources", r#"{"source":{"kind":"google","name":"Google","services":["drive","gmail"]}}"#).await;
        let gid = r["source"]["id"].as_str().unwrap().to_string();
        // 没存 client id：授权给出可执行的提示
        let (st, _) = call(&app, &token, "GET", &format!("/sources/google/start?id={gid}"), "").await;
        assert_eq!(st, 409);
        for (n, v) in [("google_client_id", "cid"), ("google_client_secret", "cs"), (&*format!("google_refresh:{gid}"), "rt")] {
            let (_, _) = call(&app, &token, "POST", "/secrets", &serde_json::json!({"name": n, "value": v}).to_string()).await;
        }
        let (st, s) = call(&app, &token, "GET", &format!("/sources/google/start?id={gid}&origin=http://127.0.0.1:7717"), "").await;
        assert_eq!(st, 200);
        assert!(s["url"].as_str().unwrap().contains("drive.readonly") && s["url"].as_str().unwrap().contains("gmail.readonly"));
        for id in [&aid, &rid, &gid] {
            let (st, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
            assert_eq!(st, 200);
        }
        let mut list = Value::Null;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            list = call(&app, &token, "GET", "/sources", "").await.1;
            if list["sources"].as_array().unwrap().iter().all(|s| s["scan"]["running"] == false) {
                break;
            }
        }
        let by = |id: &str| list["sources"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
        assert_eq!(by(&aid)["items"], 1, "{list}");
        assert_eq!(by(&rid)["items"], 1, "{list}");
        assert_eq!(by(&gid)["items"], 2, "{list}");
        let (_, d) = call(&app, &token, "GET", "/field/data", "").await;
        let titles: Vec<String> = d["docs"].as_array().unwrap().iter().filter(|x| x["k"] == "ext").map(|x| x["t"].as_str().unwrap().to_string()).collect();
        // 会话按筛选规则默认扣下等确认，不直接上场；其余照常出现
        assert!(!titles.iter().any(|t| t.starts_with("[InputFlow]")), "{titles:?}");
        assert!(titles.contains(&"接口里的一条".to_string()) && titles.contains(&"云端方案".to_string()) && titles.contains(&"周会纪要".to_string()), "{titles:?}");
        let (_, cur) = call(&app, &token, "GET", "/field/curate", "").await;
        let held = cur["held"].as_array().unwrap();
        assert_eq!(held.len(), 1, "{cur}");
        assert_eq!(held[0]["cat"], "session");
        assert!(held[0]["title"].as_str().unwrap().starts_with("[InputFlow] 候选词排序怎么改"));
        let key = held[0]["key"].as_str().unwrap().to_string();
        let (_, e) = call(&app, &token, "GET", &format!("/field/ext?p={}", urlenc(&key)), "").await;
        assert!(e["source"].as_str().unwrap().contains("## 结论") && e["source"].as_str().unwrap().contains("按频率降序"));
        let mail = d["docs"].as_array().unwrap().iter().find(|x| x["t"] == "周会纪要").unwrap()["p"].as_str().unwrap().to_string();
        let (_, m) = call(&app, &token, "GET", &format!("/field/ext?p={}", urlenc(&mail)), "").await;
        assert!(m["source"].as_str().unwrap().contains("你好，这是正文"));
        // 会话不是文件：只能复制进库
        let (st, imp) = call(&app, &token, "POST", "/field/ext/import", &serde_json::json!({"p": mail, "mode": "move"}).to_string()).await;
        assert_eq!(st, 200);
        assert_eq!(imp["moved"], false);
    }

    #[tokio::test]
    async fn connection_is_written_to_toml_and_mcp_source_reports_errors() {
        let (app, token, dir) = app();
        let cfg = dir.path().join("litmus.toml");
        let before = std::fs::read_to_string(&cfg).unwrap();
        std::fs::write(&cfg, format!("# 我的注释\n{before}")).unwrap();
        let (st, _) = call(&app, &token, "POST", "/sources/connection", r#"{"name":"bad name","command":"x"}"#).await;
        assert_eq!(st, 400);
        let (st, _) = call(&app, &token, "POST", "/sources/connection", r#"{"name":"linkto","command":"/nonexistent/linkto","args":["mcp","--stdio"]}"#).await;
        assert_eq!(st, 200);
        let (_, _) = call(&app, &token, "POST", "/sources/connection", r#"{"name":"linkto","command":"/nonexistent/linkto2","args":[]}"#).await;
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.starts_with("# 我的注释") && after.matches("[[connections]]").count() == 1 && after.contains("linkto2"), "{after}");
        assert_eq!(kernel_core::Vault::open(dir.path()).unwrap().config.connections[0].command, "/nonexistent/linkto2");
        let (_, r) = call(&app, &token, "POST", "/sources", r#"{"source":{"kind":"mcp","connection":"linkto"}}"#).await;
        let id = r["source"]["id"].as_str().unwrap().to_string();
        let (_, _) = call(&app, &token, "POST", &format!("/sources/{id}/scan"), "").await;
        let mut list = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            list = call(&app, &token, "GET", "/sources", "").await.1;
            if list["sources"][0]["scan"]["running"] == false {
                break;
            }
        }
        assert!(list["sources"][0]["scan"]["error"].as_str().unwrap().contains("linkto"), "{list}");
    }

    /// 0.20 接回来的：评论（正文、回复、解决）、移动时位置和评论跟着走、文件夹移动改链接、
    /// 复制不覆盖、回收站按份找回和彻底删除、分支名校验、笔记库不能同步到 GitHub。
    #[tokio::test]
    async fn restored_classic_features_keep_their_promises() {
        let (app, t, dir) = app();
        seed(&app, &t).await;
        let root = dir.path();
        let pos = |p: &str| -> Value {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(root.join(".litmus/local/field.json")).unwrap()).unwrap();
            v["pos"][p].clone()
        };
        call(&app, &t, "GET", "/field/data", "").await;
        let before = pos("Notes/同步/协议.md");
        assert!(!before.is_null());

        // 评论：正文要存下（以前 add 把 text 丢了）
        let (st, r) = call(&app, &t, "POST", "/comments", r#"{"path":"Notes/同步/协议.md","op":"add","start":2,"end":6,"quote":"同步协议","text":"这里要写清楚","author":"我"}"#).await;
        assert_eq!(st, 200);
        let id = r["comments"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(r["comments"][0]["text"], "这里要写清楚");
        let (_, r) = call(&app, &t, "POST", "/comments", &format!(r#"{{"path":"Notes/同步/协议.md","op":"reply","id":"{id}","text":"好","author":"我"}}"#)).await;
        assert_eq!(r["comments"][0]["replies"][0]["text"], "好");

        // 改名：位置、评论跟过去
        let (st, _) = call(&app, &t, "POST", "/doc/move", r#"{"from":"Notes/同步/协议.md","to":"Notes/同步/同步协议.md"}"#).await;
        assert_eq!(st, 200);
        assert_eq!(pos("Notes/同步/同步协议.md"), before, "改名后场上的位置应跟过去");
        let (_, r) = call(&app, &t, "GET", &format!("/comments?path={}", urlenc("Notes/同步/同步协议.md")), "").await;
        assert_eq!(r["comments"].as_array().unwrap().len(), 1, "评论应跟过去");

        // 文件夹移动：按路径写的链接改好，位置、评论跟过去
        call(&app, &t, "PUT", &format!("/doc?path={}", urlenc("Notes/发布/索引.md")), "# 索引\n\n见 [[Notes/同步/冲突]] 和 [多设备](Notes/同步/设备.md)。").await;
        call(&app, &t, "GET", "/field/data", "").await;
        let dev = pos("Notes/同步/设备.md");
        let (st, r) = call(&app, &t, "POST", "/dir/move", r#"{"from":"Notes/同步","to":"Notes/协作/同步"}"#).await;
        assert_eq!(st, 200, "{r}");
        assert!(r["relinked"].as_u64().unwrap() >= 1);
        let idx = std::fs::read_to_string(root.join("Notes/发布/索引.md")).unwrap();
        assert!(idx.contains("[[Notes/协作/同步/冲突]]") && idx.contains("](Notes/协作/同步/设备.md)"), "{idx}");
        assert_eq!(pos("Notes/协作/同步/设备.md"), dev);
        let (_, r) = call(&app, &t, "GET", &format!("/comments?path={}", urlenc("Notes/协作/同步/同步协议.md")), "").await;
        assert_eq!(r["comments"].as_array().unwrap().len(), 1, "文件夹移动后评论也跟过去");

        // 复制不覆盖
        let (st, _) = call(&app, &t, "POST", "/doc/copy", r#"{"from":"Notes/发布/站点.md","to":"Notes/发布/部署.md"}"#).await;
        assert_eq!(st, 409);
        assert!(std::fs::read_to_string(root.join("Notes/发布/部署.md")).unwrap().contains("部署流程"));
        let (st, _) = call(&app, &t, "POST", "/dir/copy", r#"{"from":"Notes/没有","to":"Notes/没有 副本"}"#).await;
        assert_eq!(st, 404);
        assert!(!root.join("Notes/没有 副本").exists(), "源不存在时不该留下空文件夹");

        // 回收站：同一路径删两次，找回最近那份；按 trash 精确彻底删除
        let p = urlenc("Notes/发布/域名.md");
        call(&app, &t, "DELETE", &format!("/doc?path={p}"), "").await;
        call(&app, &t, "PUT", &format!("/doc?path={p}"), "# 域名 第二版").await;
        call(&app, &t, "DELETE", &format!("/doc?path={p}"), "").await;
        let (_, tr) = call(&app, &t, "GET", "/trash", "").await;
        let es = tr["entries"].as_array().unwrap();
        assert_eq!(es.len(), 2);
        assert!(es[0]["trash"].is_string());
        let (st, _) = call(&app, &t, "POST", "/trash/restore", r#"{"path":"Notes/发布/域名.md"}"#).await;
        assert_eq!(st, 200);
        assert!(std::fs::read_to_string(root.join("Notes/发布/域名.md")).unwrap().contains("第二版"), "应找回最近删的那份");
        let (_, tr) = call(&app, &t, "GET", "/trash", "").await;
        let key = tr["entries"][0]["trash"].as_str().unwrap().to_string();
        let (st, r) = call(&app, &t, "POST", "/trash/purge", &format!(r#"{{"path":"Notes/发布/域名.md","trash":"{key}"}}"#)).await;
        assert_eq!((st, r["purged"].as_u64()), (200, Some(1)));
        let (_, tr) = call(&app, &t, "GET", "/trash", "").await;
        assert!(tr["entries"].as_array().unwrap().is_empty());

        // 「Notes/」不是一篇文档：筛选不能把整个库挪进回收站
        let (_, r) = call(&app, &t, "POST", "/field/curate/apply", r#"{"drop":["Notes/","Notes"]}"#).await;
        assert!(root.join("Notes/发布/部署.md").exists(), "{r}");
        assert!(!kernel_core::is_safe_doc_path("Notes/") && !kernel_core::is_safe_doc_path("Notes/a/") && !kernel_core::is_safe_doc_path("Notes/./a.md"));

        // 评论随删除停进回收站：删了 A、把 B 改名成 A，新的 A 不带旧评论；找回旧 A 时评论一起回来
        let a = "Notes/发布/站点.md";
        call(&app, &t, "POST", "/comments", &format!(r#"{{"path":"{a}","op":"add","quote":"发布","text":"旧的评论"}}"#)).await;
        call(&app, &t, "DELETE", &format!("/doc?path={}", urlenc(a)), "").await;
        let (st, _) = call(&app, &t, "POST", "/doc/move", &format!(r#"{{"from":"Notes/发布/部署.md","to":"{a}"}}"#)).await;
        assert_eq!(st, 200);
        let (_, r) = call(&app, &t, "GET", &format!("/comments?path={}", urlenc(a)), "").await;
        assert!(r["comments"].as_array().unwrap().is_empty(), "新的 A 不该带旧 A 的评论：{r}");
        call(&app, &t, "POST", "/doc/move", &format!(r#"{{"from":"{a}","to":"Notes/发布/部署.md"}}"#)).await;
        call(&app, &t, "POST", "/trash/restore", &format!(r#"{{"path":"{a}"}}"#)).await;
        let (_, r) = call(&app, &t, "GET", &format!("/comments?path={}", urlenc(a)), "").await;
        assert_eq!(r["comments"][0]["text"], "旧的评论");

        // 对话流只收 Notes/Agent/ 下的
        let (st, _) = call(&app, &t, "POST", "/conflow/digest", r#"{"path":"Notes/发布/部署.md"}"#).await;
        assert_eq!(st, 400);

        // 分支名：不能被 git 当成选项
        for bad in ["-b", "--orphan=x", "a..b", "a b", ""] {
            let (st, _) = call(&app, &t, "POST", "/git/checkout", &serde_json::json!({"name": bad, "create": true}).to_string()).await;
            assert_eq!(st, 400, "分支名 {bad:?} 应被拒绝");
        }
        // 笔记库不能同步到 GitHub
        let (st, r) = call(&app, &t, "POST", "/git/sync", r#"{"remote":"git@github.com:someone/notes.git"}"#).await;
        assert_eq!(st, 400);
        assert!(r.to_string().contains("GitHub"));
    }

    /// 搬家：Obsidian 库先出清单、再后台搬，链接和图接好，原处不动；能撤销；大文件不再 413
    #[tokio::test]
    async fn wizard_moves_an_obsidian_vault_in() {
        let (app, t, dir) = app();
        let ob = tempfile::tempdir().unwrap();
        let v = ob.path().join("我的库");
        std::fs::create_dir_all(v.join(".obsidian")).unwrap();
        std::fs::create_dir_all(v.join("卡片")).unwrap();
        std::fs::create_dir_all(v.join("附件")).unwrap();
        std::fs::write(v.join("首页.md"), "见 [[卡片/概念|概念卡]] 和 ![[图 1.png]]\n").unwrap();
        std::fs::write(v.join("卡片/概念.md"), "# 概念\n\n回到 [[首页]]").unwrap();
        std::fs::write(v.join("附件/图 1.png"), b"\x89PNG fake").unwrap();
        let body = serde_json::json!({ "path": v.to_string_lossy() }).to_string();
        let (st, plan) = call(&app, &t, "POST", "/field/wizard/plan", &body).await;
        assert_eq!(st, 200, "{plan}");
        assert_eq!(plan["kind"], "obsidian");
        assert_eq!((plan["docs"].as_u64(), plan["assets"].as_u64(), plan["broken"].as_u64()), (Some(2), Some(1), Some(0)), "{plan}");
        assert!(!dir.path().join("Notes/导入/我的库").exists(), "清单阶段不写");
        let (st, _) = call(&app, &t, "POST", "/field/wizard/run", &body).await;
        assert_eq!(st, 200);
        let mut runs = Value::Null;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            runs = call(&app, &t, "GET", "/field/wizard", "").await.1;
            if runs["job"]["running"] == false { break; }
        }
        assert_eq!(runs["runs"][0]["docs"], 2, "{runs}");
        let home = std::fs::read_to_string(dir.path().join("Notes/导入/我的库/首页.md")).unwrap();
        assert!(home.contains("[[Notes/导入/我的库/卡片/概念|概念卡]]") && home.contains("![](Assets/imported/"), "{home}");
        assert!(home.starts_with("---\nimported_from: "), "{home}");
        let img = home.split("](").nth(1).unwrap().split(')').next().unwrap();
        assert!(dir.path().join(img).exists(), "图要存进库：{img}");
        assert!(v.join("首页.md").exists(), "原处不动");
        let (_, j) = call(&app, &t, "GET", "/jobs", "").await;
        assert!(j["jobs"].as_array().unwrap().iter().any(|x| x["kind"] == "wizard"), "{j}");
        // 再搬一次：内容一样的跳过，不会多出「首页 2」
        call(&app, &t, "POST", "/field/wizard/run", &body).await;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if call(&app, &t, "GET", "/field/wizard", "").await.1["job"]["running"] == false { break; }
        }
        assert!(!dir.path().join("Notes/导入/我的库/首页 2.md").exists());
        // 撤销第一次：文档进回收站，附件拿掉
        let id = runs["runs"][0]["id"].as_str().unwrap().to_string();
        let (st, r) = call(&app, &t, "POST", "/field/wizard/undo", &format!(r#"{{"id":"{id}"}}"#)).await;
        assert_eq!((st, r["trashed"].as_u64()), (200, Some(2)), "{r}");
        assert!(!dir.path().join("Notes/导入/我的库/首页.md").exists());
        assert!(!dir.path().join(img).exists());
        // 库里的文件夹不能当来源搬
        let inside = serde_json::json!({ "path": dir.path().join("Notes").to_string_lossy() }).to_string();
        let (st, _) = call(&app, &t, "POST", "/field/wizard/plan", &inside).await;
        assert_eq!(st, 400);
        // 3MB 的附件能传（以前 2MB 就 413）
        let big = "x".repeat(3 << 20);
        let (st, _) = call(&app, &t, "POST", "/asset?name=big.bin", &big).await;
        assert_eq!(st, 200, "大请求体不该被拒");
    }

    /// 凭证库加密：明文迁移、文件里看不到值；模型密钥存进凭证库而不是随库同步的 litmus.toml；恢复码
    #[tokio::test]
    async fn secrets_are_encrypted_and_ai_key_leaves_litmus_toml() {
        let (app, t, dir) = app();
        let root = dir.path();
        std::fs::write(root.join(".litmus/local/secrets.json"), r#"{"secrets":[{"name":"openai","value":"sk-old"}]}"#).unwrap();
        let (_, r) = call(&app, &t, "GET", "/secrets", "").await;
        assert_eq!(r["secrets"][0]["value"], "sk-old");
        assert_eq!(r["state"]["encrypted"], true);
        let raw = std::fs::read_to_string(root.join(".litmus/local/secrets.json")).unwrap();
        assert!(!raw.contains("sk-old"), "读一次就该加密：{raw}");
        // 旧做法：密钥写在 litmus.toml
        let cfg = root.join("litmus.toml");
        let mut toml = std::fs::read_to_string(&cfg).unwrap();
        toml.push_str("\n[ai]\nbase_url = \"http://127.0.0.1:9\"\nmodel = \"m\"\napi_key = \"sk-in-toml\"\n");
        std::fs::write(&cfg, toml).unwrap();
        let (_, a) = call(&app, &t, "GET", "/field/ai", "").await;
        let _ = a;
        let (st, _) = call(&app, &t, "POST", "/field/ai/move-key", "").await;
        assert_eq!(st, 200);
        assert!(!std::fs::read_to_string(&cfg).unwrap().contains("sk-in-toml"));
        // 新填的密钥也只进凭证库
        let (st, _) = call(&app, &t, "POST", "/field/ai", r#"{"base_url":"http://127.0.0.1:9","model":"m","api_key":"sk-new"}"#).await;
        assert_eq!(st, 200);
        assert!(!std::fs::read_to_string(&cfg).unwrap().contains("sk-new"));
        assert_eq!(kernel_core::secrets::get(root, "ai_api_key").as_deref(), Some("sk-new"));
        let v = kernel_core::Vault::open(root).unwrap();
        assert_eq!(v.config.ai.unwrap().api_key, "sk-new", "打开库时从凭证库拿密钥");
        let (_, a) = call(&app, &t, "GET", "/field/ai", "").await;
        assert_eq!((a["has_key"].as_bool(), a["key_in_toml"].as_bool()), (Some(true), Some(false)));
        let (st, c) = call(&app, &t, "GET", "/secrets/recovery", "").await;
        assert_eq!(st, 200);
        assert_eq!(c["code"].as_str().unwrap().chars().filter(|c| c.is_ascii_hexdigit()).count(), 64);
    }

    /// 逐题复习：按规则出题、打分排期、到期队列、改题删题；跨文档事实冲突能找出来、能标为不算
    #[tokio::test]
    async fn questions_are_scheduled_one_by_one_and_conflicting_facts_surface() {
        let (app, t, _dir) = app();
        seed(&app, &t).await;
        let doc = "Notes/同步/端口.md";
        call(&app, &t, "PUT", &format!("/doc?path={}", urlenc(doc)), "# 同步服务\n\n同步 冲突 合并 CRDT 设备 离线。\n\n- 端口：7700\n- 部署目录：/www/wwwroot/litmus\n\n## 冲突策略\n\n同名不同内容另存冲突副本，绝不静默覆盖任何一侧。\n\n离线后用 **CRDT** 合并改动。").await;
        call(&app, &t, "PUT", &format!("/doc?path={}", urlenc("Notes/同步/端口旧.md")), "# 同步服务（旧）\n\n同步 冲突 合并 CRDT 设备 离线。\n\n- 端口：7800\n- 部署目录：/www/wwwroot/litmus\n").await;
        let (st, r) = call(&app, &t, "POST", "/field/learn/questions", &format!(r#"{{"path":"{doc}"}}"#)).await;
        assert_eq!(st, 200, "{r}");
        assert_eq!(r["by"], "rule");
        let qs = r["qs"].as_array().unwrap();
        assert!(qs.len() >= 3, "{r}");
        assert!(qs.iter().any(|q| q["q"].as_str().unwrap().contains("端口") && q["a"] == "7700"), "{r}");
        let (_, d) = call(&app, &t, "GET", "/field/learn/due", "").await;
        assert_eq!(d["due_total"].as_u64(), Some(qs.len() as u64), "新出的题马上能复习");
        let id = qs[0]["id"].as_str().unwrap().to_string();
        let (st, r) = call(&app, &t, "POST", "/field/learn/review", &format!(r#"{{"id":"{id}","grade":"good"}}"#)).await;
        assert_eq!(st, 200);
        assert_eq!(r["q"]["interval"].as_f64(), Some(3.0));
        let (_, d) = call(&app, &t, "GET", "/field/learn/due", "").await;
        assert_eq!(d["due_total"].as_u64(), Some(qs.len() as u64 - 1));
        // 重新出题：复习过的那道留着（进度不丢）
        let (_, r) = call(&app, &t, "POST", "/field/learn/questions", &format!(r#"{{"path":"{doc}"}}"#)).await;
        assert!(r["qs"].as_array().unwrap().iter().any(|q| q["id"] == id.as_str() && q["reps"] == 1), "{r}");
        // 自己改一道、删一道、加一道
        let (_, r) = call(&app, &t, "POST", "/field/learn/question", &format!(r#"{{"id":"{id}","a":"改过的答案"}}"#)).await;
        assert!(r["qs"].as_array().unwrap().iter().any(|q| q["a"] == "改过的答案" && q["by"] == "you"));
        let n0 = r["qs"].as_array().unwrap().len();
        let (_, r) = call(&app, &t, "POST", "/field/learn/question", &format!(r#"{{"id":"{id}","delete":true}}"#)).await;
        assert_eq!(r["qs"].as_array().unwrap().len(), n0 - 1);
        let (_, r) = call(&app, &t, "POST", "/field/learn/question", &format!(r#"{{"path":"{doc}","q":"默认端口？","a":"7700"}}"#)).await;
        assert_eq!(r["qs"].as_array().unwrap().len(), n0);
        // 冲突：两篇讲同一件事，端口写得不一样；部署目录一样的不算
        let (_, o) = call(&app, &t, "POST", "/field/organize", "{}").await;
        let cf = o["conflicts"].as_array().unwrap();
        let port = cf.iter().find(|c| c["key"] == "端口").unwrap_or_else(|| panic!("{o}"));
        let vals: Vec<&str> = port["entries"].as_array().unwrap().iter().map(|e| e["value"].as_str().unwrap()).collect();
        assert!(vals.contains(&"7700") && vals.contains(&"7800"), "{port}");
        assert!(!cf.iter().any(|c| c["key"] == "部署目录"), "{o}");
        let sig = port["sig"].as_str().unwrap();
        call(&app, &t, "POST", "/field/organize/conflict-ignore", &serde_json::json!({ "sig": sig }).to_string()).await;
        let (_, o) = call(&app, &t, "POST", "/field/organize", "{}").await;
        assert!(!o["conflicts"].as_array().unwrap().iter().any(|c| c["key"] == "端口"), "标为不算以后不再出现");
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
