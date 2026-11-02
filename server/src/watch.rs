//! 观察目录（桌面独占）：盯着几个文件夹，有新文档出现时提醒你导入。
//!
//! - 只提醒，绝不自动导入；关掉就什么都不看。
//! - 只看这台电脑：配置在 `~/.config/litmus/watch/`（`LITMUS_WATCH_DIR` 可改），
//!   不进库、不同步、不上传。
//! - 判断在本地做：扩展名、大小、正文、是否和库里重名/已导入过，不请求任何模型。

use super::{audit_log, check_token, desktop_guard, err, import_one, now_secs, read_import_index, write_import_index, AppState, ImportOutcome};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

/// 默认收的文档类型（和导入向导一致，外加 .doc/.pages）。
const DOC_EXTS: [&str; 14] = ["md", "markdown", "txt", "html", "htm", "docx", "doc", "pdf", "pptx", "xlsx", "epub", "rtf", "odt", "pages"];
/// 能直接读正文做判断的。
const TEXTY: [&str; 5] = ["md", "markdown", "txt", "html", "htm"];
/// 标题里出现这些词，更像一篇要留下的东西。
const KEYWORDS: [&str; 20] = ["笔记", "草稿", "会议", "纪要", "方案", "计划", "总结", "备忘", "想法", "提纲", "复盘", "周报", "日记", "note", "draft", "meeting", "memo", "idea", "plan", "summary"];
const MAX_WALK: usize = 20_000;
const MAX_PREVIEW_BYTES: u64 = 4 * 1024;
const MAX_SHA_BYTES: u64 = 3 * 1024 * 1024;

const MIN_BYTES: u64 = 256;
const MAX_BYTES: u64 = 50 * 1024 * 1024;
const WORTH: f32 = 4.0;
const MAYBE: f32 = 2.4;

#[derive(Clone, Serialize, Deserialize)]
pub struct Conf {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default = "yes")]
    pub recursive: bool,
    /// 第一次打开时回看几天；0 = 不限（不建议）。
    #[serde(default = "days7")]
    pub days: u32,
    /// 用户点了「不用」的文件：path -> mtime。mtime 变了会重新提醒。
    #[serde(default)]
    pub dismissed: BTreeMap<String, u64>,
}
fn yes() -> bool {
    true
}
fn days7() -> u32 {
    7
}
impl Default for Conf {
    fn default() -> Self {
        Conf { enabled: false, folders: Vec::new(), recursive: true, days: 7, dismissed: BTreeMap::new() }
    }
}

#[derive(Clone, Serialize, Default)]
pub struct Candidate {
    pub path: String,
    pub name: String,
    pub title: String,
    pub ext: String,
    pub size: u64,
    pub mtime: u64,
    pub folder: String,
    pub score: f32,
    /// worth | maybe
    pub verdict: String,
    pub reasons: Vec<String>,
    pub preview: String,
    pub duplicate: bool,
}

#[derive(Clone, Serialize, Default)]
pub struct Snapshot {
    pub candidates: Vec<Candidate>,
    pub scanned_at: u64,
    pub scanning: bool,
    pub error: Option<String>,
    pub folders: Vec<String>,
}

fn snapshot() -> &'static Mutex<Snapshot> {
    static S: OnceLock<Mutex<Snapshot>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Snapshot::default()))
}
static SCAN_REQ: AtomicBool = AtomicBool::new(false);

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LITMUS_WATCH_DIR") {
        return PathBuf::from(d);
    }
    home().unwrap_or_else(std::env::temp_dir).join(".config/litmus/watch")
}
fn conf_path() -> PathBuf {
    dir().join("watch.json")
}
fn expand(raw: &str) -> String {
    let s = raw.trim();
    if s == "~" {
        return home().map(|h| h.display().to_string()).unwrap_or_else(|| s.to_string());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = home() {
            return h.join(rest).display().to_string();
        }
    }
    s.to_string()
}
fn load_from(p: &Path) -> Conf {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_to(p: &Path, c: &Conf) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(c).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}
fn load_conf() -> Conf {
    load_from(&conf_path())
}
fn save_conf(c: &Conf) -> Result<(), String> {
    save_to(&conf_path(), c)
}
fn file_stamp() -> (u64, u64) {
    std::fs::metadata(conf_path()).ok()
        .map(|m| (m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0), m.len()))
        .unwrap_or((0, 0))
}
fn mtime_of(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn guard(st: &AppState, h: &HeaderMap) -> Result<(), Response> {
    check_token(st, h).map_err(|e| e.into_response())?;
    desktop_guard(st).map_err(|e| e.into_response())?;
    Ok(())
}

/// 启动观察线程：只在这台电脑的桌面 App 里跑。
pub fn spawn(state: Arc<AppState>) -> anyhow::Result<()> {
    if !state.desktop {
        return Ok(());
    }
    std::thread::Builder::new().name("litmus-watch".into()).spawn(move || supervise(state))?;
    Ok(())
}

fn supervise(st: Arc<AppState>) {
    let (tx, rx) = std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
    let mut watcher: Option<notify::RecommendedWatcher> = None;
    let mut sig = String::new();
    let mut stamp = (0u64, 0u64);
    let mut last_scan = 0u64;
    let mut dirty = true;
    loop {
        let conf = load_conf();
        let now_stamp = file_stamp();
        if now_stamp != stamp {
            stamp = now_stamp;
            let new_sig = format!("{}|{}|{}", conf.enabled, conf.recursive, conf.folders.join("\n"));
            if new_sig != sig {
                sig = new_sig;
                watcher = None; // 关掉旧的，换新目录/模式
                if conf.enabled && !conf.folders.is_empty() {
                    match notify::recommended_watcher(tx.clone()) {
                        Ok(mut w) => {
                            use notify::Watcher;
                            let mode = if conf.recursive { notify::RecursiveMode::Recursive } else { notify::RecursiveMode::NonRecursive };
                            for f in &conf.folders {
                                if let Err(e) = w.watch(Path::new(f), mode) {
                                    eprintln!("[litmus] watch {f}: {e}");
                                }
                            }
                            watcher = Some(w);
                        }
                        Err(e) => eprintln!("[litmus] watch init: {e}"),
                    }
                }
                dirty = true;
            }
        }
        let _ = watcher.as_ref(); // 活到下一次配置变化
        if !conf.enabled {
            {
                let mut s = snapshot().lock().unwrap();
                if !s.candidates.is_empty() || s.scanned_at != 0 || s.scanning {
                    *s = Snapshot { folders: conf.folders.clone(), ..Default::default() };
                }
            }
            let _ = rx.recv_timeout(Duration::from_secs(1));
            continue;
        }
        let now = now_secs();
        let forced = SCAN_REQ.swap(false, Ordering::SeqCst);
        if dirty || forced || last_scan == 0 || now.saturating_sub(last_scan) >= 60 {
            if dirty {
                // 事件防抖：等文件写完再读
                std::thread::sleep(Duration::from_millis(650));
                while rx.try_recv().is_ok() {}
            }
            {
                let mut s = snapshot().lock().unwrap();
                s.scanning = true;
                s.folders = conf.folders.clone();
            }
            let snap = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan(&st)))
                .unwrap_or_else(|_| Snapshot { error: Some("扫描时出了点问题，下次再试".into()), folders: conf.folders.clone(), ..Default::default() });
            *snapshot().lock().unwrap() = snap;
            last_scan = now_secs();
            dirty = false;
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => dirty = true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_secs(1)),
        }
    }
}

fn scan(st: &AppState) -> Snapshot {
    let conf = load_conf();
    let now = now_secs();
    let mut out = Snapshot { scanned_at: now, folders: conf.folders.clone(), ..Default::default() };
    if !conf.enabled || conf.folders.is_empty() {
        return out;
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let notes = root.join("Notes");
    let stems = vault_stems(&notes);
    let imported = read_import_index(&root);
    let cutoff = if conf.days == 0 { 0 } else { now.saturating_sub(conf.days as u64 * 86400) };

    let mut files: Vec<PathBuf> = Vec::new();
    let mut walked = 0usize;
    for f in &conf.folders {
        walk(Path::new(f), conf.recursive, &root, &mut files, &mut walked);
    }
    out.error = if walked > MAX_WALK { Some(format!("文件夹太大，只看了前 {MAX_WALK} 项")) } else { None };

    let mut seen = HashSet::<String>::new();
    for p in files {
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        let len = meta.len();
        if len < MIN_BYTES || len > MAX_BYTES {
            continue;
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        if cutoff > 0 && mtime < cutoff {
            continue;
        }
        let key = p.display().to_string();
        if conf.dismissed.get(&key).copied() == Some(mtime) {
            continue;
        }
        if !seen.insert(key) {
            continue;
        }
        if let Some(c) = evaluate(&p, len, mtime, now, &stems, &imported) {
            out.candidates.push(c);
        }
    }
    out.candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.mtime.cmp(&a.mtime)));
    out.candidates.truncate(40);
    out
}

fn walk(dir: &Path, recursive: bool, vault: &Path, out: &mut Vec<PathBuf>, count: &mut usize) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if *count > MAX_WALK {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            *count += 1;
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with("~$") || name == "node_modules" || name == "target" || name == "Library" {
                continue;
            }
            if p.is_dir() {
                if recursive && !p.starts_with(vault) {
                    stack.push(p);
                }
                continue;
            }
            if p.starts_with(vault) {
                continue; // 库自己的文件不算“外面的新文档”
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            if DOC_EXTS.contains(&ext.as_str()) {
                out.push(p);
            }
        }
    }
}

fn vault_stems(notes: &Path) -> HashSet<String> {
    static CACHE: OnceLock<Mutex<(u64, HashSet<String>)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new((0, HashSet::new())));
    let mut g = cache.lock().unwrap();
    let now = now_secs();
    if !g.1.is_empty() && now.saturating_sub(g.0) < 120 {
        return g.1.clone();
    }
    let mut out = HashSet::new();
    let mut stack = vec![notes.to_path_buf()];
    let mut n = 0usize;
    while let Some(d) = stack.pop() {
        if n > MAX_WALK {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            n += 1;
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
                if !name.starts_with('.') {
                    stack.push(p);
                }
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            if ext == "md" || ext == "markdown" || ext == "html" || ext == "htm" {
                if let Some(stem) = p.file_stem() {
                    out.insert(stem.to_string_lossy().to_lowercase());
                }
            }
        }
    }
    *g = (now, out.clone());
    out
}

fn evaluate(p: &Path, len: u64, mtime: u64, now: u64, stems: &HashSet<String>, imported: &serde_json::Map<String, Value>) -> Option<Candidate> {
    let name = p.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
    if !DOC_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut score = base_score(&ext);
    let mut reasons = vec![kind_label(&ext).to_string()];

    if len < 2048 {
        score -= 0.4;
        reasons.push("文件很小".into());
    } else if len <= 200 * 1024 {
        score += 1.0;
        reasons.push(format!("{} KB", (len + 1023) / 1024));
    } else if len <= 2 * 1024 * 1024 {
        score += 0.4;
        reasons.push(format!("{:.1} MB", len as f64 / 1048576.0));
    } else {
        score -= 0.4;
        reasons.push(format!("{:.1} MB，偏大", len as f64 / 1048576.0));
    }

    let age = now.saturating_sub(mtime);
    if age < 86400 {
        score += 1.0;
        reasons.push("最近一天".into());
    } else if age < 7 * 86400 {
        score += 0.4;
        reasons.push("最近一周".into());
    }

    let lower = name.to_lowercase();
    if KEYWORDS.iter().any(|k| lower.contains(k)) {
        score += 0.8;
        reasons.push("标题像笔记".into());
    }
    if stems.contains(&stem) {
        score -= 1.2;
        reasons.push("库里已有同名".into());
    }

    let mut preview = String::new();
    let mut title = stem.clone();
    if TEXTY.contains(&ext.as_str()) {
        if let Some(head) = read_head(p, MAX_PREVIEW_BYTES) {
            let text = head.replace('\u{feff}', "");
            preview = preview_of(&text);
            if let Some(t) = first_title(&text) {
                title = t;
            }
            let n = text.chars().count();
            if n >= 40 {
                score += 0.6;
                reasons.push("有正文".into());
            } else {
                score -= 0.8;
                reasons.push("正文很短".into());
            }
            let cjk = text.chars().filter(|c| ('一'..='鿿').contains(c)).count();
            if cjk >= 20 {
                score += 0.2;
            }
        } else {
            score -= 1.0;
            reasons.push("读不出文字".into());
        }
    } else {
        reasons.push("导入时转成文字".into());
        // 读不出正文的类型不直接判“值得”：最高只到 maybe，等你打开看一眼。
        score = score.min(WORTH - 0.1);
    }

    // 预览里不再重复标题。
    if !preview.is_empty() && !title.is_empty() {
        if let Some(rest) = preview.strip_prefix(&*title) {
            let r = rest.trim_start_matches(|c: char| c.is_whitespace() || "·-—:：|".contains(c));
            preview = r.chars().take(220).collect();
        }
    }

    // 同内容已经导入过：直接不提醒（mtime 变了会重新出现）。
    if len <= MAX_SHA_BYTES {
        if let Some(sha) = sha_file(p) {
            if imported.contains_key(&sha) {
                return None;
            }
        }
    }

    if score < MAYBE || stem.is_empty() {
        return None;
    }
    let verdict = if score >= WORTH { "worth" } else { "maybe" };
    Some(Candidate {
        path: p.display().to_string(),
        name,
        title: if title.is_empty() { stem.clone() } else { title },
        ext,
        size: len,
        mtime,
        folder: p.parent().map(|x| x.display().to_string()).unwrap_or_default(),
        score: (score * 10.0).round() / 10.0,
        verdict: verdict.into(),
        reasons,
        preview,
        duplicate: stems.contains(&stem),
    })
}

fn base_score(ext: &str) -> f32 {
    match ext {
        "md" | "markdown" => 3.4,
        "txt" => 2.8,
        "docx" => 2.6,
        "pdf" => 2.5,
        "doc" => 2.4,
        "html" | "htm" => 2.2,
        "pptx" | "pages" => 2.0,
        "xlsx" | "epub" | "rtf" | "odt" => 1.8,
        _ => 0.6,
    }
}

fn kind_label(ext: &str) -> &'static str {
    match ext {
        "md" | "markdown" => "Markdown",
        "txt" => "纯文本",
        "html" | "htm" => "网页",
        "docx" | "doc" | "odt" | "rtf" | "pages" => "文档",
        "pdf" => "PDF",
        "pptx" => "演示",
        "xlsx" => "表格",
        "epub" => "电子书",
        _ => "文件",
    }
}

fn read_head(p: &Path, n: u64) -> Option<String> {
    let f = std::fs::File::open(p).ok()?;
    let mut buf = Vec::new();
    f.take(n).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn sha_file(p: &Path) -> Option<String> {
    let bytes = std::fs::read(p).ok()?;
    Some(super::sha256_hex(&bytes))
}

fn preview_of(text: &str) -> String {
    let mut body = text;
    // YAML frontmatter 不是正文。
    if let Some(rest) = body.strip_prefix("---") {
        if let Some(i) = rest.find("\n---") {
            body = &rest[i + 4..];
        }
    }
    // 行首的 markdown 记号（#、>、-、1.）不是正文。
    let mut plain = String::with_capacity(body.len());
    for line in body.lines() {
        let mut s = line.trim_start();
        let mut cut = true;
        while cut {
            cut = false;
            for mark in ["#", ">", "- ", "* ", "+ "] {
                if let Some(r) = s.strip_prefix(mark) {
                    s = r.trim_start();
                    cut = true;
                }
            }
            let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && s[digits..].starts_with(". ") {
                s = s[digits + 2..].trim_start();
                cut = true;
            }
        }
        plain.push_str(s);
        plain.push('\n');
    }
    let mut out = String::new();
    let mut in_tag = false;
    let mut n = 0usize;
    for c in plain.chars() {
        if c == '<' {
            in_tag = true;
            continue;
        }
        if c == '>' {
            in_tag = false;
            continue;
        }
        if in_tag {
            continue;
        }
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        n += 1;
        if n >= 220 {
            break;
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_title(text: &str) -> Option<String> {
    // 开头的 frontmatter 不是标题（下载来的 md 常带）。
    let mut body = text;
    if let Some(rest) = body.strip_prefix("---") {
        if let Some(i) = rest.find("\n---") {
            body = &rest[i + 4..];
        }
    }
    body.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with("---") && !l.starts_with("<!--"))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|t| !t.is_empty() && t.chars().count() <= 120)
}

/* ───────── HTTP ───────── */

/// GET /watch：设置 + 当前候选 + 配置目录。
pub async fn status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let conf = load_conf();
    let snap = snapshot().lock().unwrap().clone();
    Json(json!({ "conf": conf, "state": snap, "dir": dir().display().to_string() })).into_response()
}

/// PUT /watch：开关、目录、是否递归、回看天数。
pub async fn configure(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let mut conf = load_conf();
    if let Some(v) = req.get("enabled").and_then(|v| v.as_bool()) {
        conf.enabled = v;
    }
    if let Some(v) = req.get("recursive").and_then(|v| v.as_bool()) {
        conf.recursive = v;
    }
    if let Some(v) = req.get("days").and_then(|v| v.as_u64()) {
        conf.days = v.clamp(1, 90) as u32;
    }
    if let Some(arr) = req.get("folders").and_then(|v| v.as_array()) {
        let mut clean: Vec<String> = Vec::new();
        for it in arr {
            let Some(raw) = it.as_str() else { continue };
            if raw.trim().is_empty() {
                continue;
            }
            let p = expand(raw);
            if !Path::new(&p).is_dir() {
                return err(StatusCode::BAD_REQUEST, format!("找不到这个文件夹：{}", raw.trim())).into_response();
            }
            if !clean.contains(&p) {
                clean.push(p);
            }
        }
        conf.folders = clean;
    }
    if let Err(e) = save_conf(&conf) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    SCAN_REQ.store(true, Ordering::SeqCst);
    Json(json!({ "conf": conf })).into_response()
}

/// POST /watch/scan：让后台马上扫一遍。
pub async fn scan_now(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    SCAN_REQ.store(true, Ordering::SeqCst);
    Json(json!({ "ok": true })).into_response()
}

/// POST /watch/dismiss {paths: []}：这次不用，mtime 变了再提醒。
pub async fn dismiss(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let paths: Vec<String> = req.get("paths").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
    let mut conf = load_conf();
    let mut n = 0usize;
    for p in &paths {
        if let Some(m) = mtime_of(Path::new(p)) {
            conf.dismissed.insert(p.clone(), m);
            n += 1;
        }
    }
    if conf.dismissed.len() > 2000 {
        let keep: BTreeMap<String, u64> = conf.dismissed.iter().rev().take(1000).map(|(k, v)| (k.clone(), *v)).collect();
        conf.dismissed = keep;
    }
    if let Err(e) = save_conf(&conf) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    {
        let mut s = snapshot().lock().unwrap();
        s.candidates.retain(|c| !paths.contains(&c.path));
    }
    Json(json!({ "dismissed": n })).into_response()
}

/// POST /watch/import {paths: []}：只导入用户勾选的文件（复制，原件留着）。
pub async fn import(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = guard(&st, &h) {
        return e;
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let paths: Vec<String> = req.get("paths").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
    if paths.is_empty() {
        return err(StatusCode::BAD_REQUEST, "没有选中要导入的文件").into_response();
    }
    let st2 = st.clone();
    let got = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let conf = load_conf();
        let root = { st2.kernel.lock().unwrap().vault.root.clone() };
        let dest_root = root.join("Notes").join("Imported");
        let mut index = read_import_index(&root);
        let mut imported: Vec<Value> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        for p in &paths {
            let path = Path::new(p);
            if !conf.folders.iter().any(|f| path.starts_with(f)) {
                failed.push(json!({ "path": p, "why": "不在观察目录里" }));
                continue;
            }
            if !path.is_file() {
                failed.push(json!({ "path": p, "why": "文件已经不在了" }));
                continue;
            }
            let Some(src) = conf.folders.iter().filter(|f| path.starts_with(f)).max_by_key(|f| f.len()).map(|f| f.as_str()) else {
                failed.push(json!({ "path": p, "why": "不在观察目录里" }));
                continue;
            };
            match import_one(&st2, &root, &mut index, Path::new(src), path, &dest_root) {
                ImportOutcome::Copied { dest, .. } => imported.push(json!({ "from": p, "to": dest })),
                ImportOutcome::Duplicate => failed.push(json!({ "path": p, "why": "这份内容已经导入过" })),
                ImportOutcome::Skipped { pending } => failed.push(json!({
                    "path": p,
                    "why": if pending { "需要转码工具，这次没转成" } else { "读不出内容" }
                })),
            }
        }
        write_import_index(&root, &index);
        if !imported.is_empty() {
            let mut k = st2.kernel.lock().unwrap();
            let _ = k.sync_all();
        }
        // 导入成功的记成「不用」，避免同一份内容反复提醒；文件以后再改 mtime 会重新出现。
        let mut conf = load_conf();
        for it in &imported {
            if let Some(p) = it["from"].as_str() {
                if let Some(m) = mtime_of(Path::new(p)) {
                    conf.dismissed.insert(p.to_string(), m);
                }
            }
        }
        let _ = save_conf(&conf);
        audit_log(&root.join(".litmus/local"), "watch-import", &json!({
            "imported": imported.len(), "failed": failed.len()
        }));
        Ok(json!({ "imported": imported, "failed": failed }))
    })
    .await;
    match got {
        Ok(Ok(v)) => {
            let imported_paths: Vec<String> = v["imported"].as_array().map(|a| a.iter().filter_map(|x| x["from"].as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
            {
                let mut s = snapshot().lock().unwrap();
                s.candidates.retain(|c| !imported_paths.contains(&c.path));
            }
            Json(v).into_response()
        }
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("litmus-watch-{tag}-{}-{}", std::process::id(), now_secs()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn conf_roundtrip_and_defaults() {
        let d = tempdir("conf");
        let p = d.join("watch.json");
        let c = Conf { enabled: true, folders: vec![d.display().to_string()], recursive: false, days: 3, dismissed: BTreeMap::new() };
        save_to(&p, &c).unwrap();
        let got = load_from(&p);
        assert!(got.enabled);
        assert_eq!(got.days, 3);
        assert!(!got.recursive);
        assert_eq!(got.folders.len(), 1);
        let missing = load_from(&d.join("nope.json"));
        assert!(!missing.enabled);
        assert_eq!(missing.days, 7);
        assert!(missing.recursive);
    }

    #[test]
    fn scores_text_above_binary_and_keeps_recent_markdown() {
        let d = tempdir("score");
        let md = d.join("会议纪要.md");
        std::fs::write(&md, "# 周会

".to_string() + &"讨论了导入流程和提醒的边界。".repeat(12)).unwrap();
        let now = now_secs();
        let c = evaluate(&md, std::fs::metadata(&md).unwrap().len(), now - 60, now, &HashSet::new(), &serde_json::Map::new()).expect("md 应该被收下");
        assert_eq!(c.verdict, "worth");
        assert!(c.title.contains("周会"));

        let bin = d.join("x.pdf");
        std::fs::write(&bin, vec![0u8; 4096]).unwrap();
        let c2 = evaluate(&bin, 4096, now - 60, now, &HashSet::new(), &serde_json::Map::new()).expect("pdf 可以提醒");
        assert_eq!(c2.verdict, "maybe", "读不出正文时最多 maybe");

        let old = d.join("旧.md");
        std::fs::write(&old, "短".to_string()).unwrap();
        assert!(evaluate(&old, 3, now - 30 * 86400, now, &HashSet::new(), &serde_json::Map::new()).is_none());
    }

    #[test]
    fn duplicate_stem_lowers_score_and_imported_sha_is_skipped() {
        let d = tempdir("dup");
        let p = d.join("计划.md");
        let body = "# 计划

".to_string() + &"这件事要分三步做。".repeat(20);
        std::fs::write(&p, &body).unwrap();
        let now = now_secs();
        let mut stems = HashSet::new();
        stems.insert("计划".to_string());
        let c = evaluate(&p, std::fs::metadata(&p).unwrap().len(), now - 60, now, &stems, &serde_json::Map::new()).expect("同名也还能是 maybe");
        assert!(c.duplicate);
        assert!(c.reasons.contains(&"库里已有同名".to_string()));

        let mut imported = serde_json::Map::new();
        imported.insert(super::super::sha256_hex(body.as_bytes()), json!({}));
        assert!(evaluate(&p, body.len() as u64, now - 60, now, &HashSet::new(), &imported).is_none());
    }

    #[test]
    fn walk_skips_hidden_and_the_vault() {
        let d = tempdir("walk");
        let vault = d.join("vault");
        std::fs::create_dir_all(vault.join("Notes")).unwrap();
        std::fs::write(vault.join("Notes/a.md"), "库里
").unwrap();
        std::fs::write(d.join("a.md"), "外面
").unwrap();
        std::fs::write(d.join(".hidden.md"), "隐藏
").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/b.md"), "子目录
").unwrap();
        let mut files = Vec::new();
        let mut n = 0;
        walk(&d, true, &vault, &mut files, &mut n);
        let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        assert!(paths.iter().any(|p| p.ends_with("a.md") && !p.contains("vault")));
        assert!(paths.iter().any(|p| p.ends_with("sub/b.md")));
        assert!(!paths.iter().any(|p| p.ends_with(".hidden.md")));
        assert!(!paths.iter().any(|p| p.contains("vault/Notes")));
    }

    #[test]
    fn preview_and_title_are_human_readable() {
        assert_eq!(preview_of("<h1>标题</h1>\n\n正文  内容"), "标题 正文 内容");
        assert_eq!(preview_of("# 标题\n\n> 引文\n- 一条\n1. 两条"), "标题 引文 一条 两条");
        assert_eq!(preview_of("---\na: 1\n---\n# 真标题\n正文"), "真标题 正文");
        assert_eq!(first_title("# 周报\n正文").as_deref(), Some("周报"));
        assert_eq!(first_title("---\na: 1\n---\n# 真标题").as_deref(), Some("真标题"));
    }
}
