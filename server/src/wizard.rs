//! 搬家向导：把 Obsidian 库、Notion 导出、飞书导出、普通 Markdown 文件夹搬进库。
//!
//! 流程和别处一样「先出清单，确认再做」：
//! - `POST /field/wizard/upload?name=x.zip`：网页版先把导出的 zip 传上来（存在 `.litmus/local/wizard/uploads/`）；
//! - `POST /field/wizard/plan {path | upload, kind?, target?}`：认出是哪种来源，算出每篇落到哪、附件放哪、
//!   多少链接接得上、哪些会改名，只读不写；
//! - `POST /field/wizard/run {…同上}`：后台一篇篇写进库（左上角看进度、能停下），写完记一份清单；
//! - `POST /field/wizard/undo {id}`：把这次搬进来的文档放进回收站、附件拿掉。
//!
//! 转换规则：
//! - 文档：md / txt 原样；html、docx、odt、pptx、xlsx、epub、rtf 转成 Markdown（内嵌图片存成附件）；
//!   Notion 数据库的 csv 转成一篇表格文档（`_all.csv` 优先）。
//! - 名字：Notion 文件名、文件夹名后面那串 32 位 id 去掉；不能做文件名的字符去掉；同名的加「 2」。
//! - 附件：图片、PDF 等放进 `Assets/imported/<来源名>/`，文件名换成不带空格的，正文里的引用跟着改。
//! - 链接：`[[笔记]]`、`[[文件夹/笔记|别名]]`、`![[图片.png]]`、`[文字](相对路径.md)`、`![](图片)` 都改到新位置；
//!   找不到的原样留着，计入「接不上的链接」。
//! - 每篇的 frontmatter 记一行 `imported_from`：原来在哪。

use super::{audit_log, check_token, err, AppState};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 解压后的总大小上限：再大请先解压成文件夹再选
const MAX_UNZIPPED: usize = 3 << 30;
/// 一次最多搬多少个文件
const MAX_FILES: usize = 50_000;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/* ───────── 来源：文件夹或 zip ───────── */

enum Store {
    Dir(PathBuf),
    Mem(HashMap<String, Vec<u8>>),
}

pub struct Src {
    store: Store,
    /// 相对路径（`/` 分隔），已去掉统一的顶层文件夹
    files: Vec<String>,
    /// 去掉的那层顶层文件夹（读文件时补回来）
    prefix: String,
    /// 来源名：库名 / 文件夹名 / zip 名
    pub name: String,
    /// 原来在哪（显示用，也写进 imported_from）
    pub origin: String,
    pub is_dir: bool,
}

fn junk_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "__MACOSX" | "node_modules" | "$RECYCLE.BIN")
}
fn junk_file(name: &str) -> bool {
    name.starts_with('.') || name.starts_with("~$") || matches!(name, "Thumbs.db" | "desktop.ini" | "Icon\r")
}

impl Src {
    pub fn open(root: &Path, path: Option<&str>, upload: Option<&str>) -> Result<Src, String> {
        if let Some(id) = upload.filter(|s| !s.is_empty()) {
            if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err("上传的编号不对".into());
            }
            let p = uploads_dir(root).join(format!("{id}.zip"));
            let meta: Value = std::fs::read_to_string(uploads_dir(root).join(format!("{id}.json"))).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
            let name = meta["name"].as_str().unwrap_or("导入").trim_end_matches(".zip").to_string();
            let bytes = std::fs::read(&p).map_err(|_| "上传的文件不在了，请重新上传".to_string())?;
            return Src::from_zip(&bytes, &name, &format!("{name}.zip"));
        }
        let raw = path.unwrap_or("").trim();
        if raw.is_empty() {
            return Err("先选一个文件夹或 zip".into());
        }
        let p = super::sources::expand_pub(raw);
        let rc = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let pc = p.canonicalize().unwrap_or_else(|_| p.clone());
        if pc.starts_with(&rc) {
            return Err("这个文件夹就在库里，不用再搬".into());
        }
        if p.is_file() {
            let lower = raw.to_ascii_lowercase();
            if !lower.ends_with(".zip") {
                return Err("只能选文件夹或 .zip".into());
            }
            let bytes = std::fs::read(&p).map_err(|e| format!("读不了 {raw}：{e}"))?;
            let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "导入".into());
            return Src::from_zip(&bytes, &name, &p.display().to_string());
        }
        if !p.is_dir() {
            return Err(format!("找不到 {raw}"));
        }
        let mut files = Vec::new();
        let mut stack = vec![p.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    // 嵌着的另一个鹿蕊库不搬
                    if !junk_dir(&name) && !e.path().join("litmus.toml").exists() {
                        stack.push(e.path());
                    }
                } else if ft.is_file() && !junk_file(&name) {
                    if let Ok(rel) = e.path().strip_prefix(&p) {
                        files.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                    if files.len() > MAX_FILES {
                        return Err(format!("文件太多了（超过 {MAX_FILES} 个）：先选其中一个子文件夹"));
                    }
                }
            }
        }
        files.sort();
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "导入".into());
        Ok(Src { store: Store::Dir(p.clone()), files, prefix: String::new(), name, origin: p.display().to_string(), is_dir: true })
    }

    /// 解开 zip（Notion 大导出是 zip 里套几个 zip，一并解开）。
    pub fn from_zip(bytes: &[u8], name: &str, origin: &str) -> Result<Src, String> {
        let mut mem: HashMap<String, Vec<u8>> = HashMap::new();
        let mut total = 0usize;
        fn unzip(bytes: &[u8], mem: &mut HashMap<String, Vec<u8>>, total: &mut usize, depth: usize) -> Result<(), String> {
            let z = kernel_convert::zip::ZipReader::new(bytes).map_err(|e| e.to_string())?;
            for n in z.names() {
                if n.ends_with('/') {
                    continue;
                }
                let clean = n.replace('\\', "/");
                let segs: Vec<&str> = clean.split('/').filter(|s| !s.is_empty()).collect();
                // 防 zip 穿越与垃圾：.. / 隐藏 / __MACOSX
                if segs.is_empty() || segs.iter().any(|s| *s == ".." || *s == ".") {
                    continue;
                }
                let (dirs, file) = segs.split_at(segs.len() - 1);
                if dirs.iter().any(|d| junk_dir(d)) || junk_file(file[0]) {
                    continue;
                }
                let Some(data) = z.read(n) else { continue };
                *total += data.len();
                if *total > MAX_UNZIPPED {
                    return Err("解开后超过 3GB：请先解压成文件夹，再选那个文件夹".into());
                }
                if depth == 0 && file[0].to_ascii_lowercase().ends_with(".zip") {
                    unzip(&data, mem, total, depth + 1)?;
                    continue;
                }
                mem.insert(segs.join("/"), data);
                if mem.len() > MAX_FILES {
                    return Err(format!("文件太多了（超过 {MAX_FILES} 个）"));
                }
            }
            Ok(())
        }
        unzip(bytes, &mut mem, &mut total, 0)?;
        if mem.is_empty() {
            return Err("zip 里没有能搬的文件".into());
        }
        let mut files: Vec<String> = mem.keys().cloned().collect();
        files.sort();
        // 整个导出套在一个顶层文件夹里：去掉这一层，来源名用它
        let mut prefix = String::new();
        let mut name = name.to_string();
        if let Some(first) = files[0].split('/').next().map(String::from) {
            let p = format!("{first}/");
            if files.iter().all(|f| f.starts_with(&p)) {
                prefix = p;
                name = first;
            }
        }
        let files = files.iter().map(|f| f[prefix.len()..].to_string()).collect();
        Ok(Src { store: Store::Mem(mem), files, prefix, name, origin: origin.to_string(), is_dir: false })
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        match &self.store {
            Store::Dir(d) => std::fs::read(d.join(rel)).ok(),
            Store::Mem(m) => m.get(&format!("{}{rel}", self.prefix)).cloned(),
        }
    }

    fn origin_of(&self, rel: &str) -> String {
        match &self.store {
            Store::Dir(d) => d.join(rel).display().to_string(),
            Store::Mem(_) => format!("{}:{rel}", self.origin),
        }
    }

    /// 猜是哪种：有 .obsidian → Obsidian；文件名带 32 位 id → Notion；以 docx 为主 → 飞书；否则 Markdown 文件夹
    pub fn detect(&self) -> &'static str {
        if let Store::Dir(d) = &self.store {
            if d.join(".obsidian").is_dir() {
                return "obsidian";
            }
        }
        let n = self.files.len().max(1);
        let notion = self.files.iter().filter(|f| f.split('/').any(|s| notion_id_at(stem_of(s)).is_some())).count();
        if notion * 3 >= n && notion > 0 {
            return "notion";
        }
        let docx = self.files.iter().filter(|f| ext_of(f) == "docx").count();
        let md = self.files.iter().filter(|f| matches!(ext_of(f).as_str(), "md" | "markdown")).count();
        if docx > md {
            return "feishu";
        }
        // Obsidian 的 zip 里会带 .obsidian（解压时当隐藏目录跳过了），这时看 ![[ ]] 和 [[ ]] 的痕迹
        if !self.is_dir && md > 0 {
            let probe = self.files.iter().filter(|f| ext_of(f) == "md").take(40).filter(|f| self.read(f).map(|b| String::from_utf8_lossy(&b).contains("[[")).unwrap_or(false)).count();
            if probe * 3 >= self.files.iter().filter(|f| ext_of(f) == "md").take(40).count() && probe > 0 {
                return "obsidian";
            }
        }
        "markdown"
    }
}

fn uploads_dir(root: &Path) -> PathBuf {
    root.join(".litmus/local/wizard/uploads")
}
fn runs_path(root: &Path) -> PathBuf {
    root.join(".litmus/local/wizard/runs.json")
}

/* ───────── 名字 ───────── */

fn ext_of(p: &str) -> String {
    let f = p.rsplit('/').next().unwrap_or(p);
    f.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}
fn stem_of(seg: &str) -> &str {
    match seg.rsplit_once('.') {
        Some((a, e)) if !a.is_empty() && e.len() <= 6 && !e.contains(' ') => a,
        _ => seg,
    }
}
/// Notion 名字末尾的「 0123…（32 位十六进制）」：返回去掉之后的位置
fn notion_id_at(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() < 33 {
        return None;
    }
    let tail = &s[s.len() - 32..];
    if tail.bytes().all(|c| c.is_ascii_hexdigit()) && b[b.len() - 33] == b' ' {
        Some(s.len() - 33)
    } else {
        None
    }
}
/// 一段路径名变成能用的名字：去掉 Notion id、去掉不能做文件名的字符
fn clean_seg(seg: &str, notion: bool) -> String {
    let mut s = seg.to_string();
    if notion {
        if let Some(i) = notion_id_at(&s) {
            s.truncate(i);
        }
    }
    let s: String = s.chars().filter(|c| !matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '\n' | '\r' | '\t' | '[' | ']')).collect();
    let s = s.trim().trim_start_matches('.').trim().to_string();
    let s: String = s.chars().take(80).collect();
    if s.is_empty() { "未命名".into() } else { s }
}
/// 附件文件名：不带空格和特殊字符（正文里的 ![](…) 不用转义也能用）
fn asset_name(file: &str) -> String {
    let (stem, ext) = match file.rsplit_once('.') {
        Some((a, e)) if !a.is_empty() => (a, format!(".{}", e.to_ascii_lowercase())),
        _ => (file, String::new()),
    };
    let mut s = kernel_core::slugify(stem);
    if s.is_empty() {
        s = "file".into();
    }
    format!("{s}{ext}")
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `a/b` 里的 `../`、`./` 收掉
fn norm_path(p: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for s in p.split('/') {
        match s {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            x => out.push(x),
        }
    }
    Some(out.join("/"))
}

/* ───────── 清单 ───────── */

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Md,
    Convert,
    Csv,
    Asset,
}

pub struct Plan {
    pub kind: &'static str,
    pub name: String,
    pub target: String,
    asset_base: String,
    docs: Vec<(String, String, Kind)>,
    assets: Vec<(String, String)>,
    skipped: Vec<String>,
    renamed: usize,
    /// 原相对路径（小写）→ 新文档路径
    by_rel: HashMap<String, String>,
    /// 原相对路径去掉扩展名（小写）→ 新文档路径
    by_rel_noext: HashMap<String, String>,
    /// 原文件名去掉扩展名（小写）→ 新文档路径（Obsidian 按名字找）
    by_stem: HashMap<String, Vec<String>>,
    asset_rel: HashMap<String, String>,
    asset_name: HashMap<String, Vec<String>>,
}

const DOC_TEXT: &[&str] = &["md", "markdown", "txt"];
const ASSET_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "heic", "avif", "pdf", "mp3", "m4a", "wav", "ogg", "mp4", "mov", "webm", "zip", "csv", "json", "canvas", "excalidraw", "drawio", "key", "pages", "numbers", "ppt", "doc", "xls"];

pub fn plan(root: &Path, src: &Src, kind: &str, target: Option<&str>) -> Plan {
    let kind: &'static str = match kind {
        "obsidian" => "obsidian",
        "notion" => "notion",
        "feishu" => "feishu",
        "markdown" => "markdown",
        _ => src.detect(),
    };
    let notion = kind == "notion";
    let label = match kind {
        "notion" if src.name.to_ascii_lowercase().starts_with("export") || notion_id_at(&src.name).is_some() => "Notion".to_string(),
        "feishu" if src.name.to_ascii_lowercase().starts_with("export") || src.name.contains("导出") => "飞书".to_string(),
        _ => clean_seg(&src.name, notion),
    };
    let target = target
        .map(|t| t.trim().trim_end_matches('/').to_string())
        .filter(|t| super::is_safe_doc_path(&format!("{t}/x.md")) && t.starts_with("Notes/"))
        .unwrap_or_else(|| format!("Notes/导入/{label}"));
    let slug: String = kernel_core::slugify(&label).trim_matches('-').chars().take(40).collect();
    let asset_base = format!("Assets/imported/{}", if slug.is_empty() { "import".to_string() } else { slug });

    // Notion 数据库：同名的 _all.csv 在就用它，另一份跳过
    let files: HashSet<&str> = src.files.iter().map(|s| s.as_str()).collect();
    let mut docs = Vec::new();
    let mut assets = Vec::new();
    let mut skipped = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut renamed = 0;
    let mut alias_csv: Vec<(String, String)> = Vec::new();
    for f in &src.files {
        let ext = ext_of(f);
        let kind_of = if DOC_TEXT.contains(&ext.as_str()) {
            Kind::Md
        } else if ext == "csv" && notion {
            let stem = &f[..f.len() - 4];
            if !stem.ends_with("_all") && files.contains(format!("{stem}_all.csv").as_str()) {
                // 页面里链的多半是这一份：链接指到 _all 那篇转出来的表格
                alias_csv.push((f.clone(), format!("{stem}_all.csv")));
                skipped.push(f.clone());
                continue;
            }
            Kind::Csv
        } else if kernel_convert::supports(&ext) {
            Kind::Convert
        } else if ASSET_EXTS.contains(&ext.as_str()) || !ext.is_empty() {
            Kind::Asset
        } else {
            skipped.push(f.clone());
            continue;
        };
        let segs: Vec<&str> = f.split('/').collect();
        let (dirs, file) = segs.split_at(segs.len() - 1);
        if kind_of == Kind::Asset {
            let first = format!("{asset_base}/{}", dirs.iter().map(|d| kernel_core::slugify(&clean_seg(d, notion))).filter(|s| !s.is_empty()).chain(std::iter::once(asset_name(&clean_seg(file[0], notion)))).collect::<Vec<_>>().join("/"));
            let (a, e) = match first.rsplit_once('.') { Some((a, e)) if !a.ends_with('/') => (a.to_string(), format!(".{e}")), _ => (first.clone(), String::new()) };
            let mut to = first.clone();
            let mut n = 2;
            // 库里已有同名附件：内容一样就直接用它（重搬一次时链接不变），不一样才另起名字
            let same = |p: &str| std::fs::read(root.join(p)).ok().zip(src.read(f)).map(|(x, y)| x == y).unwrap_or(false);
            while (used.contains(&to.to_lowercase()) || root.join(&to).exists() && !same(&to)) && n < 1000 {
                to = format!("{a}-{n}{e}");
                n += 1;
            }
            used.insert(to.to_lowercase());
            assets.push((f.clone(), to));
            continue;
        }
        let raw_stem = stem_of(file[0]);
        let raw_stem = if kind_of == Kind::Csv { raw_stem.strip_suffix("_all").unwrap_or(raw_stem) } else { raw_stem };
        let stem = clean_seg(raw_stem, notion);
        let stem0 = raw_stem.to_string();
        if stem != stem0 {
            renamed += 1;
        }
        let dir = dirs.iter().map(|d| clean_seg(d, notion)).collect::<Vec<_>>().join("/");
        let base = if dir.is_empty() { target.clone() } else { format!("{target}/{dir}") };
        let mut to = format!("{base}/{stem}.md");
        let mut n = 2;
        // 和这一批里的、库里已有的都不撞（重搬一次时内容一样的在写的时候跳过）
        while used.contains(&to.to_lowercase()) && n < 1000 {
            to = format!("{base}/{stem} {n}.md");
            n += 1;
        }
        used.insert(to.to_lowercase());
        docs.push((f.clone(), to, kind_of));
    }
    let mut p = Plan { kind, name: label, target, asset_base, docs, assets, skipped, renamed, by_rel: HashMap::new(), by_rel_noext: HashMap::new(), by_stem: HashMap::new(), asset_rel: HashMap::new(), asset_name: HashMap::new() };
    for (from, to, _) in &p.docs {
        let low = from.to_lowercase();
        p.by_rel.insert(low.clone(), to.clone());
        let noext = match low.rsplit_once('.') { Some((a, _)) => a.to_string(), None => low.clone() };
        p.by_rel_noext.insert(noext, to.clone());
        let fname = low.rsplit('/').next().unwrap_or(&low);
        p.by_stem.entry(stem_of(fname).to_string()).or_default().push(to.clone());
        if notion {
            // Notion 的链接里也常写去掉 id 的名字
            let c = clean_seg(stem_of(from.rsplit('/').next().unwrap_or(from)), true).to_lowercase();
            p.by_stem.entry(c).or_default().push(to.clone());
        }
    }
    for (skip, all) in &alias_csv {
        if let Some(to) = p.by_rel.get(&all.to_lowercase()).cloned() {
            p.by_rel.insert(skip.to_lowercase(), to);
        }
    }
    for (from, to) in &p.assets {
        let low = from.to_lowercase();
        p.asset_rel.insert(low.clone(), to.clone());
        p.asset_name.entry(low.rsplit('/').next().unwrap_or(&low).to_string()).or_default().push(to.clone());
    }
    p
}

impl Plan {
    fn find_doc_rel(&self, base_dir: &str, target: &str) -> Option<String> {
        let t = target.to_lowercase();
        let rel = if base_dir.is_empty() { t.clone() } else { format!("{}/{t}", base_dir.to_lowercase()) };
        for cand in [norm_path(&rel), norm_path(&t)].into_iter().flatten() {
            if let Some(x) = self.by_rel.get(&cand).or_else(|| self.by_rel_noext.get(&cand)) {
                return Some(x.clone());
            }
        }
        None
    }
    fn find_asset(&self, base_dir: &str, target: &str) -> Option<String> {
        let t = target.to_lowercase();
        let rel = if base_dir.is_empty() { t.clone() } else { format!("{}/{t}", base_dir.to_lowercase()) };
        for cand in [norm_path(&rel), norm_path(&t)].into_iter().flatten() {
            if let Some(x) = self.asset_rel.get(&cand) {
                return Some(x.clone());
            }
        }
        // Obsidian 只写文件名：按名字找（重名取第一个）
        let name = t.rsplit('/').next().unwrap_or(&t);
        self.asset_name.get(name).and_then(|v| v.first().cloned())
    }
    /// [[目标]] 里的目标 → 新的写法（不带 .md）；None = 接不上
    fn wiki_target(&self, raw: &str) -> Option<String> {
        let t = raw.trim();
        if t.is_empty() {
            return None;
        }
        let low = t.to_lowercase();
        if t.contains('/') {
            let noext = low.strip_suffix(".md").unwrap_or(&low).to_string();
            if let Some(x) = self.by_rel_noext.get(&noext).or_else(|| self.by_rel.get(&low)) {
                return Some(x.trim_end_matches(".md").to_string());
            }
            // 路径对不上，再按最后一段名字找
        }
        let name = low.rsplit('/').next().unwrap_or(&low);
        let name = name.strip_suffix(".md").unwrap_or(name);
        let hits = self.by_stem.get(name)?;
        let to = &hits[0];
        let new_stem = to.rsplit('/').next().unwrap_or(to).trim_end_matches(".md");
        // 名字没变、也不重名：原样留着（鹿蕊同样按名字找）；否则写成完整路径
        if hits.len() == 1 && new_stem.to_lowercase() == name && !t.contains('/') {
            Some(t.to_string())
        } else {
            Some(to.trim_end_matches(".md").to_string())
        }
    }
}

/* ───────── 正文里的链接 ───────── */

/// 改写一篇的链接：返回 (新正文, 改了几处, 接不上几处)
fn rewrite(md: &str, from_rel: &str, plan: &Plan) -> (String, usize, usize) {
    let dir = from_rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let (mut fixed, mut broken) = (0, 0);
    let mut out = String::with_capacity(md.len() + 64);
    let mut fence: Option<String> = None;
    for line in md.split_inclusive('\n') {
        let t = line.trim_start();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t[..3].to_string());
            out.push_str(line);
            continue;
        }
        out.push_str(&rewrite_line(line, dir, plan, &mut fixed, &mut broken));
    }
    (out, fixed, broken)
}

fn is_external(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:") || l.starts_with('#') || l.starts_with("data:") || l.starts_with("obsidian:") || l.starts_with("tel:") || l.starts_with("//")
}

fn rewrite_line(line: &str, dir: &str, plan: &Plan, fixed: &mut usize, broken: &mut usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // 行内代码原样
        if c == '`' {
            let ticks = chars[i..].iter().take_while(|&&x| x == '`').count();
            let close = (i + ticks..chars.len()).find(|&j| chars[j..].iter().take_while(|&&x| x == '`').count() == ticks && (j + ticks >= chars.len() || chars[j + ticks] != '`'));
            if let Some(j) = close {
                out.extend(&chars[i..j + ticks]);
                i = j + ticks;
                continue;
            }
        }
        let embed = c == '!' && chars.get(i + 1) == Some(&'[') && chars.get(i + 2) == Some(&'[');
        if (c == '[' && chars.get(i + 1) == Some(&'[')) || embed {
            let s = if embed { i + 3 } else { i + 2 };
            if let Some(e) = (s..chars.len().saturating_sub(1)).find(|&j| chars[j] == ']' && chars[j + 1] == ']') {
                let inner: String = chars[s..e].iter().collect();
                let (target, alias) = match inner.split_once('|') { Some((a, b)) => (a.to_string(), Some(b.to_string())), None => (inner.clone(), None) };
                let (path, anchor) = match target.split_once('#') { Some((a, b)) => (a.to_string(), Some(b.to_string())), None => (target.clone(), None) };
                let is_note = path.is_empty() || ext_of(&path).is_empty() || matches!(ext_of(&path).as_str(), "md" | "markdown");
                if !is_note || embed && !matches!(ext_of(&path).as_str(), "" | "md" | "markdown") {
                    if let Some(a) = plan.find_asset(dir, &path) {
                        // ![[图.png|300]] 的 300 是宽度，不当说明文字
                        let alt = alias.filter(|x| !x.chars().all(|c| c.is_ascii_digit() || c == 'x')).unwrap_or_default();
                        if embed { out.push_str(&format!("![{alt}]({a})")); } else { out.push_str(&format!("[{}]({a})", if alt.is_empty() { path.clone() } else { alt })); }
                        *fixed += 1;
                        i = e + 2;
                        continue;
                    }
                } else if !path.is_empty() {
                    if let Some(w) = plan.wiki_target(&path) {
                        let mut s2 = w.clone();
                        if let Some(a) = &anchor { s2.push('#'); s2.push_str(a); }
                        // 改成了完整路径又没有别名：显示原来写的最后一段，不把 Notes/导入/… 整串露出来
                        let alias = alias.clone().or_else(|| (w != path.trim()).then(|| path.trim().rsplit('/').next().unwrap_or(path.trim()).trim_end_matches(".md").to_string()));
                        if let Some(a) = &alias { s2.push('|'); s2.push_str(a); }
                        // 嵌入整篇笔记：鹿蕊不做嵌入，留成一个链接
                        out.push_str(&format!("[[{s2}]]"));
                        if w != path.trim() { *fixed += 1; }
                        i = e + 2;
                        continue;
                    }
                }
                if !path.is_empty() { *broken += 1; }
                out.extend(&chars[i..e + 2]);
                i = e + 2;
                continue;
            }
        }
        // [文字](地址) / ![说明](地址)
        let img = c == '!' && chars.get(i + 1) == Some(&'[');
        if c == '[' || img {
            let s = if img { i + 2 } else { i + 1 };
            let mut depth = 1;
            let mut j = s;
            while j < chars.len() {
                if chars[j] == '[' { depth += 1; }
                if chars[j] == ']' { depth -= 1; if depth == 0 { break; } }
                j += 1;
            }
            if j < chars.len() && chars.get(j + 1) == Some(&'(') {
                let mut k = j + 2;
                let mut pd = 1;
                while k < chars.len() {
                    if chars[k] == '(' { pd += 1; }
                    if chars[k] == ')' { pd -= 1; if pd == 0 { break; } }
                    k += 1;
                }
                if k < chars.len() {
                    let text: String = chars[s..j].iter().collect();
                    let raw: String = chars[j + 2..k].iter().collect();
                    let mut url = raw.trim().to_string();
                    if url.starts_with('<') { url = url.trim_start_matches('<').split('>').next().unwrap_or("").to_string(); }
                    else if let Some((u, _title)) = url.split_once(" \"") { url = u.to_string(); }
                    if !url.is_empty() && !is_external(&url) {
                        let (p, anchor) = match url.split_once('#') { Some((a, b)) => (a.to_string(), Some(b.to_string())), None => (url.clone(), None) };
                        let dec = pct_decode(&p);
                        let e = ext_of(&dec);
                        let doc = if e.is_empty() || DOC_TEXT.contains(&e.as_str()) || e == "csv" || kernel_convert::supports(&e) { plan.find_doc_rel(dir, &dec) } else { None };
                        if let Some(d) = doc {
                            let mut w = d.trim_end_matches(".md").to_string();
                            if let Some(a) = anchor { w.push('#'); w.push_str(&a); }
                            if img { out.push_str(&format!("[[{w}]]")); } else if text.trim().is_empty() { out.push_str(&format!("[[{w}]]")); } else { out.push_str(&format!("[[{w}|{}]]", text.replace(['[', ']', '|'], " ").trim())); }
                            *fixed += 1;
                            i = k + 1;
                            continue;
                        }
                        if let Some(a) = plan.find_asset(dir, &dec) {
                            out.push_str(&format!("{}[{text}]({a})", if img { "!" } else { "" }));
                            *fixed += 1;
                            i = k + 1;
                            continue;
                        }
                        if !e.is_empty() { *broken += 1; }
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// frontmatter 里记一行原来在哪（没有 frontmatter 就加一段）
fn stamp(md: &str, origin: &str) -> String {
    let q = format!("imported_from: \"{}\"", origin.replace('\\', "\\\\").replace('"', "\\\""));
    if let Some(rest) = md.strip_prefix("---\n").or_else(|| md.strip_prefix("---\r\n")) {
        if rest.contains("\n---") || rest.starts_with("---") {
            if rest.lines().any(|l| l.starts_with("imported_from:")) {
                return md.to_string();
            }
            return format!("---\n{q}\n{rest}");
        }
    }
    format!("---\n{q}\n---\n\n{md}")
}

/// Notion 数据库 csv → 表格文档；第一列能对上同库里的页面就链过去
fn csv_doc(text: &str, title: &str, plan: &Plan, dir: &str) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur = Vec::new();
    let mut cell = String::new();
    let mut q = false;
    let cs: Vec<char> = text.trim_start_matches('\u{feff}').chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if q {
            if c == '"' && cs.get(i + 1) == Some(&'"') { cell.push('"'); i += 1; }
            else if c == '"' { q = false; }
            else { cell.push(c); }
        } else if c == '"' { q = true; }
        else if c == ',' { cur.push(std::mem::take(&mut cell)); }
        else if c == '\n' { cur.push(std::mem::take(&mut cell)); rows.push(std::mem::take(&mut cur)); }
        else if c != '\r' { cell.push(c); }
        i += 1;
    }
    if !cell.is_empty() || !cur.is_empty() { cur.push(cell); rows.push(cur); }
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    let mut md = format!("# {title}\n\n");
    if rows.is_empty() {
        md.push_str("（空的数据库）\n");
        return md;
    }
    let w = rows.iter().map(|r| r.len()).max().unwrap_or(1).min(30);
    let esc = |s: &str| s.replace('|', "\\|").replace('\n', "<br>");
    for (n, r) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..w).map(|k| {
            let v = r.get(k).cloned().unwrap_or_default();
            if n > 0 && k == 0 && !v.trim().is_empty() {
                // 行页面在「数据库名/行标题 id.md」里
                let sub = format!("{}/{}", title, v.trim());
                if let Some(d) = plan.by_stem.get(&v.trim().to_lowercase()).and_then(|ds| ds.iter().find(|d| d.contains(&format!("/{title}/"))).or(ds.first())).cloned().or_else(|| plan.find_doc_rel(dir, &format!("{sub}.md"))) {
                    return format!("[[{}\\|{}]]", d.trim_end_matches(".md"), esc(v.trim()));
                }
            }
            esc(v.trim())
        }).collect();
        md.push_str(&format!("| {} |\n", cells.join(" | ")));
        if n == 0 {
            md.push_str(&format!("|{}\n", " --- |".repeat(w)));
        }
    }
    md
}

/* ───────── 后台任务 ───────── */

#[derive(Clone, Default, Serialize)]
pub struct WizJob {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub ok: usize,
    pub failed: Vec<String>,
    pub phase: String,
    pub name: String,
    pub run: String,
    pub finished: u64,
    #[serde(skip)]
    pub stop: bool,
}

fn wjobs() -> &'static Mutex<HashMap<PathBuf, WizJob>> {
    static J: std::sync::OnceLock<Mutex<HashMap<PathBuf, WizJob>>> = std::sync::OnceLock::new();
    J.get_or_init(|| Mutex::new(HashMap::new()))
}
pub fn snapshot(root: &Path) -> Option<WizJob> {
    wjobs().lock().unwrap().get(root).cloned()
}
pub fn stop(root: &Path) -> bool {
    match wjobs().lock().unwrap().get_mut(root) {
        Some(j) if j.running => {
            j.stop = true;
            true
        }
        _ => false,
    }
}
fn set(root: &Path, f: impl FnOnce(&mut WizJob)) {
    if let Some(j) = wjobs().lock().unwrap().get_mut(root) {
        f(j);
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Run {
    pub id: String,
    pub at: u64,
    pub name: String,
    pub kind: String,
    pub origin: String,
    pub target: String,
    pub docs: Vec<String>,
    pub assets: Vec<String>,
    #[serde(default)]
    pub undone: bool,
}
fn load_runs(root: &Path) -> Vec<Run> {
    std::fs::read_to_string(runs_path(root)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_runs(root: &Path, r: &[Run]) {
    let p = runs_path(root);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(p, serde_json::to_vec_pretty(r).unwrap_or_default());
}

#[derive(Deserialize, Default)]
pub struct Req {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub upload: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

/// 一篇文档的最终正文
fn doc_text(root: &Path, src: &Src, plan: &Plan, from: &str, to: &str, kind: Kind, write_assets: bool) -> Result<(String, usize, usize), String> {
    let bytes = src.read(from).ok_or("读不出来")?;
    let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let title = to.rsplit('/').next().unwrap_or(to).trim_end_matches(".md").to_string();
    let (md, fixed, broken) = match kind {
        Kind::Md => {
            let text = kernel_convert::decode_text(&bytes);
            let text = if ext_of(from) == "txt" && !text.trim_start().starts_with('#') { format!("# {title}\n\n{text}") } else { text };
            rewrite(&text, from, plan)
        }
        Kind::Csv => (csv_doc(&kernel_convert::decode_text(&bytes), &title, plan, dir), 0, 0),
        Kind::Convert => {
            let mut c = kernel_convert::convert(&ext_of(from), &bytes)?;
            let slug = kernel_core::slugify(&title);
            let base = plan.asset_base.clone();
            let mut n = 0;
            c.resolve_images(|img| {
                n += 1;
                let ext = img.name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).filter(|e| e.len() <= 5).unwrap_or_else(|| "png".into());
                let rel = format!("{base}/{slug}-{n}.{ext}");
                if write_assets {
                    let abs = root.join(&rel);
                    let _ = std::fs::create_dir_all(abs.parent()?);
                    std::fs::write(&abs, &img.bytes).ok()?;
                }
                Some(rel)
            });
            let text = c.document(&title);
            rewrite(&text, from, plan)
        }
        Kind::Asset => unreachable!(),
    };
    Ok((stamp(&md, &src.origin_of(from)), fixed, broken))
}

/// POST /field/wizard/upload?name=x.zip（正文是 zip 本身）
pub async fn upload(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    if body.len() < 22 || &body[..2] != b"PK" {
        return err(StatusCode::BAD_REQUEST, "要上传 .zip（Notion、飞书导出的就是 zip；Obsidian 库请先压缩成 zip）").into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let id = format!("{:x}-{}", now_secs(), &kernel_core::Cas::hash_hex(&body)[..10]);
    let dir = uploads_dir(&root);
    let _ = std::fs::create_dir_all(&dir);
    // 只留最近几份上传
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut old: Vec<(std::time::SystemTime, PathBuf)> = rd.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
        old.sort();
        while old.len() > 8 {
            let (_, p) = old.remove(0);
            let _ = std::fs::remove_file(p);
        }
    }
    if let Err(e) = std::fs::write(dir.join(format!("{id}.zip")), &body) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let name = q.get("name").cloned().unwrap_or_else(|| "导入.zip".into());
    let _ = std::fs::write(dir.join(format!("{id}.json")), json!({ "name": name, "size": body.len() }).to_string());
    Json(json!({ "upload": id, "size": body.len() })).into_response()
}

fn summary(root: &Path, src: &Src, p: &Plan) -> Value {
    let mut fixed = 0;
    let mut broken = 0;
    let mut exists = 0;
    let mut sample = Vec::new();
    for (from, to, kind) in &p.docs {
        if root.join(to).exists() {
            exists += 1;
        }
        if *kind == Kind::Md {
            if let Some(b) = src.read(from) {
                let (_, f, x) = rewrite(&kernel_convert::decode_text(&b), from, p);
                fixed += f;
                broken += x;
            }
        }
        if sample.len() < 30 {
            sample.push(json!({ "from": from, "to": to }));
        }
    }
    let conv = p.docs.iter().filter(|d| d.2 == Kind::Convert).count();
    let csv = p.docs.iter().filter(|d| d.2 == Kind::Csv).count();
    json!({
        "kind": p.kind, "name": p.name, "target": p.target, "origin": src.origin,
        "docs": p.docs.len(), "converted": conv, "tables": csv, "assets": p.assets.len(), "skipped": p.skipped.len(),
        "renamed": p.renamed, "links": fixed, "broken": broken, "exists": exists, "sample": sample,
        "files": src.files.len(),
    })
}

/// POST /field/wizard/plan：只看不写
pub async fn plan_api(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Req = serde_json::from_slice(&body).unwrap_or_default();
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let r = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let src = Src::open(&root, req.path.as_deref(), req.upload.as_deref())?;
        let p = plan(&root, &src, req.kind.as_deref().unwrap_or("auto"), req.target.as_deref());
        if p.docs.is_empty() && p.assets.is_empty() {
            return Err("这里面没有能搬的文档".into());
        }
        Ok(summary(&root, &src, &p))
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// POST /field/wizard/run：后台搬
pub async fn run_api(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Req = serde_json::from_slice(&body).unwrap_or_default();
    let root = st.kernel.lock().unwrap().vault.root.clone();
    {
        let mut js = wjobs().lock().unwrap();
        if js.get(&root).map(|j| j.running).unwrap_or(false) {
            return err(StatusCode::CONFLICT, "已经在搬了，等这一次搬完").into_response();
        }
        js.insert(root.clone(), WizJob { running: true, phase: "读来源".into(), ..Default::default() });
    }
    let st2 = st.clone();
    let root2 = root.clone();
    super::sources::spawn_background("litmus-wizard", move || {
        let root = root2;
        let fail = |e: String| set(&root, |j| { j.running = false; j.failed.push(e); j.phase = "没搬成".into(); j.finished = now_secs(); });
        let src = match Src::open(&root, req.path.as_deref(), req.upload.as_deref()) {
            Ok(s) => s,
            Err(e) => return fail(e),
        };
        let p = plan(&root, &src, req.kind.as_deref().unwrap_or("auto"), req.target.as_deref());
        let id = format!("{:x}", now_secs());
        set(&root, |j| { j.total = p.docs.len() + p.assets.len(); j.name = p.name.clone(); j.run = id.clone(); j.phase = "附件".into(); });
        let mut run = Run { id: id.clone(), at: now_secs(), name: p.name.clone(), kind: p.kind.into(), origin: src.origin.clone(), target: p.target.clone(), ..Default::default() };
        let stopped = || wjobs().lock().unwrap().get(&root).map(|j| j.stop).unwrap_or(false);
        // 先放附件，文档里的图就能马上显示
        for (from, to) in &p.assets {
            if stopped() { break; }
            let abs = root.join(to);
            let ok = src.read(from).map(|b| { let _ = std::fs::create_dir_all(abs.parent().unwrap()); std::fs::write(&abs, b).is_ok() }).unwrap_or(false);
            if ok { run.assets.push(to.clone()); }
            set(&root, |j| { j.done += 1; if !ok { j.failed.push(from.clone()); } });
        }
        set(&root, |j| j.phase = "文档".into());
        for (from, to, kind) in &p.docs {
            if stopped() { break; }
            set(&root, |j| j.phase = format!("文档：{}", to.rsplit('/').next().unwrap_or(to)));
            let r = doc_text(&root, &src, &p, from, to, *kind, true).and_then(|(md, _, _)| {
                // 重搬一次：同一篇内容一样就跳过，不一样另存
                let mut to = to.clone();
                if let Ok(old) = std::fs::read_to_string(root.join(&to)) {
                    // 库里存的是规范化以后的样子（frontmatter 等会重排）：两种都比一下
                    let norm = kernel_core::from_markdown(&md).ok().map(|d| kernel_core::to_markdown(&d));
                    if old == md || norm.as_deref() == Some(old.as_str()) { return Ok(None); }
                    let base = to.trim_end_matches(".md").to_string();
                    let mut n = 2;
                    while root.join(&to).exists() && n < 1000 { to = format!("{base} {n}.md"); n += 1; }
                }
                let mut k = st2.kernel.lock().unwrap();
                k.put_doc(&to, &md).map_err(|e| e.to_string())?;
                Ok(Some(to))
            });
            set(&root, |j| {
                j.done += 1;
                match &r {
                    Ok(_) => j.ok += 1,
                    Err(e) => j.failed.push(format!("{from}：{e}")),
                }
            });
            if let Ok(Some(t)) = r { run.docs.push(t); }
        }
        let stop_now = stopped();
        let mut runs = load_runs(&root);
        runs.push(run.clone());
        save_runs(&root, &runs);
        if let Some(id) = req.upload.as_deref() {
            if !stop_now {
                let _ = std::fs::remove_file(uploads_dir(&root).join(format!("{id}.zip")));
                let _ = std::fs::remove_file(uploads_dir(&root).join(format!("{id}.json")));
            }
        }
        audit_log(&root.join(".litmus/local"), "wizard-import", &json!({ "id": run.id, "kind": run.kind, "origin": run.origin, "docs": run.docs.len(), "assets": run.assets.len() }));
        super::sources::REV.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        set(&root, |j| { j.running = false; j.phase = if stop_now { "停下了：已搬的都留着".into() } else { "搬完了".into() }; j.finished = now_secs(); });
    });
    Json(json!({ "ok": true })).into_response()
}

/// GET /field/wizard：最近的搬家记录
pub async fn runs_api(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let runs: Vec<Value> = load_runs(&root).iter().rev().take(20).map(|r| json!({ "id": r.id, "at": r.at, "name": r.name, "kind": r.kind, "origin": r.origin, "target": r.target, "docs": r.docs.len(), "assets": r.assets.len(), "undone": r.undone, "paths": r.docs.iter().take(2000).collect::<Vec<_>>() })).collect();
    Json(json!({ "runs": runs, "job": snapshot(&root) })).into_response()
}

/// POST /field/wizard/undo {id}：这次搬进来的文档进回收站（能找回），附件拿掉
pub async fn undo_api(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let mut runs = load_runs(&root);
    let Some(r) = runs.iter_mut().find(|r| r.id == id && !r.undone) else { return err(StatusCode::NOT_FOUND, "这次搬家找不到了，或已经撤销过").into_response() };
    let mut moved = 0;
    {
        let mut k = st.kernel.lock().unwrap();
        for d in &r.docs {
            if super::trash_move(&mut k, d, false).is_ok() {
                moved += 1;
            }
        }
        let _ = k.sync_all();
    }
    for a in &r.assets {
        if a.starts_with("Assets/imported/") && !a.contains("..") {
            let _ = std::fs::remove_file(root.join(a));
        }
    }
    r.undone = true;
    save_runs(&root, &runs);
    super::sources::REV.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Json(json!({ "trashed": moved })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(files: &[(&str, &str)]) -> Src {
        let m: HashMap<String, Vec<u8>> = files.iter().map(|(k, v)| (k.to_string(), v.as_bytes().to_vec())).collect();
        let mut fs: Vec<String> = m.keys().cloned().collect();
        fs.sort();
        Src { store: Store::Mem(m), files: fs, prefix: String::new(), name: "测试".into(), origin: "t.zip".into(), is_dir: false }
    }

    #[test]
    fn notion_names_and_links() {
        let id = "0123456789abcdef0123456789abcdef";
        let id2 = "fedcba9876543210fedcba9876543210";
        let s = mem(&[
            (&format!("项目 {id}.md"), &format!("# 项目\n\n见 [子页](%E9%A1%B9%E7%9B%AE%20{id}/%E5%AD%90%E9%A1%B5%20{id2}.md) 和 ![图](%E9%A1%B9%E7%9B%AE%20{id}/a%20b.png)\n")),
            (&format!("项目 {id}/子页 {id2}.md"), "# 子页\n\n内容"),
            (&format!("项目 {id}/a b.png"), "PNG"),
        ]);
        assert_eq!(s.detect(), "notion");
        let p = plan(Path::new("/nonexistent"), &s, "auto", None);
        let tos: Vec<&str> = p.docs.iter().map(|d| d.1.as_str()).collect();
        assert!(tos.contains(&"Notes/导入/测试/项目.md") && tos.contains(&"Notes/导入/测试/项目/子页.md"), "{tos:?}");
        let (md, fixed, broken) = rewrite(&String::from_utf8_lossy(&s.read(&format!("项目 {id}.md")).unwrap()), &format!("项目 {id}.md"), &p);
        assert_eq!((fixed, broken), (2, 0), "{md}");
        assert!(md.contains("[[Notes/导入/测试/项目/子页|子页]]"), "{md}");
        assert!(md.contains("![图](Assets/imported/"), "{md}");
        assert!(!md.contains(' ') || md.contains("Assets/imported/") && !md.split("](Assets").nth(1).unwrap().split(')').next().unwrap().contains(' '));
    }

    #[test]
    fn obsidian_wikilinks_embeds_and_code() {
        let s = mem(&[
            ("日记/今天.md", "---\ntags: [a]\n---\n见 [[想法]]、[[卡片/想法|另一个]]、[[想法#小节]]\n![[pic.png|300]]\n![[想法]]\n`[[别动]]`\n```\n[[也别动]]\n```\n[[没有这篇]]\n"),
            ("想法.md", "# 想法"),
            ("卡片/想法.md", "# 卡片里的想法"),
            ("附件/pic.png", "PNG"),
        ]);
        let p = plan(Path::new("/nonexistent"), &s, "obsidian", None);
        let (md, _, broken) = rewrite(&String::from_utf8_lossy(&s.read("日记/今天.md").unwrap()), "日记/今天.md", &p);
        assert!(md.contains("[[Notes/导入/测试/卡片/想法|另一个]]"), "{md}");
        assert!(md.contains("![](Assets/imported/"), "{md}");
        assert!(md.contains("`[[别动]]`") && md.contains("[[也别动]]"), "{md}");
        assert_eq!(broken, 1, "{md}");
        let st = stamp(&md, "/x/今天.md");
        assert!(st.starts_with("---\nimported_from: \"/x/今天.md\"\ntags: [a]\n---"), "{st}");
    }

    #[test]
    fn notion_csv_becomes_a_table() {
        let id = "0123456789abcdef0123456789abcdef";
        let s = mem(&[
            (&format!("任务 {id}.csv"), "名称,状态\n旧,x\n"),
            (&format!("任务 {id}_all.csv"), "\u{feff}名称,状态\n写周报,进行中\n\"带,逗号\",完成\n"),
            (&format!("任务 {id}/写周报 {id}.md"), "# 写周报"),
        ]);
        let p = plan(Path::new("/nonexistent"), &s, "notion", None);
        assert_eq!(p.docs.iter().filter(|d| d.2 == Kind::Csv).count(), 1);
        let (from, to, _) = p.docs.iter().find(|d| d.2 == Kind::Csv).unwrap().clone();
        assert_eq!(to, "Notes/导入/测试/任务.md");
        let (md, _, _) = doc_text(Path::new("/nonexistent"), &s, &p, &from, &to, Kind::Csv, false).unwrap();
        assert!(md.contains("| 名称 | 状态 |") && md.contains("[[Notes/导入/测试/任务/写周报\\|写周报]]") && md.contains("| 带,逗号 | 完成 |"), "{md}");
    }

    #[test]
    fn zip_with_nested_zip_and_top_folder() {
        let mut inner = kernel_convert::zip::ZipWriter::new();
        inner.add("Export/页 0123456789abcdef0123456789abcdef.md", "# 页".as_bytes(), false);
        let inner = inner.finish();
        let mut outer = kernel_convert::zip::ZipWriter::new();
        outer.add("Export-Part-1.zip", &inner, true);
        outer.add("__MACOSX/._x", b"junk", false);
        let s = Src::from_zip(&outer.finish(), "Export-abc", "x.zip").unwrap();
        assert_eq!(s.files, vec!["页 0123456789abcdef0123456789abcdef.md".to_string()]);
        let p = plan(Path::new("/nonexistent"), &s, "auto", None);
        assert_eq!(p.docs[0].1, "Notes/导入/Notion/页.md", "Notion 的导出叫 Export-…，落到「Notion」下");
    }

    #[test]
    fn feishu_word_export_becomes_markdown() {
        let none = |_: &str| None;
        let docx = kernel_convert::export::to_docx("# 季度复盘\n\n这一季做了三件事。\n\n## 下一步\n\n- 补数据\n", &kernel_convert::export::ExportOpts { title: None, load_image: &none }).unwrap();
        let mut z = kernel_convert::zip::ZipWriter::new();
        z.add("飞书导出/季度复盘.docx", &docx, false);
        let s = Src::from_zip(&z.finish(), "飞书导出", "f.zip").unwrap();
        assert_eq!(s.detect(), "feishu");
        let p = plan(Path::new("/nonexistent"), &s, "auto", None);
        assert_eq!(p.target, "Notes/导入/飞书");
        let (from, to, k) = p.docs[0].clone();
        assert_eq!(to, "Notes/导入/飞书/季度复盘.md");
        let (md, _, _) = doc_text(Path::new("/nonexistent"), &s, &p, &from, &to, k, false).unwrap();
        assert!(md.contains("# 季度复盘") && md.contains("这一季做了三件事") && md.contains("- 补数据"), "{md}");
    }

    #[test]
    fn pct_and_paths() {
        assert_eq!(pct_decode("a%20b%E4%B8%AD"), "a b中");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(norm_path("a/b/../c/./d.md").as_deref(), Some("a/c/d.md"));
        assert_eq!(norm_path("../x"), None);
    }
}
