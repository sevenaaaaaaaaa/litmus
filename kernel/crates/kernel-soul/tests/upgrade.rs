//! 升级：临时卡 → 真卡。
//! 这一层的判据只有一条，但它是硬的：**升级后锚点集合必须是升级前的超集**。
//! 做法是逐字节不动地翻 frontmatter 那一行，让超集成为结构性结论，而不是靠人记得。

use kernel_soul::sources::{check_sources, SourceEntry, Sources};
use kernel_soul::{parse, reanchor, upgrade};

const TPL: &str = include_str!("fixtures/soul-template.md");
const EX: &str = include_str!("fixtures/soul-example.md");

fn cover(doc: &kernel_soul::SoulDoc) -> Sources {
    let mut s = Sources::new("Notes/灵魂卡.md");
    for section in &doc.sections {
        for item in &section.items {
            if let Some(a) = &item.anchor {
                s.anchors.insert(
                    a.clone(),
                    SourceEntry { source: "note".into(), reference: "Notes/旧笔记.md:1".into(), quote: item.text.clone(), edited: false },
                );
            }
        }
    }
    s
}

#[test]
fn upgrade_flips_confirmed_and_keeps_every_anchor() {
    let up = upgrade(TPL).expect("模板卡是可升级的临时卡");
    let before = parse(TPL);
    let after = parse(&up.text);
    assert!(before.temporary);
    assert!(!after.temporary, "升级后必须不再是临时卡");
    assert_eq!(up.anchors_before, up.anchors_after, "升级不该动锚点");
    assert_eq!(after.anchor_count, before.anchor_count);
}

#[test]
fn upgrade_only_changes_the_confirmed_line() {
    let up = upgrade(TPL).expect("可升级");
    let a: Vec<&str> = TPL.split('\n').collect();
    let b: Vec<&str> = up.text.split('\n').collect();
    assert_eq!(a.len(), b.len(), "行数都不该变");
    let diff: Vec<usize> = (0..a.len()).filter(|i| a[*i] != b[*i]).collect();
    assert_eq!(diff.len(), 1, "只该有 frontmatter 那一行变了，实际变了 {:?}", diff);
    assert!(b[diff[0]].trim().starts_with("confirmed:"), "变的必须是 confirmed 行");
    assert_eq!(b[diff[0]].trim(), "confirmed: true");
}

#[test]
fn upgrading_a_confirmed_card_is_refused() {
    let err = upgrade(EX).expect_err("真卡不该被「升级」");
    assert_eq!(err.code, "upgrade-already-confirmed");
}

#[test]
fn card_without_confirmed_line_is_refused() {
    let stripped: String = TPL.split('\n')
        .filter(|l| !l.trim_start().starts_with("confirmed:"))
        .collect::<Vec<_>>()
        .join("\n");
    let err = upgrade(&stripped).expect_err("没有那一行就拒绝，不猜");
    assert_eq!(err.code, "upgrade-no-confirmed-line");
}

#[test]
fn invalid_card_is_refused() {
    // 撞号是最常见的一种真错：两条判断共用一个锚点。
    let dup = TPL.replace("^criteria-1", "^belief-1");
    assert!(!parse(&dup).errors.is_empty(), "造出来的卡必须真的不合法，否则这条用例什么都没测");
    let err = upgrade(&dup).expect_err("坏卡不升级");
    assert_eq!(err.code, "upgrade-invalid-card");
}

#[test]
fn provenance_survives_the_upgrade() {
    // 这条是 P1-12 与 P1-10b 的接缝：旁挂按锚点键，而升级保证锚点不动，
    // 所以「升级前建的旁挂在升级后仍然逐条对得上」应当直接成立。
    let before = parse(TPL);
    let sources = cover(&before);
    assert!(!sources.anchors.is_empty(), "模板卡至少有一条判断");
    let up = upgrade(TPL).expect("可升级");
    let after = parse(&up.text);
    assert_eq!(check_sources(&after, &sources), vec![], "升级后出处应当仍然对得上");
}

#[test]
fn upgrade_does_not_reanchor_a_later_edit() {
    // 升级之后再改字，走的是 reanchor，不是升级；这里钉住两者的分界：
    // 升级本身不动锚点，改字才需要重挂。
    let up = upgrade(TPL).expect("可升级");
    let edited = up.text.replace("我的声音", "我的声音");
    let same = reanchor(&up.text, &edited);
    assert!(same.minted.is_empty(), "原样重挂不该铸新号：{:?}", same.minted);
    assert_eq!(parse(&same.text).anchor_count, parse(&up.text).anchor_count);
}
