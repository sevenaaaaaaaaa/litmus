//! P1-10a：把 P1-01 的 17 条参考测试原样搬到 Rust。
//! 参考实现：docs/spec/r2/reference/p1-01-soul/soul.mjs + soul.test.mjs。
//! 两边跑同一批用例，才算「模型进了内核」，而不是「另写了一个像它的东西」。

use kernel_soul::{all_anchors, diff, dice, is_anchor, parse, reanchor, resolve_refs};

const EX: &str = include_str!("fixtures/soul-example.md");
const TPL: &str = include_str!("fixtures/soul-template.md");

fn strip_anchors(text: &str) -> String {
    text.split('\n')
        .map(|l| match l.rfind('^') {
            Some(i) => l[..i].trim_end().to_string(),
            None => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn codes(errors: &[kernel_soul::Issue]) -> Vec<String> {
    errors.iter().map(|e| e.code.clone()).collect()
}

#[test]
fn example_card_six_sections_16_items_16_anchors_no_errors() {
    let d = parse(EX);
    assert!(d.errors.is_empty(), "errors: {:?}", d.errors);
    assert_eq!(d.sections.len(), 6);
    assert_eq!(d.item_count, 16);
    assert_eq!(d.anchor_count, 16);
    assert!(!d.temporary);
    assert_eq!(d.version.as_deref(), Some("1"));
}

#[test]
fn every_anchor_is_legal_and_unique() {
    let d = parse(EX);
    let anchors = all_anchors(&d);
    let unique: std::collections::BTreeSet<_> = anchors.iter().collect();
    assert_eq!(unique.len(), anchors.len());
    for a in &anchors {
        assert!(is_anchor(a), "{a} 不合法");
    }
}

#[test]
fn duplicate_anchor_is_an_error_naming_the_other_line() {
    let d = parse(&EX.replace("^voice-2", "^voice-1"));
    let e = d
        .errors
        .iter()
        .find(|x| x.code == "duplicate-anchor")
        .expect("没有抓到重复锚点");
    assert!(e.message.contains("voice-1"), "报错没说清是哪一条：{}", e.message);
}

#[test]
fn missing_anchor_is_an_error() {
    let d = parse(&EX.replace("不写没有来源的结论。 ^refuse-3", "不写没有来源的结论。"));
    assert!(codes(&d.errors).contains(&"missing-anchor".to_string()));
}

#[test]
fn illegal_anchor_is_an_error() {
    assert!(codes(&parse(&EX.replace("^belief-1", "^BELIEF_1")).errors).contains(&"bad-anchor".to_string()));
    // 带空格的 ^ 不是锚点，而是正文里的字符：算「漏写锚点」，不算「锚点不合规」
    assert!(codes(&parse(&EX.replace("^belief-1", "^bad anchor")).errors)
        .contains(&"missing-anchor".to_string()));
}

#[test]
fn unknown_section_is_an_error() {
    let d = parse(&format!("{EX}\n## 我的星座\n- 不许加。 ^zodiac-1\n"));
    assert!(codes(&d.errors).contains(&"unknown-section".to_string()));
}

#[test]
fn empty_card_is_an_error_thin_card_is_a_warning() {
    let empty = parse("---\ntype: soul-card\n---\n");
    assert!(codes(&empty.errors).contains(&"empty-card".to_string()));
    let thin = parse(
        &[
            "---", "type: soul-card", "confirmed: true", "---",
            "## 我相信", "- 一。 ^belief-1",
            "## 我拒绝", "- 二。 ^refuse-1",
        ]
        .join("\n"),
    );
    assert!(codes(&thin.warnings).contains(&"thin-card".to_string()));
}

#[test]
fn temporary_card_is_flagged_but_not_an_error() {
    let d = parse(TPL);
    assert!(d.temporary);
    assert!(d.errors.is_empty(), "errors: {:?}", d.errors);
}

#[test]
fn diff_reports_one_edited_line() {
    let base = parse(EX);
    let next = parse(&EX.replace("知识不是收藏，是被用过的判断。", "知识不是收藏，是被反复用过的判断。"));
    let d = diff(&base, &next);
    assert_eq!(d.edited.len(), 1);
    assert_eq!(d.added.len(), 0);
    assert_eq!(d.removed.len(), 0);
    assert_eq!(d.edited[0].anchor, "belief-1");
    assert_eq!(d.unchanged, 15);
}

#[test]
fn diff_reports_added_with_new_anchor() {
    let base = parse(EX);
    let next = parse(&EX.replace(
        "- 不写没有来源的结论。 ^refuse-3",
        "- 不写没有来源的结论。 ^refuse-3\n- 不替别人下结论。 ^refuse-4",
    ));
    let d = diff(&base, &next);
    assert_eq!(d.added.len(), 1);
    assert_eq!(d.added[0].anchor, "refuse-4");
}

#[test]
fn diff_reports_removed() {
    let base = parse(EX);
    let next = parse(&EX.replace("- 不追热点：除非它和我的长期问题有关。 ^refuse-2\n", ""));
    let d = diff(&base, &next);
    assert_eq!(d.removed.len(), 1);
    assert_eq!(d.removed[0].anchor, "refuse-2");
}

#[test]
fn reanchor_carries_every_anchor_through_a_one_char_edit() {
    let mutated = strip_anchors(EX).replace(
        "知识不是收藏，是被用过的判断。",
        "知识不是收藏，是被反复用过的判断。",
    );
    let r = reanchor(EX, &mutated);
    assert!(r.minted.is_empty(), "不该新铸：{}", r.minted.join(","));
    assert!(r.deleted.is_empty(), "不该丢：{}", r.deleted.join(","));
    let d = parse(&r.text);
    assert!(d.errors.is_empty(), "errors: {:?}", d.errors);
    assert_eq!(d.anchor_count, 16);
    assert!(r.text.contains("被反复用过的判断。 ^belief-1"), "锚点没跟上：\n{}", r.text);
}

#[test]
fn reanchor_mints_exactly_one_for_a_new_item_and_continues_numbering() {
    let with_new = strip_anchors(EX).replace(
        "- 特征：短句、具体、少形容词、先结论后理由。",
        "- 特征：短句、具体、少形容词、先结论后理由。\n- 样本 4：写完先删掉三句。",
    );
    let r = reanchor(EX, &with_new);
    assert_eq!(r.minted, vec!["voice-5".to_string()]);
    assert!(r.deleted.is_empty());
}

#[test]
fn anchors_survive_40_random_one_char_edits() {
    let base = parse(EX);
    let base_anchors = all_anchors(&base);
    let lines: Vec<String> = EX.split('\n').map(|s| s.to_string()).collect();
    let item_idx: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim().starts_with("- "))
        .map(|(i, _)| i)
        .collect();

    let mut seed: u64 = 7;
    let mut rnd = || {
        seed = (seed.wrapping_mul(1_103_515_245).wrapping_add(12_345)) % 2_147_483_648;
        seed as f64 / 2_147_483_648.0
    };

    for round in 0..40 {
        let li = item_idx[(rnd() * item_idx.len() as f64) as usize];
        let line = &lines[li];
        let caret = line.rfind('^').expect("条目必须带锚点");
        let body = &line[..caret];
        let chars: Vec<char> = body.chars().collect();
        let by_colon = body.find('：').map(|i| i > 0).unwrap_or(false);
        let raw = if by_colon {
            chars.len().saturating_sub(3)
        } else {
            ((rnd() * ((chars.len() - 2) as f64)) as usize) + 1
        };
        let pos = raw.max(3).min(chars.len().saturating_sub(1));
        let mut mutated: String = chars[..pos].iter().collect();
        mutated.push('改');
        mutated.extend(chars[pos + 1..].iter());

        let mut next = lines.clone();
        next[li] = mutated;
        let candidate = next.join("\n");
        let r = reanchor(EX, &candidate);
        let d = parse(&r.text);
        assert!(d.errors.is_empty(), "第 {round} 轮产生错误：{:?}", d.errors);
        for a in &base_anchors {
            assert!(r.text.contains(&format!("^{a}")), "第 {round} 轮丢了 ^{a}");
        }
        assert!(r.minted.is_empty(), "第 {round} 轮新铸了 {}", r.minted.join(","));
    }
}

#[test]
fn resolve_refs_finds_sentences_and_lists_missing_anchors() {
    let d = parse(EX);
    let (found, missing) = resolve_refs(&d, &["belief-1", "nope-9"]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].anchor, "belief-1");
    assert!(found[0].text.contains("知识不是收藏"));
    assert_eq!(missing, vec!["nope-9".to_string()]);
}

#[test]
fn dice_is_one_for_identical_and_low_for_unrelated() {
    let a = "知识不是收藏，是被用过的判断。";
    assert!((dice(a, a) - 1.0).abs() < f64::EPSILON);
    assert!(dice(a, "今天下午三点开会讨论排期。") < 0.2);
}

#[test]
fn anchor_numbers_are_never_reused() {
    let removed = EX.replace("- 特征：短句、具体、少形容词、先结论后理由。 ^voice-4\n", "");
    let added = removed.replace(
        "- 样本 3：少形容词，多动词。 ^voice-3",
        "- 样本 3：少形容词，多动词。 ^voice-3\n- 样本 4：写完先删三句。",
    );
    let r = reanchor(EX, &strip_anchors(&added));
    assert!(!r.minted.contains(&"voice-4".to_string()), "复用了已删除的 voice-4");
    assert!(r.minted.contains(&"voice-5".to_string()), "实际新铸：{}", r.minted.join(","));
}
