//! P1-02 三入口的 Rust 断言，与 reference/p1-02-soul-entrances/entrances.test.mjs 同名同断言。
//! 同一份 fixture、同一组线索词：两边挑出的句子集合不一致，就是移植走样。

use kernel_soul::entrances::*;
use kernel_soul::parse;

const NOTE1: &str = include_str!("../../../../docs/spec/r2/reference/p1-02-soul-entrances/fixtures/note-1.md");
const NOTE2: &str = include_str!("../../../../docs/spec/r2/reference/p1-02-soul-entrances/fixtures/note-2.md");
const REF_JS: &str = include_str!("../../../../docs/spec/r2/reference/p1-02-soul-entrances/entrances.mjs");
const DATE: &str = "2026-11-24";

fn notes<'a>() -> Vec<NoteFile<'a>> {
    vec![
        NoteFile { path: "Notes/note-1.md", text: NOTE1 },
        NoteFile { path: "Notes/note-2.md", text: NOTE2 },
    ]
}

#[test]
fn interview_candidates_are_the_users_own_sentences() {
    let answers = vec![(
        "belief".to_string(),
        "知识不是收藏，是被用过的判断。好作品先有观点。".to_string(),
    )];
    let cands = from_interview(&answers).unwrap();
    assert_eq!(cands.len(), 2);
    for c in &cands {
        assert_grounded(c).unwrap();
        assert!(c.quote.contains(&c.text));
    }
    assert_eq!(cands.iter().map(|c| c.block.as_str()).collect::<Vec<_>>(), vec!["belief", "belief"]);
}

#[test]
fn fabricated_candidates_are_rejected() {
    let fake = Candidate {
        block: "belief".into(),
        text: "agent 自己编的一句".into(),
        source: "interview".into(),
        reference: "answer:belief".into(),
        quote: "用户原话是另一句".into(),
    };
    let err = assert_grounded(&fake).unwrap_err();
    assert_eq!(err.code, "candidate-not-verbatim");
    assert!(err.message.contains("不是原句"));

    let no_source = Candidate { quote: String::new(), ..fake };
    assert_eq!(assert_grounded(&no_source).unwrap_err().code, "candidate-no-source");
}

#[test]
fn extract_keeps_path_and_line_and_caps_three_per_block() {
    let cands = from_notes(&notes(), &default_rules(), 3);
    assert!(cands.len() >= 4, "至少要四条，实际 {}", cands.len());
    let mut counts = std::collections::BTreeMap::new();
    for c in &cands {
        assert!(c.reference.starts_with("Notes/note-") && c.reference.contains(':'), "{}", c.reference);
        let line: usize = c.reference.rsplit(':').next().unwrap().parse().unwrap();
        assert!(line >= 1);
        *counts.entry(c.block.clone()).or_insert(0) += 1;
    }
    for (b, n) in counts {
        assert!(n <= 3, "{b} 超了三条");
    }
}

#[test]
fn extract_skips_headings_code_lists_and_links() {
    let texts: Vec<String> = from_notes(&notes(), &default_rules(), 3).into_iter().map(|c| c.text).collect();
    assert!(!texts.iter().any(|t| t.starts_with('#')));
    assert!(!texts.iter().any(|t| t.contains("代码块")));
    assert!(!texts.iter().any(|t| t.starts_with("- ")));
}

#[test]
fn paste_splits_sentences_and_rejects_unknown_block() {
    let cands = from_paste("我写东西先给结论。然后再补理由。", "voice").unwrap();
    assert_eq!(cands.len(), 2);
    assert!(cands.iter().all(|c| c.block == "voice"));
    assert_eq!(from_paste("随便", "nope").unwrap_err().code, "unknown-block");
}

#[test]
fn review_accept_edit_drop_and_empty_edit_is_refused() {
    let mut r = Review::new(
        from_interview(&[(
            "belief".to_string(),
            "第一句原话在这里。第二句也在这里。".to_string(),
        )])
        .unwrap(),
    );
    assert_eq!(r.pending(), 2);
    r.decide(0, Decision::Accepted, None).unwrap();
    r.decide(1, Decision::Edited, Some("第二句被我改了一下")).unwrap();
    assert_eq!(r.pending(), 0);
    assert_eq!(r.decide(0, Decision::Edited, Some("   ")).unwrap_err().code, "review-empty-edit");
    assert_eq!(r.decide(0, Decision::Pending, None).unwrap_err().code, "review-bad-decision");
    assert_eq!(r.decide(9, Decision::Accepted, None).unwrap_err().code, "review-no-item");
}

#[test]
fn assemble_writes_only_accepted_and_parses_clean() {
    let mut r = Review::new(
        from_interview(&[
            ("belief".to_string(), "知识不是收藏，是被用过的判断。".to_string()),
            ("refuse".to_string(), "不写没有来源的结论。".to_string()),
        ])
        .unwrap(),
    );
    r.decide(0, Decision::Accepted, None).unwrap();
    r.decide(1, Decision::Dropped, None).unwrap();
    let d = assemble(&r.kept(), true, 0, DATE).unwrap();
    let doc = parse(&d.markdown);
    assert!(doc.errors.is_empty(), "{:?}", doc.errors);
    assert_eq!(doc.sections.iter().map(|s| s.items.len()).sum::<usize>(), 1);
    assert!(d.markdown.contains("^belief-1"));
    assert!(!d.markdown.contains("不写没有来源的结论"));
    assert!(!doc.temporary);
    assert!(d.sources.contains_key("belief-1"));
    assert_eq!(d.sources["belief-1"].reference, "answer:belief");
}

#[test]
fn unconfirmed_assembly_is_a_temporary_card() {
    let mut r = Review::new(from_interview(&[("belief".to_string(), "一句原话够不够长呢。".to_string())]).unwrap());
    r.accept_all();
    let d = assemble(&r.kept(), false, 0, DATE).unwrap();
    assert!(parse(&d.markdown).temporary);
}

#[test]
fn three_entrances_share_one_block_order() {
    let mut a = Review::new(from_interview(&[("belief".to_string(), "来自访谈的一句原话。".to_string())]).unwrap());
    a.accept_all();
    let mut b = Review::new(from_paste("来自粘贴的一句旧文字。", "voice").unwrap());
    b.accept_all();
    let mut c = Review::new(from_notes(&notes(), &default_rules(), 3));
    c.accept_all();
    for review in [a, b, c] {
        let d = assemble(&review.kept(), false, 0, DATE).unwrap();
        let doc = parse(&d.markdown);
        let names: Vec<&str> = doc.sections.iter().map(|s| s.name.as_str()).collect();
        let ranks: Vec<usize> = names
            .iter()
            .map(|n| BLOCKS.iter().position(|m| m.name == *n).unwrap())
            .collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(ranks, sorted, "区块顺序必须按 SECTIONS");
    }
}

#[test]
fn temporary_card_has_sources_and_is_unconfirmed() {
    let d = temporary_card(&notes(), &default_rules(), DATE).unwrap();
    let doc = parse(&d.markdown);
    assert!(doc.temporary);
    assert!(doc.errors.is_empty());
    assert!(doc.sections.iter().map(|s| s.items.len()).sum::<usize>() >= 2);
    assert!(d.sources.values().all(|s| s.reference.contains(':')));
}

#[test]
fn capture_keeps_the_copied_sentences_and_defaults_to_belief() {
    let cands = from_capture("短句、具体、少形容词。这句话没有任何线索词。", &default_rules(), 3);
    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0].block, "voice");
    assert_eq!(cands[1].block, "belief", "一条线索都没命中就落最宽的区块");
    assert!(cands.iter().all(|c| c.source == "capture" && c.reference == "capture:quick"));
    assert_eq!(from_capture("第一句。第二句。第三句。第四句。", &default_rules(), 3).len(), 3, "最多三条");
    assert!(from_capture("", &default_rules(), 3).is_empty());
}

#[test]
fn sentences_split_on_both_scripts_and_newlines() {
    assert_eq!(sentences("一。二！三？四\n五"), vec!["一", "二", "三", "四", "五"]);
}

#[test]
fn cues_in_the_json_still_exist_in_the_reference_implementation() {
    // 规则是从 entrances.mjs 抄过来的；JS 改了而 JSON 没跟上，这条会红。
    let rules = default_rules();
    validate_rules(&rules).unwrap();
    assert_eq!(rules.cues.len(), BLOCKS.len());
    for (block, cues) in &rules.cues {
        assert!(!cues.is_empty(), "{block} 没有线索词");
        for c in cues {
            let quoted = REF_JS.contains(&format!("\"{c}\"")) || REF_JS.contains(&format!("'{c}'"));
            assert!(quoted, "参考实现里没有线索词 {c}（{block}）");
        }
    }
}
