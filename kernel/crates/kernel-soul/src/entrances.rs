//! 灵魂卡三入口（P1-02 的 Rust 版）：访谈 / 提炼 / 粘贴。
//!
//! 三条铁律（与 reference/p1-02-soul-entrances/entrances.mjs 一一对应）：
//! 1. 候选必须逐字有出处：候选文字必须是输入里的原句子串，编造直接报错；
//! 2. 三个入口共用同一份确认与落盘，只在「候选从哪来」上不同；
//! 3. 跳过也能用：confirmed:false 的临时卡，每条候选带出处。
//!
//! 这一层不生成句子，只挑句子。生成会立刻变成「agent 替用户写卡」。
//! 纯逻辑、零 IO；落盘走 server 的 card_put（一条 PUT，三个入口共用）。

use crate::sources::{squash, SourceEntry};
use crate::Issue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 六个区块，顺序与参考实现 BLOCKS 一致（不是 SECTIONS：那张表按标题列，含别名）。
pub struct Block {
    pub key: &'static str,
    pub name: &'static str,
    pub prefix: &'static str,
}

pub const BLOCKS: &[Block] = &[
    Block { key: "belief", name: "我相信", prefix: "belief" },
    Block { key: "criteria", name: "我如何判断", prefix: "criteria" },
    Block { key: "refuse", name: "我拒绝", prefix: "refuse" },
    Block { key: "voice", name: "我的声音", prefix: "voice" },
    Block { key: "question", name: "我在追的问题", prefix: "question" },
    Block { key: "reader", name: "我的读者 / 作品", prefix: "reader" },
];

/// 规则文件版本。结构一变就升这个数，读到别的版本必须显式拒绝，不许猜。
pub const RULES_VERSION: u32 = 1;

/// 一条候选：正文 + 它从哪来（source / ref / quote）。
///
/// quote 存原话，ref 存指针：ref 里的行号会漂，只有原话让「这条最初从哪句话来」半年后仍可核。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub block: String,
    pub text: String,
    pub source: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub quote: String,
}

/// 提炼线索词。按区块分组，从 soul-rules.json 读，不硬编码。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rules {
    pub version: u32,
    pub cues: BTreeMap<String, Vec<String>>,
}

/// 编译进来的默认规则。库里有 .litmus/soul-rules.json 时用库里的（见 server）。
pub fn default_rules() -> Rules {
    serde_json::from_str(include_str!("../soul-rules.json")).expect("soul-rules.json 必须是合法 JSON")
}

pub fn validate_rules(rules: &Rules) -> Result<(), Issue> {
    if rules.version != RULES_VERSION {
        return Err(Issue::new(
            "rules-version",
            format!("规则版本 {} 不认识（这里只认 {RULES_VERSION}）", rules.version),
        ));
    }
    for meta in BLOCKS {
        match rules.cues.get(meta.key) {
            Some(list) if !list.is_empty() => {}
            _ => {
                return Err(Issue::new(
                    "rules-missing-block",
                    format!("规则缺区块 {}：这一块永远挑不出句子", meta.key),
                ))
            }
        }
    }
    for key in rules.cues.keys() {
        if block_meta(key).is_none() {
            return Err(Issue::new("rules-unknown-block", format!("规则里有不认识的区块：{key}")));
        }
    }
    Ok(())
}

fn block_meta(key: &str) -> Option<&'static Block> {
    BLOCKS.iter().find(|m| m.key == key)
}

/// 切句：中文句号 / 问号 / 叹号、半角 ! ? 与换行都算断句。不做分词，不猜语义。
pub fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n') {
            let t = buf.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
            buf.clear();
        } else {
            buf.push(ch);
        }
    }
    let t = buf.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

/// 标题、引用、表格、列表、围栏、分隔线、网址、太短太长的一律不进候选。
fn looks_like_noise(s: &str) -> bool {
    let n = s.chars().count();
    s.starts_with('#')
        || s.starts_with('>')
        || s.starts_with('|')
        || s.starts_with("- ")
        || s.starts_with("\u{60}\u{60}\u{60}")
        || s.starts_with("---")
        || s.contains("http")
        || n < 4
        || n > 80
}

/// 「逐字可核」唯一的那条判据：quote 必须覆盖 text（只抹空白）。
pub fn assert_grounded(c: &Candidate) -> Result<(), Issue> {
    if c.quote.trim().is_empty() {
        return Err(Issue::new("candidate-no-source", format!("候选没有出处：{}", c.text)));
    }
    if !squash(&c.quote).contains(&squash(&c.text)) {
        return Err(Issue::new("candidate-not-verbatim", format!("候选不是原句：{}", c.text)));
    }
    Ok(())
}

/// 入口一：访谈。用户的每句话本身就是候选。
pub fn from_interview(answers: &[(String, String)]) -> Result<Vec<Candidate>, Issue> {
    let mut out = Vec::new();
    for (block, answer) in answers {
        let meta = block_meta(block)
            .ok_or_else(|| Issue::new("unknown-block", format!("未知区块：{block}")))?;
        for s in sentences(answer) {
            if looks_like_noise(&s) {
                continue;
            }
            out.push(Candidate {
                block: meta.key.to_string(),
                text: s.clone(),
                source: "interview".to_string(),
                reference: format!("answer:{}", meta.key),
                quote: s,
            });
        }
    }
    Ok(out)
}

/// 一篇要被提炼的笔记。
pub struct NoteFile<'a> {
    pub path: &'a str,
    pub text: &'a str,
}

/// 一句话最像哪个区块：线索词命中数最高的那个；打平取靠前的区块。
fn best_block(s: &str, rules: &Rules) -> Option<&'static str> {
    let mut best: Option<&'static str> = None;
    let mut score = 0usize;
    for meta in BLOCKS {
        let empty: &[String] = &[];
        let cues = rules.cues.get(meta.key).map(|v| v.as_slice()).unwrap_or(empty);
        let hit = cues.iter().filter(|c| s.contains(c.as_str())).count();
        if hit > score {
            score = hit;
            best = Some(meta.key);
        }
    }
    best
}

/// 入口二：提炼。按线索词挑句子，带路径与行号；每区块最多 per_block 条。
pub fn from_notes(files: &[NoteFile<'_>], rules: &Rules, per_block: usize) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in files {
        let mut in_fence = false;
        for (i, line) in f.text.split('\n').enumerate() {
            let t = line.trim();
            if t.starts_with("\u{60}\u{60}\u{60}") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence || looks_like_noise(t) {
                continue;
            }
            for s in sentences(line) {
                if looks_like_noise(&s) {
                    continue;
                }
                let Some(block) = best_block(&s, rules) else { continue };
                let n = counts.get(block).copied().unwrap_or(0);
                if n >= per_block {
                    continue;
                }
                counts.insert(block, n + 1);
                out.push(Candidate {
                    block: block.to_string(),
                    text: s.clone(),
                    source: "note".to_string(),
                    reference: format!("{}:{}", f.path, i + 1),
                    quote: s,
                });
            }
        }
    }
    out
}

/// 入口三：粘贴。自我介绍 / 旧文章 / 主页简介切句，默认落「我的声音」。
pub fn from_paste(text: &str, block: &str) -> Result<Vec<Candidate>, Issue> {
    let meta = block_meta(block)
        .ok_or_else(|| Issue::new("unknown-block", format!("未知区块：{block}")))?;
    Ok(sentences(text)
        .into_iter()
        .filter(|s| !looks_like_noise(s))
        .map(|s| Candidate {
            block: meta.key.to_string(),
            text: s.clone(),
            source: "paste".to_string(),
            reference: format!("paste:{}", meta.key),
            quote: s,
        })
        .collect())
}

/// 入口四：快捕。剪贴板里的一段文字，不猜语义，只切句；最多 max_items 条。
///
/// 区块按线索词判；一条都没命中就落「我相信」——六个区块里它最宽，
/// 用户升级或以后用 reanchor 挪走都行。快捕不套提炼那套噪声过滤：
/// 这段字是用户自己选中的，宁可留下，也不替他判「这不算一句话」。
pub fn from_capture(text: &str, rules: &Rules, max_items: usize) -> Vec<Candidate> {
    let mut out = Vec::new();
    for s in sentences(text) {
        if out.len() >= max_items {
            break;
        }
        let s = s.trim();
        if s.chars().count() < 2 {
            continue;
        }
        let block = best_block(s, rules).unwrap_or("belief");
        out.push(Candidate {
            block: block.to_string(),
            text: s.to_string(),
            source: "capture".to_string(),
            reference: "capture:quick".to_string(),
            quote: s.to_string(),
        });
    }
    out
}

/// 一条已经过关、等着被写进卡里的判断。
#[derive(Clone, Debug)]
pub struct Kept {
    pub block: String,
    pub text: String,
    pub quote: String,
    pub source: String,
    pub reference: String,
    /// 改写过的条目：正文是用户改的，quote 仍是原话。
    pub edited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Pending,
    Accepted,
    Edited,
    Dropped,
}

#[derive(Clone, Debug)]
pub struct ReviewItem {
    pub candidate: Candidate,
    pub decision: Decision,
    pub edited: Option<String>,
}

/// 三个入口共用的确认状态。UI 的「接受 / 改写 / 删除」就是它。
#[derive(Clone, Debug, Default)]
pub struct Review {
    pub items: Vec<ReviewItem>,
}

impl Review {
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            items: candidates
                .into_iter()
                .map(|c| ReviewItem { candidate: c, decision: Decision::Pending, edited: None })
                .collect(),
        }
    }

    pub fn pending(&self) -> usize {
        self.items.iter().filter(|i| i.decision == Decision::Pending).count()
    }

    pub fn decide(&mut self, index: usize, decision: Decision, edited: Option<&str>) -> Result<(), Issue> {
        let item = self
            .items
            .get_mut(index)
            .ok_or_else(|| Issue::new("review-no-item", format!("没有这一条：{index}")))?;
        match decision {
            Decision::Pending => Err(Issue::new("review-bad-decision", "决定不能是 pending")),
            Decision::Edited => {
                let t = edited.unwrap_or("").trim();
                if t.is_empty() {
                    return Err(Issue::new("review-empty-edit", "改写不能是空的"));
                }
                item.decision = Decision::Edited;
                item.edited = Some(t.to_string());
                Ok(())
            }
            other => {
                item.decision = other;
                item.edited = None;
                Ok(())
            }
        }
    }

    pub fn accept_all(&mut self) {
        for it in &mut self.items {
            if it.decision == Decision::Pending {
                it.decision = Decision::Accepted;
            }
        }
    }

    /// 只留接受与改写的。改写改的是正文，quote 仍然是原话。
    pub fn kept(&self) -> Vec<Kept> {
        self.items
            .iter()
            .filter_map(|it| match it.decision {
                Decision::Accepted => Some(Kept {
                    block: it.candidate.block.clone(),
                    text: it.candidate.text.clone(),
                    quote: it.candidate.quote.clone(),
                    source: it.candidate.source.clone(),
                    reference: it.candidate.reference.clone(),
                    edited: false,
                }),
                Decision::Edited => Some(Kept {
                    block: it.candidate.block.clone(),
                    text: it.edited.clone().unwrap_or_default(),
                    quote: it.candidate.quote.clone(),
                    source: it.candidate.source.clone(),
                    reference: it.candidate.reference.clone(),
                    edited: true,
                }),
                _ => None,
            })
            .collect()
    }
}

/// 组装出来的东西：卡 + 旁挂。还没写盘。
#[derive(Clone, Debug)]
pub struct Draft {
    pub markdown: String,
    pub sources: BTreeMap<String, SourceEntry>,
}

/// 共用落盘格式：六区块、块锚点（前缀-N）、frontmatter。区块顺序按 SECTIONS。
pub fn assemble(items: &[Kept], confirmed: bool, version: u32, date: &str) -> Result<Draft, Issue> {
    let mut by_block: BTreeMap<&'static str, Vec<&Kept>> = BTreeMap::new();
    for it in items {
        let meta = block_meta(&it.block)
            .ok_or_else(|| Issue::new("unknown-block", format!("未知区块：{}", it.block)))?;
        if it.text.trim().is_empty() {
            return Err(Issue::new("empty-item", "有一条判断是空的"));
        }
        by_block.entry(meta.key).or_default().push(it);
    }

    let mut lines: Vec<String> = vec![
        "---".to_string(),
        "type: soul-card".to_string(),
        format!("version: {version}"),
        format!("updated: {date}"),
        "visibility: private".to_string(),
        format!("confirmed: {}", if confirmed { "true" } else { "false" }),
        "---".to_string(),
        String::new(),
    ];
    let mut sources: BTreeMap<String, SourceEntry> = BTreeMap::new();

    for meta in BLOCKS {
        let Some(list) = by_block.get(meta.key) else { continue };
        if list.is_empty() {
            continue;
        }
        lines.push(format!("## {}", meta.name));
        for (i, it) in list.iter().enumerate() {
            let anchor = format!("{}-{}", meta.prefix, i + 1);
            lines.push(format!("- {} ^{}", it.text.trim(), anchor));
            sources.insert(
                anchor,
                SourceEntry {
                    source: it.source.clone(),
                    reference: it.reference.clone(),
                    quote: it.quote.clone(),
                    edited: it.edited,
                },
            );
        }
        lines.push(String::new());
    }

    Ok(Draft { markdown: lines.join("\n"), sources })
}

/// 跳过路径：从旧笔记里每个区块挑一条，生成临时卡（confirmed 永远 false）。
pub fn temporary_card(files: &[NoteFile<'_>], rules: &Rules, date: &str) -> Result<Draft, Issue> {
    let cands = from_notes(files, rules, 1);
    if cands.is_empty() {
        return Err(Issue::new("no-candidates", "这些笔记里没有挑出一句可用的判断"));
    }
    let mut review = Review::new(cands);
    review.accept_all();
    assemble(&review.kept(), false, 0, date)
}
