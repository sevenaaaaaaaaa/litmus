//! 筛选（0.17）：把有用的知识留下，把会话过程里产生的文档剔掉。
//!
//! - **判断**：先按规则（来源种类、路径、文件名、正文特征）分成十二类，每类按用户的设置算「保留」或「剔除」；
//!   规则拿不准的（把握低于 0.6）分批交给模型复核，并写明理由。模型只看标题、路径与开头一段。
//! - **先出清单**：结果存在这台设备（`.litmus/local/curate.json`），用户在清单里改判个别篇，确认后才执行。
//! - **剔除**：库里的文档进回收站（按保留天数自动清除，期间可恢复）；库外的（数据源接进来的）从索引里拿掉、
//!   原件不动，以后重新扫描也不会再出现，随时可以放回来。每次执行是一批，可整批撤销。
//! - **以后**：数据源每次扫描到新东西，先按规则判；像过程文档的不直接上场，放进「待确认」，进度条里提示。
//! - 保留 / 剔除哪些类、总要留或总要剔的词，存在随库同步的 `.litmus/curate.json`。

use super::field::{current, read_synced, write_synced};
use super::{audit_log, check_token, err, AppState};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// (id, 名字, 默认保留?, 一句话说明——也给模型看)
pub const CATS: [(&str, &str, bool, &str); 12] = [
    ("doc", "文字文档", true, "说明、手册、规范、README 等成文的文档"),
    ("report", "数据报告", true, "带数据、指标、结论的分析或报告"),
    ("plan", "方案", true, "方案、设计、提案、规格、架构"),
    ("article", "文章", true, "完整成篇的文章、随笔、长文"),
    ("tutorial", "教程", true, "教程、指南、操作步骤、入门"),
    ("course", "课程", true, "课程、讲义、分章节的学习材料"),
    ("harness", "harness", true, "给 AI 用的工作约定：CLAUDE.md、AGENTS.md、系统提示词、agent 定义、命令"),
    ("skill", "skills", true, "AI 技能包：SKILL.md 与它的说明"),
    ("note", "笔记", true, "自己记的笔记、摘录、随手记"),
    ("session", "会话", false, "和 AI 的对话记录、会话导出"),
    ("process", "过程文档", false, "干活过程中产生的：进度、交接、计划草稿、审计、简报、待办清单、工作日志、临时记录、改动小结"),
    ("generated", "生成物", false, "工具自动生成的：导出页、日志、锁文件、几乎全是代码或数据的转储"),
];

pub fn cat_name(id: &str) -> &'static str {
    CATS.iter().find(|c| c.0 == id).map(|c| c.1).unwrap_or("其他")
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub keep: Vec<String>,
    #[serde(default)]
    pub drop: Vec<String>,
    /// 路径或标题含这些词：总是保留 / 总是剔除（优先于一切规则）
    #[serde(default)]
    pub keep_words: Vec<String>,
    #[serde(default)]
    pub drop_words: Vec<String>,
    /// 新扫描到的先过一遍规则，像过程文档的放进待确认
    #[serde(default = "yes")]
    pub auto: bool,
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            keep: CATS.iter().filter(|c| c.2).map(|c| c.0.to_string()).collect(),
            drop: CATS.iter().filter(|c| !c.2).map(|c| c.0.to_string()).collect(),
            keep_words: Vec::new(),
            drop_words: Vec::new(),
            auto: true,
        }
    }
}

impl Config {
    pub fn keeps(&self, cat: &str) -> bool {
        if self.drop.iter().any(|c| c == cat) {
            return false;
        }
        self.keep.iter().any(|c| c == cat) || CATS.iter().find(|c| c.0 == cat).map(|c| c.2).unwrap_or(true)
    }
}

pub fn load_config(root: &Path) -> Config {
    read_synced(root, "curate.json", "curate-config.json").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

// ───────── 规则 ─────────

/// 规则判断的结果：类别、把握（0–1）、理由。
#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    pub cat: &'static str,
    pub conf: f32,
    pub why: String,
}

fn j(cat: &'static str, conf: f32, why: impl Into<String>) -> Judged {
    Judged { cat, conf, why: why.into() }
}

fn has_any(s: &str, words: &[&str]) -> Option<String> {
    words.iter().find(|w| s.contains(*w)).map(|w| w.to_string())
}

/// 路径按段切开后，有没有某一段正好是 / 以某词开头（避免 "status" 误中 "statusbar.md" 之外的词中词）。
fn seg_hit(path: &str, words: &[&str]) -> Option<String> {
    for seg in path.split(['/', '\\']) {
        let s = seg.trim_end_matches(".md").trim_end_matches(".txt");
        let parts: Vec<&str> = s.split(['-', '_', ' ', '.', '·']).filter(|x| !x.is_empty()).collect();
        for w in words {
            if s == *w || parts.iter().any(|p| p == w) {
                return Some(w.to_string());
            }
        }
    }
    None
}

/// 一篇文档按规则判类。`path` 是库内路径或原处路径（小写比较），`kind` 是数据源条目种类（agent / md / pdf…）。
pub fn classify(path: &str, title: &str, text: &str, kind: &str) -> Judged {
    let p = path.to_lowercase();
    let t = title.to_lowercase();
    let fname = p.rsplit(['/', '\\']).next().unwrap_or(&p).to_string();
    let head: String = text.chars().take(4000).collect();
    let hl = head.to_lowercase();
    let lines: Vec<&str> = head.lines().collect();
    let n = lines.len().max(1);
    let body_chars = text.chars().filter(|c| !c.is_whitespace()).count();

    // 会话：来源就是 agent 会话，或正文是一问一答的记录
    if kind == "agent" {
        return j("session", 0.95, "来自 Claude Code / Codex 会话");
    }
    let turns = lines
        .iter()
        .filter(|l| {
            let l = l.trim_start();
            ["human:", "user:", "assistant:", "**问**：", "**答**：", "**user**", "**assistant**", "> user:", "### user", "### assistant", "## user", "## assistant"].iter().any(|m| l.to_lowercase().starts_with(m))
        })
        .count();
    if (p.contains("notes/agent/") || p.starts_with("agent/")) && has_any(&t, &["对话", "会话", "conversation", "chat"]).is_some() {
        return j("session", 0.85, "Agent 对话留下的记录");
    }
    if turns >= 3 || (head.contains("## 结论") && head.contains("## 过程") && head.contains("**问**")) {
        return j("session", 0.9, "正文是一问一答的会话记录");
    }
    // skills
    if fname == "skill.md" || (p.contains("/skills/") && (hl.contains("\ndescription:") || hl.starts_with("---\nname:"))) {
        return j("skill", 0.95, "SKILL.md 技能包");
    }
    // harness：给 AI 的工作约定
    const HARNESS_FILES: [&str; 9] = ["claude.md", "agents.md", "gemini.md", ".cursorrules", "copilot-instructions.md", "conventions.md", "cursor.md", "codex.md", "system-prompt.md"];
    if HARNESS_FILES.contains(&fname.as_str()) || fname.ends_with(".mdc") || fname.ends_with(".prompt.md") {
        return j("harness", 0.92, format!("AI 工作约定文件 {fname}"));
    }
    if ["/.claude/agents/", "/.claude/commands/", "/.cursor/rules/", "/prompts/", "/harness/", "/.github/prompts/"].iter().any(|s| p.contains(s)) || has_any(&t, &["harness", "system prompt", "系统提示词", "提示词"]).is_some() {
        return j("harness", 0.85, "agent 定义、命令或提示词");
    }
    if p.contains("/skills/") {
        return j("skill", 0.8, "放在 skills/ 里");
    }

    // 标题与路径里的「有用」信号
    let keep_kw: Option<(&'static str, &str)> = [
        ("course", &["课程", "讲义", "第一课", "第一讲", "lesson", "curriculum", "syllabus", "course"][..]),
        ("tutorial", &["教程", "指南", "入门", "上手", "tutorial", "guide", "how to", "how-to", "walkthrough", "cookbook"][..]),
        ("report", &["报告", "分析", "调研", "研究", "数据", "report", "analysis", "benchmark", "研报"][..]),
        ("plan", &["方案", "设计", "提案", "规格", "架构", "proposal", "design", "spec", "rfc", "prd", "规划"][..]),
    ]
    .iter()
    .find_map(|(c, ws)| ws.iter().find(|w| t.contains(*w) || fname.contains(*w)).map(|w| (*c, *w)));
    let lesson_no = {
        let re_hit = ["第一", "第二", "第三", "第四", "第五", "第六", "第七", "第八", "第九", "第十"].iter().any(|n| ["课", "讲", "章"].iter().any(|u| t.contains(&format!("{n}{u}"))));
        re_hit || t.starts_with("lesson") || t.starts_with("module ")
    };

    // 过程文档：路径或文件名
    const PROC_STRONG: [&str; 30] = [
        "ops", "_cowork", "handoff", "progress", "status", "scratch", "tmp", "temp", "wip", "todo", "todos", "audit", "brief", "briefing", "session", "sessions", "transcript", "worklog", "log", "logs", "changelog-draft", "notes-to-self", "交接", "进度", "审计", "简报", "草稿", "临时", "日志", "待办",
    ];
    const PROC_WEAK: [&str; 14] = ["plan", "plans", "draft", "drafts", "memo", "计划", "记录", "过程", "复盘", "周报", "日报", "summary", "recap", "worknotes"];
    let proc_seg = seg_hit(&p, &PROC_STRONG);
    if let Some(w) = &proc_seg {
        if let Some((c, kw)) = keep_kw {
            return j(c, 0.5, format!("放在「{w}」这种过程目录里，但标题像{}（{kw}）", cat_name(c)));
        }
        return j("process", 0.85, format!("路径里有「{w}」"));
    }
    // 正文特征：待办清单、时间流水、改动小结
    let checks = lines.iter().filter(|l| {
        let l = l.trim_start();
        l.starts_with("- [ ]") || l.starts_with("- [x]") || l.starts_with("- [X]") || l.starts_with("* [ ]")
    }).count();
    let stamps = lines
        .iter()
        .filter(|l| {
            let b = l.trim_start().trim_start_matches(['-', '*', '#', ' ', '[']).as_bytes();
            b.len() >= 16 && b[4] == b'-' && b[7] == b'-' && b[..4].iter().all(|c| c.is_ascii_digit()) && (b[10] == b' ' || b[10] == b'T') && b[13] == b':'
        })
        .count();
    let recap = (hl.contains("## summary") || hl.contains("## 改动") || hl.contains("## 本次") || hl.contains("## 已完成") || hl.contains("## changes"))
        && (hl.contains("next step") || hl.contains("下一步") || hl.contains("待办") || hl.contains("todo") || hl.contains("## 验证") || hl.contains("## 测试"));
    if checks >= 5 && checks * 3 >= n {
        return j("process", 0.75, format!("大半是待办勾选项（{checks} 条）"));
    }
    if stamps >= 5 {
        return j("process", 0.72, format!("按时间记的流水（{stamps} 条时间戳）"));
    }
    if recap {
        return j("process", 0.7, "像一次干活的改动小结 / 交接");
    }
    if let Some((c, kw)) = keep_kw {
        return j(c, if lesson_no && c == "course" { 0.9 } else { 0.78 }, format!("标题像{}（{kw}）", cat_name(c)));
    }
    if lesson_no {
        return j("course", 0.8, "分课 / 分讲的学习材料");
    }
    if let Some(w) = seg_hit(&p, &PROC_WEAK).or_else(|| has_any(&t, &["计划", "草稿", "过程", "复盘", "周报", "日报"])) {
        return j("process", 0.5, format!("名字里有「{w}」，可能是过程文档"));
    }
    // 生成物：几乎全是代码 / 数据
    let fenced = {
        let mut inside = false;
        let mut k = 0;
        for l in &lines {
            if l.trim_start().starts_with("```") {
                inside = !inside;
                continue;
            }
            if inside {
                k += 1;
            }
        }
        k
    };
    let symbols = head.chars().filter(|c| matches!(c, '{' | '}' | ';' | '<' | '>' | '=' | '(' | ')')).count();
    if (fenced * 10 >= n * 8 && n > 15) || (body_chars > 400 && symbols * 6 > body_chars) {
        return j("generated", 0.65, "几乎全是代码或数据");
    }
    if p.ends_with(".lock") || fname.contains(".min.") || fname.starts_with("export-") || fname.starts_with("index.html") {
        return j("generated", 0.7, "工具生成的文件");
    }
    // 剩下的：按长短与成文程度分
    let heads = lines.iter().filter(|l| l.starts_with('#')).count();
    let paras: Vec<usize> = head.split("\n\n").map(|b| b.chars().count()).filter(|c| *c > 40).collect();
    let prose = !paras.is_empty() && paras.iter().sum::<usize>() / paras.len() > 90;
    if body_chars >= 900 && prose {
        return j("article", 0.66, "成篇的长文");
    }
    if body_chars < 280 {
        return j("note", 0.62, "短的笔记 / 随手记");
    }
    if heads >= 2 || body_chars >= 600 {
        return j("doc", 0.6, "成文的文档");
    }
    j("note", 0.55, "一般的笔记")
}

/// 判断用的路径：库外条目 = 来源文件夹名 + 条目在来源里的相对路径（比如 `ops/2026-10 交接.md`），
/// 不带家目录、/tmp 这些与内容无关的前缀；其他绝对路径去掉家目录，不在家目录下的只看最后三段。
pub fn judge_path(abs: &str, ext_key: Option<(&str, Option<&str>)>) -> String {
    if let Some((key, base)) = ext_key {
        let rel = key.splitn(2, '/').nth(1).unwrap_or(key);
        let base_name = base.and_then(|b| b.trim_end_matches('/').rsplit('/').next()).unwrap_or("");
        return if base_name.is_empty() || base_name.starts_with('~') { rel.to_string() } else { format!("{base_name}/{rel}") };
    }
    if let Some(h) = std::env::var_os("HOME").map(|h| h.to_string_lossy().to_string()).filter(|h| h.len() > 1) {
        if let Some(r) = abs.strip_prefix(&h) {
            return r.trim_start_matches('/').to_string();
        }
    }
    if abs.starts_with('/') {
        let segs: Vec<&str> = abs.split('/').filter(|x| !x.is_empty()).collect();
        return segs[segs.len().saturating_sub(3)..].join("/");
    }
    abs.to_string()
}

/// 给人看的位置：家目录写成 ~。
pub fn pretty_loc(loc: &str) -> String {
    match std::env::var_os("HOME").map(|h| h.to_string_lossy().to_string()).filter(|h| h.len() > 1) {
        Some(h) if loc.starts_with(&h) => format!("~{}", &loc[h.len()..]),
        _ => loc.to_string(),
    }
}

/// 用户的「总要留 / 总要剔」词，优先于一切。
fn by_words(cfg: &Config, path: &str, title: &str) -> Option<(bool, String)> {
    let hay = format!("{} {}", path.to_lowercase(), title.to_lowercase());
    if let Some(w) = cfg.keep_words.iter().find(|w| !w.trim().is_empty() && hay.contains(&w.trim().to_lowercase())) {
        return Some((true, format!("含「{w}」，你设了总要保留")));
    }
    cfg.drop_words.iter().find(|w| !w.trim().is_empty() && hay.contains(&w.trim().to_lowercase())).map(|w| (false, format!("含「{w}」，你设了总要剔除")))
}

// ───────── 存储 ─────────

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Row {
    /// 库内路径（Notes/…）或库外 key（ext:<源>/<条目>）
    pub key: String,
    pub title: String,
    /// 给人看的位置：库内路径或原处路径
    pub loc: String,
    pub src: String,
    pub cat: String,
    pub keep: bool,
    pub conf: f32,
    pub why: String,
    /// rule | ai | words | you
    pub by: String,
    #[serde(default)]
    pub chars: usize,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Batch {
    pub id: String,
    pub at: u64,
    #[serde(default)]
    pub vault: Vec<String>,
    #[serde(default)]
    pub ext: Vec<String>,
    /// 库内每篇进回收站时的名字：撤销按它找回这一批的那一份（同一路径删过几次时不会拿错）
    #[serde(default)]
    pub trash: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Store {
    #[serde(default)]
    pub rows: Vec<Row>,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub scope: String,
    /// 剔掉的库外条目：key → 当时的索引条目（放回时原样还回去）
    #[serde(default)]
    pub excluded: HashMap<String, Value>,
    /// 新扫描到、像过程文档、等确认的库外条目
    #[serde(default)]
    pub held: HashMap<String, Row>,
    #[serde(default)]
    pub batches: Vec<Batch>,
    /// 你亲手改判过的：key → 保留?（重新筛也照你说的）
    #[serde(default)]
    pub overrides: HashMap<String, bool>,
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".litmus/local").join("curate.json")
}

pub fn load_store(root: &Path) -> Store {
    std::fs::read_to_string(store_path(root)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_store(root: &Path, s: &Store) {
    let _ = std::fs::create_dir_all(root.join(".litmus/local"));
    let p = store_path(root);
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn lock() -> &'static Mutex<()> {
    static L: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// 不上场的库外条目：剔掉的 + 等确认的。知识场与搜索据此过滤。
pub fn hidden_keys(root: &Path) -> HashSet<String> {
    let s = load_store(root);
    s.excluded.keys().chain(s.held.keys()).cloned().collect()
}

/// 剔掉的库外条目（重新扫描时不再收回来）。
pub fn excluded_keys(root: &Path) -> HashSet<String> {
    load_store(root).excluded.into_keys().collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 数据源扫到新东西时调用：按规则判，像要剔的（把握 ≥ 0.7）先扣下来等确认。返回扣下的条数。
pub fn hold_new(root: &Path, items: &[super::sources::Item], src_name: &str, base: Option<&str>) -> usize {
    let cfg = load_config(root);
    if !cfg.auto || items.is_empty() {
        return 0;
    }
    let _g = lock().lock().unwrap();
    let mut s = load_store(root);
    let mut n = 0;
    for it in items {
        if s.held.contains_key(&it.key) || s.excluded.contains_key(&it.key) || s.overrides.get(&it.key) == Some(&true) {
            continue;
        }
        let text = super::sources::read_text(root, &it.hash);
        let jp = judge_path(&it.loc, Some((&it.key, base)));
        let (keep, cat, conf, why, by) = match by_words(&cfg, &it.loc, &it.title) {
            Some((k, w)) => (k, "note", 1.0, w, "words"),
            None => {
                let jd = classify(&jp, &it.title, &text, &it.kind);
                (cfg.keeps(jd.cat), jd.cat, jd.conf, jd.why, "rule")
            }
        };
        if !keep && conf >= 0.7 {
            s.held.insert(it.key.clone(), Row { key: it.key.clone(), title: it.title.clone(), loc: pretty_loc(&it.loc), src: src_name.to_string(), cat: cat.into(), keep: false, conf, why, by: by.into(), chars: text.chars().count() });
            n += 1;
        }
    }
    if n > 0 {
        save_store(root, &s);
    }
    n
}

// ───────── 后台筛选 ─────────

#[derive(Serialize, Clone, Default)]
pub struct Job {
    pub running: bool,
    pub phase: String,
    pub done: usize,
    pub total: usize,
    pub ai_calls: usize,
    pub error: Option<String>,
    pub started: u64,
    pub finished: u64,
    #[serde(skip)]
    pub stop: bool,
}

fn jobs() -> &'static Mutex<HashMap<PathBuf, Job>> {
    static J: std::sync::OnceLock<Mutex<HashMap<PathBuf, Job>>> = std::sync::OnceLock::new();
    J.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn job_snapshot(root: &Path) -> Option<Job> {
    jobs().lock().unwrap().get(root).cloned()
}

pub fn job_stop(root: &Path) -> bool {
    match jobs().lock().unwrap().get_mut(root) {
        Some(j) if j.running => {
            j.stop = true;
            true
        }
        _ => false,
    }
}

fn set_job(root: &Path, f: impl FnOnce(&mut Job)) {
    if let Some(j) = jobs().lock().unwrap().get_mut(root) {
        f(j);
    }
}

/// 库内文档里「导入自 `原处`」那一行：判断时把原处路径也算进去。
fn origin_of(text: &str) -> Option<String> {
    let l = text.lines().take(8).find(|l| l.starts_with("> 导入自 "))?;
    let a = l.find('`')?;
    let b = l[a + 1..].find('`')?;
    Some(l[a + 1..a + 1 + b].to_string())
}

const AI_BATCH: usize = 20;

fn ai_system(cfg: &Config) -> String {
    let cats: Vec<String> = CATS.iter().map(|c| format!("- {}（{}）：{}——{}", c.0, c.1, c.3, if cfg.keeps(c.0) { "保留" } else { "剔除" })).collect();
    format!(
        "你在帮用户清理知识库：留下有用的知识（文档、报告、方案、文章、教程、课程、AI 的 harness 与 skills、自己的笔记），\
剔除和 AI 一起干活时产生的过程文档与会话记录。下面是类别和用户对每类的决定：\n{}\n\
对每一篇，根据路径、标题和开头判断它属于哪一类。拿不准时偏向保留。why 用一句中文（不超过 20 字）说依据。\
只输出 JSON：{{\"items\":[{{\"i\":1,\"cat\":\"report\",\"why\":\"…\"}}]}}",
        cats.join("\n")
    )
}

/// POST /field/curate {scope:"all"|"ext"|"vault", source?, ai?:bool}：后台筛一遍，出清单。
pub async fn start(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let scope = req.get("scope").and_then(|v| v.as_str()).unwrap_or("all").to_string();
    let source = req.get("source").and_then(|v| v.as_str()).map(String::from);
    let want_ai = req.get("ai").and_then(|v| v.as_bool()).unwrap_or(true);
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone().filter(|c| !c.base_url.is_empty() && want_ai);
    if let Some(cfg) = &ai {
        if !super::jobs::consented(&root, cfg) {
            let mut v = json!({ "needs_consent": true, "error": "拿不准的要交给模型复核：开始之前先看一下隐私告知并同意" });
            v["host"] = json!(cfg.base_url.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or(""));
            v["model"] = json!(cfg.model);
            return (StatusCode::PRECONDITION_REQUIRED, Json(v)).into_response();
        }
    }
    if jobs().lock().unwrap().get(&root).map(|j| j.running).unwrap_or(false) {
        return err(StatusCode::CONFLICT, "已经在筛了").into_response();
    }
    // 先把要判的读出来，再标「在筛」：读的时候客户端断开，handler 被丢掉，
    // 以前「在筛」就永远不会清掉，之后每次都是 409
    let c = match current(&st, false).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    {
        let mut js = jobs().lock().unwrap();
        if js.get(&root).map(|j| j.running).unwrap_or(false) {
            return err(StatusCode::CONFLICT, "已经在筛了").into_response();
        }
        js.insert(root.clone(), Job { running: true, phase: "按规则分".into(), started: now_secs(), ..Default::default() });
    }
    // 要判的：库里的 Notes/*.md 与库外条目（按范围）
    let ext_ix: HashMap<String, (super::sources::Item, String)> = {
        let bases: HashMap<String, Option<String>> = super::sources::load_sources(&root).into_iter().map(|s| (s.id, s.path)).collect();
        super::sources::all_items(&root).into_iter().map(|it| {
            let b = bases.get(&it.src).cloned().flatten().unwrap_or_default();
            (it.key.clone(), (it, b))
        }).collect()
    };
    let targets: Vec<(String, String, String)> = c
        .meta
        .iter()
        .filter(|m| {
            let ext = m.path.starts_with("ext:");
            match scope.as_str() {
                "ext" => ext,
                "vault" => !ext,
                _ => true,
            }
        })
        .filter(|m| m.path.starts_with("ext:") || m.path.starts_with("Notes/"))
        .filter(|m| source.as_ref().map(|s| m.path.starts_with(&format!("ext:{s}/"))).unwrap_or(true))
        .map(|m| (m.path.clone(), m.title.clone(), m.src.clone().unwrap_or_default()))
        .collect();
    let total = targets.len();
    set_job(&root, |j| j.total = total);
    let st2 = st.clone();
    let root2 = root.clone();
    let scope2 = scope.clone();
    tokio::spawn(async move {
        let cfg = load_config(&root2);
        // 第一步：规则（低优先级线程，读文件不占前台）
        let (tx, rx) = tokio::sync::oneshot::channel();
        let r3 = root2.clone();
        let cfg3 = cfg.clone();
        super::sources::spawn_background("litmus-curate", move || {
            let mut rows = Vec::with_capacity(targets.len());
            for (i, (key, title, src)) in targets.into_iter().enumerate() {
                if jobs().lock().unwrap().get(&r3).map(|j| j.stop).unwrap_or(true) {
                    break;
                }
                let (text, loc, kind, jp) = if let Some((it, base)) = ext_ix.get(&key) {
                    let jp = judge_path(&it.loc, Some((&it.key, Some(base.as_str()))));
                    (super::sources::read_text(&r3, &it.hash), pretty_loc(&it.loc), it.kind.clone(), jp)
                } else {
                    let t = std::fs::read_to_string(r3.join(&key)).unwrap_or_default();
                    // 从数据源导入进库的：原处路径也算进去（会话、ops/ 里的过程文档多半是这么进来的）
                    let o = origin_of(&t);
                    let inner = key.strip_prefix("Notes/").unwrap_or(&key).to_string();
                    let jp = match &o { Some(o) => format!("{}/{inner}", judge_path(o, None)), None => inner };
                    let loc = o.map(|o| format!("{key}（导入自 {}）", pretty_loc(&o))).unwrap_or_else(|| key.clone());
                    (t, loc, String::new(), jp)
                };
                let (keep, cat, conf, why, by) = match by_words(&cfg3, &loc, &title) {
                    Some((k, w)) => {
                        let jd = classify(&jp, &title, &text, &kind);
                        (k, jd.cat, 1.0, w, "words")
                    }
                    None => {
                        let jd = classify(&jp, &title, &text, &kind);
                        (cfg3.keeps(jd.cat), jd.cat, jd.conf, jd.why, "rule")
                    }
                };
                let head: String = text.chars().take(400).collect();
                rows.push((Row { key, title, loc, src: if src.is_empty() { "库内".into() } else { src }, cat: cat.into(), keep, conf, why, by: by.into(), chars: text.chars().count() }, head));
                if i % 50 == 0 {
                    set_job(&r3, |j| j.done = i + 1);
                }
            }
            let _ = tx.send(rows);
        });
        let mut rows = match rx.await {
            Ok(r) => r,
            Err(_) => {
                // 分类线程出错：别拿一张空清单把上一张冲掉
                set_job(&root2, |j| {
                    j.running = false;
                    j.error = Some("按规则分的时候出错了，上一份清单还在".into());
                    j.finished = now_secs();
                });
                return;
            }
        };
        set_job(&root2, |j| j.done = rows.len());
        // 第二步：拿不准的交给模型
        if let Some(acfg) = ai {
            let unsure: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].0.by == "rule" && rows[i].0.conf < 0.6).collect();
            let calls = unsure.len().div_ceil(AI_BATCH);
            set_job(&root2, |j| {
                j.phase = format!("模型复核拿不准的 {} 篇", unsure.len());
                j.done = 0;
                j.total = calls;
            });
            let system = ai_system(&cfg);
            for chunk in unsure.chunks(AI_BATCH) {
                if jobs().lock().unwrap().get(&root2).map(|j| j.stop).unwrap_or(true) {
                    break;
                }
                let user: Vec<String> = chunk.iter().enumerate().map(|(k, &i)| {
                    let (r, head) = &rows[i];
                    format!("{}. 位置：{}\n标题：{}\n开头：{}", k + 1, r.loc, r.title, head.replace('\n', " "))
                }).collect();
                let out = litmus_mcp::agent::complete(&acfg, &system, &user.join("\n\n")).await;
                match out.ok().and_then(|t| super::field_tools::json_in(&t)) {
                    Some(v) => {
                        for it in v.get("items").and_then(|x| x.as_array()).into_iter().flatten() {
                            let Some(k) = it.get("i").and_then(|x| x.as_u64()).map(|x| x as usize) else { continue };
                            let Some(&i) = chunk.get(k.wrapping_sub(1)) else { continue };
                            let Some(cat) = it.get("cat").and_then(|x| x.as_str()).and_then(|c| CATS.iter().find(|x| x.0 == c)).map(|x| x.0) else { continue };
                            let r = &mut rows[i].0;
                            r.cat = cat.into();
                            r.keep = cfg.keeps(cat);
                            r.conf = 0.8;
                            r.by = "ai".into();
                            r.why = it.get("why").and_then(|x| x.as_str()).unwrap_or("").chars().take(40).collect();
                        }
                    }
                    None => set_job(&root2, |j| j.error = Some("有一批模型没给出结果，这批按规则判".into())),
                }
                set_job(&root2, |j| {
                    j.done += 1;
                    j.ai_calls += 1;
                });
            }
        }
        // 你亲手改判过的照你说的
        let overrides = load_store(&root2).overrides;
        for (r, _) in rows.iter_mut() {
            if let Some(&k) = overrides.get(&r.key) {
                r.keep = k;
                r.by = "you".into();
                r.conf = 1.0;
            }
        }
        // 规则拿不准、模型也没复核的：偏向保留
        for (r, _) in rows.iter_mut().filter(|(r, _)| r.by == "rule" && r.conf < 0.6 && !r.keep) {
            r.keep = true;
            r.why = format!("拿不准，先留着：{}", r.why);
        }
        let stopped = jobs().lock().unwrap().get(&root2).map(|j| j.stop).unwrap_or(false);
        {
            let _g = lock().lock().unwrap();
            let mut s = load_store(&root2);
            let fresh: Vec<Row> = rows.into_iter().map(|x| x.0).collect();
            if stopped && !s.rows.is_empty() {
                // 半路停下：判过的换成新的，没轮到的保留上一份，不把整张清单缩成一半
                let keys: std::collections::HashSet<String> = fresh.iter().map(|r| r.key.clone()).collect();
                s.rows.retain(|r| !keys.contains(&r.key));
                s.rows.extend(fresh);
            } else {
                s.rows = fresh;
            }
            s.created = now_secs();
            s.scope = scope2;
            save_store(&root2, &s);
        }
        let _ = st2;
        set_job(&root2, |j| {
            j.running = false;
            j.phase = if stopped { "停下了".into() } else { "清单出来了".into() };
            j.finished = now_secs();
        });
    });
    Json(json!({ "started": true, "total": total })).into_response()
}

/// GET /field/curate：清单（按保留 / 剔除、类别汇总）+ 等确认的 + 进度 + 设置。
pub async fn get(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let s = load_store(&root);
    // 已经执行过的不再列（库内的进了回收站、库外的被剔掉）
    let gone: HashSet<&String> = s.excluded.keys().collect();
    let rows: Vec<&Row> = s.rows.iter().filter(|r| !gone.contains(&r.key) && (r.key.starts_with("ext:") || root.join(&r.key).exists())).collect();
    let mut by_cat: HashMap<&str, (usize, usize)> = HashMap::new();
    for r in &rows {
        let e = by_cat.entry(r.cat.as_str()).or_insert((0, 0));
        if r.keep {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }
    let cats: Vec<Value> = CATS.iter().map(|c| json!({ "id": c.0, "name": c.1, "about": c.3, "keep": by_cat.get(c.0).map(|x| x.0).unwrap_or(0), "drop": by_cat.get(c.0).map(|x| x.1).unwrap_or(0) })).collect();
    let mut held: Vec<&Row> = s.held.values().collect();
    held.sort_by(|a, b| a.title.cmp(&b.title));
    Json(json!({
        "rows": rows,
        "created": s.created,
        "scope": s.scope,
        "cats": cats,
        "held": held,
        "excluded": s.excluded.len(),
        "batches": s.batches.iter().rev().take(10).collect::<Vec<_>>(),
        "config": load_config(&root),
        "job": job_snapshot(&root),
    }))
    .into_response()
}

/// GET /field/curate/excluded：库外被剔掉、不再上场的（回收站里一起列出来，能放回）。
pub async fn excluded(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let s = load_store(&root);
    let names: HashMap<String, String> = super::sources::load_sources(&root).into_iter().map(|x| (x.id, x.name)).collect();
    let mut items: Vec<Value> = s
        .excluded
        .iter()
        .map(|(k, v)| {
            let src = v["src"].as_str().and_then(|id| names.get(id).cloned()).unwrap_or_default();
            let at = s.batches.iter().rev().find(|b| b.ext.contains(k)).map(|b| b.at).unwrap_or(0);
            json!({ "key": k, "title": v["title"].as_str().unwrap_or(k), "src": src, "loc": pretty_loc(v["loc"].as_str().unwrap_or("")), "at": at })
        })
        .collect();
    items.sort_by(|a, b| b["at"].as_u64().cmp(&a["at"].as_u64()));
    Json(json!({ "items": items })).into_response()
}

/// POST /field/curate/config {keep, drop, keep_words, drop_words, auto}
pub async fn set_config(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let Ok(mut cfg) = serde_json::from_slice::<Config>(&body) else { return err(StatusCode::BAD_REQUEST, "设置格式不对").into_response() };
    let valid = |v: &mut Vec<String>| v.retain(|c| CATS.iter().any(|x| x.0 == c));
    valid(&mut cfg.keep);
    valid(&mut cfg.drop);
    cfg.keep_words.retain(|w| !w.trim().is_empty());
    cfg.drop_words.retain(|w| !w.trim().is_empty());
    if let Err(e) = write_synced(&root, "curate.json", &serde_json::to_vec_pretty(&cfg).unwrap_or_default()) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    // 清单按新设置重算保留 / 剔除（类别不变；你亲手改判过的不动）
    let _g = lock().lock().unwrap();
    let mut s = load_store(&root);
    for r in s.rows.iter_mut().filter(|r| r.by != "you") {
        match by_words(&cfg, &r.loc, &r.title) {
            Some((k, w)) => {
                r.keep = k;
                r.why = w;
                r.by = "words".into();
                r.conf = 1.0;
            }
            None => {
                if r.by == "words" {
                    r.by = "rule".into();
                }
                r.keep = cfg.keeps(&r.cat);
            }
        }
    }
    save_store(&root, &s);
    Json(json!({ "config": cfg })).into_response()
}

/// POST /field/curate/apply {drop:[key], keep:[key]}：剔除（库内进回收站、库外从索引拿掉）与改判保留。返回这一批的 id。
pub async fn apply(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let list = |k: &str| -> Vec<String> { req.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
    let (drop, keep) = (list("drop"), list("keep"));
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let mut batch = Batch { id: format!("{:x}", now_secs()), at: now_secs(), ..Default::default() };
    let mut failed = Vec::new();
    // 只收 Notes/ 下的 Markdown 文档；整批挪完再统一同步一次
    for key in drop.iter().filter(|k| !k.starts_with("ext:")) {
        if !key.ends_with(".md") {
            failed.push(format!("{key}：只能剔除 Notes/ 下的 Markdown 文档"));
            continue;
        }
        let r = {
            let mut k = st.kernel.lock().unwrap();
            super::trash_move(&mut k, key, false)
        };
        match r {
            Ok(t) => {
                batch.vault.push(key.clone());
                batch.trash.insert(key.clone(), t);
            }
            Err(e) => failed.push(format!("{key}：{e}")),
        }
    }
    if !batch.vault.is_empty() {
        let _ = st.kernel.lock().unwrap().sync_all();
    }
    let ext_drop: Vec<&String> = drop.iter().filter(|k| k.starts_with("ext:")).collect();
    {
        let _g = lock().lock().unwrap();
        let mut s = load_store(&root);
        if !ext_drop.is_empty() {
            let removed = super::sources::take_items(&root, &ext_drop.iter().map(|k| k.as_str()).collect::<Vec<_>>());
            for (key, item) in removed {
                s.held.remove(&key);
                s.excluded.insert(key.clone(), item);
                batch.ext.push(key);
            }
            // 等确认里的、还没扫进索引的也算剔掉
            for k in &ext_drop {
                if let Some(r) = s.held.remove(*k) {
                    s.excluded.insert((*k).clone(), json!({ "key": r.key, "title": r.title }));
                    if !batch.ext.contains(k) {
                        batch.ext.push((*k).clone());
                    }
                }
            }
        }
        for key in &keep {
            s.held.remove(key);
            s.overrides.insert(key.clone(), true);
            if let Some(r) = s.rows.iter_mut().find(|r| &r.key == key) {
                r.keep = true;
                r.by = "you".into();
            }
        }
        for key in &drop {
            s.overrides.remove(key);
            if let Some(r) = s.rows.iter_mut().find(|r| &r.key == key) {
                r.keep = false;
            }
        }
        if !batch.vault.is_empty() || !batch.ext.is_empty() {
            s.batches.push(batch.clone());
            if s.batches.len() > 30 {
                s.batches.remove(0);
            }
        }
        save_store(&root, &s);
    }
    super::sources::REV.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    audit_log(&root.join(".litmus/local"), "curate-apply", &json!({ "batch": batch.id, "vault": batch.vault.len(), "ext": batch.ext.len(), "kept": keep.len() }));
    Json(json!({ "batch": batch.id, "trashed": batch.vault.len(), "hidden": batch.ext.len(), "kept": keep.len(), "failed": failed })).into_response()
}

/// POST /field/curate/undo {batch?, keys?}：整批撤销，或放回指定的几篇。
pub async fn undo(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root = st.kernel.lock().unwrap().vault.root.clone();
    // 整个撤销都拿着清单的锁：以前读的时候没锁、写的时候才锁，会把同时扣下、剔除的改动冲掉
    let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut s = load_store(&root);
    let mut batch: Option<Batch> = None;
    let (vault, ext): (Vec<String>, Vec<String>) = if let Some(id) = req.get("batch").and_then(|v| v.as_str()) {
        let Some(i) = s.batches.iter().position(|b| b.id == id) else { return err(StatusCode::NOT_FOUND, "这一批找不到了").into_response() };
        let b = s.batches.remove(i);
        let r = (b.vault.clone(), b.ext.clone());
        batch = Some(b);
        r
    } else {
        let keys: Vec<String> = req.get("keys").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        keys.into_iter().partition(|k| !k.starts_with("ext:"))
    };
    let mut back = 0;
    let mut failed = Vec::new();
    let mut not_back: Vec<String> = Vec::new();
    for p in &vault {
        let t = batch.as_ref().and_then(|b| b.trash.get(p)).cloned();
        let r = {
            let mut k = st.kernel.lock().unwrap();
            super::trash_restore_path(&mut k, p, t.as_deref(), false)
        };
        match r {
            Ok(()) => back += 1,
            Err(e) => {
                failed.push(format!("{p}：{e}"));
                not_back.push(p.clone());
            }
        }
    }
    if back > 0 {
        let _ = st.kernel.lock().unwrap().sync_all();
    }
    // 有没找回来的：这一批留着（只剩没回来的），可以再试
    if let Some(mut b) = batch {
        if !not_back.is_empty() {
            b.vault = not_back.clone();
            b.ext.clear();
            b.trash.retain(|k, _| not_back.contains(k));
            s.batches.push(b);
        }
    }
    let mut items = Vec::new();
    for k in &ext {
        if let Some(v) = s.excluded.remove(k) {
            if v.get("hash").is_some() {
                items.push(v);
            }
            back += 1;
        }
    }
    super::sources::put_items(&root, items);
    // 放回来的以后也别再被自动扣下、重新筛也留着
    for k in vault.iter().chain(ext.iter()) {
        s.overrides.insert(k.clone(), true);
        if let Some(r) = s.rows.iter_mut().find(|r| &r.key == k) {
            r.keep = true;
            r.by = "you".into();
        }
    }
    save_store(&root, &s);
    super::sources::REV.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Json(json!({ "restored": back, "failed": failed })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_harness_and_skills() {
        assert_eq!(classify("/u/.claude/projects/a.jsonl", "[x] 修一下", "", "agent").cat, "session");
        assert_eq!(classify("Notes/Sources/会话/修登录.md", "修登录", "# 修登录\n\n## 结论\n\n好了\n\n## 过程\n\n**问**：修一下\n\n**答**：好", "").cat, "session");
        assert_eq!(classify("/u/proj/CLAUDE.md", "CLAUDE", "# 项目", "md").cat, "harness");
        assert_eq!(classify("/u/proj/.claude/agents/reviewer.md", "reviewer", "---\nname: reviewer\n---", "md").cat, "harness");
        assert_eq!(classify("/u/skills/pdf/SKILL.md", "pdf", "---\nname: pdf\ndescription: x\n---", "md").cat, "skill");
    }

    #[test]
    fn process_docs_by_path_and_content() {
        let r = classify("/u/proj/ops/2026-10-27-handoff.md", "交接", "# 交接", "md");
        assert_eq!((r.cat, r.conf >= 0.8), ("process", true), "{r:?}");
        assert_eq!(classify("/u/proj/_cowork/field/notes.md", "notes", "x", "md").cat, "process");
        let todo = (0..8).map(|i| format!("- [ ] 第{i}件事")).collect::<Vec<_>>().join("\n");
        assert_eq!(classify("/u/proj/docs/next.md", "next", &todo, "md").cat, "process");
        let log = (0..6).map(|i| format!("- 2026-10-2{i} 14:3{i} 改了一点")).collect::<Vec<_>>().join("\n");
        assert_eq!(classify("/u/proj/docs/x.md", "x", &log, "md").cat, "process");
        let recap = "# 本次改动\n\n## Summary\n\n- 改了 A\n\n## Next steps\n\n- 做 B";
        assert_eq!(classify("/u/proj/docs/y.md", "y", recap, "md").cat, "process");
        // 过程目录里、但标题像报告的：拿不准，交给模型
        let r = classify("/u/proj/ops/q3-数据报告.md", "Q3 数据报告", "# Q3", "md");
        assert!(r.cat == "report" && r.conf < 0.6, "{r:?}");
        // 「status」只在整段命中时算，不误伤 statusbar
        assert_ne!(classify("/u/proj/docs/statusbar-design.md", "状态栏设计", "# 状态栏设计", "md").cat, "process");
    }

    #[test]
    fn useful_kinds_are_kept_by_default() {
        let cfg = Config::default();
        for (path, title, want) in [
            ("/u/learn/rust/第一课 所有权.md", "第一课 所有权", "course"),
            ("/u/docs/部署指南.md", "部署指南", "tutorial"),
            ("/u/docs/Q3 增长分析报告.md", "Q3 增长分析报告", "report"),
            ("/u/docs/同步方案.md", "同步方案", "plan"),
        ] {
            let r = classify(path, title, "# x\n\n正文", "md");
            assert_eq!(r.cat, want, "{path}");
            assert!(cfg.keeps(r.cat));
        }
        let long = format!("# 为什么坚持文件即真相\n\n{}\n\n{}", "我们一直认为本地优先的软件应该把文件当作唯一的真相，这样用户永远不会被锁在某个格式里，迁移也很容易。".repeat(12), "第二段同样是成段的论述，讲清楚取舍和代价，以及为什么这么做在长期看是值得的。".repeat(12));
        assert_eq!(classify("/u/blog/why.md", "为什么坚持文件即真相", &long, "md").cat, "article");
        assert!(!cfg.keeps("session") && !cfg.keeps("process") && !cfg.keeps("generated"));
    }

    #[test]
    fn user_words_win() {
        let cfg = Config { keep_words: vec!["ops/playbook".into()], drop_words: vec!["草稿".into()], ..Default::default() };
        assert_eq!(by_words(&cfg, "/u/proj/ops/playbook/上线.md", "上线").map(|x| x.0), Some(true));
        assert_eq!(by_words(&cfg, "/u/a.md", "方案草稿").map(|x| x.0), Some(false));
        assert!(by_words(&cfg, "/u/a.md", "方案").is_none());
    }

    #[test]
    fn judge_path_drops_irrelevant_prefixes() {
        assert_eq!(judge_path("/tmp/.tmpX/a.md", Some(("ext:dl/a.md", Some("/tmp/.tmpX")))), ".tmpX/a.md");
        assert_eq!(judge_path("/Users/s/proj/ops/b.md", Some(("ext:ops/2026/b.md", Some("~/proj/ops")))), "ops/2026/b.md");
        assert_eq!(judge_path("/var/x/y/z/w.md", None), "y/z/w.md");
    }

    #[test]
    fn origin_line_is_read() {
        assert_eq!(origin_of("# t\n\n> 导入自 `/u/proj/ops/a.md`（2026-10-01）\n\n正文").as_deref(), Some("/u/proj/ops/a.md"));
        assert_eq!(origin_of("# t\n\n正文"), None);
    }
}
