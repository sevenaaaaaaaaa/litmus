//! 导入侧的 Markdown 组装：带格式的文本片段 → 行内 Markdown；块 → 文档。

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fmt {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub mark: bool,
    pub sup: bool,
    pub sub: bool,
}

#[derive(Debug, Clone)]
pub enum Piece {
    Text(String, Fmt),
    /// 已成型的 Markdown（链接 / 图片 / 公式 / 脚注引用）：不再包格式记号
    Raw(String),
    /// 链接：内部片段 + 地址
    Link(Vec<Piece>, String),
}

/// 片段 → 行内 Markdown：同格式相邻片段先合并，记号内侧不留空白。
pub fn inline(pieces: &[Piece]) -> String {
    let mut merged: Vec<Piece> = Vec::new();
    for p in pieces {
        match (merged.last_mut(), p) {
            (Some(Piece::Text(a, fa)), Piece::Text(b, fb)) if fa == fb => a.push_str(b),
            _ => merged.push(p.clone()),
        }
    }
    let mut out = String::new();
    for p in &merged {
        match p {
            Piece::Raw(s) => out.push_str(s),
            Piece::Link(inner, url) => {
                let label = inline(inner);
                let label = label.trim();
                if url.is_empty() {
                    out.push_str(label);
                } else if label.is_empty() || label == url {
                    out.push_str(url);
                } else {
                    out.push_str(&format!("[{}]({})", label.replace(['[', ']'], ""), url.replace(' ', "%20")));
                }
            }
            Piece::Text(t, f) => out.push_str(&wrap(t, f)),
        }
    }
    out
}

fn wrap(t: &str, f: &Fmt) -> String {
    if t.trim().is_empty() || *f == Fmt::default() {
        return t.to_string();
    }
    let lead: String = t.chars().take_while(|c| c.is_whitespace()).collect();
    let trail: String = t.chars().rev().take_while(|c| c.is_whitespace()).collect::<Vec<_>>().into_iter().rev().collect();
    let core = t.trim();
    let mut s = core.to_string();
    if f.code {
        let ticks = if s.contains('`') { "``" } else { "`" };
        s = format!("{ticks}{s}{ticks}");
    } else {
        // 上下标：数字与常见符号转 Unicode 上下标（x² / H₂O），其余原样
        if f.sup || f.sub {
            let (from, to) = if f.sup { ("0123456789+-=()n", "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿ") } else { ("0123456789+-=()", "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎") };
            let map: Vec<(char, char)> = from.chars().zip(to.chars()).collect();
            if s.chars().all(|c| map.iter().any(|(a, _)| *a == c)) {
                s = s.chars().map(|c| map.iter().find(|(a, _)| *a == c).map(|(_, b)| *b).unwrap_or(c)).collect();
            }
        }
        if f.mark {
            s = format!("=={s}==");
        }
        if f.strike {
            s = format!("~~{s}~~");
        }
        if f.italic {
            s = format!("*{s}*");
        }
        if f.bold {
            s = format!("**{s}**");
        }
    }
    format!("{lead}{s}{trail}")
}

/// 表格 → Markdown（列数取最大值补齐；单元格内换行压成空格；| 转义）。
pub fn table(rows: &[Vec<String>]) -> String {
    let n = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if n == 0 || rows.is_empty() {
        return String::new();
    }
    let cell = |c: &str| {
        let c = c.split_whitespace().collect::<Vec<_>>().join(" ");
        let c = c.replace('|', "\\|");
        if c.is_empty() { " ".to_string() } else { c }
    };
    let fmt = |r: &Vec<String>| {
        let mut cs: Vec<String> = r.iter().map(|c| cell(c)).collect();
        cs.resize(n, " ".into());
        format!("| {} |", cs.join(" | "))
    };
    let mut s = fmt(&rows[0]);
    s.push_str(&format!("\n|{}", " --- |".repeat(n)));
    for r in &rows[1..] {
        s.push('\n');
        s.push_str(&fmt(r));
    }
    s
}

/// 块列表 → 文档：块间空行；相邻同类列表项不插空行。
pub fn join_blocks(blocks: &[String]) -> String {
    let mut out = String::new();
    let is_item = |s: &str| {
        let t = s.trim_start();
        t.starts_with("- ") || t.split_once(". ").map_or(false, |(a, _)| !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()))
    };
    for (i, b) in blocks.iter().enumerate() {
        if b.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            let prev = &blocks[..i].iter().rev().find(|x| !x.trim().is_empty()).cloned().unwrap_or_default();
            if is_item(prev) && is_item(b) {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
        out.push_str(b.trim_end());
    }
    out.push('\n');
    out
}

/// 数字 → 各种编号格式。
pub fn format_num(n: u32, fmt: &str) -> String {
    match fmt {
        "lowerLetter" | "a" => letters(n, false),
        "upperLetter" | "A" => letters(n, true),
        "lowerRoman" | "i" => roman(n).to_lowercase(),
        "upperRoman" | "I" => roman(n),
        "chineseCounting" | "chineseCountingThousand" | "ideographTraditional" | "taiwaneseCounting" | "chineseLegalSimplified" | "一, 二, 三, ..." => chinese(n),
        "decimalEnclosedCircle" | "decimalEnclosedCircleChinese" | "①" => {
            if (1..=20).contains(&n) { char::from_u32('①' as u32 + n - 1).unwrap().to_string() } else { n.to_string() }
        }
        "decimalZero" => format!("{n:02}"),
        _ => n.to_string(),
    }
}

fn letters(mut n: u32, upper: bool) -> String {
    let mut s = Vec::new();
    while n > 0 {
        n -= 1;
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
    let s: String = s.into_iter().rev().collect();
    if upper { s.to_uppercase() } else { s }
}

fn roman(mut n: u32) -> String {
    let t = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"), (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")];
    let mut s = String::new();
    for (v, r) in t {
        while n >= v {
            s.push_str(r);
            n -= v;
        }
    }
    s
}

pub fn chinese(n: u32) -> String {
    let d = ['零', '一', '二', '三', '四', '五', '六', '七', '八', '九'];
    match n {
        0..=9 => d[n as usize].to_string(),
        10..=19 => format!("十{}", if n % 10 == 0 { String::new() } else { d[(n % 10) as usize].to_string() }),
        20..=99 => format!("{}十{}", d[(n / 10) as usize], if n % 10 == 0 { String::new() } else { d[(n % 10) as usize].to_string() }),
        _ => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_merges_and_wraps() {
        let b = Fmt { bold: true, ..Default::default() };
        let p = vec![
            Piece::Text("显色".into(), Fmt::default()),
            Piece::Text(" 见".into(), b.clone()),
            Piece::Text("真 ".into(), b),
            Piece::Link(vec![Piece::Text("官网".into(), Fmt::default())], "https://a.com".into()),
        ];
        assert_eq!(inline(&p), "显色 **见真** [官网](https://a.com)");
        assert_eq!(format_num(12, "chineseCounting"), "十二");
        assert_eq!(format_num(4, "lowerRoman"), "iv");
        assert_eq!(format_num(28, "upperLetter"), "AB");
    }
}
