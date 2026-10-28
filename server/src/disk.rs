//! 全盘索引（0.27，桌面独占）：这台电脑上的文件，按名字和内容都能搜到。
//!
//! - 只有桌面壳起的 daemon 有（`LITMUS_DESKTOP=1`）。线上部署一律 404，不会去扫服务器的磁盘。
//! - 索引是**这台设备**的，不跟着库走：放在 `~/.config/litmus/disk/`（`LITMUS_DISK_DIR` 可改），
//!   换库不用重扫，也不会被同步、备份带出去。
//! - 不往场上铺：几十万个文件不是知识。一行输入「找」的时候，在下面多给几个「这台电脑上」的结果；
//!   /全盘 打开完整的搜索和设置。值得的再「收进库里」。
//! - 名字：所有文件（跳过隐藏、系统、依赖、缓存、各个库）；内容：文字类和文档类（md、txt、Word、PDF……）。
//! - 增量：大小和修改时间没变就不重读；扫完把已经不在的删掉。后台低优先级，可暂停、停下。

use super::{audit_log, check_token, desktop_guard, err, AppState};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_FILES: usize = 400_000;
const MAX_TEXTS: usize = 60_000;
const MAX_BODY: usize = 20_000;
const TEXT_BYTES: u64 = 4 * 1024 * 1024;
const DOC_BYTES: u64 = 25 * 1024 * 1024;
const TEXT: [&str; 12] = ["md", "markdown", "txt", "rst", "org", "tex", "html", "htm", "csv", "json", "yaml", "yml"];
const DOCS: [&str; 9] = ["docx", "pptx", "xlsx", "epub", "odt", "rtf", "pdf", "odp", "ods"];
/// 名字上一看就是包的目录（.app 之类）：不往里走
const BUNDLES: [&str; 9] = [".app", ".photoslibrary", ".bundle", ".framework", ".xcodeproj", ".xcworkspace", ".plugin", ".kext", ".musiclibrary"];
/// 哪儿都不进的
const SKIP: [&str; 11] = ["node_modules", "target", "Library", "__pycache__", "dist", "build", "venv", "site-packages", "AppData", "Caches", "DerivedData"];
/// 只在磁盘根上跳过的（家目录里叫 dev、tmp 的文件夹照扫）
const SKIP_TOP: [&str; 12] = ["Applications", "System", "Volumes", "proc", "sys", "dev", "tmp", "private", "Windows", "Program Files", "Program Files (x86)", "cores"];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DiskConf {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_roots")]
    pub roots: Vec<String>,
    /// 不扫的路径（前缀；可以用 ~）
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 读内容（关掉就只按名字找）
    #[serde(default = "yes")]
    pub content: bool,
}
fn default_roots() -> Vec<String> {
    vec!["~".into()]
}
fn yes() -> bool {
    true
}
impl Default for DiskConf {
    fn default() -> Self {
        DiskConf { enabled: false, roots: default_roots(), exclude: Vec::new(), content: true }
    }
}

#[derive(Default, Clone, Serialize)]
pub struct DiskJob {
    pub running: bool,
    pub paused: bool,
    #[serde(skip)]
    pub stop: bool,
    pub phase: String,
    pub seen: usize,
    pub total: usize,
    pub done: usize,
    pub files: usize,
    pub texts: usize,
    pub started: u64,
    pub finished: u64,
    pub error: Option<String>,
}

fn job() -> &'static Mutex<DiskJob> {
    static J: std::sync::OnceLock<Mutex<DiskJob>> = std::sync::OnceLock::new();
    J.get_or_init(|| Mutex::new(DiskJob::default()))
}
pub fn snapshot() -> DiskJob {
    job().lock().unwrap().clone()
}
pub fn control(action: &str) -> bool {
    let mut j = job().lock().unwrap();
    if !j.running {
        return false;
    }
    match action {
        "pause" => j.paused = true,
        "resume" => j.paused = false,
        "stop" => {
            j.stop = true;
            j.paused = false;
        }
        _ => return false,
    }
    true
}
/// 暂停就等着；停下返回 false。
fn go() -> bool {
    loop {
        let (stop, paused) = {
            let j = job().lock().unwrap();
            (j.stop, j.paused)
        };
        if stop {
            return false;
        }
        if !paused {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LITMUS_DISK_DIR") {
        return PathBuf::from(d);
    }
    home().unwrap_or_else(std::env::temp_dir).join(".config/litmus/disk")
}
fn conf_path() -> PathBuf {
    dir().join("disk.json")
}
pub fn load_conf() -> DiskConf {
    std::fs::read_to_string(conf_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_conf(c: &DiskConf) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    std::fs::write(conf_path(), serde_json::to_vec_pretty(c).unwrap_or_default())
}

fn open() -> rusqlite::Result<Connection> {
    let _ = std::fs::create_dir_all(dir());
    let c = Connection::open(dir().join("index.db"))?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, name TEXT NOT NULL, ext TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL, gen INTEGER NOT NULL, text INTEGER NOT NULL DEFAULT 0, title TEXT NOT NULL DEFAULT '');
         CREATE INDEX IF NOT EXISTS files_gen ON files(gen);
         CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(path UNINDEXED, name, title, body, tokenize='trigram');
         CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);",
    )?;
    Ok(c)
}
fn meta_get(c: &Connection, k: &str) -> Option<String> {
    c.query_row("SELECT v FROM meta WHERE k=?", [k], |r| r.get(0)).optional().ok().flatten()
}
fn meta_set(c: &Connection, k: &str, v: &str) {
    let _ = c.execute("INSERT INTO meta(k,v) VALUES(?,?) ON CONFLICT(k) DO UPDATE SET v=excluded.v", [k, v]);
}

fn expand(p: &str) -> PathBuf {
    super::sources::expand_pub(p)
}

/// 要不要往这个目录里走。
pub fn skip_dir(name: &str, path: &Path, exclude: &[PathBuf], vaults: &[PathBuf]) -> bool {
    if name.starts_with('.') || name.starts_with('$') || SKIP.contains(&name) {
        return true;
    }
    if path.parent().map(|p| p.parent().is_none()).unwrap_or(false) && SKIP_TOP.contains(&name) {
        return true;
    }
    let low = name.to_ascii_lowercase();
    if BUNDLES.iter().any(|b| low.ends_with(b)) {
        return true;
    }
    if exclude.iter().any(|e| path.starts_with(e)) {
        return true;
    }
    // 知识库（自己的和别的）：库里的已经在场上了
    vaults.iter().any(|v| path == v) || path.join("litmus.toml").is_file() || path.join("thirdc.toml").is_file()
}

fn mtime_of(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// 从文件里读出 (标题, 正文)。读不出来返回 None。
fn read_body(p: &Path, ext: &str) -> Option<(String, String)> {
    let stem = p.file_stem()?.to_string_lossy().to_string();
    if ["csv", "json", "yaml", "yml", "tex", "org", "rst"].contains(&ext) {
        let raw = std::fs::read(p).ok()?;
        return Some((stem, kernel_convert::decode_text(&raw)));
    }
    let (title, text, _, _) = super::sources::extract_file_pub(p)?;
    Some((title, text))
}

/// 扫一遍。阻塞，后台线程里跑。
fn scan(vault_root: PathBuf) {
    let conf = load_conf();
    let fail = |e: String| {
        let mut j = job().lock().unwrap();
        j.running = false;
        j.error = Some(e);
        j.finished = now_secs();
    };
    let mut c = match open() {
        Ok(c) => c,
        Err(e) => return fail(format!("打不开索引：{e}")),
    };
    let gen: i64 = meta_get(&c, "gen").and_then(|g| g.parse().ok()).unwrap_or(0) + 1;
    let exclude: Vec<PathBuf> = conf.exclude.iter().filter(|x| !x.trim().is_empty()).map(|x| expand(x.trim())).collect();
    let vaults = vec![vault_root.canonicalize().unwrap_or(vault_root.clone()), vault_root.clone()];
    let roots: Vec<PathBuf> = conf.roots.iter().map(|r| expand(r.trim())).filter(|p| p.is_dir()).collect();
    if roots.is_empty() {
        return fail("要扫的位置都不存在".into());
    }
    {
        let mut j = job().lock().unwrap();
        j.phase = "数文件".into();
    }
    // 第一遍：走目录、记名字；变了的（或新的）文字类、文档类记下来等第二遍读内容
    let mut todo: Vec<(PathBuf, String, i64)> = Vec::new();
    let mut seen = 0usize;
    let mut stack: Vec<PathBuf> = roots.clone();
    let tx_every = 2000;
    let mut pending = 0usize;
    let _ = c.execute_batch("BEGIN");
    let mut stopped = false;
    'walk: while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if !skip_dir(&name, &p, &exclude, &vaults) {
                    stack.push(p);
                }
                continue;
            }
            if name.starts_with('.') || name.starts_with("~$") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            let ext = p.extension().and_then(|x| x.to_str()).map(|x| x.to_ascii_lowercase()).unwrap_or_default();
            let size = meta.len() as i64;
            let mtime = mtime_of(&meta);
            let ps = p.to_string_lossy().to_string();
            let old: Option<(i64, i64, i64)> = c.query_row("SELECT size, mtime, text FROM files WHERE path=?", [&ps], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional().unwrap_or(None);
            let wants_text = conf.content && ((TEXT.contains(&ext.as_str()) && meta.len() <= TEXT_BYTES) || (DOCS.contains(&ext.as_str()) && meta.len() <= DOC_BYTES)) && meta.len() > 0;
            match old {
                Some((s0, m0, t0)) if s0 == size && m0 == mtime && (t0 == 1 || !wants_text) => {
                    let _ = c.execute("UPDATE files SET gen=? WHERE path=?", params![gen, ps]);
                }
                Some((s0, m0, _)) => {
                    let changed = s0 != size || m0 != mtime;
                    let _ = c.execute("UPDATE files SET size=?, mtime=?, gen=?, text=CASE WHEN ? THEN 0 ELSE text END WHERE path=?", params![size, mtime, gen, changed, ps]);
                    if wants_text {
                        todo.push((p.clone(), ext.clone(), mtime));
                    }
                }
                None => {
                    let _ = c.execute("INSERT INTO files(path,name,ext,size,mtime,gen) VALUES(?,?,?,?,?,?)", params![ps, name, ext, size, mtime, gen]);
                    let _ = c.execute("INSERT INTO fts(path,name,title,body) VALUES(?,?,'','')", params![ps, name]);
                    if wants_text {
                        todo.push((p.clone(), ext.clone(), mtime));
                    }
                }
            }
            seen += 1;
            pending += 1;
            if pending >= tx_every {
                let _ = c.execute_batch("COMMIT; BEGIN");
                pending = 0;
                job().lock().unwrap().seen = seen;
                if !go() {
                    stopped = true;
                    break 'walk;
                }
            }
            if seen >= MAX_FILES {
                break 'walk;
            }
        }
    }
    let _ = c.execute_batch("COMMIT");
    // 第二遍：读内容。最近改过的先读；文字类在前，Word/PDF 在后
    todo.sort_by(|a, b| b.2.cmp(&a.2));
    todo.truncate(MAX_TEXTS);
    todo.sort_by_key(|t| if TEXT.contains(&t.1.as_str()) { 0 } else if t.1 == "pdf" { 2 } else { 1 });
    {
        let mut j = job().lock().unwrap();
        j.seen = seen;
        j.total = todo.len();
        j.phase = "读内容".into();
    }
    let _ = c.execute_batch("BEGIN");
    pending = 0;
    if !stopped {
        for (n, (p, ext, _)) in todo.iter().enumerate() {
            if n % 20 == 0 {
                let _ = c.execute_batch("COMMIT; BEGIN");
                {
                    let mut j = job().lock().unwrap();
                    j.done = n;
                    j.phase = if TEXT.contains(&ext.as_str()) { "读内容 · 文字".into() } else { "读内容 · 文档".into() };
                }
                if !go() {
                    stopped = true;
                    break;
                }
            }
            let ps = p.to_string_lossy().to_string();
            let (title, body) = read_body(p, ext).unwrap_or_default();
            let body: String = body.chars().take(MAX_BODY).collect();
            let name = p.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
            let _ = c.execute("DELETE FROM fts WHERE path=?", [&ps]);
            let _ = c.execute("INSERT INTO fts(path,name,title,body) VALUES(?,?,?,?)", params![ps, name, title, body]);
            let _ = c.execute("UPDATE files SET text=1, title=? WHERE path=?", params![title, ps]);
            pending += 1;
            // PDF、Word 要起外部程序或解压：每个之间喘口气
            if !TEXT.contains(&ext.as_str()) {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
    }
    let _ = c.execute_batch("COMMIT");
    let _ = pending;
    // 扫完整一遍才删不在了的（停下的那轮不删）
    if !stopped {
        if let Ok(tx) = c.transaction() {
            let _ = tx.execute("DELETE FROM fts WHERE path IN (SELECT path FROM files WHERE gen < ?)", [gen]);
            let _ = tx.execute("DELETE FROM files WHERE gen < ?", [gen]);
            let _ = tx.commit();
        }
        meta_set(&c, "gen", &gen.to_string());
        meta_set(&c, "last", &now_secs().to_string());
    }
    let (files, texts) = counts(&c);
    let mut j = job().lock().unwrap();
    j.running = false;
    j.files = files;
    j.texts = texts;
    j.done = j.total.min(j.done.max(if stopped { j.done } else { j.total }));
    j.finished = now_secs();
    j.phase = if stopped { "停下了".into() } else { "完成".into() };
    j.error = stopped.then(|| "停下了：已经记下的都能搜，下次接着扫".to_string());
}

fn counts(c: &Connection) -> (usize, usize) {
    let n = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0) as usize;
    (n("SELECT COUNT(*) FROM files"), n("SELECT COUNT(*) FROM files WHERE text=1"))
}

pub fn start(vault_root: PathBuf) -> bool {
    {
        let mut j = job().lock().unwrap();
        if j.running {
            return false;
        }
        *j = DiskJob { running: true, started: now_secs(), phase: "排队".into(), ..Default::default() };
    }
    super::sources::spawn_background("litmus-disk", move || scan(vault_root));
    true
}

/// 搜：三个字以上走全文（名字、标题、内容），短的只比名字。
pub fn search(q: &str, limit: usize) -> rusqlite::Result<Vec<Value>> {
    let c = open()?;
    let terms: Vec<String> = q.split_whitespace().map(|t| t.chars().filter(|ch| *ch != '"').collect::<String>()).filter(|t| !t.is_empty()).collect();
    let mut out = Vec::new();
    if terms.is_empty() {
        return Ok(out);
    }
    let row = |path: String, name: String, title: String, ext: String, size: i64, mtime: i64, snip: String| json!({ "path": path, "name": name, "title": title, "ext": ext, "size": size, "mtime": mtime, "snippet": snip });
    if terms.iter().all(|t| t.chars().count() >= 3) {
        let expr = terms.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" AND ");
        let mut st = c.prepare(
            "SELECT f.path, f.name, f.title, f.ext, f.size, f.mtime, snippet(fts, 3, char(2), char(3), '…', 14)
             FROM fts JOIN files f ON f.path = fts.path WHERE fts MATCH ?1 ORDER BY bm25(fts, 0.0, 10.0, 6.0, 1.0) LIMIT ?2",
        )?;
        let rows = st.query_map(params![expr, limit as i64], |r| Ok(row(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, Option<String>>(6)?.unwrap_or_default())))?;
        out.extend(rows.flatten());
    } else {
        let esc = |t: &str| t.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let like = format!("%{}%", terms.iter().map(|t| esc(t)).collect::<Vec<_>>().join("%"));
        let mut st = c.prepare("SELECT path, name, title, ext, size, mtime FROM files WHERE name LIKE ?1 ESCAPE '\\' ORDER BY mtime DESC LIMIT ?2")?;
        let rows = st.query_map(params![like, limit as i64], |r| Ok(row(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, String::new())))?;
        out.extend(rows.flatten());
    }
    Ok(out)
}

/// 索引里有的才让看、让收：不能借这个接口读电脑上任意文件。
fn indexed(c: &Connection, path: &str) -> Option<(String, String, i64, i64, String)> {
    c.query_row("SELECT name, ext, size, mtime, title FROM files WHERE path=?", [path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional().ok().flatten()
}

// ───────── 接口 ─────────

fn guard(st: &AppState, h: &HeaderMap) -> Result<(), axum::response::Response> {
    check_token(st, h).map_err(|e| e.into_response())?;
    desktop_guard(st).map_err(|e| e.into_response())?;
    Ok(())
}

/// GET /desktop/disk：设置、进度、索引了多少。
pub async fn status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let r = tokio::task::spawn_blocking(|| {
        let c = open().ok();
        let (files, texts) = c.as_ref().map(counts).unwrap_or((0, 0));
        let last = c.as_ref().and_then(|c| meta_get(c, "last")).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let bytes = std::fs::metadata(dir().join("index.db")).map(|m| m.len()).unwrap_or(0);
        json!({ "conf": load_conf(), "job": snapshot(), "files": files, "texts": texts, "last": last, "bytes": bytes, "dir": dir().to_string_lossy() })
    })
    .await
    .unwrap_or(json!({}));
    Json(r).into_response()
}

/// POST /desktop/disk {enabled?, roots?, exclude?, content?, scan?}：改设置；打开或要求时开扫。
pub async fn configure(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let mut c = load_conf();
    let was = c.enabled;
    if let Some(b) = req["enabled"].as_bool() {
        c.enabled = b;
    }
    if let Some(b) = req["content"].as_bool() {
        c.content = b;
    }
    let list = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>());
    if let Some(r) = list(&req["roots"]) {
        if r.is_empty() {
            return err(StatusCode::BAD_REQUEST, "至少留一个要扫的位置").into_response();
        }
        if let Some(bad) = r.iter().find(|x| !expand(x).is_dir()) {
            return err(StatusCode::BAD_REQUEST, format!("找不到文件夹 {bad}")).into_response();
        }
        c.roots = r;
    }
    if let Some(x) = list(&req["exclude"]) {
        c.exclude = x;
    }
    if let Err(e) = save_conf(&c) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    audit_log(&root.join(".litmus/local"), "disk-index", &json!({ "enabled": c.enabled, "roots": c.roots, "exclude": c.exclude, "content": c.content }));
    let started = c.enabled && ((!was && c.enabled) || req["scan"] == true) && start(root);
    // 关掉：停下正在扫的，索引留着（再打开不用从头来）；要删掉点「清空索引」
    if !c.enabled {
        control("stop");
    }
    if req["clear"] == true && !snapshot().running {
        let _ = tokio::task::spawn_blocking(|| {
            if let Ok(c) = open() {
                let _ = c.execute_batch("DELETE FROM fts; DELETE FROM files; DELETE FROM meta;");
                let _ = c.execute_batch("VACUUM");
            }
        })
        .await;
    }
    Json(json!({ "conf": c, "started": started })).into_response()
}

/// GET /desktop/disk/search?q=&limit=
pub async fn find(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    if !load_conf().enabled {
        return Json(json!({ "results": [], "off": true })).into_response();
    }
    let text = q.get("q").cloned().unwrap_or_default();
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(30).clamp(1, 200);
    match tokio::task::spawn_blocking(move || search(&text, limit)).await {
        Ok(Ok(r)) => Json(json!({ "results": r })).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("搜不了：{e}")).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /desktop/disk/peek?path=：看一眼内容（索引里存的那段，没存就当场读文字类）。
pub async fn peek(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let path = q.get("path").cloned().unwrap_or_default();
    let r = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let c = open().map_err(|e| e.to_string())?;
        let (name, ext, size, mtime, title) = indexed(&c, &path).ok_or("索引里没有这个文件")?;
        let body: Option<String> = c.query_row("SELECT body FROM fts WHERE path=?", [&path], |r| r.get(0)).optional().ok().flatten();
        let body = match body.filter(|b| !b.trim().is_empty()) {
            Some(b) => b,
            None if TEXT.contains(&ext.as_str()) && size as u64 <= TEXT_BYTES => read_body(Path::new(&path), &ext).map(|x| x.1.chars().take(MAX_BODY).collect()).unwrap_or_default(),
            None => String::new(),
        };
        Ok(json!({ "path": path, "name": name, "ext": ext, "size": size, "mtime": mtime, "title": title, "body": body, "exists": Path::new(&path).is_file() }))
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e).into_response(),
    }
}

/// POST /desktop/disk/import {path, folder?}：收进库里（复制，原件留着）。
pub async fn import(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req["path"].as_str().unwrap_or("").to_string();
    let folder = req["folder"].as_str().filter(|f| f.starts_with("Notes") && !f.contains("..")).unwrap_or("Notes/Sources/这台电脑").trim_end_matches('/').to_string();
    let p2 = path.clone();
    let got = tokio::task::spawn_blocking(move || -> Result<(String, String, i64), String> {
        let c = open().map_err(|e| e.to_string())?;
        let (name, ext, _, mtime, _) = indexed(&c, &p2).ok_or("索引里没有这个文件")?;
        if !Path::new(&p2).is_file() {
            return Err("这个文件已经不在了（移走或删掉了）".into());
        }
        let (title, text) = read_body(Path::new(&p2), &ext).ok_or("读不出这个文件的文字")?;
        let title = if title.trim().is_empty() { name.rsplit_once('.').map(|x| x.0.to_string()).unwrap_or(name) } else { title };
        Ok((title, text, mtime))
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    let (title, text, mtime) = match got {
        Ok(x) => x,
        Err(e) => return err(StatusCode::NOT_FOUND, e).into_response(),
    };
    if text.trim().is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "这个文件里没读出文字，收进来也是空的").into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let safe: String = title.chars().map(|c| if "/\\:*?\"<>|#^[]".contains(c) { ' ' } else { c }).collect::<String>().trim().chars().take(80).collect();
    let safe = if safe.is_empty() { "导入".to_string() } else { safe };
    let mut target = format!("{folder}/{safe}.md");
    let mut n = 2;
    while root.join(&target).exists() && n < 200 {
        target = format!("{folder}/{safe} {n}.md");
        n += 1;
    }
    let body = text.trim();
    let (head, rest) = match body.lines().next() {
        Some(l) if l.starts_with("# ") => (l.to_string(), body[l.len()..].trim().to_string()),
        _ => (format!("# {title}"), body.to_string()),
    };
    let md = format!("{head}\n\n> 导入自 `{path}`（{}）\n\n{rest}\n", super::field::ymd(mtime));
    if let Err(e) = st.kernel.lock().unwrap().put_doc(&target, &md) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    audit_log(&root.join(".litmus/local"), "disk-import", &json!({ "from": path, "to": target }));
    Json(json!({ "path": target })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_system_and_vaults() {
        let t = tempfile_dir();
        let v = t.join("库");
        std::fs::create_dir_all(&v).unwrap();
        std::fs::write(v.join("litmus.toml"), "").unwrap();
        assert!(skip_dir("node_modules", &t.join("node_modules"), &[], &[]));
        assert!(skip_dir(".git", &t.join(".git"), &[], &[]));
        assert!(skip_dir("Photos Library.photoslibrary", &t.join("Photos Library.photoslibrary"), &[], &[]));
        assert!(skip_dir("库", &v, &[], &[]), "别的库不扫");
        assert!(skip_dir("私人", &t.join("私人"), &[t.join("私人")], &[]));
        assert!(!skip_dir("文稿", &t.join("文稿"), &[], &[]));
        assert!(!skip_dir("dev", &t.join("dev"), &[], &[]), "家里的 dev 照扫");
        assert!(skip_dir("dev", Path::new("/dev"), &[], &[]));
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("litmus-disk-t-{}-{}", std::process::id(), now_secs()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
