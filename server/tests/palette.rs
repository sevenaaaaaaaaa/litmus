//! 配色体系契约（docs/spec/design-system.md「配色体系 v2」）。
//!
//! 在 `cargo test` 里强制三件事，任何一屏都不用靠肉眼复核：
//! 1. 零 hex：tokens.css 全文、index.html 的 <style> 与静态标记不得出现 hex/rgb()/hsl() 色值；
//! 2. 明度阶梯：五级表面在两套主题下严格单调，且相邻级差足以被看见；
//! 3. WCAG AA：文字/强调/语义色对五级表面 ≥4.5:1，text-4 ≥3:1，on-accent ≥4.5:1——
//!    亮/暗 × 石蕊双色/经典蓝 四种组合全部覆盖。
//!
//! 零依赖：oklch → sRGB、alpha 合成、相对亮度全部手写（公式见 CSS Color 4 / WCAG 2.2）。

use std::collections::HashMap;

const TOKENS: &str = include_str!("../web/tokens.css");
const INDEX: &str = include_str!("../web/index.html");

// ───────────── 解析 tokens.css ─────────────

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// 选择器 → 该块声明的自定义属性（只取顶层块，@font-face 等忽略）。
fn blocks(css: &str) -> Vec<(String, HashMap<String, String>)> {
    let css = strip_comments(css);
    let mut out = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let sel = rest[..open].trim().rsplit('}').next().unwrap_or("").trim().to_string();
        // 找匹配的右括号（块内可能有括号嵌套的 color-mix(...)，但没有花括号）
        let close = rest[open..].find('}').map(|c| open + c).expect("unbalanced css");
        let body = &rest[open + 1..close];
        let mut decls = HashMap::new();
        for d in split_decls(body) {
            if let Some((k, v)) = d.split_once(':') {
                let k = k.trim();
                if k.starts_with("--") {
                    decls.insert(k.to_string(), v.trim().to_string());
                }
            }
        }
        out.push((sel, decls));
        rest = &rest[close + 1..];
    }
    out
}

/// 按分号切分声明，但忽略括号内的分号（理论上不会有，防御）。
fn split_decls(body: &str) -> Vec<String> {
    let (mut depth, mut cur, mut out) = (0i32, String::new(), Vec::new());
    for ch in body.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Theme {
    Light,
    Dark,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Accent {
    Litmus,
    Blue,
}

/// 按 CSS 层叠顺序合成某主题 × 强调版本下的原语表。
fn palette(theme: Theme, accent: Accent) -> HashMap<String, String> {
    let all = blocks(TOKENS);
    let pick = |sel: &str| -> HashMap<String, String> {
        all.iter()
            .find(|(s, _)| s == sel)
            .unwrap_or_else(|| panic!("tokens.css 缺少选择器块：{sel}"))
            .1
            .clone()
    };
    let mut p = pick(r#":root,[data-theme="light"]"#);
    if theme == Theme::Dark {
        p.extend(pick(r#"[data-theme="dark"]"#));
    }
    if accent == Accent::Blue {
        p.extend(pick(r#"[data-accent="blue"]"#));
        if theme == Theme::Dark {
            p.extend(pick(r#"[data-theme="dark"][data-accent="blue"]"#));
        }
    }
    p
}

// ───────────── 颜色数学 ─────────────

#[derive(Clone, Copy, Debug)]
struct Rgba {
    lin: [f64; 3], // 线性 sRGB，已裁剪进色域
    a: f64,
}

fn parse_oklch(v: &str) -> Option<Rgba> {
    let v = v.trim();
    let inner = v.strip_prefix("oklch(")?.strip_suffix(')')?;
    let (main, alpha) = match inner.split_once('/') {
        Some((m, a)) => (m, a.trim().parse::<f64>().ok()?),
        None => (inner, 1.0),
    };
    let parts: Vec<&str> = main.split_whitespace().collect();
    if parts.len() != 3 {
        return None;
    }
    let l = parts[0].trim_end_matches('%').parse::<f64>().ok()? / 100.0;
    let c: f64 = parts[1].parse().ok()?;
    let h: f64 = parts[2].parse().ok()?;
    let (a_, b_) = (c * h.to_radians().cos(), c * h.to_radians().sin());
    let l_ = l + 0.396_337_777_4 * a_ + 0.215_803_757_3 * b_;
    let m_ = l - 0.105_561_345_8 * a_ - 0.063_854_172_8 * b_;
    let s_ = l - 0.089_484_177_5 * a_ - 1.291_485_548_0 * b_;
    let (l3, m3, s3) = (l_.powi(3), m_.powi(3), s_.powi(3));
    let r = 4.076_741_662_1 * l3 - 3.307_711_591_3 * m3 + 0.230_969_929_2 * s3;
    let g = -1.268_438_004_6 * l3 + 2.609_757_401_1 * m3 - 0.341_319_396_5 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_614_7 * m3 + 1.707_614_701_0 * s3;
    Some(Rgba { lin: [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)], a: alpha })
}

fn enc(x: f64) -> f64 {
    if x <= 0.003_130_8 { 12.92 * x } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 }
}
fn dec(x: f64) -> f64 {
    if x <= 0.040_45 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) }
}
/// 浏览器默认在 gamma 编码的 sRGB 空间做 alpha 合成。
fn over(fg: Rgba, bg: Rgba) -> Rgba {
    let mut lin = [0.0; 3];
    for i in 0..3 {
        lin[i] = dec(fg.a * enc(fg.lin[i]) + (1.0 - fg.a) * enc(bg.lin[i]));
    }
    Rgba { lin, a: 1.0 }
}
fn luminance(c: Rgba) -> f64 {
    0.2126 * c.lin[0] + 0.7152 * c.lin[1] + 0.0722 * c.lin[2]
}
fn contrast(a: Rgba, b: Rgba) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

fn color(p: &HashMap<String, String>, name: &str, ctx: &str) -> Rgba {
    let v = p.get(name).unwrap_or_else(|| panic!("[{ctx}] 缺少原语 {name}"));
    parse_oklch(v).unwrap_or_else(|| panic!("[{ctx}] {name} 必须是字面 oklch()，实际：{v}"))
}

const SURFACES: [&str; 5] = ["--bg-0", "--bg-1", "--surface-1", "--surface-2", "--surface-3"];
const COMBOS: [(Theme, Accent); 4] = [
    (Theme::Light, Accent::Litmus),
    (Theme::Light, Accent::Blue),
    (Theme::Dark, Accent::Litmus),
    (Theme::Dark, Accent::Blue),
];

// ───────────── 测试 ─────────────

#[test]
fn surfaces_form_a_visible_monotonic_ladder() {
    for theme in [Theme::Light, Theme::Dark] {
        let p = palette(theme, Accent::Litmus);
        let ctx = format!("{theme:?}");
        let lum: Vec<f64> = SURFACES.iter().map(|s| luminance(color(&p, s, &ctx))).collect();
        for w in lum.windows(2) {
            assert!(w[1] > w[0], "[{ctx}] 表面阶梯必须逐级变亮：{lum:?}");
        }
        // 暗色是主场景：相邻级差 ≥1.08:1 才能在截图里分辨（亮色靠阴影与边框补足，不强求）
        if theme == Theme::Dark {
            for (i, w) in lum.windows(2).enumerate() {
                let step = (w[1] + 0.05) / (w[0] + 0.05);
                assert!(step >= 1.08, "[dark] {} → {} 级差 {step:.3} 太小", SURFACES[i], SURFACES[i + 1]);
            }
        }
    }
}

#[test]
fn text_and_accents_pass_wcag_aa_on_every_surface() {
    let mut report = Vec::new();
    for (theme, accent) in COMBOS {
        let ctx = format!("{theme:?}/{accent:?}");
        let p = palette(theme, accent);
        let body = [
            "--text-1", "--text-2", "--text-3", "--accent", "--accent-strong", "--accent-2",
            "--ok", "--warn", "--danger", "--info",
        ];
        for s in SURFACES {
            let bg = color(&p, s, &ctx);
            for f in body {
                let r = contrast(color(&p, f, &ctx), bg);
                if r < 4.5 {
                    report.push(format!("[{ctx}] {f} on {s} = {r:.2} < 4.5"));
                }
            }
            let r = contrast(color(&p, "--text-4", &ctx), bg);
            if r < 3.0 {
                report.push(format!("[{ctx}] --text-4 on {s} = {r:.2} < 3.0"));
            }
        }
        // 彩色文字压在自己的 -soft 底上（徽章/标签/选中态）：以最常见的承载面合成
        let host = color(&p, if theme == Theme::Dark { "--surface-1" } else { "--surface-3" }, &ctx);
        for (fg, soft) in [
            ("--accent", "--accent-soft"),
            ("--accent-2", "--accent-2-soft"),
            ("--ok", "--ok-soft"),
            ("--warn", "--warn-soft"),
            ("--danger", "--danger-soft"),
            ("--info", "--info-soft"),
        ] {
            let bg = over(color(&p, soft, &ctx), host);
            let r = contrast(color(&p, fg, &ctx), bg);
            if r < 4.5 {
                report.push(format!("[{ctx}] {fg} on {soft} = {r:.2} < 4.5"));
            }
        }
        for (fg, bg) in [("--on-accent", "--accent"), ("--on-accent-2", "--accent-2"), ("--on-accent", "--accent-strong"), ("--on-danger", "--danger")] {
            let r = contrast(color(&p, fg, &ctx), color(&p, bg, &ctx));
            if r < 4.5 {
                report.push(format!("[{ctx}] {fg} on {bg} = {r:.2} < 4.5"));
            }
        }
    }
    assert!(report.is_empty(), "WCAG AA 未通过：\n{}", report.join("\n"));
}

#[test]
fn litmus_red_never_masquerades_as_danger() {
    // 石蕊红只做 AI/品牌显色；与危险色色相至少相隔 12°，免得"显色"被读成"报错"
    let hue = |v: &str| -> f64 { v.split_whitespace().nth(2).unwrap().trim_end_matches(')').parse().unwrap() };
    for theme in [Theme::Light, Theme::Dark] {
        let p = palette(theme, Accent::Litmus);
        let (a, d) = (hue(&p["--accent-2"]), hue(&p["--danger"]));
        let gap = (a - d).abs().min(360.0 - (a - d).abs());
        assert!(gap >= 12.0, "[{theme:?}] accent-2 h={a} 与 danger h={d} 太近");
    }
}

/// 在 `hay` 里找疑似色值的 hex / rgb() / hsl()。CSS 选择器里的 #id 不算（前一个字符是选择器语境）。
fn color_literals(hay: &str) -> Vec<String> {
    let b = hay.as_bytes();
    let mut hits = Vec::new();
    for (i, &c) in b.iter().enumerate() {
        if c == b'#' {
            let prev = if i == 0 { b' ' } else { b[i - 1] };
            // 值语境：冒号/逗号/括号/空格/等号引号之后；选择器 #id 前面是 { } , 换行 或行首空白+选择器
            if !matches!(prev, b':' | b'(' | b',' | b'"' | b'\'' | b' ') {
                continue;
            }
            let n = b[i + 1..].iter().take_while(|x| x.is_ascii_hexdigit()).count();
            let next = b.get(i + 1 + n).copied().unwrap_or(b' ');
            if matches!(n, 3 | 4 | 6 | 8) && !(next.is_ascii_alphanumeric() || next == b'-' || next == b'_') {
                // 选择器里 "a #abc{" 这种以空格开头的 id：看后面是不是紧跟 { 或 , 或 空格+{
                let tail = hay[i + 1 + n..].trim_start();
                if matches!(prev, b' ' | b',') && (tail.starts_with('{') || tail.starts_with(',') || tail.starts_with('.') || tail.starts_with(':')) {
                    continue;
                }
                hits.push(hay[i..i + 1 + n].to_string());
            }
        }
    }
    for f in ["rgb(", "rgba(", "hsl(", "hsla("] {
        let mut from = 0;
        while let Some(j) = hay[from..].find(f) {
            let at = from + j;
            let prev = if at == 0 { b' ' } else { b[at - 1] };
            if !prev.is_ascii_alphanumeric() && prev != b'-' {
                hits.push(hay[at..(at + 24).min(hay.len())].to_string());
            }
            from = at + f.len();
        }
    }
    hits
}

#[test]
fn zero_hex_in_tokens_and_shell_chrome() {
    let t = color_literals(&strip_comments(TOKENS));
    assert!(t.is_empty(), "tokens.css 出现非 oklch 色值：{t:?}");
    // index.html：<style> + 静态标记（到主脚本为止）。主脚本里的导出管线面向不支持 oklch 的外部渲染器
    // （公众号内联、导出 HTML 代码高亮），属于"出口格式"，不在壳的配色契约内。
    let end = INDEX.find("<script data-cfasync").expect("主脚本标记");
    let shell = strip_comments(&INDEX[..end]);
    let s = color_literals(&shell);
    assert!(s.is_empty(), "index.html 壳（样式与标记）出现非 oklch 色值：{s:?}");
}

#[test]
fn every_legacy_token_still_resolves() {
    // 旧名是预设/插件的 ABI：一个都不能少
    let all: HashMap<String, String> = blocks(TOKENS).into_iter().flat_map(|(_, d)| d).collect();
    for k in [
        "--bg", "--bg-soft", "--bg-deep", "--surface", "--surface-strong", "--fg", "--muted", "--faint",
        "--border", "--border-strong", "--border-soft", "--hover", "--hover-strong", "--accent",
        "--accent-strong", "--accent-soft", "--on-accent", "--ok", "--ok-soft", "--warn", "--warn-soft",
        "--danger", "--danger-soft", "--glass", "--glass-bright", "--glass-border", "--blob-a", "--blob-b",
        "--blob-c", "--shadow", "--shadow-sm", "--node-bg", "--node-bg-active", "--node-shadow", "--chat-bg",
        "--grid-dot", "--edge", "--edge-soft", "--col-bg", "--col-head", "--type-doc", "--type-capture",
        "--type-asset", "--type-query", "--axis",
    ] {
        assert!(all.contains_key(k), "旧 token {k} 丢失——预设与插件会静默失效");
    }
}
