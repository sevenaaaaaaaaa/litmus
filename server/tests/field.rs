//! 知识场壳（server/web/field.html）的工艺与配色纪律，与 craft.rs / palette.rs 同一套标准：
//! - 字号只走 token（--fs-1…5 与壳内声明的 --fs-* 档），样式与标记里没有裸 px 字号；
//! - 零 hex / rgb / hsl：颜色全部 oklch；
//! - chrome 不直出 emoji；
//! - 纸与墨的对比度：正文墨、次级墨、石蕊蓝在纸与页面上都 ≥ 4.5:1（两种明暗）。

const FIELD: &str = include_str!("../web/field.html");

fn shell() -> &'static str {
    let end = FIELD.find("<script data-cfasync").expect("主脚本标记");
    &FIELD[..end]
}

#[test]
fn font_sizes_come_from_tokens() {
    let mut bad = Vec::new();
    for (i, line) in shell().lines().enumerate() {
        for pat in ["font-size:", "font:"] {
            let mut rest = line;
            while let Some(j) = rest.find(pat) {
                let tail = &rest[j + pat.len()..];
                let decl: String = tail.chars().take_while(|&c| c != ';' && c != '}' && c != '"').collect();
                let b = decl.as_bytes();
                if decl.match_indices("px").any(|(k, _)| k > 0 && b[k - 1].is_ascii_digit()) {
                    bad.push(format!("L{}: {pat}{decl}", i + 1));
                }
                rest = &tail[decl.len().min(tail.len())..];
            }
        }
    }
    assert!(bad.is_empty(), "知识场壳出现裸 px 字号：\n{}", bad.join("\n"));
}

#[test]
fn zero_hex_and_no_legacy_color_functions() {
    let s = shell();
    let b = s.as_bytes();
    let mut hits = Vec::new();
    for (i, &c) in b.iter().enumerate() {
        if c == b'#' && i > 0 && matches!(b[i - 1], b':' | b'(' | b',' | b' ') {
            let n = b[i + 1..].iter().take_while(|x| x.is_ascii_hexdigit()).count();
            let next = b.get(i + 1 + n).copied().unwrap_or(b' ');
            let tail = s[i + 1 + n..].trim_start();
            let selector = tail.starts_with('{') || tail.starts_with(',') || tail.starts_with('.') || tail.starts_with(':');
            if matches!(n, 3 | 4 | 6 | 8) && !(next.is_ascii_alphanumeric() || next == b'-' || next == b'_') && !selector {
                hits.push(s[i..i + 1 + n].to_string());
            }
        }
    }
    for f in ["rgb(", "rgba(", "hsl(", "hsla("] {
        if let Some(j) = s.find(f) {
            hits.push(s[j..(j + 20).min(s.len())].to_string());
        }
    }
    assert!(hits.is_empty(), "知识场壳出现非 oklch 色值：{hits:?}");
}

#[test]
fn chrome_markup_has_no_emoji() {
    let hits: Vec<char> = shell()
        .chars()
        .filter(|&c| {
            let u = c as u32;
            (0x1F300..=0x1FAFF).contains(&u) || (0x2600..=0x27BF).contains(&u) || (0x2B00..=0x2BFF).contains(&u)
        })
        .collect();
    assert!(hits.is_empty(), "知识场壳直出了 emoji：{hits:?}");
}

// ── 对比度：从 :root 与暗色块读三元组，算 WCAG 对比 ──

fn triple(block: &str, name: &str) -> (f64, f64, f64) {
    let key = format!("{name}:");
    let at = block.find(&key).unwrap_or_else(|| panic!("缺少 {name}"));
    let v: Vec<f64> = block[at + key.len()..].split(';').next().unwrap().split_whitespace().map(|x| x.parse().unwrap()).collect();
    (v[0], v[1], v[2])
}

fn lin(c: (f64, f64, f64)) -> [f64; 3] {
    let (l, ch, h) = c;
    let (a, b) = (ch * h.to_radians().cos(), ch * h.to_radians().sin());
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;
    let (l3, m3, s3) = (l_.powi(3), m_.powi(3), s_.powi(3));
    [
        (4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3).clamp(0.0, 1.0),
        (-1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3).clamp(0.0, 1.0),
        (-0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3).clamp(0.0, 1.0),
    ]
}

fn contrast(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    let y = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let (ya, yb) = (y(lin(a)), y(lin(b)));
    (ya.max(yb) + 0.05) / (ya.min(yb) + 0.05)
}

#[test]
fn ink_and_litmus_pass_aa_on_paper_in_both_themes() {
    let s = shell();
    let light = &s[s.find(":root{").unwrap()..];
    let dark = &s[s.find(":root[data-theme=\"dark\"]{").unwrap()..];
    for (name, block) in [("亮色", light), ("暗色", dark)] {
        let paper = triple(block, "--paper-c");
        let sheet = triple(block, "--sheet-c");
        for fg in ["--ink-c", "--ink2-c", "--litmus-c", "--acid-c"] {
            let f = triple(block, fg);
            for (bgn, bg) in [("纸", paper), ("页", sheet)] {
                let c = contrast(f, bg);
                assert!(c >= 4.5, "[{name}] {fg} 压在{bgn}上只有 {c:.2}:1");
            }
        }
        // 淡墨只做装饰 / 占位，仍需 ≥ 1.6:1 才看得见
        let c = contrast(triple(block, "--ink3-c"), paper);
        assert!(c >= 1.6, "[{name}] --ink3-c 太淡：{c:.2}:1");
    }
}

/// 加密分享查看页（server/web/share.html）同一套纪律：零 hex / rgb / hsl，字号走 token，不直出 emoji。
#[test]
fn share_page_follows_the_same_discipline() {
    const SHARE: &str = include_str!("../web/share.html");
    let end = SHARE.find("<script id=\"share-data\"").expect("数据占位");
    let s = &SHARE[..end];
    for f in ["rgb(", "rgba(", "hsl(", "hsla("] {
        assert!(!s.contains(f), "分享页出现 {f}");
    }
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'#' && i > 0 && matches!(b[i - 1], b':' | b'(' | b',' | b' ') {
            let n = b[i + 1..].iter().take_while(|x| x.is_ascii_hexdigit()).count();
            assert!(!matches!(n, 3 | 4 | 6 | 8), "分享页出现 hex 色值：{}", &s[i..i + 1 + n]);
        }
    }
    for line in s.lines() {
        if let Some(j) = line.find("font-size:") {
            let decl: String = line[j + 10..].chars().take_while(|&c| c != ';' && c != '}').collect();
            assert!(!decl.contains("px"), "分享页裸 px 字号：{decl}");
        }
    }
    assert!(!s.chars().any(|c| ('\u{1F300}'..='\u{1FAFF}').contains(&c)), "分享页直出 emoji");
    assert!(SHARE.contains("__SHARE_DATA__"));
}
