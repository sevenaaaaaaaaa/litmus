//! 壳的工艺纪律（docs/spec/design-system.md「图标与字体纪律」）。
//!
//! - 字号只走五级 token：<style> 与静态标记里不许出现裸 px 字号；
//! - chrome 不直出 emoji：静态标记里没有象形符号（键盘符 ⌘⇧↩ 与箭头属排印，放行）；
//! - 每个 data-ic / iconSvg('名') 都能在 IC 图标表里找到，不会渲染成空方块。

const INDEX: &str = include_str!("../web/index.html");

fn shell() -> &'static str {
    let end = INDEX.find("<script data-cfasync").expect("主脚本标记");
    &INDEX[..end]
}

fn main_script() -> &'static str {
    let start = INDEX.find("<script data-cfasync").expect("主脚本标记");
    &INDEX[start..]
}

#[test]
fn font_sizes_come_from_the_five_step_scale() {
    let s = shell();
    let mut bad = Vec::new();
    for (i, line) in s.lines().enumerate() {
        for pat in ["font-size:", "font:"] {
            let mut rest = line;
            while let Some(j) = rest.find(pat) {
                let tail = &rest[j + pat.len()..];
                let decl: String = tail.chars().take_while(|&c| c != ';' && c != '}' && c != '"').collect();
                // 裸 px 字号：形如 "12px" / "500 12px" / "12px/1.6"（px 前紧跟数字）
                let b = decl.as_bytes();
                let raw = decl.match_indices("px").any(|(k, _)| {
                    k > 0 && b[k - 1].is_ascii_digit() && {
                        // 往前找到数字起点，确认不是 var(...) 内的东西
                        let start = decl[..k].rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |p| p + 1);
                        !decl[..start].ends_with("--")
                    }
                });
                if raw {
                    bad.push(format!("L{}: {pat}{decl}", i + 1));
                }
                rest = &tail[decl.len().min(tail.len())..];
            }
        }
    }
    assert!(bad.is_empty(), "壳里出现裸 px 字号（应改用 --fs-1…5）：\n{}", bad.join("\n"));
}

fn is_pictograph(c: char) -> bool {
    let u = c as u32;
    (0x1F300..=0x1FAFF).contains(&u)      // 象形符号与表情
        || (0x2600..=0x27BF).contains(&u) // 杂项符号与装饰符（✓✕✨⚡…）
        || (0x2B00..=0x2BFF).contains(&u) // ⭐ 等
        || u == 0xFF0B                    // 全角＋
        || u == 0x22EF                    // ⋯
}

#[test]
fn chrome_markup_has_no_emoji() {
    let s = shell();
    let mut hits = Vec::new();
    for (i, line) in s.lines().enumerate() {
        // 注释里的说明文字不算
        if line.trim_start().starts_with("<!--") || line.trim_start().starts_with("/*") {
            continue;
        }
        let found: String = line.chars().filter(|&c| is_pictograph(c)).collect();
        if !found.is_empty() {
            hits.push(format!("L{}: {found}", i + 1));
        }
    }
    assert!(hits.is_empty(), "静态标记直出了 emoji/符号，请换成 IC 线性图标：\n{}", hits.join("\n"));
}

#[test]
fn every_icon_reference_exists() {
    let js = main_script();
    let start = js.find("const IC={").expect("IC 图标表");
    let end = start + js[start..].find("\n};").expect("IC 结束");
    let table = &js[start..end];
    let names: Vec<&str> = table
        .lines()
        .filter_map(|l| l.trim().split_once(":'").map(|(k, _)| k.trim()))
        .filter(|k| !k.is_empty() && !k.contains(' '))
        .collect();
    assert!(names.len() >= 50, "IC 表解析异常：{names:?}");
    let mut missing = Vec::new();
    let mut check = |n: &str, at: &str| {
        if !names.contains(&n) {
            missing.push(format!("{n}（{at}）"));
        }
    };
    for (i, _) in INDEX.match_indices("data-ic=\"") {
        let n: String = INDEX[i + 9..].chars().take_while(|&c| c != '"').collect();
        if !n.contains('$') {
            check(&n, "data-ic");
        }
    }
    for (i, _) in INDEX.match_indices("iconSvg('") {
        let n: String = INDEX[i + 9..].chars().take_while(|&c| c != '\'').collect();
        check(&n, "iconSvg");
    }
    for (i, _) in INDEX.match_indices("{icon:'") {
        let n: String = INDEX[i + 7..].chars().take_while(|&c| c != '\'').collect();
        check(&n, "菜单 icon");
    }
    assert!(missing.is_empty(), "引用了不存在的图标：{missing:?}");
}

#[test]
fn transitions_use_the_motion_family() {
    // 壳里的 transition 时长只能来自 --dur-1…5（0s 作为"立即"放行）；循环类 animation 不受此限
    let s = shell();
    let style = &s[s.find("<style>").unwrap()..s.find("</style>").unwrap()];
    let mut bad = Vec::new();
    for (i, _) in style.match_indices("transition:") {
        let decl: String = style[i + 11..].chars().take_while(|&c| c != ';' && c != '}').collect();
        let b = decl.as_bytes();
        for (k, _) in decl.match_indices('s') {
            // 形如 ".2s" / "1s"：s 前是数字、后面不是字母
            let prev_digit = k > 0 && b[k - 1].is_ascii_digit();
            let next_alpha = b.get(k + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'-');
            if prev_digit && !next_alpha {
                let start = decl[..k].rfind(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(0, |p| p + 1);
                let num = &decl[start..k];
                if num.parse::<f64>().map_or(true, |v| v != 0.0) {
                    bad.push(format!("transition:{decl}"));
                    break;
                }
            }
        }
    }
    assert!(bad.is_empty(), "transition 出现裸时长（应改用 --dur-1…5）：\n{}", bad.join("\n"));
}

#[test]
fn backdrop_blur_has_two_tiers_only() {
    // 毛玻璃半径只有两档：--blur-1（遮罩）/ --blur-2（玻璃表面）
    let mut bad = Vec::new();
    for (i, _) in INDEX.match_indices("backdrop-filter:") {
        let decl: String = INDEX[i + 16..].chars().take_while(|&c| c != ';' && c != '}' && c != '\'' && c != '`').collect();
        if decl.contains("blur(") && !(decl.contains("blur(var(--blur-1))") || decl.contains("blur(var(--blur-2))")) {
            bad.push(decl);
        }
    }
    assert!(bad.is_empty(), "backdrop-filter 出现两档之外的模糊半径：{bad:?}");
}
