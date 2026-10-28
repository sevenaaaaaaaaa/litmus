//! 全库整理（G2）：不只看近期——长期与近期的碎片一起聚合，过时的知识找出来复核或更新。
//!
//! - 信号在场构建时顺手算好（`signals`）：正文字数、未完成的 TODO、提到的计划日期。
//! - `fragments`：短文档（任何年代）按近邻连成组，建议合成一篇。
//! - `stale`：被更新的同主题文档取代、计划日期已过、TODO 放了很久、很久没动却仍被引用。
//! - 巡检（patrol）：后台按批把范围内的文档交给模型——补标签、起标题、核实过时——带进度，可停，结果留在本机。
//! - 合成（merge）与更新（refresh）都只返回「怎样撤销」，执行在壳里，撤销走现有的 /doc 接口。

use super::field::{current, load_region_names, Cached, DocMeta};
use super::{audit_log, check_token, err, AppState};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const PLAN_WORDS: [&str; 14] = ["计划", "预计", "截止", "上线", "发布", "deadline", "Deadline", "due", "里程碑", "交付", "排期", "到期", "前完成", "目标"];
const TODO_WORDS: [&str; 7] = ["TODO", "待定", "待补充", "TBD", "WIP", "- [ ]", "待确认"];
/// 碎片：正文少于这么多字
pub const FRAGMENT_CHARS: usize = 280;

/// 从正文抽信号：(正文字数, 有没有未完成事项, 计划里提到的日期 YYYY-MM[-DD])。
pub fn signals(text: &str) -> (usize, bool, Vec<String>) {
    let mut in_fm = false;
    let mut chars = 0usize;
    let mut todo = false;
    let mut plans: Vec<String> = Vec::new();
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
        if t.starts_with("# ") {
            continue;
        }
        chars += t.chars().filter(|c| !c.is_whitespace()).count();
        if !todo && TODO_WORDS.iter().any(|w| t.contains(w)) {
            todo = true;
        }
        if plans.len() < 6 && PLAN_WORDS.iter().any(|w| t.contains(w)) {
            for d in dates_in(t) {
                if !plans.contains(&d) {
                    plans.push(d);
                }
            }
        }
    }
    (chars, todo, plans)
}

// ───────── 跨文档事实冲突 ─────────

/// 一条写下的事实：「端口：7700」「- **发布日期**：2026-03-01」「| 服务器 | 172.96.253.73 |」
#[derive(Clone, Debug)]
pub struct Fact {
    /// 规范化的键（去记号、小写、去空白）
    pub key: String,
    /// 原样的键与值（显示、改写用）
    pub k: String,
    pub v: String,
    /// 规范化的值（比较用）
    pub norm: String,
}

/// 每篇自己的、不同才正常的键：不拿来比
const DOC_OWN: [&str; 40] = ["日期", "时间", "作者", "来源", "备注", "说明", "状态", "标签", "tags", "链接", "原文", "原件", "问题", "原因", "结论", "决定", "待办", "例如", "比如", "注意", "提示", "标题", "名称", "类型", "结果", "步骤", "目标", "背景", "摘要", "关键词", "分类", "进度", "评分", "地点", "参与", "参会", "主持", "记录", "下一步", "todo"];

fn strip_marks(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '*' | '`' | '_' | '~' | '=')).collect::<String>().trim().to_string()
}

pub fn facts_of(text: &str) -> Vec<Fact> {
    let mut out = Vec::new();
    let mut fence = false;
    let mut in_fm = false;
    let lines: Vec<&str> = text.lines().collect();
    let is_sep = |l: &str| { let t = l.trim(); t.starts_with('|') && t.contains("--") && t.chars().all(|x| matches!(x, '-' | ':' | ' ' | '|')) };
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if i == 0 && t == "---" {
            in_fm = true;
            continue;
        }
        if in_fm {
            if t == "---" { in_fm = false; }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence || t.starts_with('#') || t.is_empty() {
            continue;
        }
        let (k, v) = if t.starts_with('|') {
            // 两列的表格行；表头（下一行是 |---|）和分隔行不算
            let cells: Vec<&str> = t.trim_matches('|').split('|').map(|c| c.trim()).collect();
            if cells.len() != 2 || is_sep(t) || lines.get(i + 1).map(|n| is_sep(n)).unwrap_or(false) {
                continue;
            }
            (cells[0].to_string(), cells[1].to_string())
        } else {
            let item = t.trim_start_matches(['-', '*', '+', '>', ' ']).trim();
            let Some((k, v)) = item.split_once('：').or_else(|| item.split_once(": ")) else { continue };
            (k.to_string(), v.to_string())
        };
        let k = strip_marks(&k);
        let v = strip_marks(v.trim().trim_end_matches(['。', '；', ';', '，', ',']));
        let kn: String = k.to_lowercase().chars().filter(|c| !c.is_whitespace() && !matches!(c, '「' | '」' | '"' | '\'' | '（' | '）' | '(' | ')')).collect();
        let kc = kn.chars().count();
        let ascii = kn.chars().all(|c| c.is_ascii());
        if kc < 2 || kc > 16 || (ascii && kc < 3) || kn.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true) || DOC_OWN.contains(&kn.as_str()) || k.split_whitespace().count() > 3 || k.contains("http") {
            continue;
        }
        let vc = v.chars().count();
        // 像个事实：带数字，或者很短
        if vc == 0 || vc > 60 || !(v.chars().any(|c| c.is_ascii_digit()) || vc <= 16) {
            continue;
        }
        let norm: String = v.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        out.push(Fact { key: kn, k, v, norm });
        if out.len() >= 40 {
            break;
        }
    }
    out
}

/// 同一个键，不同的文档写了不同的值。只比「像事实」的键，且两篇要么意思相近、要么值里有数字。
pub fn conflicts(c: &Cached, keep: &[bool], ignored: &std::collections::HashSet<String>, limit: usize) -> (Vec<Value>, usize) {
    let knn = c.knn();
    let mut by_key: HashMap<&str, Vec<(usize, &Fact)>> = HashMap::new();
    for (i, m) in c.meta.iter().enumerate() {
        if !keep[i] || m.src.is_some() || m.kind == "source" {
            continue;
        }
        let mut seen = std::collections::HashSet::new();
        for f in &m.facts {
            // 同一篇里同一个键只取第一次
            if seen.insert(f.key.as_str()) {
                by_key.entry(f.key.as_str()).or_default().push((i, f));
            }
        }
    }
    let near = |a: usize, b: usize| knn.get(a).map(|v| v.iter().any(|&(j, s)| j == b && s >= 0.25)).unwrap_or(false) || knn.get(b).map(|v| v.iter().any(|&(j, s)| j == a && s >= 0.25)).unwrap_or(false);
    let mut out: Vec<(i64, Value)> = Vec::new();
    for (key, list) in by_key {
        let docs: std::collections::HashSet<usize> = list.iter().map(|x| x.0).collect();
        if docs.len() < 2 || docs.len() > 8 {
            continue;
        }
        let vals: std::collections::HashSet<&str> = list.iter().map(|x| x.1.norm.as_str()).collect();
        if vals.len() < 2 {
            continue;
        }
        let digits = list.iter().all(|x| x.1.v.chars().any(|c| c.is_ascii_digit()));
        let related = list.iter().any(|&(a, fa)| list.iter().any(|&(b, fb)| a != b && fa.norm != fb.norm && (near(a, b) || digits && key.chars().count() >= 3)));
        if !related {
            continue;
        }
        let mut entries: Vec<&(usize, &Fact)> = list.iter().collect();
        entries.sort_by_key(|x| std::cmp::Reverse(c.meta[x.0].mtime));
        let mut sig_parts: Vec<String> = entries.iter().map(|(i, f)| format!("{}={}", c.meta[*i].path, f.norm)).collect();
        sig_parts.sort();
        let sig = format!("{key}|{}", sig_parts.join("|"));
        if ignored.contains(&sig) {
            continue;
        }
        let newest = c.meta[entries[0].0].mtime;
        let score = (if digits { 100 } else { 50 }) + entries.len() as i64 * 5 + newest / 86_400 / 365;
        out.push((
            score,
            json!({
                "key": entries[0].1.k, "sig": sig,
                "entries": entries.iter().map(|(i, f)| {
                    let m = &c.meta[*i];
                    json!({ "path": m.path, "title": m.title, "k": f.k, "value": f.v, "updated": super::field::ymd(m.mtime) })
                }).collect::<Vec<_>>(),
            }),
        ));
    }
    let total = out.len();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    (out.into_iter().take(limit).map(|x| x.1).collect(), total)
}

fn conflict_ignore_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".litmus/local/conflicts-ignore.json")
}
pub fn conflict_ignored(root: &std::path::Path) -> std::collections::HashSet<String> {
    std::fs::read_to_string(conflict_ignore_path(root)).ok().and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default().into_iter().collect()
}

/// POST /field/organize/conflict-ignore {sig}：这组不算冲突，以后别提
pub async fn conflict_ignore(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let Some(sig) = req.get("sig").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else { return err(StatusCode::BAD_REQUEST, "要 sig").into_response() };
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let mut set: Vec<String> = conflict_ignored(&root).into_iter().collect();
    if !set.iter().any(|x| x == sig) {
        set.push(sig.to_string());
    }
    let _ = std::fs::create_dir_all(root.join(".litmus/local"));
    let _ = std::fs::write(conflict_ignore_path(&root), serde_json::to_vec(&set).unwrap_or_default());
    Json(json!({ "ok": true })).into_response()
}

/// 行里的日期：2026-03-15、2026-03、2026/3/15、2026年3月(15日)。统一成 YYYY-MM 或 YYYY-MM-DD。
pub fn dates_in(s: &str) -> Vec<String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let num = |b: &[char], i: usize, max: usize| -> (u32, usize) {
        let mut v = 0u32;
        let mut k = 0;
        while i + k < b.len() && k < max && b[i + k].is_ascii_digit() {
            v = v * 10 + b[i + k].to_digit(10).unwrap();
            k += 1;
        }
        (v, k)
    };
    while i + 4 < b.len() {
        let (y, ky) = num(&b, i, 4);
        let prev_digit = i > 0 && b[i - 1].is_ascii_digit();
        if ky == 4 && !prev_digit && (2000..=2099).contains(&y) {
            let sep = b[i + 4];
            if matches!(sep, '-' | '/' | '年' | '.') {
                let (m, km) = num(&b, i + 5, 2);
                if km > 0 && (1..=12).contains(&m) {
                    let mut j = i + 5 + km;
                    if j < b.len() && b[j] == '月' {
                        j += 1;
                    }
                    let mut d = 0;
                    if j < b.len() && matches!(b[j], '-' | '/' | '.') || (sep == '年' && j < b.len() && b[j].is_ascii_digit()) {
                        let st = if b[j].is_ascii_digit() { j } else { j + 1 };
                        let (dd, kd) = num(&b, st, 2);
                        if kd > 0 && (1..=31).contains(&dd) {
                            d = dd;
                            j = st + kd;
                        }
                    }
                    out.push(if d > 0 { format!("{y:04}-{m:02}-{d:02}") } else { format!("{y:04}-{m:02}") });
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    super::field::ymd(secs)
}

fn days_between(a: &str, b: &str) -> i64 {
    let p = |s: &str| -> Option<i64> {
        let y: i64 = s.get(0..4)?.parse().ok()?;
        let m: i64 = s.get(5..7)?.parse().ok()?;
        let d: i64 = s.get(8..10).and_then(|x| x.parse().ok()).unwrap_or(28);
        // 粗算即可（只用来比「放了多少天」）
        Some(y * 365 + (m - 1) * 30 + d)
    };
    match (p(a), p(b)) {
        (Some(x), Some(y)) => y - x,
        _ => 0,
    }
}

/// 整理的范围：区域（自动名）、日期区间、文件夹前缀。都不给 = 全库。
#[derive(Default, Clone)]
pub struct Scope {
    pub region: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub folder: Option<String>,
}

impl Scope {
    pub fn from_json(v: &Value) -> Self {
        let s = |k: &str| v.get("scope").and_then(|x| x.get(k)).and_then(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
        Scope { region: s("region"), from: s("from"), to: s("to"), folder: s("folder") }
    }
    pub fn keep(&self, c: &Cached) -> Vec<bool> {
        let f = &c.field;
        c.meta
            .iter()
            .enumerate()
            .map(|(i, m)| {
                // 库外的不参与整理（不能写）
                if m.src.is_some() {
                    return false;
                }
                if let Some(r) = &self.region {
                    if f.region_names.get(f.region[i]).map(|a| a != r).unwrap_or(true) {
                        return false;
                    }
                }
                if let Some(a) = &self.from {
                    if m.date.as_str() < a.as_str() {
                        return false;
                    }
                }
                if let Some(b) = &self.to {
                    if m.date.as_str() > b.as_str() {
                        return false;
                    }
                }
                if let Some(d) = &self.folder {
                    if !m.path.starts_with(d.as_str()) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }
    pub fn is_all(&self) -> bool {
        self.region.is_none() && self.from.is_none() && self.to.is_none() && self.folder.is_none()
    }
}

fn is_fragment(m: &DocMeta) -> bool {
    m.chars < FRAGMENT_CHARS && m.kind != "answer" && m.kind != "html" && !m.path.starts_with("Notes/Agent/")
}

/// 碎片组：短文档按近邻（相似度 ≥ 0.3）连通，3 到 12 篇一组；跨越的时间越长越靠前（长期碎片最容易被遗忘）。
pub fn fragments(c: &Cached, keep: &[bool], limit: usize) -> (Vec<Value>, usize) {
    let n = c.meta.len();
    let frag: Vec<bool> = (0..n).map(|i| keep[i] && is_fragment(&c.meta[i])).collect();
    let knn = c.knn();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for i in 0..n {
        if !frag[i] {
            continue;
        }
        for &(j, s) in &knn[i] {
            if s >= 0.3 && frag[j] {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        if frag[i] {
            let r = find(&mut parent, i);
            groups.entry(r).or_default().push(i);
        }
    }
    let mut out: Vec<(i64, usize, Value)> = Vec::new();
    for (_, mut g) in groups {
        if g.len() < 3 {
            continue;
        }
        // 太大的连通块只取最中心的 12 篇（组内被连得最多的）
        if g.len() > 12 {
            let set: std::collections::HashSet<usize> = g.iter().copied().collect();
            g.sort_by_key(|&i| (std::cmp::Reverse(knn[i].iter().filter(|(j, _)| set.contains(j)).count()), i));
            g.truncate(12);
        }
        g.sort_by(|&a, &b| c.meta[a].date.cmp(&c.meta[b].date).then(a.cmp(&b)));
        let first = c.meta[g[0]].date.clone();
        let last = c.meta[*g.last().unwrap()].date.clone();
        let span = days_between(&first, &last).max(0);
        let set: std::collections::HashSet<usize> = g.iter().copied().collect();
        let hub = *g.iter().max_by_key(|&&i| (knn[i].iter().filter(|(j, _)| set.contains(j)).count(), std::cmp::Reverse(i))).unwrap();
        let theme = kernel_core::field::short_title(&c.meta[hub].title);
        let folder = {
            let mut t: HashMap<&str, usize> = HashMap::new();
            for &i in &g {
                let d = c.meta[i].path.rsplit_once('/').map(|x| x.0).unwrap_or("Notes");
                if !d.starts_with("Notes/Memos") {
                    *t.entry(d).or_insert(0) += 1;
                }
            }
            t.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0))).map(|x| x.0.to_string()).unwrap_or_else(|| "Notes".into())
        };
        out.push((
            span,
            g.len(),
            json!({
                "theme": theme,
                "paths": g.iter().map(|&i| c.meta[i].path.clone()).collect::<Vec<_>>(),
                "titles": g.iter().map(|&i| c.meta[i].title.clone()).collect::<Vec<_>>(),
                "from": first, "to": last, "span_days": span, "folder": folder,
                "chars": g.iter().map(|&i| c.meta[i].chars).sum::<usize>(),
            }),
        ));
    }
    let total = out.len();
    out.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    (out.into_iter().take(limit).map(|x| x.2).collect(), total)
}

/// 过时候选。kind：superseded（有更新的同主题文档）、expired（计划日期已过）、todo（未完成事项放太久）、aging（很久没动但被引用）。
pub fn stale(c: &Cached, keep: &[bool], inbound: &[usize], limit: usize) -> (Vec<Value>, usize) {
    let now = today();
    let knn = c.knn();
    let mut out: Vec<(i32, Value)> = Vec::new();
    for (i, m) in c.meta.iter().enumerate() {
        if !keep[i] || m.kind == "memo" || m.kind == "source" || m.path.starts_with("Notes/Agent/") {
            continue;
        }
        let updated = super::field::ymd(m.mtime);
        let age = days_between(&updated, &now);
        // 1) 被取代：很像的一篇明显更新（晚 30 天以上），而且它更长或标题几乎一样
        if let Some(&(j, s)) = knn[i].iter().find(|&&(j, s)| {
            let o = &c.meta[j];
            s >= 0.45 && o.kind != "memo" && days_between(&updated, &super::field::ymd(o.mtime)) >= 30 && (o.chars >= m.chars / 2)
        }) {
            let o = &c.meta[j];
            out.push((
                90 + (s * 10.0) as i32,
                json!({ "path": m.path, "title": m.title, "kind": "superseded", "newer": o.path, "newer_title": o.title, "sim": (s * 100.0).round() / 100.0,
                        "reason": format!("《{}》讲的是同一件事，晚了 {} 天", o.title, days_between(&updated, &super::field::ymd(o.mtime))), "updated": updated }),
            ));
            continue;
        }
        // 2) 计划日期已过：正文写着「预计 2025-03 上线」，而今天已经过了
        let past: Vec<&String> = m.plans.iter().filter(|d| d.as_str() < now.get(..d.len()).unwrap_or("")).collect();
        if !past.is_empty() && age >= 14 {
            out.push((
                70,
                json!({ "path": m.path, "title": m.title, "kind": "expired", "dates": past, "reason": format!("写着 {} 的计划，日期已经过了，之后没再改过", past.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("、")), "updated": updated }),
            ));
            continue;
        }
        // 3) TODO 放了两个月以上
        if m.todo && age >= 60 {
            out.push((50, json!({ "path": m.path, "title": m.title, "kind": "todo", "reason": format!("有没做完的事项，已经放了 {age} 天"), "updated": updated })));
            continue;
        }
        // 4) 一年没动，却还被 3 篇以上引用
        if age >= 365 && inbound.get(i).copied().unwrap_or(0) >= 3 {
            out.push((40, json!({ "path": m.path, "title": m.title, "kind": "aging", "reason": format!("{} 天没改过，但还有 {} 篇链到它", age, inbound[i]), "updated": updated })));
        }
    }
    let total = out.len();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    (out.into_iter().take(limit).map(|x| x.1).collect(), total)
}

// ───────── 合成碎片 ─────────

fn body_of(md: &str) -> String {
    let mut s = md;
    if let Some(rest) = s.strip_prefix("---\n") {
        if let Some(i) = rest.find("\n---") {
            s = rest[i + 4..].trim_start_matches('\n');
        }
    }
    let s = s.trim();
    let s = if s.starts_with("# ") { s.split_once('\n').map(|x| x.1).unwrap_or("") } else { s };
    s.trim().to_string()
}

fn safe_name(t: &str) -> String {
    t.chars().filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '\n' | '\r' | '\t')).collect::<String>().trim().chars().take(60).collect()
}

/// POST /field/merge {paths, title, folder?, mode:"plain"|"ai", originals:"keep"|"archive"}
/// 新建一篇合成文档；原文要么留着并在末尾注明「已并入」，要么挪进 Notes/归档/碎片/。返回 undo 说明。
pub async fn merge(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let paths: Vec<String> = req.get("paths").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|p| kernel_core::is_safe_doc_path(p)).take(30).collect()).unwrap_or_default();
    if paths.len() < 2 {
        return err(StatusCode::BAD_REQUEST, "至少两篇才合得成").into_response();
    }
    let title = safe_name(req.get("title").and_then(|v| v.as_str()).unwrap_or(""));
    if title.is_empty() {
        return err(StatusCode::BAD_REQUEST, "给合成的这篇起个标题").into_response();
    }
    let folder = req.get("folder").and_then(|v| v.as_str()).filter(|f| f.starts_with("Notes") && !f.contains("..")).unwrap_or("Notes").trim_end_matches('/').to_string();
    let mode = req.get("mode").and_then(|v| v.as_str()).unwrap_or("plain");
    let archive = req.get("originals").and_then(|v| v.as_str()) == Some("archive");
    let (root, ai) = {
        let k = st.kernel.lock().unwrap();
        (k.vault.root.clone(), k.vault.config.ai.clone().filter(|c| !c.base_url.is_empty()))
    };
    let mut srcs: Vec<(String, String, String, String)> = Vec::new(); // (path, title, body, raw)
    for p in &paths {
        let Ok(raw) = std::fs::read_to_string(root.join(p)) else { continue };
        let t = super::field::title_of(p, &raw);
        srcs.push((p.clone(), t, body_of(&raw), raw));
    }
    if srcs.len() < 2 {
        return err(StatusCode::NOT_FOUND, "来源文档读不到了，刷新一下再试").into_response();
    }
    let stem = super::stem_of;
    let refs = srcs.iter().enumerate().map(|(i, (p, _, _, _))| format!("{}. [[{}]]", i + 1, stem(p))).collect::<Vec<_>>().join("\n");
    let mut note = None;
    let body_md = if mode == "ai" {
        match &ai {
            Some(cfg) => {
                let ctx = srcs.iter().enumerate().map(|(i, (_, t, b, _))| format!("[{}] 《{}》\n{}", i + 1, t, b.chars().take(1200).collect::<String>())).collect::<Vec<_>>().join("\n\n");
                let system = "把用户知识库里的几条零散笔记合成一篇条理清楚的文档。保留所有事实、数字、结论和不同意见，不编造；按主题分 2 到 5 个二级标题（## ），\
                    每条论断后用 [编号] 标出来自哪条笔记。不要写开场白和总结套话。只输出正文（不要一级标题）。";
                match litmus_mcp::agent::complete(cfg, system, &format!("标题：{title}\n\n笔记：\n{ctx}")).await {
                    Ok(t) if t.trim().len() > 20 => t.trim().to_string(),
                    Ok(_) | Err(_) => {
                        note = Some("模型这次没给出合成稿，先按原文拼在一起了，之后可以再让 AI 改写。".to_string());
                        String::new()
                    }
                }
            }
            None => {
                note = Some("没配模型：按原文拼在一起了。".to_string());
                String::new()
            }
        }
    } else {
        String::new()
    };
    let body_md = if body_md.is_empty() {
        srcs.iter().map(|(p, t, b, _)| format!("## {t}\n\n{b}\n\n> 出自 [[{}]]", stem(p))).collect::<Vec<_>>().join("\n\n")
    } else {
        body_md
    };
    let md = format!("# {title}\n\n> 由 {} 篇笔记合成。\n\n{body_md}\n\n## 来源\n\n{refs}\n", srcs.len());
    let mut target = format!("{folder}/{title}.md");
    let mut n = 2;
    while root.join(&target).exists() && n < 50 {
        target = format!("{folder}/{title} {n}.md");
        n += 1;
    }
    let mut undo_moves: Vec<Value> = Vec::new();
    let mut undo_restore: Vec<Value> = Vec::new();
    {
        let mut k = st.kernel.lock().unwrap();
        if let Err(e) = k.put_doc(&target, &md) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        let new_stem = stem(&target);
        for (p, _, _, raw) in &srcs {
            if archive {
                let name = p.rsplit('/').next().unwrap_or(p);
                let mut to = format!("Notes/归档/碎片/{name}");
                let mut m = 2;
                while root.join(&to).exists() && m < 50 {
                    to = format!("Notes/归档/碎片/{} {m}.md", name.trim_end_matches(".md"));
                    m += 1;
                }
                let dst = root.join(&to);
                let _ = std::fs::create_dir_all(dst.parent().unwrap());
                if std::fs::rename(root.join(p), &dst).is_ok() {
                    undo_moves.push(json!({ "from": to, "to": p }));
                }
            } else {
                let next = format!("{}\n\n> 已并入 [[{new_stem}]]\n", raw.trim_end());
                if k.put_doc(p, &next).is_ok() {
                    undo_restore.push(json!({ "path": p, "md": raw }));
                }
            }
        }
        let _ = k.sync_all();
        audit_log(&k.vault.sidecar(), "merge", &json!({ "into": target, "from": paths.len(), "archive": archive }));
    }
    Json(json!({ "path": target, "note": note, "undo": { "delete": [target], "moves": undo_moves, "restore": undo_restore } })).into_response()
}

// ───────── 更新过时的一篇 ─────────

/// POST /field/refresh {path, newer?:[...]}：让模型参照更新的同主题文档改写这一篇，返回新稿与改了什么（不写盘）。
pub async fn refresh(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !kernel_core::is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let Some(cfg) = st.kernel.lock().unwrap().vault.config.ai.clone().filter(|c| !c.base_url.is_empty()) else {
        return err(StatusCode::CONFLICT, "还没有配置模型：可以先「标记待复核」，或参照新文档自己改；配上模型（/设置）后能起草更新稿").into_response();
    };
    let c = match current(&st, false).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let Ok(raw) = std::fs::read_to_string(root.join(&path)) else { return err(StatusCode::NOT_FOUND, "读不到这篇").into_response() };
    // 参照：调用方给的，否则取近邻里比它新的前 3 篇
    let mut newer: Vec<String> = req.get("newer").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).filter(|p| kernel_core::is_safe_doc_path(p)).take(3).collect()).unwrap_or_default();
    if newer.is_empty() {
        if let Some(i) = c.meta.iter().position(|m| m.path == path) {
            let mine = c.meta[i].mtime;
            newer = c.knn()[i].iter().filter(|(j, s)| *s >= 0.25 && c.meta[*j].mtime > mine).take(3).map(|(j, _)| c.meta[*j].path.clone()).collect();
        }
    }
    let refs: Vec<(String, String)> = newer.iter().filter_map(|p| std::fs::read_to_string(root.join(p)).ok().map(|t| (p.clone(), t))).collect();
    let now = today();
    let ctx = refs.iter().enumerate().map(|(i, (p, t))| format!("[参照{}] {}\n{}", i + 1, p, body_of(t).chars().take(1500).collect::<String>())).collect::<Vec<_>>().join("\n\n");
    let system = "用户知识库里有一篇可能过时的文档。参照给出的更新资料（以及今天的日期），把它改成现在仍然正确的版本：\
        保留原有结构和仍然正确的内容；过时的计划、日期、结论改掉或注明已变化；不知道的不要编。\
        先输出改好的完整 Markdown（保留 frontmatter 和一级标题），然后单独一行 ===CHANGES===，再用 2 到 6 条短句列出改了什么。";
    let user = format!("今天是 {now}。\n\n要更新的文档（{path}）：\n{}\n\n更新的资料：\n{}", raw.chars().take(6000).collect::<String>(), if ctx.is_empty() { "（没有更新的同主题文档，只按今天的日期判断时间相关的表述）".into() } else { ctx });
    match litmus_mcp::agent::complete(&cfg, system, &user).await {
        Ok(t) => {
            let (md, changes) = match t.split_once("===CHANGES===") {
                Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
                None => (t.trim().to_string(), String::new()),
            };
            let md = md.trim_start_matches("```markdown").trim_start_matches("```md").trim_start_matches("```").trim_end_matches("```").trim().to_string();
            if md.len() < 10 {
                return err(StatusCode::BAD_GATEWAY, "模型没给出更新稿").into_response();
            }
            Json(json!({ "path": path, "markdown": format!("{md}\n"), "before": raw, "changes": changes, "sources": newer })).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("这次没有连上模型：{e}")).into_response(),
    }
}

// ───────── 巡检：后台分批交给模型 ─────────

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Patrol {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub calls: usize,
    pub started: u64,
    pub finished: u64,
    pub scope: String,
    pub tags: Vec<Value>,
    pub titles: Vec<Value>,
    pub verified: Vec<Value>,
    pub error: Option<String>,
    #[serde(skip)]
    pub stop: bool,
}

fn patrols() -> &'static Mutex<HashMap<std::path::PathBuf, Patrol>> {
    static P: std::sync::OnceLock<Mutex<HashMap<std::path::PathBuf, Patrol>>> = std::sync::OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 给后台任务一览用：正在跑的，或者上次的结果。
pub fn patrol_snapshot(root: &std::path::Path) -> Option<Patrol> {
    patrols().lock().unwrap().get(root).cloned()
}

pub fn patrol_request_stop(root: &std::path::Path) -> bool {
    match patrols().lock().unwrap().get_mut(root) {
        Some(p) if p.running => {
            p.stop = true;
            true
        }
        _ => false,
    }
}

fn patrol_file(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".litmus/local").join("patrol.json")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// GET /field/patrol：进度与已得出的建议（重启后读上次的结果）。
pub async fn patrol_status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let p = patrols().lock().unwrap().get(&root).cloned().or_else(|| std::fs::read_to_string(patrol_file(&root)).ok().and_then(|s| serde_json::from_str(&s).ok())).unwrap_or_default();
    Json(serde_json::to_value(&p).unwrap_or(json!({}))).into_response()
}

/// DELETE /field/patrol：停下（做完当前这一批就停）。
pub async fn patrol_stop(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    if let Some(p) = patrols().lock().unwrap().get_mut(&root) {
        p.stop = true;
    }
    Json(json!({ "ok": true })).into_response()
}

/// POST /field/patrol {scope, dry?:true}：估算或开始。估算回 {docs, calls}，不调模型。
pub async fn patrol_start(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let scope = Scope::from_json(&req);
    let c = match current(&st, false).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let keep = scope.keep(&c);
    // 要模型看的：没标签或标题过短的文档（全范围，不再限前 40 篇）+ 规则找出的过时候选
    let label_docs: Vec<usize> = (0..c.meta.len())
        .filter(|&i| keep[i])
        .filter(|&i| {
            let m = &c.meta[i];
            m.path.ends_with(".md") && !m.path.starts_with("Notes/Agent/") && (m.tags.is_empty() || m.title.chars().count() <= 4)
        })
        .collect();
    let inbound = inbound_counts(&c);
    let (stale_list, _) = stale(&c, &keep, &inbound, 400);
    const BATCH: usize = 30;
    const STALE_BATCH: usize = 6;
    let calls = label_docs.len().div_ceil(BATCH) + stale_list.len().div_ceil(STALE_BATCH);
    if req.get("dry").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Json(json!({ "docs": label_docs.len(), "stale": stale_list.len(), "calls": calls })).into_response();
    }
    let Some(cfg) = st.kernel.lock().unwrap().vault.config.ai.clone().filter(|c| !c.base_url.is_empty()) else {
        return err(StatusCode::CONFLICT, "巡检要用模型：先在 /设置 里配上").into_response();
    };
    // 巡检会把文档的标题和摘录发给模型：先看过隐私告知、同意了才开始
    if !super::jobs::consented(&root, &cfg) {
        let mut v = json!({ "needs_consent": true, "error": "开始之前要先看一下隐私告知并同意" });
        v["host"] = json!(cfg.base_url.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or(""));
        v["model"] = json!(cfg.model);
        return (StatusCode::PRECONDITION_REQUIRED, Json(v)).into_response();
    }
    {
        let mut ps = patrols().lock().unwrap();
        if ps.get(&root).map(|p| p.running).unwrap_or(false) {
            return err(StatusCode::CONFLICT, "已经在巡检了").into_response();
        }
        ps.insert(
            root.clone(),
            Patrol { running: true, total: calls, started: now_secs(), scope: if scope.is_all() { "全库".into() } else { scope_label(&scope) }, ..Default::default() },
        );
    }
    let st2 = st.clone();
    let root2 = root.clone();
    tokio::spawn(async move {
        // 词表：全库已有标签按次数
        let mut vocab: HashMap<String, usize> = HashMap::new();
        for m in &c.meta {
            for t in &m.tags {
                *vocab.entry(t.clone()).or_insert(0) += 1;
            }
        }
        let mut vocab: Vec<(String, usize)> = vocab.into_iter().collect();
        vocab.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let vocab: Vec<String> = vocab.into_iter().take(120).map(|x| x.0).collect();
        let stopped = |root: &std::path::Path| patrols().lock().unwrap().get(root).map(|p| p.stop).unwrap_or(true);
        let bump = |root: &std::path::Path, f: &dyn Fn(&mut Patrol)| {
            if let Some(p) = patrols().lock().unwrap().get_mut(root) {
                f(p);
            }
        };
        for chunk in label_docs.chunks(BATCH) {
            if stopped(&root2) {
                break;
            }
            let docs: Vec<(String, String, String, Vec<String>)> = chunk.iter().map(|&i| {
                let m = &c.meta[i];
                (m.path.clone(), m.title.clone(), m.excerpt.clone(), m.tags.clone())
            }).collect();
            let (tags, titles, note) = super::field_tools::ai_suggestions(&st2, docs, vocab.clone()).await;
            bump(&root2, &|p| {
                p.tags.extend(tags.iter().cloned());
                p.titles.extend(titles.iter().cloned());
                p.done += 1;
                p.calls += 1;
                if let Some(n) = &note {
                    p.error = Some(n.clone());
                }
            });
        }
        for chunk in stale_list.chunks(STALE_BATCH) {
            if stopped(&root2) {
                break;
            }
            let items: Vec<String> = chunk
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let p = s["path"].as_str().unwrap_or("");
                    let mine = std::fs::read_to_string(root2.join(p)).map(|t| body_of(&t).chars().take(700).collect::<String>()).unwrap_or_default();
                    let newer = s["newer"].as_str().and_then(|q| std::fs::read_to_string(root2.join(q)).ok()).map(|t| body_of(&t).chars().take(500).collect::<String>()).unwrap_or_default();
                    format!("{}. path={p}\n规则怀疑：{}\n原文：{mine}\n{}", i + 1, s["reason"].as_str().unwrap_or(""), if newer.is_empty() { String::new() } else { format!("更新的同主题文档：{newer}") })
                })
                .collect();
            let system = "判断用户知识库里这几篇是否真的过时。今天是 TODAY。对每篇：outdated=true 表示其中有已经不成立或需要更新的内容；\
                what 用一句中文说清哪里过时（不过时就说明为什么仍然成立）。只输出 JSON：{\"items\":[{\"path\":\"…\",\"outdated\":true,\"what\":\"…\"}]}"
                .replace("TODAY", &today());
            let out = litmus_mcp::agent::complete(&cfg, &system, &items.join("\n\n")).await;
            let verified: Vec<Value> = match out.ok().and_then(|t| super::field_tools::json_in(&t)) {
                Some(v) => v.get("items").and_then(|x| x.as_array()).cloned().unwrap_or_default().into_iter().filter(|it| chunk.iter().any(|s| s["path"] == it["path"])).collect(),
                None => Vec::new(),
            };
            bump(&root2, &|p| {
                p.verified.extend(verified.iter().cloned());
                p.done += 1;
                p.calls += 1;
            });
        }
        let snapshot = {
            let mut ps = patrols().lock().unwrap();
            let p = ps.get_mut(&root2).unwrap();
            p.running = false;
            p.finished = now_secs();
            p.clone()
        };
        let _ = std::fs::write(patrol_file(&root2), serde_json::to_vec(&snapshot).unwrap_or_default());
    });
    Json(json!({ "started": true, "calls": calls })).into_response()
}

fn scope_label(s: &Scope) -> String {
    let mut v = Vec::new();
    if let Some(r) = &s.region {
        v.push(format!("区域「{r}」"));
    }
    match (&s.from, &s.to) {
        (Some(a), Some(b)) => v.push(format!("{a} 到 {b}")),
        (Some(a), None) => v.push(format!("{a} 以后")),
        (None, Some(b)) => v.push(format!("{b} 以前")),
        _ => {}
    }
    if let Some(f) = &s.folder {
        v.push(f.trim_start_matches("Notes/").to_string());
    }
    v.join(" · ")
}

/// 每篇被多少篇链到（真实写下的链接）。
pub fn inbound_counts(c: &Cached) -> Vec<usize> {
    let mut v = vec![0usize; c.meta.len()];
    if let Ok(body) = serde_json::from_slice::<Value>(&c.body) {
        for l in body.get("links").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
            if let Some(j) = l.get(1).and_then(|x| x.as_u64()) {
                if let Some(x) = v.get_mut(j as usize) {
                    *x += 1;
                }
            }
        }
    }
    v
}

/// 给 /field/organize 用：在范围内算碎片与过时，带总数。
pub fn plan_extras(c: &Cached, scope: &Scope, root: &std::path::Path) -> Value {
    let keep = scope.keep(c);
    let inbound = inbound_counts(c);
    let (frag, nf) = fragments(c, &keep, 60);
    let (st, ns) = stale(c, &keep, &inbound, 200);
    let (cf, nc) = conflicts(c, &keep, &conflict_ignored(root), 100);
    let in_scope = keep.iter().filter(|&&k| k).count();
    let names_note = if scope.is_all() { "全库".to_string() } else { scope_label(scope) };
    json!({ "fragments": frag, "fragments_total": nf, "stale": st, "stale_total": ns, "conflicts": cf, "conflicts_total": nc, "in_scope": in_scope, "scope_label": names_note })
}

/// 范围选择器要用的：区域（自动名 → 显示名，篇数）与最早最晚日期。
pub fn scope_options(c: &Cached, root: &std::path::Path) -> Value {
    let names = load_region_names(root);
    let mut count = vec![0usize; c.field.region_names.len()];
    for &r in &c.field.region {
        count[r] += 1;
    }
    let regions: Vec<Value> = c.field.region_names.iter().zip(count).map(|(a, n)| json!({ "auto": a, "name": names.get(a).cloned().unwrap_or_else(|| a.clone()), "count": n })).collect();
    let (mut lo, mut hi) = (String::from("9999"), String::new());
    for m in &c.meta {
        if m.date < lo {
            lo = m.date.clone();
        }
        if m.date > hi {
            hi = m.date.clone();
        }
    }
    json!({ "regions": regions, "from": lo, "to": hi })
}

#[cfg(test)]
mod fact_tests {
    use super::*;

    #[test]
    fn facts_come_from_key_value_lines_and_two_column_tables() {
        let md = "---\nstatus: draft\n---\n# 部署\n\n- **端口**：7700\n- 服务器：172.96.253.73\n- 日期：2026-01-01\n这一句里有：冒号但很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长\n\n| 项 | 值 |\n|---|---|\n| 版本 | 0.19 |\n\n```\n密码：不算\n```\n";
        let f = facts_of(md);
        let kv: Vec<(String, String)> = f.iter().map(|x| (x.key.clone(), x.v.clone())).collect();
        assert!(kv.contains(&("端口".into(), "7700".into())), "{kv:?}");
        assert!(kv.contains(&("服务器".into(), "172.96.253.73".into())), "{kv:?}");
        assert!(kv.contains(&("版本".into(), "0.19".into())), "{kv:?}");
        assert!(!kv.iter().any(|(k, _)| k == "日期" || k == "status" || k == "密码" || k == "项"), "{kv:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_found_in_all_shapes() {
        assert_eq!(dates_in("预计 2025-03 上线"), vec!["2025-03"]);
        assert_eq!(dates_in("截止 2025/3/15，目标 2026年4月"), vec!["2025-03-15", "2026-04"]);
        assert_eq!(dates_in("2025年3月15日发布"), vec!["2025-03-15"]);
        assert!(dates_in("版本 12025-01 不是日期").is_empty());
        assert!(dates_in("2025-13 也不是").is_empty());
    }

    #[test]
    fn signals_skip_frontmatter_and_title() {
        let (chars, todo, plans) = signals("---\ncreated: 2024-01-01\n---\n# 标题很长很长\n\n计划 2024-06 上线。TODO 写测试");
        assert!(chars < 30, "{chars}");
        assert!(todo);
        assert_eq!(plans, vec!["2024-06"]);
    }
}
