//! 临时卡的出处（provenance）：按块锚点键，落在卡旁边的旁挂文件里。
//!
//! 为什么是旁挂、不写进卡的 Markdown：
//! 1. 出处是「每一条判断」一份，而卡的 frontmatter 是整卡一份的字符串表，
//!    装不下逐条出处——硬塞就要改卡的格式；
//! 2. 改格式要动 parse/validate/reanchor、18 条对齐测试和两个逐字节对齐的 fixture；
//! 3. 锚点稳定（本 crate 的第一条不变量），所以「按锚点键」的旁挂天然跟着卡走：
//!    P1-10b 的升级只保证锚点是超集，旁挂自动仍然对得上。
//!
//! 三条判定，全部是「显式报，不静默补」：
//! - 卡里有锚点、旁挂没有   → missing-source（不知道这条判断最初从哪句话来）；
//! - 旁挂有锚点、卡里没有   → dangling-source（悬空，不静默删）；
//! - quote 不再覆盖条目正文 → unquoted-source（逐字可核失败）。
//!
//! 纯逻辑、零 IO：落盘路径归 fs 层（参考实现是 p1-02 的 sources.mjs，
//! 产品侧随 P1-10b 的端点走 kernel-store）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{Issue, SoulDoc};

/// 旁挂格式版本。结构一变就升这个数，读到别的版本必须显式拒绝，不许猜。
pub const SOURCES_VERSION: u32 = 1;

/// 旁挂落在 vault 里的哪个目录。与 Node 参考实现 `p1-12-provenance/sources.mjs`
/// 的 `SOURCES_DIR` 必须同值——两边指的不是同一个地方，就谈不上逐条对齐。
pub const SOURCES_DIR: &str = ".litmus/soul-sources";

/// 一条出处：从哪个入口来（source）、具体指到哪（ref）、原话（quote）。
///
/// quote 存原话而不是只存 ref，是因为 ref 里的行号会漂、笔记会被改：
/// 只有原话本身能让「这条判断最初是从哪句话来的」在半年后仍然可核。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceEntry {
    pub source: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub quote: String,
}

/// 一张卡的出处表。`anchors` 的键是块锚点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sources {
    pub version: u32,
    /// 卡在 vault 里的相对路径，如 `Notes/灵魂卡.md`。出处跟着卡走，所以要记它。
    pub card: String,
    pub anchors: BTreeMap<String, SourceEntry>,
}

impl Sources {
    pub fn new(card: impl Into<String>) -> Self {
        Self { version: SOURCES_VERSION, card: card.into(), anchors: BTreeMap::new() }
    }
}

/// 读旁挂。JSON 坏或版本不认识都返回一条可读 Issue，不返回半个对象。
pub fn parse_sources(json: &str) -> Result<Sources, Issue> {
    let s: Sources = serde_json::from_str(json)
        .map_err(|e| Issue::new("bad-sources-json", format!("旁挂不是合法 JSON：{e}")))?;
    if s.version != SOURCES_VERSION {
        return Err(Issue::new(
            "unsupported-sources-version",
            format!("旁挂版本 {} 不认识（这里只认 {}）", s.version, SOURCES_VERSION),
        ));
    }
    Ok(s)
}

/// 写旁挂。键是按锚点排序的 BTreeMap，所以同一份内容两次序列化逐字节相同
/// （写在版本库里不会因为键顺序抖出假 diff）。
pub fn sources_to_json(sources: &Sources) -> String {
    let mut out = serde_json::to_string_pretty(sources).unwrap_or_default();
    out.push('\n');
    out
}

/// 把卡和旁挂对一遍。空 Vec 表示「每条判断都答得出出处，且没有悬空条目」。
pub fn check_sources(doc: &SoulDoc, sources: &Sources) -> Vec<Issue> {
    let mut found: Vec<Issue> = Vec::new();
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();

    for section in &doc.sections {
        for item in &section.items {
            let Some(anchor) = item.anchor.as_deref() else { continue };
            seen.insert(anchor, ());
            match sources.anchors.get(anchor) {
                None => found.push(Issue::at(
                    "missing-source",
                    item.line,
                    format!("{anchor} 没有出处：答不出这条判断最初是从哪句话来的"),
                )),
                Some(entry) => {
                    if !squash(&entry.quote).contains(&squash(&item.text)) {
                        found.push(Issue::at(
                            "unquoted-source",
                            item.line,
                            format!("{anchor} 的 quote 不再覆盖条目正文（逐字可核失败）"),
                        ));
                    }
                }
            }
        }
    }

    for anchor in sources.anchors.keys() {
        if !seen.contains_key(anchor.as_str()) {
            found.push(Issue::new(
                "dangling-source",
                format!("{anchor} 在旁挂里有、卡里没有：悬空出处不静默删"),
            ));
        }
    }

    found
}

/// 与 p1-11 的 `squash` 逐字一致：只去掉空格、\t、\n、\r 和全角空格 U+3000。
/// 不用 `char::is_whitespace`——它还会吃掉 \x0b、\x0c、NBSP，那不是同一套规则。
fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{3000}'))
        .collect()
}
