//! 数据源（G3）：库外的内容原处索引，导入才进库。
//!
//! - 配置在 `.litmus/local/sources.json`（路径与凭证都是这台设备的，不同步）。
//! - 扫描结果在 `.litmus/local/sources/index.json`，正文抽出来的文字缓存在 `.litmus/local/sources/text/<hash>.txt`；
//!   原文件不动、不复制。知识场把它们当「库外」节点一起铺开、一起显色。
//! - 种类：folder（本地文件夹：md / txt / html / docx / pdf / 截图……）、agent（Claude Code / Codex 会话）、
//!   mcp（连接器的资源，比如 LinkTo）、google（Drive / Gmail / Calendar）、rest（任意 JSON 接口）。
//! - 导入：`move` 把原文件移进库的 `Assets/imported/` 并生成一篇 Markdown；`copy` 只生成 Markdown、原文件留着。

use super::{audit_log, check_token, err, secrets_load, AppState};
use axum::{
    body::Bytes,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 库外内容的版本号：扫描完成或配置变了就加一，知识场据此重建。
pub static REV: AtomicU64 = AtomicU64::new(1);

const MAX_FILES: usize = 4000;
const MAX_TEXT: usize = 60_000;
const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
const TEXT_EXTS: [&str; 6] = ["md", "markdown", "txt", "html", "htm", "rst"];
const DOC_EXTS: [&str; 9] = ["docx", "pptx", "xlsx", "epub", "odt", "rtf", "pdf", "odp", "ods"];
const IMG_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "heic", "gif"];

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Source {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// folder | agent | mcp | google | rest
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub exts: Vec<String>,
    #[serde(default = "yes")]
    pub recursive: bool,
    /// agent：claude | codex
    #[serde(default)]
    pub agent: Option<String>,
    /// mcp：litmus.toml 里 [[connections]] 的名字
    #[serde(default)]
    pub connection: Option<String>,
    /// google：drive / gmail / calendar 任选
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub query: Option<String>,
    /// rest：{url, headers:{k:v}, items:"/data/items", id, title, body, date}
    #[serde(default)]
    pub rest: Option<Value>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub added: u64,
    /// 只接最近的 N 个（按修改时间）。第一次接入时给个上限，先用起来；之后可以放开（None = 全部）。
    #[serde(default)]
    pub limit: Option<usize>,
    /// 对接（github / notion / sanity / openflow / mflow）的设置：仓库、项目、地址、发回去的位置……密钥不在这里，在 /凭证。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conf: Option<Value>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Item {
    /// ext:<源>/<条目>
    pub key: String,
    pub src: String,
    pub title: String,
    /// md | txt | html | docx | pdf | img | agent | mcp | mail | event | drive | rest
    pub kind: String,
    /// 原处：本地绝对路径或 URI / URL
    pub loc: String,
    pub date: String,
    pub mtime: i64,
    pub size: u64,
    pub hash: String,
    #[serde(default)]
    pub note: Option<String>,
    /// 远端版本标记（GitHub blob sha、Sanity _rev、Notion 编辑时间……）：没变就不再抓正文
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Index {
    items: Vec<Item>,
    #[serde(default)]
    scans: HashMap<String, Value>,
    /// 已经入库的库外条目：key → 库里的路径。入了库就以库里那篇为准，原件不再上场、重扫也不回来。
    #[serde(default)]
    imported: HashMap<String, String>,
}

fn dir(root: &Path) -> PathBuf {
    root.join(".litmus/local").join("sources")
}
fn cfg_path(root: &Path) -> PathBuf {
    root.join(".litmus/local").join("sources.json")
}

pub fn load_sources(root: &Path) -> Vec<Source> {
    std::fs::read_to_string(cfg_path(root)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.get("sources").cloned()).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}
fn save_sources(root: &Path, list: &[Source]) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join(".litmus/local"))?;
    std::fs::write(cfg_path(root), serde_json::to_vec_pretty(&json!({ "sources": list })).unwrap_or_default())
}
/// 索引是「读出来改、再整份写回」：几个来源同时扫描时要排队，不然后写的会盖掉先写的。
fn ix_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

fn load_index(root: &Path) -> Index {
    std::fs::read_to_string(dir(root).join("index.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_index(root: &Path, ix: &Index) {
    let d = dir(root);
    let _ = std::fs::create_dir_all(&d);
    let tmp = d.join("index.json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(ix).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, d.join("index.json"));
    }
}
pub const MAX_TEXT_PUB: usize = MAX_TEXT;
pub fn put_text_pub(root: &Path, text: &str) -> String {
    put_text(root, text)
}
pub fn secret_pub(root: &Path, name: &str) -> Option<String> {
    secret(root, name)
}

fn text_path(root: &Path, hash: &str) -> PathBuf {
    dir(root).join("text").join(format!("{hash}.txt"))
}
pub fn read_text(root: &Path, hash: &str) -> String {
    std::fs::read_to_string(text_path(root, hash)).unwrap_or_default()
}
fn put_text(root: &Path, text: &str) -> String {
    let hash = kernel_core::Cas::hash_hex(text.as_bytes())[..20].to_string();
    let p = text_path(root, &hash);
    if !p.exists() {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(&p, text);
    }
    hash
}

/// 索引里的全部条目（不看是否启用、是否被筛掉）。
pub fn all_items(root: &Path) -> Vec<Item> {
    load_index(root).items
}

/// 从索引里拿掉这些条目（筛选剔除），返回拿掉的原样（放回时用）。
pub fn take_items(root: &Path, keys: &[&str]) -> Vec<(String, Value)> {
    let _ixg = ix_lock();
    let mut ix = load_index(root);
    let mut out = Vec::new();
    ix.items.retain(|i| {
        if keys.contains(&i.key.as_str()) {
            out.push((i.key.clone(), serde_json::to_value(i).unwrap_or(json!({}))));
            false
        } else {
            true
        }
    });
    if !out.is_empty() {
        save_index(root, &ix);
        REV.fetch_add(1, Ordering::SeqCst);
    }
    out
}

/// 把拿掉的条目放回索引（撤销剔除）。
pub fn put_items(root: &Path, items: Vec<Value>) {
    let items: Vec<Item> = items.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect();
    if items.is_empty() {
        return;
    }
    let _ixg = ix_lock();
    let mut ix = load_index(root);
    let keys: std::collections::HashSet<String> = items.iter().map(|i| i.key.clone()).collect();
    ix.items.retain(|i| !keys.contains(&i.key));
    ix.items.extend(items);
    save_index(root, &ix);
    REV.fetch_add(1, Ordering::SeqCst);
}

/// 知识场要的：所有已启用来源的条目 + 正文（截断）。筛掉的、等确认的不上场。
pub fn corpus(root: &Path) -> Vec<(Item, String, String)> {
    let enabled: HashMap<String, String> = load_sources(root).into_iter().filter(|s| s.enabled).map(|s| (s.id, s.name)).collect();
    let hidden = super::curate::hidden_keys(root);
    let ix = load_index(root);
    let imported: std::collections::HashSet<String> = ix.imported.iter().filter(|(_, p)| root.join(p).exists()).map(|(k, _)| k.clone()).collect();
    ix
        .items
        .into_iter()
        .filter(|it| !hidden.contains(&it.key) && !imported.contains(&it.key))
        .filter_map(|it| {
            let name = enabled.get(&it.src)?.clone();
            let t = read_text(root, &it.hash);
            Some((it, name, t))
        })
        .collect()
}

pub fn find_item(root: &Path, key: &str) -> Option<(Item, Source)> {
    let it = load_index(root).items.into_iter().find(|i| i.key == key)?;
    let s = load_sources(root).into_iter().find(|s| s.id == it.src)?;
    Some((it, s))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_dir())
}
fn expand(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(p),
    }
}
fn ymd_of(secs: i64) -> String {
    super::field::ymd(secs)
}
fn mtime_of(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ───────── 抽文字 ─────────

/// 外部命令在不在（结果缓存：扫描时每个文件都会问）。
fn has(cmd: &str) -> bool {
    static SEEN: std::sync::OnceLock<Mutex<HashMap<String, bool>>> = std::sync::OnceLock::new();
    let m = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&v) = m.lock().unwrap().get(cmd) {
        return v;
    }
    let v = std::process::Command::new(cmd).arg("--version").output().map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty()).unwrap_or(false);
    m.lock().unwrap().insert(cmd.to_string(), v);
    v
}

/// macOS 自带的文字识别（Vision）：第一次用时把一个小 Swift 程序编译进 `.litmus/local/sources/ocr`（需要 Xcode 命令行工具，
/// 能编 Rust 的机器都有），之后每张图调一次。中文、英文都认。
#[cfg(target_os = "macos")]
fn ocr_macos(path: &Path) -> Option<String> {
    static BIN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let bin = BIN.get_or_init(|| {
        let dir = std::env::temp_dir().join("litmus-ocr");
        let _ = std::fs::create_dir_all(&dir);
        let exe = dir.join("ocr");
        if exe.exists() {
            return Some(exe);
        }
        let src = dir.join("ocr.swift");
        std::fs::write(&src, r#"import Foundation
import Vision
let url = URL(fileURLWithPath: CommandLine.arguments[1])
let req = VNRecognizeTextRequest()
req.recognitionLevel = .accurate
req.usesLanguageCorrection = true
req.recognitionLanguages = ["zh-Hans", "zh-Hant", "en-US"]
let h = VNImageRequestHandler(url: url, options: [:])
try? h.perform([req])
for o in req.results ?? [] { if let t = o.topCandidates(1).first { print(t.string) } }
"#).ok()?;
        let ok = std::process::Command::new("swiftc").args(["-O", "-o"]).arg(&exe).arg(&src).output().map(|o| o.status.success()).unwrap_or(false);
        ok.then_some(exe)
    });
    let out = std::process::Command::new(bin.as_ref()?).arg(path).output().ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!t.is_empty()).then_some(t)
}

#[cfg(not(target_os = "macos"))]
fn ocr_macos(_path: &Path) -> Option<String> {
    None
}

fn ocr(path: &Path) -> Option<String> {
    if let Some(t) = ocr_macos(path) {
        return Some(t);
    }
    if !has("tesseract") {
        return None;
    }
    static LANG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let lang = LANG.get_or_init(|| {
        let langs = std::process::Command::new("tesseract").arg("--list-langs").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        ["chi_sim", "chi_tra", "eng"].iter().filter(|l| langs.lines().any(|x| x.trim() == **l)).copied().collect::<Vec<_>>().join("+")
    });
    let out = std::process::Command::new("tesseract").arg(path).arg("-").args(if lang.is_empty() { vec![] } else { vec!["-l".to_string(), lang.clone()] }).output().ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// 本地文件 → (标题, 正文, 种类, 备注)。不认识或抽不出来返回 None 或带备注。
fn extract_file(p: &Path) -> Option<(String, String, String, Option<String>)> {
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    let stem = p.file_stem()?.to_string_lossy().to_string();
    if TEXT_EXTS.contains(&ext.as_str()) {
        let raw = std::fs::read(p).ok()?;
        let s = kernel_convert::decode_text(&raw);
        if ext == "html" || ext == "htm" {
            let c = kernel_convert::convert("html", s.as_bytes()).ok()?;
            return Some((c.title.filter(|t| !t.trim().is_empty()).unwrap_or(stem), c.markdown, "html".into(), None));
        }
        let title = s.lines().find_map(|l| l.trim().strip_prefix("# ").map(|t| t.trim().to_string())).unwrap_or(stem);
        return Some((title, s, if ext == "txt" { "txt".into() } else { "md".into() }, None));
    }
    if DOC_EXTS.contains(&ext.as_str()) {
        if ext == "pdf" {
            if !has("pdftotext") {
                return Some((stem, String::new(), "pdf".into(), Some("这台机器没有 pdftotext，PDF 只按文件名收录".into())));
            }
            let out = std::process::Command::new("pdftotext").args(["-enc", "UTF-8", "-l", "60"]).arg(p).arg("-").output().ok()?;
            let t = String::from_utf8_lossy(&out.stdout).to_string();
            return Some((stem, t, "pdf".into(), None));
        }
        let bytes = std::fs::read(p).ok()?;
        return match kernel_convert::convert(&ext, &bytes) {
            Ok(c) => Some((c.title.filter(|t| !t.trim().is_empty()).unwrap_or(stem), c.markdown, ext, None)),
            Err(e) => Some((stem, String::new(), ext, Some(format!("没抽出文字：{e}")))),
        };
    }
    if IMG_EXTS.contains(&ext.as_str()) {
        return Some(match ocr(p) {
            Some(t) => (stem, t, "img".into(), None),
            None => (stem, String::new(), "img".into(), Some("没识别出文字（macOS 用系统自带的识别；其他系统要装 tesseract）".into())),
        });
    }
    None
}

// ───────── 扫描 ─────────

#[derive(Default, Clone, Serialize)]
pub struct ScanState {
    pub running: bool,
    pub seen: usize,
    pub items: usize,
    pub error: Option<String>,
    pub finished: u64,
    /// 这一轮要处理的条目数（走完目录才知道；0 = 还在数）
    #[serde(default)]
    pub total: usize,
    /// 当前阶段：「数文件」「文字」「文档」「PDF」「截图」……
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub started: u64,
    #[serde(skip)]
    pub stop: bool,
}

pub fn scans_snapshot() -> HashMap<String, ScanState> {
    scans().lock().unwrap().clone()
}

/// 暂停 / 继续 / 停下一次扫描（做完手上这一个文件就生效）。
pub fn scan_control(id: &str, action: &str) -> bool {
    let mut sc = scans().lock().unwrap();
    let Some(x) = sc.get_mut(id).filter(|x| x.running) else { return false };
    match action {
        "pause" => x.paused = true,
        "resume" => x.paused = false,
        "stop" => {
            x.stop = true;
            x.paused = false;
        }
        _ => return false,
    }
    true
}

/// 扫描器与后台线程之间的那根线：报进度、问要不要停、分批把结果交出去（场上分批出现）。
pub struct Ctl<'a> {
    id: &'a str,
    flush: &'a dyn Fn(&[Item]),
    last: std::cell::Cell<std::time::Instant>,
    flushed: std::cell::Cell<usize>,
}

impl<'a> Ctl<'a> {
    fn set(&self, f: impl FnOnce(&mut ScanState)) {
        if let Some(x) = scans().lock().unwrap().get_mut(self.id) {
            f(x);
        }
    }
    pub fn total(&self, n: usize) {
        self.set(|x| x.total = n);
    }
    pub fn phase(&self, p: &str) {
        self.set(|x| x.phase = p.to_string());
    }
    pub fn seen(&self, n: usize) {
        self.set(|x| x.seen = n);
    }
    /// 暂停时在这里等；返回 false 表示该停了。
    pub fn go(&self) -> bool {
        loop {
            let (stop, paused) = scans().lock().unwrap().get(self.id).map(|x| (x.stop, x.paused)).unwrap_or((true, false));
            if stop {
                return false;
            }
            if !paused {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    /// 攒够了（隔 4 秒、多了至少一条）就交一批：写进索引、场上出现。
    pub fn maybe_flush(&self, out: &[Item], force: bool) {
        if out.len() > self.flushed.get() && (force || self.last.get().elapsed() >= std::time::Duration::from_secs(4)) {
            (self.flush)(out);
            self.flushed.set(out.len());
            self.last.set(std::time::Instant::now());
        }
    }
}

/// 后台干重活的线程：调低优先级，前台（打字、翻页、铺场）永远先走。
pub fn spawn_background(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.to_string()).spawn(move || {
        lower_priority();
        f();
    });
}

#[cfg(target_os = "macos")]
fn lower_priority() {
    extern "C" {
        fn pthread_set_qos_class_self_np(qos: u32, rel: i32) -> i32;
    }
    // QOS_CLASS_UTILITY = 0x11：系统把它排在交互任务之后，也更省电
    unsafe {
        pthread_set_qos_class_self_np(0x11, 0);
    }
}
#[cfg(target_os = "linux")]
fn lower_priority() {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn lower_priority() {}

/// 处理顺序：先文字（最快出结果），再 Office / 网页，再 PDF，最后截图（要 OCR，最慢）。
fn cost_class(ext: &str) -> u8 {
    if TEXT_EXTS.contains(&ext) {
        0
    } else if ext == "pdf" {
        2
    } else if IMG_EXTS.contains(&ext) {
        3
    } else {
        1
    }
}
const PHASES: [&str; 4] = ["文字", "文档", "PDF", "截图"];

fn scans() -> &'static Mutex<HashMap<String, ScanState>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, ScanState>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | "Library" | "__pycache__" | "dist" | "build" | ".git")
}

/// 文件夹：增量（同一路径、大小与修改时间没变就沿用上次抽出的文字）。
/// 先把要处理的文件数出来（最近的在前，有上限就截断），再按「文字 → 文档 → PDF → 截图」分步处理，
/// 每隔几秒交一批，场上一批一批出现，不用等全部扫完。
fn scan_folder(root: &Path, s: &Source, prev: &HashMap<String, Item>, ctl: &Ctl) -> Result<Vec<Item>, String> {
    let base = expand(s.path.as_deref().unwrap_or(""));
    if !base.is_dir() {
        return Err(format!("找不到文件夹 {}", base.display()));
    }
    let want: Vec<String> = if s.exts.is_empty() { TEXT_EXTS.iter().chain(DOC_EXTS.iter()).map(|x| x.to_string()).collect() } else { s.exts.iter().map(|x| x.to_ascii_lowercase()).collect() };
    ctl.phase("数文件");
    let mut cand: Vec<(PathBuf, std::fs::Metadata, String)> = Vec::new();
    let mut stack = vec![base.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if s.recursive && !skip_dir(&name) {
                    stack.push(p);
                }
                continue;
            }
            if name.starts_with('.') || name.starts_with("~$") {
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).map(|x| x.to_ascii_lowercase()).unwrap_or_default();
            if !want.contains(&ext) || meta.len() > MAX_FILE_BYTES || meta.len() == 0 {
                continue;
            }
            cand.push((p, meta, ext));
            if cand.len() % 500 == 0 {
                ctl.seen(cand.len());
            }
        }
        if cand.len() > MAX_FILES * 4 {
            break;
        }
    }
    // 最近的在前；有上限（第一次接入）就只取最近的那些
    cand.sort_by(|a, b| mtime_of(&b.1).cmp(&mtime_of(&a.1)).then_with(|| a.0.cmp(&b.0)));
    cand.truncate(s.limit.unwrap_or(MAX_FILES).min(MAX_FILES));
    cand.sort_by_key(|c| cost_class(&c.2));
    ctl.total(cand.len());
    let mut out = Vec::new();
    let mut phase = u8::MAX;
    for (n, (p, meta, ext)) in cand.iter().enumerate() {
        if !ctl.go() {
            break;
        }
        let cls = cost_class(ext);
        if cls != phase {
            ctl.maybe_flush(&out, true);
            phase = cls;
            ctl.phase(PHASES[cls as usize]);
        }
        ctl.seen(n + 1);
        let rel = p.strip_prefix(&base).unwrap_or(p).to_string_lossy().to_string();
        let key = format!("ext:{}/{}", s.id, rel);
        let mtime = mtime_of(meta);
        if let Some(old) = prev.get(&key).filter(|o| o.mtime == mtime && o.size == meta.len()) {
            out.push(old.clone());
            continue;
        }
        let Some((title, text, kind, note)) = extract_file(p) else { continue };
        let text: String = text.chars().take(MAX_TEXT).collect();
        let hash = put_text(root, &format!("# {title}\n\n{text}"));
        out.push(Item { key, src: s.id.clone(), title: title.chars().take(120).collect(), kind, loc: p.to_string_lossy().to_string(), date: ymd_of(mtime), mtime, size: meta.len(), hash, note, etag: None });
        ctl.maybe_flush(&out, false);
        // 抽 PDF、认截图要起外部程序：每个之间喘口气，别把风扇吹起来
        if cls >= 2 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Ok(out)
}

/// Claude Code（~/.claude/projects/*/*.jsonl）与 Codex（~/.codex/sessions/**.jsonl）的会话：
/// 标题 = 第一句提问，正文开头放「结论」= 最后一段回答，后面是问答全文（截断）。
fn scan_agent(root: &Path, s: &Source, prev: &HashMap<String, Item>, ctl: &Ctl) -> Result<Vec<Item>, String> {
    let which = s.agent.clone().unwrap_or_else(|| "claude".into());
    let base = expand(s.path.as_deref().unwrap_or(if which == "codex" { "~/.codex/sessions" } else { "~/.claude/projects" }));
    if !base.is_dir() {
        return Err(format!("找不到 {}", base.display()));
    }
    let mut files = Vec::new();
    let mut stack = vec![base.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                files.push(p);
            }
        }
    }
    // 最近的会话在前
    files.sort_by_cached_key(|p| std::cmp::Reverse(std::fs::metadata(p).map(|m| mtime_of(&m)).unwrap_or(0)));
    files.truncate(s.limit.unwrap_or(MAX_FILES).min(MAX_FILES));
    ctl.total(files.len());
    ctl.phase("会话");
    let mut out = Vec::new();
    for (n, p) in files.into_iter().enumerate() {
        if !ctl.go() {
            break;
        }
        ctl.seen(n + 1);
        ctl.maybe_flush(&out, false);
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        let rel = p.strip_prefix(&base).unwrap_or(&p).to_string_lossy().to_string();
        let key = format!("ext:{}/{}", s.id, rel);
        let mtime = mtime_of(&meta);
        if let Some(old) = prev.get(&key).filter(|o| o.mtime == mtime && o.size == meta.len()) {
            out.push(old.clone());
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else { continue };
        let (turns, project) = parse_session(&raw);
        let Some(first_q) = turns.iter().find(|t| t.0 == "user").map(|t| t.1.clone()) else { continue };
        let last_a = turns.iter().rev().find(|t| t.0 == "assistant").map(|t| t.1.clone()).unwrap_or_default();
        let title: String = format!("{}{}", project.as_ref().map(|p| format!("[{p}] ")).unwrap_or_default(), first_q.lines().next().unwrap_or("").chars().take(60).collect::<String>());
        let mut body = String::new();
        if !last_a.is_empty() {
            body.push_str(&format!("## 结论\n\n{}\n\n", last_a.chars().take(3000).collect::<String>()));
        }
        body.push_str("## 过程\n\n");
        for (role, text) in &turns {
            if body.len() > MAX_TEXT {
                break;
            }
            body.push_str(&format!("**{}**：{}\n\n", if role == "user" { "问" } else { "答" }, text.chars().take(2000).collect::<String>()));
        }
        let hash = put_text(root, &format!("# {title}\n\n{body}"));
        out.push(Item { key, src: s.id.clone(), title, kind: "agent".into(), loc: p.to_string_lossy().to_string(), date: ymd_of(mtime), mtime, size: meta.len(), hash, note: None, etag: None });
    }
    Ok(out)
}

/// 会话 jsonl → [(角色, 文字)] 与项目名。兼容 Claude Code（message.content 字符串或块数组）与 Codex（payload.content[].text）。
pub fn parse_session(raw: &str) -> (Vec<(String, String)>, Option<String>) {
    let mut turns: Vec<(String, String)> = Vec::new();
    let mut project = None;
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if project.is_none() {
            project = v.get("cwd").or_else(|| v.pointer("/payload/cwd")).and_then(|c| c.as_str()).and_then(|c| c.rsplit('/').next()).map(String::from);
        }
        let msg = v.get("message").or_else(|| v.get("payload")).unwrap_or(&v);
        let role = msg.get("role").and_then(|r| r.as_str()).or_else(|| v.get("type").and_then(|t| t.as_str())).unwrap_or("");
        let role = match role {
            "user" | "human" => "user",
            "assistant" => "assistant",
            _ => continue,
        };
        let text = match msg.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(a)) => a
                .iter()
                .filter(|b| matches!(b.get("type").and_then(|t| t.as_str()), Some("text") | Some("input_text") | Some("output_text")))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let text = text.trim();
        // 跳过系统注入、工具回执与命令回显
        if text.is_empty() || text.starts_with('<') || text.starts_with("Caveat:") {
            continue;
        }
        turns.push((role.to_string(), text.to_string()));
    }
    (turns, project)
}

fn scan_mcp(root: &Path, st: &Arc<AppState>, s: &Source) -> Result<Vec<Item>, String> {
    let name = s.connection.clone().unwrap_or_default();
    let cfg = st.kernel.lock().unwrap().vault.config.connections.iter().find(|c| c.name == name).cloned().ok_or_else(|| format!("litmus.toml 里没有名为 {name} 的连接"))?;
    let mut c = litmus_mcp::client::McpClient::connect(&cfg).map_err(|e| format!("连不上 {name}：{e}"))?;
    let list = c.list_resources().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in list.into_iter().take(800) {
        let Some(uri) = r.get("uri").and_then(|u| u.as_str()) else { continue };
        let Ok(v) = c.read_resource(uri) else { continue };
        let Some(text) = litmus_mcp::client::extract_text(&v) else { continue };
        let title = r.get("name").or_else(|| r.get("title")).and_then(|t| t.as_str()).unwrap_or(uri).to_string();
        let hash = put_text(root, &format!("# {title}\n\n{}", text.chars().take(MAX_TEXT).collect::<String>()));
        let id = kernel_core::Cas::hash_hex(uri.as_bytes())[..16].to_string();
        out.push(Item { key: format!("ext:{}/{id}", s.id), src: s.id.clone(), title, kind: "mcp".into(), loc: uri.to_string(), date: ymd_of(now_secs() as i64), mtime: now_secs() as i64, size: text.len() as u64, hash, note: None, etag: None });
    }
    if out.is_empty() {
        return Err(format!("{name} 没有提供资源（resources/list 为空）"));
    }
    Ok(out)
}

/// `{{secret:名字}}` → 凭证库里的值。
fn fill_secrets(root: &Path, s: &str) -> String {
    let rows = secrets_load(root);
    let mut out = s.to_string();
    while let Some(i) = out.find("{{secret:") {
        let Some(j) = out[i..].find("}}") else { break };
        let name = out[i + 9..i + j].trim().to_string();
        let val = rows.iter().find(|r| r.get("name").and_then(|n| n.as_str()) == Some(name.as_str())).and_then(|r| r.get("value").and_then(|v| v.as_str())).unwrap_or("").to_string();
        out.replace_range(i..i + j + 2, &val);
    }
    out
}

fn pick(v: &Value, path: &str) -> Option<String> {
    let x = if path.starts_with('/') { v.pointer(path) } else { v.get(path) }?;
    Some(match x {
        Value::String(s) => s.clone(),
        Value::Null => return None,
        other => other.to_string(),
    })
}

fn scan_rest(root: &Path, s: &Source) -> Result<Vec<Item>, String> {
    let r = s.rest.clone().ok_or("没有接口配置")?;
    let url = fill_secrets(root, r.get("url").and_then(|u| u.as_str()).unwrap_or(""));
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("接口地址要以 http(s):// 开头".into());
    }
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(30)).user_agent("Litmus sources").build().map_err(|e| e.to_string())?;
    let mut req = client.get(&url);
    if let Some(h) = r.get("headers").and_then(|h| h.as_object()) {
        for (k, v) in h {
            req = req.header(k.as_str(), fill_secrets(root, v.as_str().unwrap_or("")));
        }
    }
    let resp = req.send().map_err(|e| format!("请求失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("接口返回 {}", resp.status()));
    }
    let body: Value = resp.json().map_err(|e| format!("不是 JSON：{e}"))?;
    let items_at = r.get("items").and_then(|x| x.as_str()).unwrap_or("");
    let arr = if items_at.is_empty() { body.as_array().cloned() } else { body.pointer(items_at).and_then(|x| x.as_array()).cloned() }.ok_or("在返回里找不到条目数组（检查 items 路径）")?;
    let f = |k: &str, d: &str| r.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string();
    let (fid, ftitle, fbody, fdate) = (f("id", "id"), f("title", "title"), f("body", "body"), f("date", "date"));
    let mut out = Vec::new();
    for (n, it) in arr.iter().take(MAX_FILES).enumerate() {
        let id = pick(it, &fid).unwrap_or_else(|| n.to_string());
        let title = pick(it, &ftitle).unwrap_or_else(|| format!("条目 {id}"));
        let text = pick(it, &fbody).unwrap_or_else(|| serde_json::to_string_pretty(it).unwrap_or_default());
        let date = pick(it, &fdate).map(|d| d.chars().take(10).collect()).unwrap_or_else(|| ymd_of(now_secs() as i64));
        let hash = put_text(root, &format!("# {title}\n\n{}", text.chars().take(MAX_TEXT).collect::<String>()));
        let key_id = kernel_core::Cas::hash_hex(id.as_bytes())[..16].to_string();
        out.push(Item { key: format!("ext:{}/{key_id}", s.id), src: s.id.clone(), title: title.chars().take(120).collect(), kind: "rest".into(), loc: url.clone(), date, mtime: now_secs() as i64, size: text.len() as u64, hash, note: None, etag: None });
    }
    Ok(out)
}

// ───────── Google（OAuth 本机回环 + Drive / Gmail / Calendar） ─────────

fn google_base(kind: &str) -> String {
    std::env::var(format!("LITMUS_GOOGLE_{}", kind.to_uppercase())).unwrap_or_else(|_| match kind {
        "oauth" => "https://oauth2.googleapis.com".into(),
        "auth" => "https://accounts.google.com".into(),
        "drive" => "https://www.googleapis.com/drive/v3".into(),
        "gmail" => "https://gmail.googleapis.com/gmail/v1".into(),
        _ => "https://www.googleapis.com/calendar/v3".into(),
    })
}

fn secret(root: &Path, name: &str) -> Option<String> {
    secrets_load(root).into_iter().find(|r| r.get("name").and_then(|n| n.as_str()) == Some(name)).and_then(|r| r.get("value").and_then(|v| v.as_str()).map(String::from)).filter(|v| !v.is_empty())
}

fn google_token(root: &Path, s: &Source) -> Result<String, String> {
    let cid = secret(root, "google_client_id").ok_or("先在 /凭证 里存 google_client_id 与 google_client_secret（Google Cloud 控制台 → 凭据 → OAuth 客户端，类型选「桌面应用」）")?;
    let csec = secret(root, "google_client_secret").ok_or("缺 google_client_secret")?;
    let refresh = secret(root, &format!("google_refresh:{}", s.id)).ok_or("还没授权：点「连接 Google」")?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    let r: Value = client
        .post(format!("{}/token", google_base("oauth")))
        .form(&[("client_id", cid.as_str()), ("client_secret", csec.as_str()), ("refresh_token", refresh.as_str()), ("grant_type", "refresh_token")])
        .send()
        .map_err(|e| format!("换 token 失败：{e}"))?
        .json()
        .map_err(|e| e.to_string())?;
    r.get("access_token").and_then(|t| t.as_str()).map(String::from).ok_or_else(|| format!("Google 没给 access_token：{r}"))
}

fn b64url_decode(s: &str) -> Vec<u8> {
    let t: String = s.chars().map(|c| match c { '-' => '+', '_' => '/', x => x }).collect();
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in t.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}

fn gmail_text(payload: &Value) -> String {
    if payload.get("mimeType").and_then(|m| m.as_str()) == Some("text/plain") {
        if let Some(d) = payload.pointer("/body/data").and_then(|d| d.as_str()) {
            return String::from_utf8_lossy(&b64url_decode(d)).to_string();
        }
    }
    if let Some(parts) = payload.get("parts").and_then(|p| p.as_array()) {
        for p in parts {
            let t = gmail_text(p);
            if !t.trim().is_empty() {
                return t;
            }
        }
    }
    if let Some(d) = payload.pointer("/body/data").and_then(|d| d.as_str()) {
        let h = String::from_utf8_lossy(&b64url_decode(d)).to_string();
        return super::strip_all_tags(&h);
    }
    String::new()
}

fn scan_google(root: &Path, s: &Source) -> Result<Vec<Item>, String> {
    let token = google_token(root, s)?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let get = |url: &str| -> Result<Value, String> {
        let r = client.get(url).bearer_auth(&token).send().map_err(|e| e.to_string())?;
        let st = r.status();
        let v: Value = r.json().map_err(|e| e.to_string())?;
        if !st.is_success() {
            return Err(format!("Google 返回 {st}：{}", v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("")));
        }
        Ok(v)
    };
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let mut out = Vec::new();
    let services = if s.services.is_empty() { vec!["drive".to_string()] } else { s.services.clone() };
    for svc in services {
        match svc.as_str() {
            "drive" => {
                let q = s.query.clone().unwrap_or_else(|| "mimeType='application/vnd.google-apps.document' and trashed=false".into());
                let list = get(&format!("{}/files?pageSize=200&orderBy=modifiedTime%20desc&fields=files(id,name,modifiedTime,mimeType,webViewLink)&q={}", google_base("drive"), enc(&q)))?;
                for f in list.get("files").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                    let id = f["id"].as_str().unwrap_or("").to_string();
                    let text = client.get(format!("{}/files/{id}/export?mimeType=text/markdown", google_base("drive"))).bearer_auth(&token).send().ok().filter(|r| r.status().is_success()).and_then(|r| r.text().ok()).unwrap_or_default();
                    let title = f["name"].as_str().unwrap_or("文档").to_string();
                    let hash = put_text(root, &format!("# {title}\n\n{}", text.chars().take(MAX_TEXT).collect::<String>()));
                    out.push(Item { key: format!("ext:{}/drive-{id}", s.id), src: s.id.clone(), title, kind: "drive".into(), loc: f["webViewLink"].as_str().unwrap_or("").to_string(), date: f["modifiedTime"].as_str().unwrap_or("").chars().take(10).collect(), mtime: now_secs() as i64, size: text.len() as u64, hash, note: None, etag: None });
                }
            }
            "gmail" => {
                let q = s.query.clone().unwrap_or_else(|| "newer_than:90d -category:promotions".into());
                let list = get(&format!("{}/users/me/messages?maxResults=150&q={}", google_base("gmail"), enc(&q)))?;
                for m in list.get("messages").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                    let id = m["id"].as_str().unwrap_or("").to_string();
                    let Ok(full) = get(&format!("{}/users/me/messages/{id}?format=full", google_base("gmail"))) else { continue };
                    let header = |n: &str| full.pointer("/payload/headers").and_then(|h| h.as_array()).and_then(|a| a.iter().find(|x| x["name"].as_str().map(|s| s.eq_ignore_ascii_case(n)).unwrap_or(false))).and_then(|x| x["value"].as_str()).unwrap_or("").to_string();
                    let title = header("Subject");
                    let text = format!("发件人：{}\n日期：{}\n\n{}", header("From"), header("Date"), gmail_text(&full["payload"]));
                    let ms: i64 = full["internalDate"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0);
                    let hash = put_text(root, &format!("# {}\n\n{}", if title.is_empty() { "（无主题）" } else { &title }, text.chars().take(MAX_TEXT).collect::<String>()));
                    out.push(Item { key: format!("ext:{}/mail-{id}", s.id), src: s.id.clone(), title: if title.is_empty() { "（无主题）".into() } else { title }, kind: "mail".into(), loc: format!("https://mail.google.com/mail/u/0/#all/{id}"), date: ymd_of(ms / 1000), mtime: ms / 1000, size: text.len() as u64, hash, note: None, etag: None });
                }
            }
            "calendar" => {
                let now = now_secs() as i64;
                let iso = |t: i64| {
                    let d = ymd_of(t);
                    format!("{d}T00:00:00Z")
                };
                let list = get(&format!("{}/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults=250&timeMin={}&timeMax={}", google_base("calendar"), enc(&iso(now - 60 * 86400)), enc(&iso(now + 30 * 86400))))?;
                for e in list.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
                    let id = e["id"].as_str().unwrap_or("").to_string();
                    let title = e["summary"].as_str().unwrap_or("（无标题日程）").to_string();
                    let when = e.pointer("/start/dateTime").or_else(|| e.pointer("/start/date")).and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let people = e.get("attendees").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x["email"].as_str()).collect::<Vec<_>>().join("、")).unwrap_or_default();
                    let text = format!("时间：{when}\n地点：{}\n参与：{people}\n\n{}", e["location"].as_str().unwrap_or(""), e["description"].as_str().unwrap_or(""));
                    let hash = put_text(root, &format!("# {title}\n\n{text}"));
                    out.push(Item { key: format!("ext:{}/event-{id}", s.id), src: s.id.clone(), title, kind: "event".into(), loc: e["htmlLink"].as_str().unwrap_or("").to_string(), date: when.chars().take(10).collect(), mtime: now_secs() as i64, size: text.len() as u64, hash, note: None, etag: None });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn oauth_pending() -> &'static Mutex<HashMap<String, (String, String)>> {
    static P: std::sync::OnceLock<Mutex<HashMap<String, (String, String)>>> = std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// GET /sources/google/start?id=来源&origin=壳的地址 → {url}（在浏览器里打开授权）
pub async fn google_start(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let id = q.get("id").cloned().unwrap_or_default();
    let Some(s) = load_sources(&root).into_iter().find(|s| s.id == id && s.kind == "google") else { return err(StatusCode::NOT_FOUND, "没有这个 Google 来源").into_response() };
    let Some(cid) = secret(&root, "google_client_id") else {
        return err(StatusCode::CONFLICT, "先在 /凭证 里存 google_client_id 与 google_client_secret（Google Cloud 控制台 → 凭据 → 创建 OAuth 客户端，类型选「桌面应用」）").into_response();
    };
    let origin = q.get("origin").cloned().unwrap_or_else(|| format!("http://{}", h.get("host").and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1:7717")));
    let redirect = format!("{}/sources/google/callback", origin.trim_end_matches('/'));
    let scopes: Vec<&str> = s.services.iter().map(|x| match x.as_str() {
        "gmail" => "https://www.googleapis.com/auth/gmail.readonly",
        "calendar" => "https://www.googleapis.com/auth/calendar.readonly",
        _ => "https://www.googleapis.com/auth/drive.readonly",
    }).collect();
    let state = kernel_core::Cas::hash_hex(format!("{id}{}{}", now_secs(), std::process::id()).as_bytes())[..24].to_string();
    oauth_pending().lock().unwrap().insert(state.clone(), (id.clone(), redirect.clone()));
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let url = format!(
        "{}/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&access_type=offline&prompt=consent&scope={}&state={state}",
        google_base("auth"), enc(&cid), enc(&redirect), enc(&scopes.join(" "))
    );
    Json(json!({ "url": url, "redirect": redirect })).into_response()
}

/// GET /sources/google/callback?code&state：免令牌（浏览器从 Google 跳回来），state 必须是刚发出去的那个。
pub async fn google_callback(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let page = |msg: &str| axum::response::Html(format!("<!doctype html><meta charset=utf-8><title>鹿蕊 · Google</title><body style=\"font:16px/1.7 system-ui;padding:48px;max-width:560px;margin:auto\"><h2>鹿蕊 Litmus</h2><p>{msg}</p></body>"));
    let state = q.get("state").cloned().unwrap_or_default();
    let Some((id, redirect)) = oauth_pending().lock().unwrap().remove(&state) else { return page("授权链接已失效，请回到鹿蕊重新点「连接 Google」。").into_response() };
    let Some(code) = q.get("code").cloned() else { return page(&format!("没有授权：{}", q.get("error").cloned().unwrap_or_default())).into_response() };
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let (Some(cid), Some(csec)) = (secret(&root, "google_client_id"), secret(&root, "google_client_secret")) else { return page("凭证库里缺 google_client_id / google_client_secret。").into_response() };
    let r = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
        let v: Value = client
            .post(format!("{}/token", google_base("oauth")))
            .form(&[("code", code.as_str()), ("client_id", cid.as_str()), ("client_secret", csec.as_str()), ("redirect_uri", redirect.as_str()), ("grant_type", "authorization_code")])
            .send()
            .map_err(|e| e.to_string())?
            .json()
            .map_err(|e| e.to_string())?;
        v.get("refresh_token").and_then(|t| t.as_str()).map(String::from).ok_or_else(|| format!("Google 没给 refresh_token：{v}"))
    })
    .await;
    match r {
        Ok(Ok(refresh)) => {
            let mut rows = secrets_load(&root);
            let name = format!("google_refresh:{id}");
            rows.retain(|r| r.get("name").and_then(|n| n.as_str()) != Some(name.as_str()));
            rows.insert(0, json!({ "name": name, "value": refresh, "note": "Google 授权（鹿蕊数据源）", "updated": now_secs() * 1000 }));
            let _ = super::secrets_save(&root, &rows);
            page("已连接 Google。可以关掉这页，回到鹿蕊点「扫描」。").into_response()
        }
        Ok(Err(e)) => page(&format!("换取授权失败：{e}")).into_response(),
        Err(e) => page(&format!("出错了：{e}")).into_response(),
    }
}

// ───────── HTTP：配置、发现、扫描 ─────────

fn slug(s: &str) -> String {
    let h = kernel_core::Cas::hash_hex(format!("{s}{}", now_secs()).as_bytes());
    h[..10].to_string()
}

/// GET /sources：来源列表、每个来源的条目数与扫描状态。
pub async fn list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let ix = load_index(&root);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for it in &ix.items {
        *counts.entry(it.src.clone()).or_insert(0) += 1;
    }
    let sc = scans().lock().unwrap().clone();
    let rows: Vec<Value> = load_sources(&root)
        .into_iter()
        .map(|s| {
            let mut v = serde_json::to_value(&s).unwrap_or(json!({}));
            v["items"] = json!(counts.get(&s.id).copied().unwrap_or(0));
            v["scan"] = sc.get(&s.id).map(|x| serde_json::to_value(x).unwrap_or(json!({}))).or_else(|| ix.scans.get(&s.id).cloned()).unwrap_or(Value::Null);
            if s.kind == "google" {
                v["authorized"] = json!(secret(&root, &format!("google_refresh:{}", s.id)).is_some());
            }
            v
        })
        .collect();
    let dismissed = root.join(".litmus/local").join("sources-onboarded").exists();
    Json(json!({ "sources": rows, "onboarded": dismissed || !rows.is_empty(), "tools": { "pdftotext": has("pdftotext"), "tesseract": has("tesseract") } })).into_response()
}

/// POST /sources {source}：新建或更新（带 id 即更新）；POST /sources {onboarded:true} 记下「看过引导了」。
pub async fn upsert(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    if req.get("onboarded").and_then(|v| v.as_bool()) == Some(true) {
        let _ = std::fs::create_dir_all(root.join(".litmus/local"));
        let _ = std::fs::write(root.join(".litmus/local").join("sources-onboarded"), b"1");
        return Json(json!({ "ok": true })).into_response();
    }
    let Ok(mut s) = serde_json::from_value::<Source>(req.get("source").cloned().unwrap_or(json!({}))) else { return err(StatusCode::BAD_REQUEST, "来源格式不对").into_response() };
    if !matches!(s.kind.as_str(), "folder" | "agent" | "mcp" | "google" | "rest") && !super::connectors::KINDS.contains(&s.kind.as_str()) {
        return err(StatusCode::BAD_REQUEST, "kind 只能是 folder / agent / mcp / google / rest / github / notion / sanity / openflow / mflow").into_response();
    }
    if s.kind == "folder" {
        let p = expand(s.path.as_deref().unwrap_or(""));
        if !p.is_dir() {
            return err(StatusCode::BAD_REQUEST, format!("找不到文件夹 {}", p.display())).into_response();
        }
        // 不许把库自己当来源（会重复）
        if p.starts_with(&root) || root.starts_with(&p) && (p.join("litmus.toml").exists() || p.join("thirdc.toml").exists()) {
            return err(StatusCode::BAD_REQUEST, "这是知识库自己的文件夹，不用再当数据源").into_response();
        }
    }
    let mut list = load_sources(&root);
    if s.name.trim().is_empty() {
        s.name = s.path.clone().or_else(|| s.connection.clone()).unwrap_or_else(|| s.kind.clone());
    }
    match list.iter_mut().find(|x| !s.id.is_empty() && x.id == s.id) {
        Some(x) => *x = s.clone(),
        None => {
            s.id = slug(&s.name);
            s.added = now_secs();
            list.push(s.clone());
        }
    }
    if let Err(e) = save_sources(&root, &list) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    REV.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "source": s })).into_response()
}

/// DELETE /sources/{id}：去掉来源与它的索引（原文件不动）。
pub async fn remove(State(st): State<Arc<AppState>>, h: HeaderMap, AxumPath(id): AxumPath<String>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let mut list = load_sources(&root);
    let before = list.len();
    list.retain(|s| s.id != id);
    if list.len() == before {
        return err(StatusCode::NOT_FOUND, "没有这个来源").into_response();
    }
    let _ = save_sources(&root, &list);
    let _ixg = ix_lock();
    let mut ix = load_index(&root);
    ix.items.retain(|i| i.src != id);
    ix.scans.remove(&id);
    save_index(&root, &ix);
    REV.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "ok": true })).into_response()
}

/// 在后台（低优先级线程）扫一个来源。正在扫就返回 false。
pub fn start_scan(st: &Arc<AppState>, root: &Path, s: Source) -> bool {
    let id = s.id.clone();
    {
        let mut sc = scans().lock().unwrap();
        if sc.get(&id).map(|x| x.running).unwrap_or(false) {
            return false;
        }
        sc.insert(id.clone(), ScanState { running: true, started: now_secs(), phase: "排队".into(), ..Default::default() });
    }
    let st2 = st.clone();
    let root = root.to_path_buf();
    spawn_background("litmus-scan", move || {
        let ix0 = load_index(&root);
        let prev: HashMap<String, Item> = ix0.items.iter().filter(|i| i.src == s.id).map(|i| (i.key.clone(), i.clone())).collect();
        // 分批交付：已扫到的 + 还没扫到的旧条目（这样中途场上不会少东西），完成时再按本轮结果收口
        let mut excluded = super::curate::excluded_keys(&root);
        // 已经入库的也不再收回来（库里那篇还在的话）
        excluded.extend(ix0.imported.iter().filter(|(_, p)| root.join(p).exists()).map(|(k, _)| k.clone()));
        let flush = |part: &[Item]| {
            let part: Vec<Item> = part.iter().filter(|i| !excluded.contains(&i.key)).cloned().collect();
            // 分批上场前先过筛选规则：像过程文档的直接扣下，不会先出现再消失
            let fresh: Vec<Item> = part.iter().filter(|i| !prev.contains_key(&i.key)).cloned().collect();
            super::curate::hold_new(&root, &fresh, &s.name, s.path.as_deref());
            let part = &part[..];
            let _ixg = ix_lock();
            let mut ix = load_index(&root);
            let fresh: std::collections::HashSet<&str> = part.iter().map(|i| i.key.as_str()).collect();
            ix.items.retain(|i| i.src != s.id || !fresh.contains(i.key.as_str()));
            ix.items.extend(part.iter().cloned());
            save_index(&root, &ix);
            REV.fetch_add(1, Ordering::SeqCst);
            if let Some(x) = scans().lock().unwrap().get_mut(&s.id) {
                x.items = ix.items.iter().filter(|i| i.src == s.id).count();
            }
        };
        let ctl = Ctl { id: &s.id, flush: &flush, last: std::cell::Cell::new(std::time::Instant::now()), flushed: std::cell::Cell::new(0) };
        let r = match s.kind.as_str() {
            "folder" => scan_folder(&root, &s, &prev, &ctl),
            "agent" => scan_agent(&root, &s, &prev, &ctl),
            "mcp" => scan_mcp(&root, &st2, &s),
            "google" => scan_google(&root, &s),
            "rest" => scan_rest(&root, &s),
            _ => super::connectors::scan(&root, &s, &prev).unwrap_or_else(|| Err("不认识的来源".into())),
        };
        let stopped = scans().lock().unwrap().get(&s.id).map(|x| x.stop).unwrap_or(false);
        let r = r.map(|items| items.into_iter().filter(|i| !excluded.contains(&i.key)).collect::<Vec<_>>());
        // 新扫到的先按筛选规则过一遍：像过程文档的扣下等确认，不直接上场
        if let Ok(items) = &r {
            let fresh: Vec<Item> = items.iter().filter(|i| !prev.contains_key(&i.key)).cloned().collect();
            super::curate::hold_new(&root, &fresh, &s.name, s.path.as_deref());
        }
        let pre = format!("ext:{}/", s.id);
        let held_n = super::curate::load_store(&root).held.keys().filter(|k| k.starts_with(&pre)).count();
        let _ixg = ix_lock();
        let mut ix = load_index(&root);
        let state = match r {
            Ok(items) => {
                let n = items.len();
                if stopped {
                    // 停下的：留着已扫到的和旧的，不删任何东西
                    let fresh: std::collections::HashSet<String> = items.iter().map(|i| i.key.clone()).collect();
                    ix.items.retain(|i| i.src != s.id || !fresh.contains(&i.key));
                } else {
                    ix.items.retain(|i| i.src != s.id);
                }
                ix.items.extend(items);
                let total = ix.items.iter().filter(|i| i.src == s.id).count();
                ScanState { running: false, seen: n, items: total, error: stopped.then(|| "停下了：已扫到的都留着，下次接着扫".to_string()), finished: now_secs(), phase: if stopped { "停下了".into() } else if held_n > 0 { format!("完成；{held_n} 篇像过程文档，等你确认") } else { "完成".into() }, ..Default::default() }
            }
            Err(e) => ScanState { running: false, error: Some(e), finished: now_secs(), items: prev.len(), ..Default::default() },
        };
        ix.scans.insert(s.id.clone(), serde_json::to_value(&state).unwrap_or(json!({})));
        save_index(&root, &ix);
        scans().lock().unwrap().insert(s.id.clone(), state);
        REV.fetch_add(1, Ordering::SeqCst);
    });
    true
}

/// POST /sources/{id}/scan：后台扫描（增量）。进度看 GET /sources 或 GET /jobs。
pub async fn scan(State(st): State<Arc<AppState>>, h: HeaderMap, AxumPath(id): AxumPath<String>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let Some(s) = load_sources(&root).into_iter().find(|s| s.id == id) else { return err(StatusCode::NOT_FOUND, "没有这个来源").into_response() };
    if !start_scan(&st, &root, s) {
        return err(StatusCode::CONFLICT, "正在扫描").into_response();
    }
    Json(json!({ "started": true })).into_response()
}

/// GET /sources/detect：这台机器上值得接的东西（只数个数，不读内容）。
pub async fn detect(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let conns: Vec<String> = st.kernel.lock().unwrap().vault.config.connections.iter().map(|c| c.name.clone()).collect();
    let out = tokio::task::spawn_blocking(move || {
        let Some(home) = home() else { return json!({ "candidates": [] }) };
        let count = |dir: &Path, exts: &[&str], depth: usize, cap: usize| -> usize {
            let mut n = 0;
            let mut stack = vec![(dir.to_path_buf(), 0usize)];
            while let Some((d, lv)) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let p = e.path();
                    if p.is_dir() {
                        if lv < depth && !skip_dir(&name) {
                            stack.push((p, lv + 1));
                        }
                    } else if p.extension().and_then(|x| x.to_str()).map(|x| exts.contains(&x.to_ascii_lowercase().as_str())).unwrap_or(false) {
                        n += 1;
                        if n >= cap {
                            return n;
                        }
                    }
                }
            }
            n
        };
        let docs: Vec<&str> = TEXT_EXTS.iter().chain(DOC_EXTS.iter()).copied().collect();
        let mut c: Vec<Value> = Vec::new();
        for (rel, name, why) in [("Downloads", "下载", "下载下来的文档、PDF"), ("Documents", "文稿", "文稿里的文档"), ("Desktop", "桌面", "桌面上的文档")] {
            let p = home.join(rel);
            if p.is_dir() && !root.starts_with(&p.join("Litmus")) {
                let n = count(&p, &docs, 3, 5000);
                if n > 0 {
                    c.push(json!({ "kind": "folder", "name": name, "path": format!("~/{rel}"), "count": n, "why": why, "exts": docs }));
                }
            }
        }
        // 截图：macOS 默认存桌面（文件名「截屏」/「Screenshot」），或「图片/截屏」
        for (rel, name) in [("Desktop", "桌面上的截图"), ("Pictures/Screenshots", "图片 · 截屏"), ("Pictures/截屏", "图片 · 截屏")] {
            let p = home.join(rel);
            if !p.is_dir() {
                continue;
            }
            let n = std::fs::read_dir(&p).map(|rd| rd.flatten().filter(|e| { let s = e.file_name().to_string_lossy().to_string(); (s.starts_with("截屏") || s.starts_with("Screenshot") || s.starts_with("屏幕快照") || rel != "Desktop") && IMG_EXTS.iter().any(|x| s.to_ascii_lowercase().ends_with(x)) }).count()).unwrap_or(0);
            if n > 0 {
                c.push(json!({ "kind": "folder", "name": name, "path": format!("~/{rel}"), "count": n, "why": "截图里的文字（识别后能搜）", "exts": IMG_EXTS, "recursive": false }));
            }
        }
        // 散落的 Markdown：家目录下一层的项目里的 docs/、ops/、notes/
        if let Ok(rd) = std::fs::read_dir(&home) {
            let mut projects: Vec<_> = rd.flatten().filter(|e| e.path().is_dir() && !skip_dir(&e.file_name().to_string_lossy())).collect();
            projects.sort_by_key(|e| e.file_name());
            for e in projects.into_iter().take(80) {
                for sub in ["docs", "ops", "notes", "Notes"] {
                    let p = e.path().join(sub);
                    if p.is_dir() && !p.starts_with(&root) {
                        let n = count(&p, &["md", "markdown"], 4, 3000);
                        if n >= 3 {
                            c.push(json!({ "kind": "folder", "name": format!("{} / {sub}", e.file_name().to_string_lossy()), "path": format!("~/{}/{sub}", e.file_name().to_string_lossy()), "count": n, "why": "项目里的过程文档与说明", "exts": ["md", "markdown"] }));
                        }
                    }
                }
            }
        }
        // 本地 agent 的会话
        for (rel, agent, name) in [(".claude/projects", "claude", "Claude Code 会话"), (".codex/sessions", "codex", "Codex 会话")] {
            let p = home.join(rel);
            if p.is_dir() {
                let n = count(&p, &["jsonl"], 4, 20000);
                if n > 0 {
                    c.push(json!({ "kind": "agent", "agent": agent, "name": name, "path": format!("~/{rel}"), "count": n, "why": "各个项目里和 agent 的对话，提出结论" }));
                }
            }
        }
        for name in conns {
            c.push(json!({ "kind": "mcp", "connection": name, "name": format!("连接器 · {name}"), "why": "MCP 资源（比如 LinkTo）" }));
        }
        c.push(json!({ "kind": "google", "name": "Google", "services": ["drive", "gmail", "calendar"], "why": "Drive 文档、邮件、日程（要先在 Google Cloud 建一个桌面应用 OAuth 客户端）" }));
        c.push(json!({ "kind": "rest", "name": "任意 JSON 接口", "why": "填地址、凭证与字段，把返回的条目当文档" }));
        json!({ "candidates": c, "home": home.to_string_lossy() })
    })
    .await
    .unwrap_or(json!({ "candidates": [] }));
    Json(out).into_response()
}

/// POST /sources/connection {name, command, args[]}：往 litmus.toml 加（或更新）一个 [[connections]]，原有内容与注释保留。
/// 比如 LinkTo 的 MCP：name = "linkto"，command = 它的启动命令。
pub async fn add_connection(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = req.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let command = req.get("command").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let args: Vec<String> = req.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    if name.is_empty() || command.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return err(StatusCode::BAD_REQUEST, "要连接名（字母数字、- 或 _）和启动命令").into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let cfg_path = root.join("litmus.toml");
    let r = (|| -> Result<(), String> {
        let raw = std::fs::read_to_string(&cfg_path).map_err(|e| format!("读不到 litmus.toml：{e}"))?;
        let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|e| format!("litmus.toml 格式有误：{e}"))?;
        if !doc.contains_key("connections") {
            doc["connections"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        }
        let arr = doc["connections"].as_array_of_tables_mut().ok_or("litmus.toml 里的 connections 不是 [[connections]] 表数组")?;
        let mut t = toml_edit::Table::new();
        t["name"] = toml_edit::value(name.as_str());
        t["command"] = toml_edit::value(command.as_str());
        let mut a = toml_edit::Array::new();
        for x in &args {
            a.push(x.as_str());
        }
        t["args"] = toml_edit::value(a);
        let found = arr.iter().position(|x| x.get("name").and_then(|v| v.as_str()) == Some(name.as_str()));
        match found {
            Some(i) => *arr.get_mut(i).unwrap() = t,
            None => arr.push(t),
        }
        let tmp = cfg_path.with_extension("tmp");
        std::fs::write(&tmp, doc.to_string()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &cfg_path).map_err(|e| e.to_string())
    })();
    if let Err(e) = r {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    {
        let mut k = st.kernel.lock().unwrap();
        let c = kernel_core::ConnectionConfig { name: name.clone(), command, args, env: Default::default(), url: None, headers: Default::default() };
        k.vault.config.connections.retain(|x| x.name != name);
        k.vault.config.connections.push(c);
    }
    Json(json!({ "ok": true, "name": name })).into_response()
}

// ───────── 库外文档：读、导入、存学习成果 ─────────

/// GET /field/ext?p=ext:…：库外一篇的正文（只读）。
pub async fn ext_get(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let key = q.get("p").cloned().unwrap_or_default();
    let Some((it, s)) = find_item(&root, &key) else { return err(StatusCode::NOT_FOUND, "库外的这篇找不到了（来源被移除或重新扫描过）").into_response() };
    let text = read_text(&root, &it.hash);
    let html = kernel_core::to_html(&kernel_core::from_markdown(&text).unwrap_or_default());
    let exists = it.loc.starts_with('/') && Path::new(&it.loc).exists();
    Json(json!({ "path": key, "title": it.title, "source": text, "html": html, "format": "ext", "kind": it.kind, "loc": it.loc, "exists": exists, "src": s.name, "src_kind": s.kind, "date": it.date, "note": it.note })).into_response()
}

fn safe_name(t: &str) -> String {
    t.chars().filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '\n' | '\r' | '\t')).collect::<String>().trim().chars().take(60).collect()
}

/// POST /field/ext/import {p, mode:"move"|"copy", folder?}
/// 本地文件：move = 原文件移进库的 Assets/imported/ 并生成一篇 Markdown（链着原件）；copy = 只生成 Markdown。
/// 非文件来源（会话、邮件、接口……）只有 copy。
/// 把一条库外条目导入库：copy = 只生成 Markdown（原件留着）；move = 原件也搬进库（纯文本直接成为这篇，其他进 Assets/imported/）。
/// 入了库就以库里那篇为准：这一条不再上场、重扫也不回来，场上的位置交给新的那篇。
pub fn import_one(st: &AppState, root: &Path, key: &str, mode: &str, folder: Option<&str>) -> Result<Value, (StatusCode, String)> {
    let key = key.to_string();
    let Some((it, s)) = find_item(root, &key) else { return Err((StatusCode::NOT_FOUND, "库外的这篇找不到了".into())) };
    let root = root.to_path_buf();
    let folder = folder.filter(|f| f.starts_with("Notes") && !f.contains("..")).map(|f| f.trim_end_matches('/').to_string()).unwrap_or_else(|| format!("Notes/Sources/{}", safe_name(&s.name)));
    let text = read_text(&root, &it.hash);
    let is_file = it.loc.starts_with('/') && Path::new(&it.loc).is_file();
    let mut body_md = text.clone();
    // 正文已经带着 # 标题：补上来源行
    let origin = if is_file { format!("> 导入自 `{}`（{}）", it.loc, it.date) } else if it.loc.starts_with("http") { format!("> 导入自 [{}]({})（{}）", s.name, it.loc, it.date) } else { format!("> 导入自 {}（{}）", s.name, it.date) };
    let mut moved_to = None;
    if mode == "move" && is_file {
        // 纯文本（md / txt）：正文本身就是原件，生成的这篇即全部内容，不另存附件。
        // 其他（PDF、Word、截图、网页……）：原件放进库的 Assets/imported/，正文里链过去。
        if it.kind == "md" || it.kind == "txt" {
            moved_to = Some(String::new());
        } else {
            let fname = Path::new(&it.loc).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            let (stem, ext) = match fname.rsplit_once('.') {
                Some((a, b)) => (a.to_string(), format!(".{b}")),
                None => (fname.clone(), String::new()),
            };
            let mut rel = format!("Assets/imported/{fname}");
            let mut n = 2;
            while root.join(&rel).exists() && n < 200 {
                rel = format!("Assets/imported/{stem} {n}{ext}");
                n += 1;
            }
            let dst = root.join(&rel);
            let _ = std::fs::create_dir_all(dst.parent().unwrap());
            if let Err(e) = std::fs::copy(&it.loc, &dst) {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("原件没复制进库：{e}")));
            }
            let link = rel.replace(' ', "%20");
            body_md = format!("{}\n\n原件：[{fname}]({link})\n", body_md.trim_end());
            moved_to = Some(rel);
        }
    }
    let lines: Vec<&str> = body_md.lines().collect();
    let (title_line, rest) = if lines.first().map(|l| l.starts_with("# ")).unwrap_or(false) { (lines[0].to_string(), lines[1..].join("\n")) } else { (format!("# {}", it.title), body_md.clone()) };
    let md = format!("{title_line}\n\n{origin}\n\n{}\n", rest.trim());
    let mut target = format!("{folder}/{}.md", safe_name(&it.title).trim().to_string().replace(' ', " "));
    if target.ends_with("/.md") {
        target = format!("{folder}/导入.md");
    }
    let mut n = 2;
    while root.join(&target).exists() && n < 100 {
        target = format!("{folder}/{} {n}.md", safe_name(&it.title));
        n += 1;
    }
    let mut k = st.kernel.lock().unwrap();
    if let Err(e) = k.put_doc(&target, &md) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    drop(k);
    // 移动：资产已存进库，再删原件
    let mut removed_original = false;
    if moved_to.is_some() {
        removed_original = std::fs::remove_file(&it.loc).is_ok();
    }
    // 入了库就以库里那篇为准：这一条不再上场、重扫也不回来；场上的位置交给新的那篇（原地变成实心）
    {
        let _ixg = ix_lock();
        let mut ix = load_index(&root);
        ix.items.retain(|i| i.key != key);
        ix.imported.insert(key.clone(), target.clone());
        save_index(&root, &ix);
        super::field::carry_position(&root, &key, &target);
        REV.fetch_add(1, Ordering::SeqCst);
    }
    audit_log(&root.join(".litmus/local"), "ext-import", &json!({ "key": key, "to": target, "mode": mode, "moved": removed_original }));
    Ok(json!({ "path": target, "asset": moved_to.filter(|a| !a.is_empty()), "moved": removed_original, "original": it.loc, "from": key }))
}

/// POST /field/ext/import {p, mode:"copy"|"move", folder?}（默认复制：原件留在原处）
pub async fn ext_import(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let key = req.get("p").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mode = req.get("mode").and_then(|v| v.as_str()).unwrap_or("copy").to_string();
    let folder = req.get("folder").and_then(|v| v.as_str()).map(String::from);
    let root = st.kernel.lock().unwrap().vault.root.clone();
    match import_one(&st, &root, &key, &mode, folder.as_deref()) {
        Ok(v) => Json(v).into_response(),
        Err((code, e)) => err(code, e).into_response(),
    }
}

// ───────── 批量入库（后台，可停下） ─────────

#[derive(Serialize, Clone, Default)]
pub struct ImportJob {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub ok: usize,
    pub failed: Vec<String>,
    pub paths: Vec<String>,
    pub current: String,
    pub finished: u64,
    #[serde(skip)]
    pub stop: bool,
}

fn imports() -> &'static Mutex<HashMap<PathBuf, ImportJob>> {
    static I: std::sync::OnceLock<Mutex<HashMap<PathBuf, ImportJob>>> = std::sync::OnceLock::new();
    I.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn import_snapshot(root: &Path) -> Option<ImportJob> {
    imports().lock().unwrap().get(root).cloned()
}

pub fn import_stop(root: &Path) -> bool {
    match imports().lock().unwrap().get_mut(root) {
        Some(j) if j.running => {
            j.stop = true;
            true
        }
        _ => false,
    }
}

/// POST /field/ext/import-batch {keys?:[…], source?:id, mode?:"copy"|"move", dry?}
/// 一次导入很多篇：后台低优先级一篇篇来，进度在 /jobs；默认复制（原件留在原处）。
pub async fn ext_import_batch(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let mode = req.get("mode").and_then(|v| v.as_str()).unwrap_or("copy").to_string();
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let mut keys: Vec<String> = req.get("keys").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).filter(|k| k.starts_with("ext:")).map(String::from).collect()).unwrap_or_default();
    if let Some(src) = req.get("source").and_then(|v| v.as_str()) {
        // 一整个来源：场上能看到的那些（筛掉的、已入库的不算）
        keys.extend(corpus(&root).into_iter().filter(|(it, _, _)| it.src == src).map(|(it, _, _)| it.key));
    }
    keys.sort();
    keys.dedup();
    if req.get("dry").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Json(json!({ "count": keys.len() })).into_response();
    }
    if keys.is_empty() {
        return err(StatusCode::BAD_REQUEST, "没有要入库的：选中的都已经在库里了，或者不是库外的条目").into_response();
    }
    {
        let mut js = imports().lock().unwrap();
        if js.get(&root).map(|j| j.running).unwrap_or(false) {
            return err(StatusCode::CONFLICT, "上一批还在入库：等它做完，或者先停下").into_response();
        }
        js.insert(root.clone(), ImportJob { running: true, total: keys.len(), ..Default::default() });
    }
    let total = keys.len();
    let st2 = st.clone();
    let root2 = root.clone();
    spawn_background("litmus-import", move || {
        for key in keys {
            if imports().lock().unwrap().get(&root2).map(|j| j.stop).unwrap_or(true) {
                break;
            }
            let title = find_item(&root2, &key).map(|(it, _)| it.title).unwrap_or_else(|| key.clone());
            if let Some(j) = imports().lock().unwrap().get_mut(&root2) {
                j.current = title.clone();
            }
            let r = import_one(&st2, &root2, &key, &mode, None);
            if let Some(j) = imports().lock().unwrap().get_mut(&root2) {
                j.done += 1;
                match r {
                    Ok(v) => {
                        j.ok += 1;
                        if let Some(p) = v["path"].as_str() {
                            j.paths.push(p.to_string());
                        }
                    }
                    Err((_, e)) => j.failed.push(format!("{title}：{e}")),
                }
            }
        }
        if let Some(j) = imports().lock().unwrap().get_mut(&root2) {
            j.running = false;
            j.current.clear();
            j.finished = now_secs();
        }
    });
    Json(json!({ "started": true, "total": total })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_codex_sessions_parse() {
        let claude = r#"{"type":"user","cwd":"/Users/x/InputFlow","message":{"role":"user","content":"把候选词排序改成按频率"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我先看看排序代码。"},{"type":"tool_use","name":"Read"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"..."}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"改好了：按频率降序，同频按最近使用。"}]}}"#;
        let (t, p) = parse_session(claude);
        assert_eq!(p.as_deref(), Some("InputFlow"));
        assert_eq!(t.len(), 3);
        assert_eq!(t.last().unwrap().1, "改好了：按频率降序，同频按最近使用。");
        let codex = r#"{"type":"session_meta","payload":{"cwd":"/home/a/litmus"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"修一下登录页"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"已修复：令牌过期时跳回登录。"}]}}"#;
        let (t, p) = parse_session(codex);
        assert_eq!(p.as_deref(), Some("litmus"));
        assert_eq!(t[0], ("user".to_string(), "修一下登录页".to_string()));
        assert_eq!(t[1].0, "assistant");
    }

    #[test]
    fn gmail_body_is_decoded() {
        let payload = json!({"mimeType":"multipart/alternative","parts":[{"mimeType":"text/plain","body":{"data":"5L2g5aW977yM6L-Z5piv5q2j5paH"}}]});
        assert_eq!(gmail_text(&payload), "你好，这是正文");
    }

    #[test]
    fn rest_pick_supports_pointer_and_key() {
        let v = json!({"a":{"b":"x"},"n":3});
        assert_eq!(pick(&v, "/a/b").as_deref(), Some("x"));
        assert_eq!(pick(&v, "n").as_deref(), Some("3"));
        assert!(pick(&v, "missing").is_none());
    }
}
