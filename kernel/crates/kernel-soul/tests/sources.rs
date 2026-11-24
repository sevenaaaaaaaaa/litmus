//! P1-12：出处（provenance）的旁挂表。
//! 用例与 Node 侧 reference/p1-12-provenance/sources.test.mjs 同名同断言——
//! 两边跑同一批输入、同一批判据，才算「这一层进了内核」。

use kernel_soul::sources::{check_sources, parse_sources, sources_to_json, SourceEntry, Sources};
use kernel_soul::{parse, SoulDoc};

const EX: &str = include_str!("fixtures/soul-example.md");

fn items(doc: &SoulDoc) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    for section in &doc.sections {
        for item in &section.items {
            if let Some(anchor) = &item.anchor {
                out.push((anchor.clone(), item.text.clone(), item.line));
            }
        }
    }
    out
}

/// 给每一只锚点配一条「quote 就是正文本身」的出处：最保守的合法旁挂。
fn covering_all(doc: &SoulDoc) -> Sources {
    let mut s = Sources::new("Notes/灵魂卡.md");
    for (anchor, text, _) in items(doc) {
        s.anchors.insert(
            anchor,
            SourceEntry { source: "note".to_string(), reference: "Notes/旧笔记.md:1".to_string(), quote: text, edited: false },
        );
    }
    s
}

fn codes(issues: &[kernel_soul::Issue]) -> Vec<String> {
    issues.iter().map(|i| i.code.clone()).collect()
}

#[test]
fn every_anchor_covered_reports_nothing() {
    let doc = parse(EX);
    let s = covering_all(&doc);
    assert_eq!(s.anchors.len(), 16, "示例卡 16 条判断，旁挂就该有 16 条");
    assert_eq!(check_sources(&doc, &s), vec![], "全覆盖时不该有任何问题");
}

#[test]
fn sources_round_trip_is_byte_stable() {
    let doc = parse(EX);
    let s = covering_all(&doc);
    let once = sources_to_json(&s);
    let back = parse_sources(&once).expect("自己写出来的必须自己能读回去");
    assert_eq!(back, s);
    assert_eq!(sources_to_json(&back), once, "同一份内容两次序列化必须逐字节相同");
    assert_eq!(back.version, 1);
    assert_eq!(back.card, "Notes/灵魂卡.md");
}

#[test]
fn missing_provenance_is_reported_not_filled() {
    let doc = parse(EX);
    let mut s = covering_all(&doc);
    let (anchor, _, line) = items(&doc).into_iter().find(|(a, _, _)| a == "belief-1").expect("fixture 必有 belief-1");
    s.anchors.remove(&anchor);

    let found = check_sources(&doc, &s);
    assert_eq!(codes(&found), vec!["missing-source"], "只该报这一条缺出处");
    assert_eq!(found[0].line, Some(line), "行号要指到那条判断");
    assert!(!s.anchors.contains_key(&anchor), "报缺不等于悄悄补上");
}

#[test]
fn dangling_provenance_is_reported_not_dropped() {
    let doc = parse(EX);
    let mut s = covering_all(&doc);
    s.anchors.insert(
        "ghost-9".to_string(),
        SourceEntry { source: "paste".to_string(), reference: "paste:belief".to_string(), quote: "一句卡里已经没有的话。".to_string(), edited: false },
    );

    let found = check_sources(&doc, &s);
    assert_eq!(codes(&found), vec!["dangling-source"]);
    assert!(s.anchors.contains_key("ghost-9"), "悬空出处不静默删");
}

#[test]
fn quote_that_no_longer_covers_the_item_is_an_error() {
    let doc = parse(EX);
    let mut s = covering_all(&doc);
    s.anchors.insert(
        "belief-1".to_string(),
        SourceEntry { source: "interview".to_string(), reference: "answer:belief".to_string(), quote: "一句和条目毫无关系的话。".to_string(), edited: false },
    );

    let found = check_sources(&doc, &s);
    assert_eq!(codes(&found), vec!["unquoted-source"]);
}

#[test]
fn edited_items_are_not_judged_by_the_verbatim_rule() {
    // 改写过的判断：正文是用户改的，quote 仍指原话。逐字覆盖不适用，但锚点仍然对得上。
    let doc = parse(EX);
    let mut s = covering_all(&doc);
    s.anchors.insert(
        "belief-1".to_string(),
        SourceEntry {
            source: "interview".to_string(),
            reference: "answer:belief".to_string(),
            quote: "用户原来说的那一句。".to_string(),
            edited: true,
        },
    );
    assert_eq!(check_sources(&doc, &s), vec![], "改写过的条目不报 unquoted-source");
}

#[test]
fn whitespace_differences_do_not_break_the_quote_check() {
    let doc = parse(EX);
    let mut s = covering_all(&doc);
    let (_, text, _) = items(&doc).into_iter().find(|(a, _, _)| a == "belief-1").expect("fixture 必有 belief-1");
    // 原话里加了空格、换行和全角空格：位置变了，字没变，仍应算「逐字可核」。
    let spaced = format!("  {} \n\u{3000} ", text.replace('，', "， "));
    s.anchors.insert(
        "belief-1".to_string(),
        SourceEntry { source: "interview".to_string(), reference: "answer:belief".to_string(), quote: spaced, edited: false },
    );

    assert_eq!(check_sources(&doc, &s), vec![], "只差空白不该判成对不上");
}

#[test]
fn new_item_after_upgrade_is_reported_as_missing_source() {
    // 升级只保证锚点是超集：旧旁挂对新条目本来就一无所知。
    // 这里要钉的是「它必须报缺，而不是被旧旁挂冒充成有出处」。
    let doc = parse(EX);
    let s = covering_all(&doc);
    let with_new = strip_anchors_pub(EX).replace(
        "- 样本 3：少形容词，多动词。",
        "- 样本 3：少形容词，多动词。\n- 样本 9：升级时新写进来的一条。",
    );
    let upgraded = kernel_soul::reanchor(EX, &with_new);

    let found = check_sources(&parse(&upgraded.text), &s);
    assert_eq!(codes(&found), vec!["missing-source"], "只该有一条新判断缺出处：{found:?}");
    assert_eq!(found.len(), 1);
}

/// 与 tests/soul.rs 同名的私有 helper 不同：这里只要去掉行尾锚点，保留每行其余内容。
fn strip_anchors_pub(text: &str) -> String {
    text.split('\n')
        .map(|l| match l.rfind('^') {
            Some(i) => l[..i].trim_end().to_string(),
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn unknown_version_is_refused() {
    let err = parse_sources(r#"{"version":2,"card":"Notes/a.md","anchors":{}}"#).expect_err("版本 2 必须拒绝");
    assert_eq!(err.code, "unsupported-sources-version");
}

#[test]
fn bad_json_is_refused() {
    let err = parse_sources("{ 这不是 JSON").expect_err("坏 JSON 必须拒绝");
    assert_eq!(err.code, "bad-sources-json");
}
