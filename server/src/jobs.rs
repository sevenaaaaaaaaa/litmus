//! 上手与后台（H，0.16）：
//!
//! - **引导**：第一次用边用边引导（场上一张小卡片，几步真实操作）；每个 `X.Y.0` 正式版第一次打开时一张「这一版的新东西」，
//!   内容直接取自内置的 CHANGELOG 那一节。进度随库同步（`.litmus/onboarding.json`），换设备不会再从头来。
//! - **隐私告知**：任何要把内容发给模型的后台活（AI 学习、AI 巡检）开始前，先讲清发什么、发给谁、存在哪，
//!   用户点同意才开始。同意按「接口地址的主机」记在这台设备（`.litmus/local/consent.json`），换了服务商重新问。
//! - **后台任务**：扫描数据源、AI 学习、AI 巡检汇总成一个 `GET /jobs`，壳里一个小进度条随时看、能暂停 / 停下。
//! - **AI 学习**：把库外的条目一篇篇交给模型，学成「要点 + 出处」的学习笔记；先放在这台设备的待确认区
//!   （`.litmus/local/digest.json`），用户点「收下」才写进 `Notes/学习/`，并排进复习。原件一律不动。

use super::field::{read_synced, write_synced};
use super::{audit_log, check_token, err, AppState};
use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const CHANGELOG: &str = include_str!("../../CHANGELOG.md");
/// 老用户的门槛：库里已有这么多篇，就不再从「记下第一条」教起，只给这一版的新东西。
const EXISTING_USER_DOCS: usize = 20;
/// 每篇交给模型的字数上限（隐私告知里如实写出来）。
pub const DIGEST_CHARS: usize = 6000;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn root_of(st: &AppState) -> PathBuf {
    st.kernel.lock().unwrap().vault.root.clone()
}
fn ai_cfg(st: &AppState) -> Option<kernel_core::AiConfig> {
    st.kernel.lock().unwrap().vault.config.ai.clone().filter(|c| !c.base_url.is_empty())
}

// ═══════════ 引导 ═══════════

/// 当前版本的「X.Y」；预发布（Nightly）与 X.Y.Z 修复版都不算大版本，返回 None。
pub fn major_minor(v: &str) -> Option<String> {
    if v.contains('-') {
        return None;
    }
    let mut it = v.split('.');
    let (a, b, c) = (it.next()?, it.next()?, it.next()?);
    (c == "0").then(|| format!("{a}.{b}"))
}

fn strip_md(s: &str) -> String {
    let mut t = s.trim().trim_start_matches("- ").replace("**", "").replace('`', "");
    // [文字](链接) → 文字
    while let (Some(a), Some(b)) = (t.find("]("), t.find('[')) {
        if b >= a {
            break;
        }
        let Some(close) = t[a..].find(')') else { break };
        t = format!("{}{}{}", &t[..b], &t[b + 1..a], &t[a + close + 1..]);
    }
    t
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    // 截在句读上更好读
    if let Some(i) = out.rfind(['。', '；', '，']) {
        if i > out.len() / 2 {
            out.truncate(i);
        }
    }
    out + "…"
}

/// 一段文字里出现的 `/指令`（前面不是字母、数字或路径分隔）。
fn commands_in(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let cs: Vec<char> = s.chars().collect();
    for i in 0..cs.len() {
        if cs[i] != '/' || (i > 0 && (cs[i - 1].is_alphanumeric() || matches!(cs[i - 1], '/' | '.' | ':' | '~' | '_' | '-'))) {
            continue;
        }
        let w: String = cs[i + 1..].iter().take_while(|c| c.is_alphanumeric() && !c.is_ascii_digit()).take(8).collect();
        if !w.is_empty() && !cs.get(i + 1 + w.chars().count()).map(|c| *c == '/').unwrap_or(false) {
            let c = format!("/{w}");
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// 从 CHANGELOG 取「X.Y.0」那一节：主题、导语、每个小节一条（标题 + 第一句 + 里面提到的指令）。
pub fn whats_new(changelog: &str, mm: &str) -> Option<Value> {
    let head = format!("## {mm}.0 ");
    let start = changelog.find(&head)?;
    let rest = &changelog[start + head.len()..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    let sec = &rest[..end];
    let mut lines = sec.lines();
    let first = lines.next().unwrap_or("");
    let theme = first.rsplit_once('·').map(|(_, t)| t.trim().to_string()).unwrap_or_default();
    let body: Vec<&str> = lines.collect();
    let intro = body.iter().take_while(|l| !l.starts_with("### ")).map(|l| l.trim()).find(|l| !l.is_empty()).map(strip_md).unwrap_or_default();
    let mut items = Vec::new();
    let mut i = 0;
    while i < body.len() {
        if let Some(h) = body[i].strip_prefix("### ") {
            let mut j = i + 1;
            let mut text = String::new();
            while j < body.len() && !body[j].starts_with("### ") {
                text.push_str(body[j]);
                text.push('\n');
                j += 1;
            }
            let first_line = text.lines().map(str::trim).find(|l| !l.is_empty()).map(strip_md).unwrap_or_default();
            items.push(json!({ "title": h.trim(), "body": cut(&first_line, 52), "cmds": commands_in(&text) }));
            i = j;
        } else {
            i += 1;
        }
    }
    if items.is_empty() && intro.is_empty() {
        return None;
    }
    Some(json!({ "version": format!("{mm}.0"), "theme": theme, "intro": cut(&intro, 80), "items": items.into_iter().take(6).collect::<Vec<_>>() }))
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Onboarding {
    /// 做过（或跳过）的步骤：note / ask / source / digest / progress → 时间
    #[serde(default)]
    pub steps: HashMap<String, u64>,
    #[serde(default)]
    pub dismissed: bool,
    /// 看过哪一版的「新东西」（X.Y）
    #[serde(default)]
    pub seen: String,
}

fn load_onboarding(root: &Path) -> Option<Onboarding> {
    read_synced(root, "onboarding.json", "onboarding.json").and_then(|s| serde_json::from_str(&s).ok())
}

/// 这次打开要给什么引导：first（第一次用，边用边引导）/ whatsnew（这一版的新东西）/ none。
pub fn onboarding_mode(state: Option<&Onboarding>, docs: usize, mm: Option<&str>, has_notes: bool) -> &'static str {
    let new_version = |s: &Onboarding| mm.map(|m| s.seen != m).unwrap_or(false) && has_notes;
    match state {
        None if docs < EXISTING_USER_DOCS => "first",
        None => {
            if new_version(&Onboarding::default()) {
                "whatsnew"
            } else {
                "none"
            }
        }
        Some(s) if !s.dismissed => "first",
        Some(s) if new_version(s) => "whatsnew",
        _ => "none",
    }
}

/// GET /field/onboarding：{mode, state, version, docs, whatsnew?}
pub async fn onboarding_get(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = root_of(&st);
    let docs = st.kernel.lock().unwrap().doc_stats().map(|m| m.len()).unwrap_or(0);
    let state = load_onboarding(&root);
    let version = env!("CARGO_PKG_VERSION");
    let mm = major_minor(version);
    let notes = mm.as_deref().and_then(|m| whats_new(CHANGELOG, m));
    let mode = onboarding_mode(state.as_ref(), docs, mm.as_deref(), notes.is_some());
    Json(json!({
        "mode": mode,
        "state": state.unwrap_or_default(),
        "version": version,
        "docs": docs,
        "existing": docs >= EXISTING_USER_DOCS,
        "whatsnew": notes,
    }))
    .into_response()
}

/// POST /field/onboarding {step?, skip?, dismiss?, seen?, reset?}：记下进度（随库同步）。
pub async fn onboarding_post(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root = root_of(&st);
    let mut s = if req.get("reset").and_then(|v| v.as_bool()) == Some(true) { Onboarding::default() } else { load_onboarding(&root).unwrap_or_default() };
    for k in ["step", "skip"] {
        if let Some(step) = req.get(k).and_then(|v| v.as_str()) {
            s.steps.entry(step.to_string()).or_insert_with(now_secs);
        }
    }
    if let Some(steps) = req.get("steps").and_then(|v| v.as_array()) {
        for x in steps.iter().filter_map(|x| x.as_str()) {
            s.steps.entry(x.to_string()).or_insert_with(now_secs);
        }
    }
    if req.get("dismiss").and_then(|v| v.as_bool()) == Some(true) {
        s.dismissed = true;
    }
    if req.get("seen").and_then(|v| v.as_bool()) == Some(true) {
        if let Some(mm) = major_minor(env!("CARGO_PKG_VERSION")) {
            s.seen = mm;
        }
    }
    if let Err(e) = write_synced(&root, "onboarding.json", &serde_json::to_vec_pretty(&s).unwrap_or_default()) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "state": s })).into_response()
}

// ═══════════ 隐私告知 ═══════════

fn host_of(base_url: &str) -> String {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map(|x| x.1).unwrap_or(host);
    host.to_ascii_lowercase()
}

/// 模型是不是就跑在这台电脑 / 局域网里（内容不出这台机器或家里的网）。
pub fn is_local_host(host: &str) -> bool {
    let h = host.split(':').next().unwrap_or(host).trim_matches(['[', ']']);
    h == "localhost" || h.starts_with("127.") || h == "::1" || h.ends_with(".local") || h.starts_with("192.168.") || h.starts_with("10.")
}

fn consent_path(root: &Path) -> PathBuf {
    root.join(".litmus/local").join("consent.json")
}

/// 这台设备对当前模型服务商点过同意没有（按主机记：换了服务商要重新问）。
pub fn consented(root: &Path, cfg: &kernel_core::AiConfig) -> bool {
    let host = host_of(&cfg.base_url);
    std::fs::read_to_string(consent_path(root)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.pointer("/ai/host").and_then(|h| h.as_str()).map(|h| h == host)).unwrap_or(false)
}

fn consent_info(root: &Path, cfg: Option<&kernel_core::AiConfig>) -> Value {
    match cfg {
        None => json!({ "configured": false, "agreed": false }),
        Some(c) => {
            let host = host_of(&c.base_url);
            let at = std::fs::read_to_string(consent_path(root)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.pointer("/ai/at").cloned());
            json!({ "configured": true, "host": host, "model": c.model, "local": is_local_host(&host), "agreed": consented(root, c), "at": at, "chars": DIGEST_CHARS })
        }
    }
}

/// 要同意才能开始时的回答：428 + 告知需要的信息。
fn needs_consent(root: &Path, cfg: &kernel_core::AiConfig) -> axum::response::Response {
    let mut v = consent_info(root, Some(cfg));
    v["needs_consent"] = json!(true);
    v["error"] = json!("开始之前要先看一下隐私告知并同意");
    (StatusCode::PRECONDITION_REQUIRED, Json(v)).into_response()
}

/// GET /field/consent：{configured, host, model, local, agreed, at}
pub async fn consent_get(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = root_of(&st);
    Json(consent_info(&root, ai_cfg(&st).as_ref())).into_response()
}

/// POST /field/consent {agree:true|false}：同意（记下当前服务商）或撤回。
pub async fn consent_post(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root = root_of(&st);
    let cfg = ai_cfg(&st);
    let agree = req.get("agree").and_then(|v| v.as_bool()).unwrap_or(false);
    let _ = std::fs::create_dir_all(root.join(".litmus/local"));
    let v = match (&cfg, agree) {
        (Some(c), true) => json!({ "ai": { "host": host_of(&c.base_url), "model": c.model, "at": now_secs() } }),
        (None, true) => return err(StatusCode::CONFLICT, "还没有配置模型：先在 /设置 里配上").into_response(),
        _ => json!({}),
    };
    if let Err(e) = std::fs::write(consent_path(&root), serde_json::to_vec_pretty(&v).unwrap_or_default()) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    audit_log(&root.join(".litmus/local"), "ai-consent", &json!({ "agree": agree, "host": cfg.as_ref().map(|c| host_of(&c.base_url)) }));
    Json(consent_info(&root, cfg.as_ref())).into_response()
}

// ═══════════ AI 学习：学成笔记，确认后入库 ═══════════

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Draft {
    pub id: String,
    /// 学的是哪一条（ext:<源>/<条目>）
    pub key: String,
    /// 原条目的标题、来源名、原处
    pub orig: String,
    pub src: String,
    pub loc: String,
    pub date: String,
    /// 学习笔记的标题与全文（Markdown）
    pub title: String,
    pub summary: String,
    pub md: String,
    pub created: u64,
    /// pending | accepted | rejected
    pub status: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct DigestStore {
    #[serde(default)]
    drafts: Vec<Draft>,
    /// 学过的条目 → 当时的正文 hash（原件改了才会再学）
    #[serde(default)]
    done: HashMap<String, String>,
}

fn digest_path(root: &Path) -> PathBuf {
    root.join(".litmus/local").join("digest.json")
}
fn load_store(root: &Path) -> DigestStore {
    std::fs::read_to_string(digest_path(root)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_store(root: &Path, s: &DigestStore) {
    let _ = std::fs::create_dir_all(root.join(".litmus/local"));
    let p = digest_path(root);
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

#[derive(Serialize, Clone, Default)]
pub struct DigestJob {
    pub running: bool,
    pub paused: bool,
    pub done: usize,
    pub total: usize,
    pub made: usize,
    pub failed: usize,
    pub current: String,
    pub started: u64,
    pub finished: u64,
    pub error: Option<String>,
    #[serde(skip)]
    pub stop: bool,
}

fn digests() -> &'static Mutex<HashMap<PathBuf, DigestJob>> {
    static D: std::sync::OnceLock<Mutex<HashMap<PathBuf, DigestJob>>> = std::sync::OnceLock::new();
    D.get_or_init(|| Mutex::new(HashMap::new()))
}

const DIGEST_SYSTEM: &str = "你在帮用户把一份库外的资料学成一篇学习笔记，存进他的知识库。用中文，只依据原文，不要编造原文里没有的事实。\
只输出 JSON：{\"title\":\"学习笔记的标题，不超过 24 字，说清讲的是什么\",\"summary\":\"两三句话：这份资料讲了什么、结论是什么\",\
\"points\":[\"3 到 6 条要点，每条一句，尽量具体（数字、名字、做法）\"],\"use\":\"一句话：对用户可能有什么用；看不出来就留空\",\"tags\":[\"1 到 3 个标签\"]}";

fn safe_name(t: &str) -> String {
    t.chars().filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '\n' | '\r' | '\t' | '[' | ']')).collect::<String>().trim().chars().take(60).collect()
}

fn yaml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 模型的回答 → 学习笔记（frontmatter 记下出处与学的方式）。抠不出 JSON 返回 None。
pub fn draft_md(answer: &str, orig: &str, src: &str, loc: &str, date: &str, model: &str, today: &str) -> Option<(String, String, String)> {
    let v = super::field_tools::json_in(answer)?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let summary = s("summary");
    let points: Vec<String> = v.get("points").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|p| p.as_str()).map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    if summary.is_empty() && points.is_empty() {
        return None;
    }
    let mut title = s("title");
    if title.is_empty() {
        title = orig.to_string();
    }
    let title: String = safe_name(&title).chars().take(40).collect();
    let mut tags = vec!["学习".to_string()];
    for t in v.get("tags").and_then(|x| x.as_array()).into_iter().flatten().filter_map(|t| t.as_str()) {
        let t = t.trim().trim_start_matches('#').to_string();
        if !t.is_empty() && !tags.contains(&t) && tags.len() < 4 {
            tags.push(t);
        }
    }
    let origin = if loc.starts_with("http") { format!("[{src}]({loc})") } else if loc.starts_with('/') { format!("`{loc}`") } else { src.to_string() };
    let mut md = format!(
        "---\ntags: [{}]\nsource: {}\norigin: {}\nlearned: {today}\nby: {}\n---\n# {title}\n\n> 由 AI 从「{orig}」（{src}{}）学成，你确认后收进了库。原件留在原处：{origin}\n\n{summary}\n",
        tags.join(", "),
        yaml_str(src),
        yaml_str(loc),
        yaml_str(&format!("AI 学习（{model}）")),
        if date.is_empty() { String::new() } else { format!(" · {date}") },
    );
    if !points.is_empty() {
        md.push_str("\n## 要点\n\n");
        for p in &points {
            md.push_str(&format!("- {p}\n"));
        }
    }
    let use_ = s("use");
    if !use_.is_empty() {
        md.push_str(&format!("\n## 可以怎么用\n\n{use_}\n"));
    }
    Some((title, summary, md))
}

fn today() -> String {
    super::field::ymd(now_secs() as i64)
}

/// 挑要学的条目：指定的 key，或某个来源（不指定就是所有启用的来源）里还没学过的，最近的在前。
fn pick(root: &Path, keys: &[String], source: Option<&str>, limit: usize) -> Vec<(super::sources::Item, String, String)> {
    let store = load_store(root);
    let mut all = super::sources::corpus(root);
    all.sort_by(|a, b| b.0.mtime.cmp(&a.0.mtime));
    all.into_iter()
        .filter(|(it, _, text)| {
            if !keys.is_empty() {
                return keys.contains(&it.key);
            }
            source.map(|s| it.src == s).unwrap_or(true) && store.done.get(&it.key) != Some(&it.hash) && text.chars().count() >= 80
        })
        .take(limit)
        .collect()
}

/// POST /field/digest {keys?, source?, limit?, dry?}
/// dry：只估算 {count, chars, host, model, local, agreed}；否则在后台一篇篇学，学成的放进待确认区。
pub async fn digest_start(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let keys: Vec<String> = req.get("keys").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let source = req.get("source").and_then(|v| v.as_str()).map(String::from);
    let limit = req.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(10).clamp(1, 500);
    let root = root_of(&st);
    let items = {
        let r = root.clone();
        let (k, s) = (keys.clone(), source.clone());
        tokio::task::spawn_blocking(move || pick(&r, &k, s.as_deref(), limit)).await.unwrap_or_default()
    };
    let chars: usize = items.iter().map(|(_, _, t)| t.chars().count().min(DIGEST_CHARS)).sum();
    let cfg = ai_cfg(&st);
    if req.get("dry").and_then(|v| v.as_bool()).unwrap_or(false) {
        let mut v = consent_info(&root, cfg.as_ref());
        v["count"] = json!(items.len());
        v["sent_chars"] = json!(chars);
        v["titles"] = json!(items.iter().take(5).map(|(it, _, _)| it.title.clone()).collect::<Vec<_>>());
        return Json(v).into_response();
    }
    let Some(cfg) = cfg else { return err(StatusCode::CONFLICT, "AI 学习要用模型：先在 /设置 里配上").into_response() };
    if !consented(&root, &cfg) {
        return needs_consent(&root, &cfg);
    }
    if items.is_empty() {
        return err(StatusCode::BAD_REQUEST, "没有要学的：这些都学过了，或者正文太短").into_response();
    }
    {
        let mut ds = digests().lock().unwrap();
        if ds.get(&root).map(|d| d.running).unwrap_or(false) {
            return err(StatusCode::CONFLICT, "已经在学了：等这一批学完，或者先停下").into_response();
        }
        ds.insert(root.clone(), DigestJob { running: true, total: items.len(), started: now_secs(), ..Default::default() });
    }
    let total = items.len();
    let root2 = root.clone();
    tokio::spawn(async move {
        let get = |f: &dyn Fn(&DigestJob) -> bool| digests().lock().unwrap().get(&root2).map(f).unwrap_or(true);
        let set = |f: &dyn Fn(&mut DigestJob)| {
            if let Some(d) = digests().lock().unwrap().get_mut(&root2) {
                f(d);
            }
        };
        let mut streak = 0;
        for (it, src, text) in items {
            // 暂停就等；停下就收工（已学成的都留着）
            loop {
                if get(&|d| d.stop) {
                    break;
                }
                if !get(&|d| d.paused) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            if get(&|d| d.stop) {
                break;
            }
            let title = it.title.clone();
            set(&|d| d.current = title.clone());
            let body: String = text.chars().take(DIGEST_CHARS).collect();
            let user = format!("资料《{}》（来源：{}，日期：{}）：\n{}", it.title, src, it.date, body);
            let out = litmus_mcp::agent::complete(&cfg, DIGEST_SYSTEM, &user).await;
            let made = match &out {
                Ok(a) => draft_md(a, &it.title, &src, &it.loc, &it.date, &cfg.model, &today()),
                Err(_) => None,
            };
            match made {
                Some((t, summary, md)) => {
                    streak = 0;
                    let root3 = root2.clone();
                    let d = Draft {
                        id: format!("{:x}", now_secs()) + &kernel_core::Cas::hash_hex(it.key.as_bytes())[..8],
                        key: it.key.clone(),
                        orig: it.title.clone(),
                        src: src.clone(),
                        loc: it.loc.clone(),
                        date: it.date.clone(),
                        title: t,
                        summary,
                        md,
                        created: now_secs(),
                        status: "pending".into(),
                        path: None,
                    };
                    let hash = it.hash.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let mut s = load_store(&root3);
                        s.drafts.retain(|x| !(x.key == d.key && x.status == "pending"));
                        s.drafts.push(d);
                        s.done.insert(it.key.clone(), hash);
                        save_store(&root3, &s);
                    })
                    .await;
                    set(&|d| {
                        d.done += 1;
                        d.made += 1;
                    });
                }
                None => {
                    streak += 1;
                    let why = match &out {
                        Err(e) => format!("这次没有连上模型：{e}"),
                        Ok(_) => "模型的回答没法整理成笔记".to_string(),
                    };
                    set(&|d| {
                        d.done += 1;
                        d.failed += 1;
                        d.error = Some(why.clone());
                    });
                    if streak >= 3 {
                        set(&|d| d.error = Some(format!("连续 3 篇没学成，先停下了：{why}")));
                        break;
                    }
                }
            }
        }
        set(&|d| {
            d.running = false;
            d.paused = false;
            d.current.clear();
            d.finished = now_secs();
        });
    });
    Json(json!({ "started": true, "total": total })).into_response()
}

pub fn digest_control(root: &Path, action: &str) -> bool {
    let mut ds = digests().lock().unwrap();
    let Some(d) = ds.get_mut(root).filter(|d| d.running) else { return false };
    match action {
        "pause" => d.paused = true,
        "resume" => d.paused = false,
        "stop" => {
            d.stop = true;
            d.paused = false;
        }
        _ => return false,
    }
    true
}

/// GET /field/digest：待确认的学习笔记 + 后台进度。
pub async fn digest_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = root_of(&st);
    let s = load_store(&root);
    let pending: Vec<&Draft> = s.drafts.iter().filter(|d| d.status == "pending").collect();
    let job = digests().lock().unwrap().get(&root).cloned();
    Json(json!({ "drafts": pending, "job": job, "learned": s.done.len() })).into_response()
}

/// POST /field/digest/accept {ids, folder?}：收进库（Notes/学习/），排进复习。
pub async fn digest_accept(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let ids: Vec<String> = req.get("ids").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let folder = req.get("folder").and_then(|v| v.as_str()).filter(|f| f.starts_with("Notes") && !f.contains("..")).map(|f| f.trim_end_matches('/').to_string()).unwrap_or_else(|| "Notes/学习".into());
    let root = root_of(&st);
    let mut store = load_store(&root);
    let mut written = Vec::new();
    let now = now_secs();
    let mut learn: Value = read_synced(&root, "learn.json", "field-learn.json").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({ "cards": {} }));
    if !learn["cards"].is_object() {
        learn["cards"] = json!({});
    }
    for d in store.drafts.iter_mut().filter(|d| d.status == "pending" && ids.contains(&d.id)) {
        let name = safe_name(&d.title);
        let name = if name.is_empty() { "学习笔记".to_string() } else { name };
        let mut path = format!("{folder}/{name}.md");
        let mut n = 2;
        while root.join(&path).exists() && n < 200 {
            path = format!("{folder}/{name} {n}.md");
            n += 1;
        }
        let r = st.kernel.lock().unwrap().put_doc(&path, &d.md);
        if let Err(e) = r {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        // 学进来的东西明天先过一遍：新卡，一天后到期
        learn["cards"][&path] = json!({ "due": now + 86_400, "interval": 1.0, "ease": 2.5, "reps": 0, "last": 0, "grade": "new" });
        d.status = "accepted".into();
        d.path = Some(path.clone());
        written.push(path);
    }
    let _ = write_synced(&root, "learn.json", &serde_json::to_vec_pretty(&learn).unwrap_or_default());
    save_store(&root, &store);
    audit_log(&root.join(".litmus/local"), "digest-accept", &json!({ "paths": written }));
    Json(json!({ "paths": written })).into_response()
}

/// POST /field/digest/reject {ids}：不要了（记住学过，不会再学同一版原文）。
pub async fn digest_reject(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let ids: Vec<String> = req.get("ids").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let root = root_of(&st);
    let mut store = load_store(&root);
    let mut n = 0;
    for d in store.drafts.iter_mut().filter(|d| d.status == "pending" && ids.contains(&d.id)) {
        d.status = "rejected".into();
        n += 1;
    }
    // 只留最近 300 条已处理的记录
    let keep_from = store.drafts.iter().filter(|d| d.status != "pending").count().saturating_sub(300);
    let mut seen = 0;
    store.drafts.retain(|d| {
        if d.status == "pending" {
            return true;
        }
        seen += 1;
        seen > keep_from
    });
    save_store(&root, &store);
    Json(json!({ "rejected": n })).into_response()
}

// ═══════════ 后台任务一览 ═══════════

/// GET /jobs：正在跑的（和十分钟内刚结束的）后台活 + 库外版本号 + 待确认篇数。壳里的小进度条就看这一个。
pub async fn jobs(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = root_of(&st);
    let now = now_secs();
    let recent = |running: bool, finished: u64| running || (finished > 0 && now.saturating_sub(finished) < 600);
    let names: HashMap<String, String> = super::sources::load_sources(&root).into_iter().map(|s| (s.id, s.name)).collect();
    let mut out = Vec::new();
    let mut scans: Vec<(String, super::sources::ScanState)> = super::sources::scans_snapshot().into_iter().filter(|(id, x)| names.contains_key(id) && recent(x.running, x.finished)).collect();
    scans.sort_by(|a, b| b.1.running.cmp(&a.1.running).then(b.1.started.cmp(&a.1.started)));
    for (id, x) in scans {
        out.push(json!({
            "id": format!("scan:{id}"), "kind": "scan", "title": format!("接入「{}」", names.get(&id).cloned().unwrap_or(id.clone())),
            "state": if x.running { if x.paused { "paused" } else { "running" } } else if x.error.is_some() { "error" } else { "done" },
            "done": x.seen, "total": x.total, "phase": x.phase, "items": x.items, "error": x.error, "finished": x.finished,
            "can": if x.running { json!(["pause", "resume", "stop"]) } else { json!([]) },
        }));
    }
    if let Some(d) = digests().lock().unwrap().get(&root).cloned().filter(|d| recent(d.running, d.finished)) {
        out.push(json!({
            "id": "digest", "kind": "digest", "title": "AI 学习",
            "state": if d.running { if d.paused { "paused" } else { "running" } } else if d.error.is_some() && d.made == 0 { "error" } else { "done" },
            "done": d.done, "total": d.total, "phase": d.current, "items": d.made, "failed": d.failed, "error": d.error, "finished": d.finished,
            "can": if d.running { json!(["pause", "resume", "stop"]) } else { json!([]) },
        }));
    }
    if let Some(p) = super::organize::patrol_snapshot(&root).filter(|p| recent(p.running, p.finished)) {
        out.push(json!({
            "id": "patrol", "kind": "patrol", "title": format!("AI 巡检 · {}", p.scope),
            "state": if p.running { "running" } else if p.error.is_some() { "error" } else { "done" },
            "done": p.done, "total": p.total, "phase": "", "items": p.tags.len() + p.titles.len() + p.verified.len(), "error": p.error, "finished": p.finished,
            "can": if p.running { json!(["stop"]) } else { json!([]) },
        }));
    }
    if let Some(c) = super::curate::job_snapshot(&root).filter(|c| recent(c.running, c.finished)) {
        out.push(json!({
            "id": "curate", "kind": "curate", "title": "筛选：留下知识、剔除过程文档",
            "state": if c.running { "running" } else if c.error.is_some() && c.total == 0 { "error" } else { "done" },
            "done": c.done, "total": c.total, "phase": c.phase, "items": 0, "error": c.error, "finished": c.finished,
            "can": if c.running { json!(["stop"]) } else { json!([]) },
        }));
    }
    let held = super::curate::load_store(&root).held.len();
    let pending = {
        let r = root.clone();
        tokio::task::spawn_blocking(move || load_store(&r).drafts.iter().filter(|d| d.status == "pending").count()).await.unwrap_or(0)
    };
    let busy = out.iter().any(|j| j["state"] == "running" || j["state"] == "paused");
    Json(json!({ "jobs": out, "rev": super::sources::REV.load(std::sync::atomic::Ordering::SeqCst), "pending": pending, "held": held, "busy": busy })).into_response()
}

/// POST /jobs/{id}/{action}：pause | resume | stop。
pub async fn job_control(State(st): State<Arc<AppState>>, h: HeaderMap, AxumPath((id, action)): AxumPath<(String, String)>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = root_of(&st);
    let ok = if let Some(sid) = id.strip_prefix("scan:") {
        super::sources::scan_control(sid, &action)
    } else if id == "digest" {
        digest_control(&root, &action)
    } else if id == "curate" && action == "stop" {
        super::curate::job_stop(&root)
    } else if id == "patrol" && action == "stop" {
        super::organize::patrol_request_stop(&root)
    } else {
        false
    };
    if !ok {
        return err(StatusCode::CONFLICT, "这个任务已经结束了，或者不支持这个操作").into_response();
    }
    Json(json!({ "ok": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_feature_releases_count_as_major() {
        assert_eq!(major_minor("0.16.0").as_deref(), Some("0.16"));
        assert_eq!(major_minor("0.16.1"), None);
        assert_eq!(major_minor("0.17.0-nightly.20261028"), None);
    }

    #[test]
    fn whats_new_reads_the_changelog_section() {
        let cl = "# 更新日志\n\n## Unreleased\n\n## 0.16.0 — 2026-10-28 · 上手与后台\n\n第一次打开边用边引导。\n\n### 边用边上手\n\n- 右下角一张小卡片带你走几步：记一条、问一句、接一个来源（/数据源）。\n\n### 进度一眼看到\n\n- 左上角的小条，点开能暂停。路径 Notes/学习/ 不算指令。\n\n## 0.15.0 — 2026-10-27 · 改名\n\n### 旧的\n\n- 不该出现\n";
        let w = whats_new(cl, "0.16").unwrap();
        assert_eq!(w["theme"], "上手与后台");
        assert_eq!(w["intro"], "第一次打开边用边引导。");
        let items = w["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "边用边上手");
        assert_eq!(items[0]["cmds"], json!(["/数据源"]));
        assert_eq!(items[1]["cmds"], json!([]));
        assert!(whats_new(cl, "0.14").is_none());
    }

    #[test]
    fn onboarding_mode_for_new_and_existing_users() {
        let mm = Some("0.16");
        assert_eq!(onboarding_mode(None, 0, mm, true), "first");
        assert_eq!(onboarding_mode(None, 500, mm, true), "whatsnew");
        assert_eq!(onboarding_mode(None, 500, None, true), "none");
        let mut s = Onboarding::default();
        assert_eq!(onboarding_mode(Some(&s), 500, mm, true), "first");
        s.dismissed = true;
        assert_eq!(onboarding_mode(Some(&s), 500, mm, true), "whatsnew");
        s.seen = "0.16".into();
        assert_eq!(onboarding_mode(Some(&s), 500, mm, true), "none");
        assert_eq!(onboarding_mode(Some(&s), 500, Some("0.17"), false), "none");
    }

    #[test]
    fn local_model_hosts() {
        assert!(is_local_host("localhost:11434"));
        assert!(is_local_host("127.0.0.1:7799"));
        assert!(!is_local_host("api.openai.com"));
        assert_eq!(host_of("https://user:pw@API.example.com/v1/"), "api.example.com");
    }

    #[test]
    fn draft_note_carries_its_origin() {
        let a = "好的：```json\n{\"title\":\"候选词按频率排序\",\"summary\":\"排序改为按频率降序。\",\"points\":[\"同频按最近使用\",\"冷启动用词典频率\"],\"use\":\"改输入法时参考\",\"tags\":[\"输入法\",\"#排序\"]}\n```";
        let (t, s, md) = draft_md(a, "把候选词排序改成按频率", "Claude Code", "/Users/x/.claude/projects/a.jsonl", "2026-10-01", "demo", "2026-10-28").unwrap();
        assert_eq!(t, "候选词按频率排序");
        assert_eq!(s, "排序改为按频率降序。");
        assert!(md.starts_with("---\ntags: [学习, 输入法, 排序]\n"));
        assert!(md.contains("origin: \"/Users/x/.claude/projects/a.jsonl\""));
        assert!(md.contains("\n## 要点\n\n- 同频按最近使用\n- 冷启动用词典频率\n"));
        assert!(md.contains("原件留在原处：`/Users/x/.claude/projects/a.jsonl`"));
        assert!(draft_md("没有 JSON", "x", "y", "", "", "m", "d").is_none());
    }
}
