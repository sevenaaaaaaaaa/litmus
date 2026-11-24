//! kernel-soul: 灵魂卡（Markdown 六区块 + 行尾块锚点）的解析、校验、diff 与锚点重挂。
//!
//! 三条不变量（docs/spec/r2/11-soul-model.md）——这是这个 crate 存在的全部理由：
//! 1. 锚点唯一：重复即错误，并指出另一处行号；
//! 2. 锚点属于「这一条判断」，不属于「这一行字符串」：改一个字、挪一段位置，锚点不变；
//! 3. 新条目才铸新号，编号永不复用：删掉 voice-4 之后再新增，拿到的是 voice-5。
//!
//! 纯逻辑、零 IO：server 直接调用，也可以进 wasm 目标。参考实现是
//! docs/spec/r2/reference/p1-01-soul/soul.mjs（Node），这里逐条对齐它的 18 条测试。
//!
//! 出处（provenance）在 `sources` 子模块：按锚点键的旁挂表，不改卡本身的格式。

pub mod sources;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 区块元数据。「我的读者 / 作品」同时接受 reader / work 两个前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SectionMeta {
    pub name: &'static str,
    pub key: &'static str,
    pub prefix: &'static str,
    pub prefixes: &'static [&'static str],
}

pub const SECTIONS: &[SectionMeta] = &[
    SectionMeta { name: "我相信", key: "belief", prefix: "belief", prefixes: &["belief"] },
    SectionMeta { name: "我如何判断", key: "criteria", prefix: "criteria", prefixes: &["criteria"] },
    SectionMeta { name: "我拒绝", key: "refuse", prefix: "refuse", prefixes: &["refuse"] },
    SectionMeta { name: "我的声音", key: "voice", prefix: "voice", prefixes: &["voice"] },
    SectionMeta { name: "我在追的问题", key: "question", prefix: "question", prefixes: &["question"] },
    SectionMeta { name: "我的读者 / 作品", key: "reader", prefix: "reader", prefixes: &["reader", "work"] },
    SectionMeta { name: "我的读者", key: "reader", prefix: "reader", prefixes: &["reader"] },
    SectionMeta { name: "我的作品", key: "work", prefix: "work", prefixes: &["work"] },
];

/// 按标题文字找区块。未知区块返回 None（调用方要报 unknown-section）。
pub fn section_of(name: &str) -> Option<&'static SectionMeta> {
    SECTIONS.iter().find(|s| s.name == name)
}

/// 一条可读的解析/校验问题。line 是 1 起的行号，结构性错误没有行号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

impl Issue {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into(), line: None }
    }

    pub fn at(code: &str, line: usize, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into(), line: Some(line) }
    }
}

/// 一条判断。anchor 为 None 表示作者漏写了块锚点（解析会报 missing-anchor）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub anchor: Option<String>,
    pub text: String,
    pub line: usize,
}

/// 一个区块。未知标题的区块 key/prefix 为 None，条目仍然保留（diff 要看得见）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub name: String,
    pub key: Option<String>,
    pub prefix: Option<String>,
    pub prefixes: Vec<String>,
    pub items: Vec<Item>,
}

/// 解析后的灵魂卡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoulDoc {
    pub frontmatter: BTreeMap<String, String>,
    pub sections: Vec<Section>,
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
    /// confirmed: false 的卡就是临时灵魂卡：AI 输出必须带「未确认」。
    pub temporary: bool,
    pub version: Option<String>,
    pub anchor_count: usize,
    pub item_count: usize,
}

impl SoulDoc {
    /// 全部锚点，按文档顺序。
    pub fn anchors(&self) -> Vec<String> {
        all_anchors(self)
    }

    /// 锚点 → (句子, 区块名)：给「读了哪几条」和 diff 用。
    pub fn index(&self) -> BTreeMap<String, (String, String)> {
        let mut m = BTreeMap::new();
        for s in &self.sections {
            for i in &s.items {
                if let Some(a) = &i.anchor {
                    m.insert(a.clone(), (i.text.clone(), s.name.clone()));
                }
            }
        }
        m
    }
}

const ANCHOR_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789-_";

/// 块锚点合规：3–48 个字符，只用 a-z 0-9 - _，必须带前缀分隔符，且不以 - 起止。
pub fn is_anchor(s: &str) -> bool {
    let n = s.chars().count();
    if !(3..=48).contains(&n) {
        return false;
    }
    if !s.chars().all(|c| ANCHOR_CHARS.contains(c)) {
        return false;
    }
    if !s.contains('-') {
        return false;
    }
    !(s.starts_with('-') || s.ends_with('-'))
}

/// 拆出「正文 + 行尾 ^锚点」。没有锚点、或锚点里带空白（说明作者写的是正文里的 ^），
/// 一律当作「这一条没有锚点」——报错比静默吞掉好。
pub fn split_anchor(raw: &str) -> (String, Option<String>) {
    let Some(i) = raw.rfind('^') else {
        return (raw.trim().to_string(), None);
    };
    let anchor = raw[i + 1..].trim();
    let text = raw[..i].trim_end();
    if anchor.is_empty() {
        return (raw.trim().to_string(), None);
    }
    if anchor.chars().any(|c| c == ' ' || c == '\t' || c == '\u{3000}') {
        return (raw.trim().to_string(), None);
    }
    (text.to_string(), Some(anchor.to_string()))
}

fn strip_fence(lines: &[&str]) -> (BTreeMap<String, String>, usize) {
    let mut fm = BTreeMap::new();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return (fm, 0);
    }
    let end = (1..lines.len()).find(|&i| lines[i].trim() == "---");
    let Some(end) = end else {
        return (fm, 0);
    };
    for l in &lines[1..end] {
        if let Some(i) = l.find(':') {
            fm.insert(l[..i].trim().to_string(), l[i + 1..].trim().to_string());
        }
    }
    (fm, end + 1)
}

/// 解析灵魂卡。错误与提醒都写进 errors / warnings，不 panic、不中断——
/// 一张坏卡要能整张显示出来，用户才知道改哪一行。
pub fn parse(text: &str) -> SoulDoc {
    let lines: Vec<&str> = text.split('\n').collect();
    let (frontmatter, body_start) = strip_fence(&lines);
    let mut errors: Vec<Issue> = Vec::new();
    let mut warnings: Vec<Issue> = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<usize> = None;

    for (i, line) in lines.iter().enumerate().skip(body_start) {
        let lineno = i + 1;
        if let Some(rest) = line.strip_prefix("## ") {
            let name = rest.trim().to_string();
            match section_of(&name) {
                Some(meta) => sections.push(Section {
                    name,
                    key: Some(meta.key.to_string()),
                    prefix: Some(meta.prefix.to_string()),
                    prefixes: meta.prefixes.iter().map(|p| p.to_string()).collect(),
                    items: Vec::new(),
                }),
                None => {
                    errors.push(Issue::at("unknown-section", lineno, format!("未知区块「{name}」")));
                    sections.push(Section {
                        name,
                        key: None,
                        prefix: None,
                        prefixes: Vec::new(),
                        items: Vec::new(),
                    });
                }
            }
            current = Some(sections.len() - 1);
            continue;
        }
        let Some(ci) = current else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("- ") {
            warnings.push(Issue::at(
                "non-item-line",
                lineno,
                "区块里有一行不是项目符号，不会被灵魂卡读到",
            ));
            continue;
        }
        let (item_text, anchor) = split_anchor(&trimmed[2..]);
        match &anchor {
            None => errors.push(Issue::at(
                "missing-anchor",
                lineno,
                format!("这一条没有块锚点：{}", item_text.chars().take(24).collect::<String>()),
            )),
            Some(a) if !is_anchor(a) => {
                errors.push(Issue::at("bad-anchor", lineno, format!("块锚点不合规：^{a}")))
            }
            Some(_) => {}
        }
        sections[ci].items.push(Item { anchor, text: item_text, line: lineno });
    }

    let mut doc = SoulDoc {
        frontmatter,
        sections,
        errors,
        warnings,
        temporary: false,
        version: None,
        anchor_count: 0,
        item_count: 0,
    };
    validate(&mut doc);
    doc
}

/// 校验：唯一性、前缀一致性、空卡与薄卡。解析后自动调用，也可以单独调用。
pub fn validate(doc: &mut SoulDoc) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut new_errors: Vec<Issue> = Vec::new();
    let mut new_warnings: Vec<Issue> = Vec::new();
    let mut filled = 0usize;
    let mut item_count = 0usize;

    for s in &doc.sections {
        item_count += s.items.len();
        if s.key.is_none() {
            continue;
        }
        if !s.items.is_empty() {
            filled += 1;
        }
        for item in &s.items {
            let Some(anchor) = &item.anchor else { continue };
            if let Some(prev) = seen.get(anchor) {
                new_errors.push(Issue::at(
                    "duplicate-anchor",
                    item.line,
                    format!("块锚点重复：^{anchor}（另一处在第 {prev} 行）"),
                ));
            } else {
                seen.insert(anchor.clone(), item.line);
            }
            let pfx = &anchor[..anchor.rfind('-').unwrap_or(0)];
            if !s.prefixes.iter().any(|p| p == pfx) {
                new_warnings.push(Issue::at(
                    "anchor-prefix",
                    item.line,
                    format!(
                        "锚点 ^{anchor} 与区块前缀 {} 不一致（不致命，但会让 grep 变难）",
                        s.prefixes.join("/")
                    ),
                ));
            }
        }
    }

    if filled == 0 {
        new_errors.push(Issue::new("empty-card", "这张卡没有任何内容"));
    } else if filled < 4 {
        new_warnings.push(Issue::new(
            "thin-card",
            format!("只有 {filled} 个区块有内容，至少 4 个才算可用"),
        ));
    }

    doc.errors.extend(new_errors);
    doc.warnings.extend(new_warnings);
    doc.temporary = doc.frontmatter.get("confirmed").map(|v| v == "false").unwrap_or(false);
    doc.version = doc.frontmatter.get("version").cloned();
    doc.anchor_count = seen.len();
    doc.item_count = item_count;
}

/// 全部锚点，按文档顺序。
pub fn all_anchors(doc: &SoulDoc) -> Vec<String> {
    let mut out = Vec::new();
    for s in &doc.sections {
        for i in &s.items {
            if let Some(a) = &i.anchor {
                out.push(a.clone());
            }
        }
    }
    out
}

/// diff 的一行。added/removed 用 text，edited 用 from/to。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffEntry {
    pub anchor: String,
    pub section: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoulDiff {
    pub added: Vec<DiffEntry>,
    pub removed: Vec<DiffEntry>,
    pub edited: Vec<DiffEntry>,
    pub unchanged: usize,
}

/// 按锚点比对两张卡：只报改动的锚点，锚点顺序不影响结果。
pub fn diff(old: &SoulDoc, new: &SoulDoc) -> SoulDiff {
    let a = old.index();
    let b = new.index();
    let mut added = Vec::new();
    let mut edited = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = 0usize;

    for (anchor, (text, section)) in &b {
        match a.get(anchor) {
            None => added.push(DiffEntry {
                anchor: anchor.clone(),
                section: section.clone(),
                text: Some(text.clone()),
                from: None,
                to: None,
            }),
            Some((old_text, _)) if old_text != text => edited.push(DiffEntry {
                anchor: anchor.clone(),
                section: section.clone(),
                text: None,
                from: Some(old_text.clone()),
                to: Some(text.clone()),
            }),
            Some(_) => unchanged += 1,
        }
    }
    for (anchor, (text, section)) in &a {
        if !b.contains_key(anchor) {
            removed.push(DiffEntry {
                anchor: anchor.clone(),
                section: section.clone(),
                text: Some(text.clone()),
                from: None,
                to: None,
            });
        }
    }

    SoulDiff { added, removed, edited, unchanged }
}

/// 人能读的 diff 文本（CLI 与卡背共用）。
pub fn format_diff(d: &SoulDiff) -> String {
    let mut out: Vec<String> = Vec::new();
    for e in &d.edited {
        out.push(format!(
            "- 改 ^{}：{}  →  {}",
            e.anchor,
            e.from.clone().unwrap_or_default(),
            e.to.clone().unwrap_or_default()
        ));
    }
    for e in &d.added {
        out.push(format!("+ 新 ^{}：{}", e.anchor, e.text.clone().unwrap_or_default()));
    }
    for e in &d.removed {
        out.push(format!("- 删 ^{}：{}", e.anchor, e.text.clone().unwrap_or_default()));
    }
    out.push(format!("（未变 {} 条）", d.unchanged));
    out.join("\n")
}

/// 锚点重挂的账：带过去几条、新铸几条、丢了几条。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reanchor {
    pub text: String,
    pub carried: Vec<String>,
    pub minted: Vec<String>,
    pub deleted: Vec<String>,
}

fn norm_line(line: &str) -> String {
    let t = line.trim();
    match t.strip_prefix("- ") {
        Some(rest) => split_anchor(rest).0,
        None => split_anchor(t).0,
    }
}

fn anchor_of(line: &str) -> Option<String> {
    let t = line.trim();
    let rest = t.strip_prefix("- ").unwrap_or(t);
    split_anchor(rest).1
}

fn lcs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

fn prefixes_of(lines: &[&str]) -> Vec<Option<String>> {
    let mut cur: Option<String> = None;
    lines
        .iter()
        .map(|l| {
            if let Some(rest) = l.strip_prefix("## ") {
                cur = section_of(rest.trim()).map(|m| m.prefix.to_string());
            }
            cur.clone()
        })
        .collect()
}

/// 把锚点跟着「这一条判断」走：用户改文字、挪位置之后重新挂锚点。
///
/// 两步匹配：先按整行相等做 LCS，再在同一区块内按字符二元组相似度（阈值 0.55）
/// 兜住「改了一两个字」的情况。只有真的新写的条目才铸新号，且历史锚点参与取号。
pub fn reanchor(old_text: &str, new_text: &str) -> Reanchor {
    let old_lines: Vec<&str> = old_text.split('\n').collect();
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    let old_norm: Vec<String> = old_lines.iter().map(|l| norm_line(l)).collect();
    let new_norm: Vec<String> = new_lines.iter().map(|l| norm_line(l)).collect();
    let old_anchor: Vec<Option<String>> = old_lines.iter().map(|l| anchor_of(l)).collect();

    // 旧锚点按「去掉锚点后的文本」建索引；同文多条时取第一条。
    let mut by_text: HashMap<String, String> = HashMap::new();
    for (i, t) in old_norm.iter().enumerate() {
        if let Some(a) = &old_anchor[i] {
            by_text.entry(t.clone()).or_insert_with(|| a.clone());
        }
    }

    let mut matched: HashMap<usize, usize> = HashMap::new();
    for (oi, ni) in lcs(&old_norm, &new_norm) {
        matched.insert(ni, oi);
    }

    let mut used: BTreeSet<String> = BTreeSet::new();
    for l in &new_lines {
        if let Some(a) = anchor_of(l) {
            used.insert(a);
        }
    }

    let new_prefix = prefixes_of(&new_lines);
    let old_prefix = prefixes_of(&old_lines);

    let prefix_at = |idx: usize| -> Option<String> {
        let mut p: Option<String> = None;
        for l in new_lines.iter().take(idx + 1) {
            if let Some(rest) = l.strip_prefix("## ") {
                p = section_of(rest.trim()).map(|m| m.prefix.to_string());
            }
        }
        p
    };

    // 历史锚点参与取号：删掉 voice-4 之后再新增，应该是 voice-5。
    let mut historical: BTreeSet<String> = used.clone();
    for a in old_anchor.iter().flatten() {
        historical.insert(a.clone());
    }
    let mut max_num: HashMap<String, usize> = HashMap::new();
    for a in &historical {
        if let Some((prefix, num)) = a.rsplit_once('-') {
            if let Ok(n) = num.parse::<usize>() {
                let e = max_num.entry(prefix.to_string()).or_insert(0);
                if n > *e {
                    *e = n;
                }
            }
        }
    }

    // 相似度兜底：整行不同、但仍是同一条判断。
    let taken: BTreeSet<usize> = matched.values().copied().collect();
    let mut spare: Vec<(usize, Option<String>, String)> = old_lines
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            old_anchor[*i].is_some()
                && !taken.contains(i)
                && !used.contains(old_anchor[*i].as_ref().unwrap())
        })
        .map(|(i, _)| (i, old_prefix[i].clone(), old_norm[i].clone()))
        .collect();

    for ni in 0..new_lines.len() {
        if matched.contains_key(&ni) {
            continue;
        }
        let t = new_lines[ni].trim();
        let Some(rest) = t.strip_prefix("- ") else { continue };
        let (text, anchor) = split_anchor(rest);
        if anchor.is_some() || text.is_empty() {
            continue;
        }
        let mut best: Option<usize> = None;
        let mut best_score = 0.0f64;
        for (k, (_, prefix, old_text)) in spare.iter().enumerate() {
            if *prefix != new_prefix[ni] {
                continue;
            }
            let s = dice(&text, old_text);
            if s > best_score {
                best_score = s;
                best = Some(k);
            }
        }
        if let Some(k) = best {
            if best_score >= 0.55 {
                let oi = spare[k].0;
                matched.insert(ni, oi);
                spare.remove(k);
            }
        }
    }

    let mut out_lines: Vec<String> = Vec::new();
    let mut carried: Vec<String> = Vec::new();
    let mut minted: Vec<String> = Vec::new();

    for (idx, line) in new_lines.iter().enumerate() {
        let t = line.trim();
        if !t.starts_with("- ") {
            out_lines.push((*line).to_string());
            continue;
        }
        let (text, anchor) = split_anchor(&t[2..]);
        if anchor.is_some() {
            out_lines.push((*line).to_string());
            continue;
        }

        let mut carried_anchor: Option<String> = None;
        if let Some(oi) = matched.get(&idx) {
            if let Some(a) = &old_anchor[*oi] {
                if !used.contains(a) {
                    carried_anchor = Some(a.clone());
                }
            }
        }
        if carried_anchor.is_none() {
            if let Some(hit) = by_text.get(&text) {
                if !used.contains(hit) {
                    carried_anchor = Some(hit.clone());
                }
            }
        }

        match carried_anchor {
            Some(a) => {
                used.insert(a.clone());
                carried.push(a.clone());
                out_lines.push(format!("{} ^{}", line.trim_end(), a));
            }
            None => {
                let prefix = prefix_at(idx).unwrap_or_else(|| "item".to_string());
                let next = max_num.entry(prefix.clone()).or_insert(0);
                *next += 1;
                let id = format!("{prefix}-{next}");
                // 防撞号靠上面那个 max_num 计数器：它在铸号前从 historical 播种一次、
                // 每铸一个就自增，所以同一轮里新增两条会拿到 5 和 6。Node 版每次重扫
                // 冻结的 historical，第二轮仍算出同一个 max+1，于是铸出两个 voice-5
                // （tests/soul.rs 的 two_new_items_in_one_round_never_share_an_anchor 钉住这点）。
                used.insert(id.clone());
                minted.push(id.clone());
                out_lines.push(format!("{} ^{}", line.trim_end(), id));
            }
        }
    }

    let text = out_lines.join("\n");
    let new_doc = parse(&text);
    let new_set: BTreeSet<String> = all_anchors(&new_doc).into_iter().collect();
    let old_set: BTreeSet<String> = old_anchor.iter().flatten().cloned().collect();
    let deleted: Vec<String> = old_set.difference(&new_set).cloned().collect();

    Reanchor { text, carried, minted, deleted }
}

/// 字符二元组 Dice 相似度（0–1）。用来判断「改了一两个字，还是同一条判断」。
pub fn dice(a: &str, b: &str) -> f64 {
    fn bigrams(s: &str) -> Vec<String> {
        let clean: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..clean.len().saturating_sub(1))
            .map(|i| clean[i..i + 2].iter().collect())
            .collect()
    }
    let a = bigrams(a);
    let b = bigrams(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut pool: HashMap<String, usize> = HashMap::new();
    for g in &a {
        *pool.entry(g.clone()).or_insert(0) += 1;
    }
    let mut hit = 0usize;
    for g in &b {
        if let Some(c) = pool.get_mut(g) {
            if *c > 0 {
                hit += 1;
                *c -= 1;
            }
        }
    }
    (2.0 * hit as f64) / ((a.len() + b.len()) as f64)
}

/// 一条被还原出来的引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRef {
    pub anchor: String,
    pub text: String,
    pub section: String,
}

/// 「读了灵魂卡的哪几条」：把锚点还原成句子；未知锚点显式列出，绝不静默丢。
pub fn resolve_refs(doc: &SoulDoc, ids: &[&str]) -> (Vec<ResolvedRef>, Vec<String>) {
    let index = doc.index();
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match index.get(*id) {
            Some((text, section)) => found.push(ResolvedRef {
                anchor: (*id).to_string(),
                text: text.clone(),
                section: section.clone(),
            }),
            None => missing.push((*id).to_string()),
        }
    }
    (found, missing)
}
