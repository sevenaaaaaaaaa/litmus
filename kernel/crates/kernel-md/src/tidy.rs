//! 魔法排版（规则版）：识别并修复「复制粘贴来的」常见格式问题。
//!
//! 设计约束：
//! - **只动格式，不动内容**：每条规则只增删空白、标记符号与换行，不改写文字；
//! - **代码块不碰**：已有 fence 内容原样保留（只补语言标记 / 重排 JSON 缩进）；
//! - **确定性**：同一输入永远同一输出，二次运行不再产生变化（幂等）；
//! - 每条修复按类别计数，供界面展示「发现了什么问题」。
//!
//! 覆盖的问题（kind → 中文标签见 [`label_of`]）：
//! invisible 隐藏字符 / artifact 文档残留 / spaces 多余空格 / cjk-spaces 字间空格 /
//! blank-lines 大面积空行 / line-breaks 多行挤成一段 / hard-wrap 句子被硬换行打断 /
//! run-on 条目挤在一段 / heading 标题丢失标记 / list-marker 列表符号不规范 /
//! list-number 序号不连续 / table 表格未识别或错乱 / json JSON 未成块 /
//! code 代码未成块 / code-lang 代码块缺语言 / diagram 流程图未渲染 / math 公式未渲染 /
//! link 链接失效 / emphasis 粗体斜体失效 / fullwidth 全角数字字母 / title 缺少标题 / pangu 中英文间距

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TidyIssue {
    pub kind: String,
    pub label: String,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct TidyOptions {
    /// 首行短句提升为一级标题（文档没有 H1 时）
    pub promote_title: bool,
    /// 中英文之间补空格（盘古之白）；风格偏好，默认关
    pub pangu: bool,
}

impl Default for TidyOptions {
    fn default() -> Self {
        TidyOptions { promote_title: true, pangu: false }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TidyReport {
    pub markdown: String,
    pub issues: Vec<TidyIssue>,
    pub total: usize,
    pub changed: bool,
}

const LABELS: &[(&str, &str)] = &[
    ("line-breaks", "多行挤成一段"),
    ("run-on", "条目挤在一段"),
    ("heading", "标题丢失标记"),
    ("title", "缺少文档标题"),
    ("table", "表格未识别或错乱"),
    ("list-number", "序号不连续"),
    ("list-marker", "列表符号不规范"),
    ("json", "JSON 未成块"),
    ("code", "代码未成块"),
    ("code-lang", "代码块缺语言标记"),
    ("diagram", "流程图未渲染"),
    ("math", "公式未渲染"),
    ("link", "链接失效"),
    ("emphasis", "粗体/斜体失效"),
    ("cjk-spaces", "文字之间多余空格"),
    ("spaces", "多余空格"),
    ("blank-lines", "大面积空行"),
    ("hard-wrap", "句子被硬换行打断"),
    ("invisible", "隐藏字符 / 不换行空格"),
    ("artifact", "文档残留（页码等）"),
    ("fullwidth", "全角数字字母"),
    ("pangu", "中英文间距"),
];

pub fn label_of(kind: &str) -> &'static str {
    LABELS.iter().find(|(k, _)| *k == kind).map(|(_, l)| *l).unwrap_or("格式问题")
}

#[derive(Default)]
struct Ctx {
    hits: BTreeMap<&'static str, usize>,
}
impl Ctx {
    fn hit(&mut self, k: &'static str, n: usize) {
        if n > 0 {
            *self.hits.entry(k).or_default() += n;
        }
    }
}

/// 一行 + 是否受保护（fence / frontmatter / raw 容器内，规则不碰）。
#[derive(Clone, Debug)]
struct L {
    t: String,
    code: bool,
}
fn txt(s: impl Into<String>) -> L {
    L { t: s.into(), code: false }
}
fn cod(s: impl Into<String>) -> L {
    L { t: s.into(), code: true }
}

/// 只诊断不修改。
pub fn diagnose(md: &str) -> Vec<TidyIssue> {
    tidy(md, &TidyOptions::default()).issues
}

pub fn tidy(md: &str, opts: &TidyOptions) -> TidyReport {
    let mut cx = Ctx::default();
    let src = normalize(md, &mut cx);
    let mut lines = split_protected(&src, &mut cx);
    protect_html_and_continuations(&mut lines);
    lines = remove_artifacts(lines, &mut cx);
    lines = detect_blocks(lines, &mut cx);
    let levels = heading_levels(&lines);
    lines = lines
        .into_iter()
        .flat_map(|l| if l.code { vec![l] } else { fix_line(l.t, &levels, opts, &mut cx) })
        .collect();
    lines = split_run_on(lines, &mut cx);
    lines = fix_paragraph_breaks(lines, &mut cx);
    lines = fix_ordered_lists(lines, &mut cx);
    if opts.promote_title {
        lines = promote_title(lines, &mut cx);
    }
    let out = finish(lines, &mut cx);
    let issues: Vec<TidyIssue> = LABELS
        .iter()
        .filter_map(|(k, l)| cx.hits.get(k).map(|n| TidyIssue { kind: k.to_string(), label: l.to_string(), count: *n }))
        .collect();
    let total = issues.iter().map(|i| i.count).sum();
    let changed = out != md;
    TidyReport { markdown: out, issues, total, changed }
}

// ───────────────────────── 字符类 ─────────────────────────

fn is_han(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' | '\u{f900}'..='\u{faff}')
}
fn is_cjk_punct(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{303f}' | '\u{ff01}'..='\u{ff0f}' | '\u{ff1a}'..='\u{ff20}' | '\u{ff3b}'..='\u{ff40}' | '\u{ff5b}'..='\u{ff65}')
        || "“”‘’…—·".contains(c)
}
fn is_cjk(c: char) -> bool {
    is_han(c) || is_cjk_punct(c)
}
/// 两侧都不该有空格的全角标点（· — … 引号这类分隔/排印符号不算）
const END_P: &str = "，。、；：！？）】」』》";
const OPEN_P: &str = "（【「『《";
fn is_tight(c: char) -> bool {
    is_han(c) || END_P.contains(c) || OPEN_P.contains(c)
}
fn has_han(s: &str) -> bool {
    s.chars().any(is_han)
}
const SENT_END: &str = "。！？!?；;：:…」』”\"）)】.";
fn ends_sentence(s: &str) -> bool {
    s.trim_end().chars().last().map_or(false, |c| SENT_END.contains(c))
}
fn width(s: &str) -> usize {
    s.chars().map(|c| if is_cjk(c) { 2 } else { 1 }).sum()
}

// ───────────────────────── 0. 规范化 ─────────────────────────

const KANGXI: &str = "一丨丶丿乙亅二亠人儿入八冂冖冫几凵刀力勹匕匚匸十卜卩厂厶又口囗土士夂夊夕大女子宀寸小尢尸屮山巛工己巾干幺广廴廾弋弓彐彡彳心戈戶手支攴文斗斤方无日曰月木欠止歹殳毋比毛氏气水火爪父爻爿片牙牛犬玄玉瓜瓦甘生用田疋疒癶白皮皿目矛矢石示禸禾穴立竹米糸缶网羊羽老而耒耳聿肉臣自至臼舌舛舟艮色艸虍虫血行衣襾見角言谷豆豕豸貝赤走足身車辛辰辵邑酉釆里金長門阜隶隹雨靑非面革韋韭音頁風飛食首香馬骨高髟鬥鬯鬲鬼魚鳥鹵鹿麥麻黃黍黑黹黽鼎鼓鼠鼻齊齒龍龜龠";
/// 部首补充区里的简化部首：成对出现（部首, 常用字）
const RADICAL_SUP: &str = "⻅见⻆角⻉贝⻋车⻔门⻓长⻚页⻛风⻜飞⻝食⻢马⻦鸟⻤鬼⻥鱼⻨麦⻩黄⻬齐⻮齿⻯龙⻳龟⺟母⺠民⻊足⻈讠⻏阝⺮竹⺈刀⺊卜⺌小⺍小⺕彐⺗心⺡氵⺢水⺣灬⺤爫⺨犭⺶羊⺻聿⻂衤⻎辶";

fn normalize(md: &str, cx: &mut Ctx) -> String {
    let s = md.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    let mut n = 0;
    let mut line_start = true;
    for c in s.chars() {
        match c {
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' | '\u{00ad}' => n += 1,
            '\u{00a0}' | '\u{2002}' | '\u{2003}' | '\u{2009}' | '\u{202f}' => {
                n += 1;
                out.push(' ');
            }
            // 全角空格：行首当缩进去掉，行内当普通空格
            '\u{3000}' => {
                n += 1;
                if !line_start {
                    out.push(' ');
                }
            }
            // PDF 抽出的康熙部首 / 部首补充（⼀ ⽣ ⻚）→ 常用字
            c if ('\u{2f00}'..='\u{2fd5}').contains(&c) => {
                n += 1;
                out.push(KANGXI.chars().nth(c as usize - 0x2f00).unwrap_or(c));
            }
            c if ('\u{2e80}'..='\u{2ef3}').contains(&c) && RADICAL_SUP.contains(c) => {
                n += 1;
                let k = RADICAL_SUP.chars().position(|x| x == c).unwrap_or(0);
                out.push(RADICAL_SUP.chars().nth(k + 1).unwrap_or(c));
            }
            '\u{000c}' | '\u{000b}' => {
                n += 1;
                out.push('\n');
            }
            c if c.is_control() && c != '\n' && c != '\t' => n += 1,
            c => out.push(c),
        }
        if c == '\n' {
            line_start = true;
        } else if c != '\u{3000}' && c != ' ' {
            line_start = false;
        }
    }
    cx.hit("invisible", n);
    out
}

// ───────────────────────── 1. 受保护区 ─────────────────────────

fn mermaid_head(s: &str) -> bool {
    let t = s.trim_start();
    const HEADS: &[&str] = &[
        "graph TD", "graph TB", "graph LR", "graph RL", "graph BT", "flowchart ", "sequenceDiagram", "classDiagram",
        "stateDiagram", "erDiagram", "gantt", "pie", "mindmap", "journey", "gitGraph", "timeline", "quadrantChart",
    ];
    HEADS.iter().any(|h| t == h.trim() || t.starts_with(h))
}

fn split_protected(src: &str, cx: &mut Ctx) -> Vec<L> {
    let raw: Vec<&str> = src.split('\n').collect();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    // frontmatter
    if raw.first().map_or(false, |l| l.trim() == "---") {
        if let Some(end) = raw.iter().skip(1).position(|l| l.trim() == "---") {
            for l in &raw[..end + 2] {
                out.push(cod(*l));
            }
            i = end + 2;
        }
    }
    while i < raw.len() {
        let line = raw[i];
        let t = line.trim_start();
        if t.starts_with("<!-- tc:block raw") {
            out.push(cod(line));
            i += 1;
            while i < raw.len() {
                out.push(cod(raw[i]));
                i += 1;
                if raw[i - 1].trim() == "tc:block raw -->" {
                    break;
                }
            }
            continue;
        }
        let fch = if t.starts_with("```") { Some('`') } else if t.starts_with("~~~") { Some('~') } else { None };
        if let Some(fc) = fch {
            let flen = t.chars().take_while(|c| *c == fc).count();
            let lang = t[flen..].trim().to_string();
            let mut body = Vec::new();
            let mut j = i + 1;
            let mut closed = false;
            while j < raw.len() {
                let tj = raw[j].trim();
                if tj.len() >= flen && tj.chars().all(|c| c == fc) {
                    closed = true;
                    break;
                }
                body.push(raw[j].to_string());
                j += 1;
            }
            let fence = "`".repeat(flen.max(3));
            let mut lang2 = lang.clone();
            let mut body2 = body.clone();
            if lang.is_empty() {
                let joined = body.join("\n");
                if let Some(p) = json_pretty(&joined) {
                    lang2 = "json".into();
                    body2 = p.lines().map(String::from).collect();
                } else if body.iter().find(|l| !l.trim().is_empty()).map_or(false, |l| mermaid_head(l)) {
                    lang2 = "mermaid".into();
                } else if let Some(g) = guess_lang(&body) {
                    lang2 = g.into();
                }
                if !lang2.is_empty() {
                    cx.hit("code-lang", 1);
                }
            } else if lang.eq_ignore_ascii_case("json") {
                if let Some(p) = json_pretty(&body.join("\n")) {
                    if body.iter().filter(|l| !l.trim().is_empty()).count() == 1 && p.lines().count() > 1 {
                        body2 = p.lines().map(String::from).collect();
                        cx.hit("json", 1);
                    }
                }
            }
            if fc == '~' {
                cx.hit("code", 1);
            }
            out.push(cod(format!("{fence}{lang2}")));
            for b in body2 {
                out.push(cod(b));
            }
            out.push(cod(fence));
            i = if closed { j + 1 } else { j };
            continue;
        }
        out.push(txt(line));
        i += 1;
    }
    out
}

/// HTML 块（以 <标签 开头，到空行为止）与列表项的缩进续行：作者有意为之，规则不碰。
fn protect_html_and_continuations(lines: &mut [L]) {
    let mut in_html = false;
    let mut in_list = false;
    for l in lines.iter_mut() {
        if l.code {
            in_html = false;
            in_list = false;
            continue;
        }
        let t = l.t.trim_start();
        if t.is_empty() {
            in_html = false;
            continue;
        }
        let html_start = t.starts_with('<') && t.chars().nth(1).map_or(false, |c| c.is_ascii_alphabetic() || c == '/' || c == '!') && t.contains('>');
        if html_start || in_html {
            in_html = true;
            l.code = true;
            continue;
        }
        let indented = l.t.starts_with("  ") || l.t.starts_with('\t');
        let is_item = t.starts_with("- ") || t.starts_with("* ") || ordered_num(t).is_some();
        if indented && in_list && !is_item {
            l.code = true;
            continue;
        }
        in_list = is_item || (indented && in_list);
    }
}

// ───────────────────────── JSON ─────────────────────────

/// 合法 JSON → 保序 2 空格缩进；非 JSON / 太小（无结构）返回 None。
fn json_pretty(s: &str) -> Option<String> {
    let t = s.trim();
    if !(t.starts_with('{') && t.ends_with('}') || t.starts_with('[') && t.ends_with(']')) || t.len() < 2 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    let structured = match &v {
        serde_json::Value::Object(m) => !m.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty() && a.iter().any(|x| x.is_object() || x.is_array()),
        _ => false,
    };
    if !structured {
        return None;
    }
    // 词法重排（保持键序与原数值字面量）
    let mut out = String::new();
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut esc = false;
    let chars: Vec<char> = t.chars().collect();
    let nl = |out: &mut String, d: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(d));
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                // 空容器保持紧凑
                let mut k = i + 1;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if k < chars.len() && (chars[k] == '}' || chars[k] == ']') {
                    out.push(c);
                    out.push(chars[k]);
                    i = k + 1;
                    continue;
                }
                out.push(c);
                depth += 1;
                nl(&mut out, depth);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                nl(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                nl(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
        i += 1;
    }
    Some(out)
}

fn guess_lang(body: &[String]) -> Option<&'static str> {
    let s = body.join("\n");
    let has = |k: &str| s.contains(k);
    if s.trim_start().starts_with("<") && has("</") {
        return Some("html");
    }
    if (has("SELECT ") || has("select ")) && (has(" FROM ") || has(" from ")) {
        return Some("sql");
    }
    if has("def ") && has(":") || has("import ") && !has(";") && !has("{") {
        return Some("python");
    }
    if has("fn ") && (has("let ") || has("->")) {
        return Some("rust");
    }
    if has("function ") || has("const ") || has("=> ") || has("console.") {
        return Some("javascript");
    }
    if body.iter().all(|l| l.trim().is_empty() || l.trim_start().starts_with("$ ") || l.trim_start().starts_with('#')) {
        return Some("bash");
    }
    None
}

// ───────────────────────── 2. 文档残留 ─────────────────────────

fn is_page_marker(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() {
        return false;
    }
    let digits_only = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    // 第 3 页 / 第3页 共10页 / 第 3 页，共 10 页
    if let Some(rest) = t.strip_prefix('第') {
        let parts: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
        if let Some((n, tail)) = parts.split_once('页') {
            if digits_only(n) {
                let tail = tail.trim_start_matches(|c| c == '，' || c == ',' || c == '/');
                if tail.is_empty() {
                    return true;
                }
                if let Some(m) = tail.strip_prefix('共').and_then(|x| x.strip_suffix('页')) {
                    return digits_only(m);
                }
            }
        }
    }
    // Page 3 of 10 / - 3 -
    let low = t.to_ascii_lowercase();
    if let Some(r) = low.strip_prefix("page ") {
        let w: Vec<&str> = r.split_whitespace().collect();
        return (w.len() == 1 && digits_only(w[0])) || (w.len() == 3 && digits_only(w[0]) && w[1] == "of" && digits_only(w[2]));
    }
    if let Some(r) = t.strip_prefix("- ").and_then(|x| x.strip_suffix(" -")) {
        return digits_only(r.trim());
    }
    false
}

fn remove_artifacts(lines: Vec<L>, cx: &mut Ctx) -> Vec<L> {
    let mut n = 0;
    let out = lines
        .into_iter()
        .filter(|l| {
            if !l.code && is_page_marker(&l.t) {
                n += 1;
                false
            } else {
                true
            }
        })
        .collect();
    cx.hit("artifact", n);
    out
}

// ───────────────────────── 3. 成块识别 ─────────────────────────

/// 按空行切组（只看未受保护行），每组整体判定：表格 / JSON / 流程图 / 公式 / 代码。
fn detect_blocks(lines: Vec<L>, cx: &mut Ctx) -> Vec<L> {
    let mut out: Vec<L> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].code || lines[i].t.trim().is_empty() {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        let mut j = i;
        while j < lines.len() && !lines[j].code && !lines[j].t.trim().is_empty() {
            j += 1;
        }
        let group: Vec<String> = lines[i..j].iter().map(|l| l.t.clone()).collect();
        let prev_blank = i == 0 || lines[i - 1].t.trim().is_empty() || lines[i - 1].code;
        match classify_group(&group, prev_blank, cx) {
            Some(repl) => out.extend(repl),
            None => out.extend(embedded_tables(&group, cx)),
        }
        i = j;
    }
    out
}

/// 组内夹着的制表符表格（标题 + 正文 + 粘贴的表格挤在一起）。
fn mermaid_body(l: &str) -> bool {
    let t = l.trim();
    if t.is_empty() {
        return false;
    }
    const ARROWS: &[&str] = &["-->", "---", "-.->", "==>", "->>", "-->>", "--x", "--o", "-.-", ":::", "->"];
    const KW: &[&str] = &["subgraph", "end", "participant", "actor", "style ", "classDef", "class ", "click ", "section", "title ", "note ", "Note ", "loop", "alt ", "else", "opt ", "dateFormat", "linkStyle"];
    if ARROWS.iter().any(|a| t.contains(a)) || KW.iter().any(|k| t.starts_with(k)) {
        return true;
    }
    // A[文字] / B(文字) / C{文字} 节点声明
    let id: String = t.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    !id.is_empty() && t[id.len()..].starts_with(|c| c == '[' || c == '(' || c == '{') && t.ends_with(|c| c == ']' || c == ')' || c == '}')
}

/// 组内夹着的表格 / JSON / 流程图（标题、正文、粘贴物挤在一起没有空行）。
fn embedded_tables(g: &[String], cx: &mut Ctx) -> Vec<L> {
    let mut out = Vec::new();
    let mut k = 0;
    while k < g.len() {
        let t = g[k].trim();
        // JSON：从 { / [ 起累积到能解析为止
        if t.starts_with('{') || t.starts_with('[') {
            let mut acc = String::new();
            let mut hit = None;
            for e in k..g.len().min(k + 400) {
                acc.push_str(&g[e]);
                acc.push('\n');
                if let Some(p) = json_pretty(&acc) {
                    hit = Some((e, p));
                    break;
                }
            }
            if let Some((e, p)) = hit {
                cx.hit("json", 1);
                out.extend(fenced("json", p.lines().map(String::from)));
                k = e + 1;
                continue;
            }
        }
        if mermaid_head(t) {
            let mut e = k + 1;
            while e < g.len() && mermaid_body(&g[e]) {
                e += 1;
            }
            if e > k + 1 {
                cx.hit("diagram", 1);
                out.extend(fenced("mermaid", g[k..e].iter().cloned()));
                k = e;
                continue;
            }
        }
        if g[k].contains('\t') {
            let mut e = k;
            while e < g.len() && g[e].contains('\t') {
                e += 1;
            }
            if e - k >= 2 {
                if let Some(rows) = table_rows(&g[k..e]) {
                    cx.hit("table", 1);
                    out.extend(md_table(&rows));
                    k = e;
                    continue;
                }
            }
            out.extend(g[k..e].iter().map(|l| txt(l.clone())));
            k = e;
            continue;
        }
        out.push(txt(g[k].clone()));
        k += 1;
    }
    out
}

fn fenced(lang: &str, body: impl IntoIterator<Item = String>) -> Vec<L> {
    let mut v = vec![txt(""), cod(format!("```{lang}"))];
    v.extend(body.into_iter().map(cod));
    v.push(cod("```"));
    v.push(txt(""));
    v
}

fn classify_group(g: &[String], prev_blank: bool, cx: &mut Ctx) -> Option<Vec<L>> {
    let joined = g.join("\n");
    let first = g[0].trim();

    // 已是结构化 Markdown 的组：只做表格体检
    if first.starts_with('|') || first.starts_with('｜') {
        return fix_pipe_table(g, cx);
    }
    if first.starts_with('#') || first.starts_with('>') {
        return None;
    }

    // JSON（单行压缩或多行）
    if let Some(p) = json_pretty(&joined) {
        cx.hit("json", 1);
        return Some(fenced("json", p.lines().map(String::from)));
    }
    // 流程图
    if mermaid_head(first) && g[1..].iter().all(|l| mermaid_body(l)) {
        cx.hit("diagram", 1);
        return Some(fenced("mermaid", g.iter().cloned()));
    }
    // 公式：\[ … \] → $$；纯 LaTeX 行 → $$
    if let Some(m) = display_math(&joined) {
        cx.hit("math", 1);
        let mut v = vec![txt(""), txt("$$")];
        v.extend(m.lines().map(|l| txt(l.trim())));
        v.push(txt("$$"));
        v.push(txt(""));
        return Some(v);
    }
    // 表格：制表符 / 多空格对齐 / 管道无分隔行 / CSV
    if let Some(rows) = table_rows(g) {
        cx.hit("table", 1);
        return Some(md_table(&rows));
    }
    // 缩进代码（中文缩进段落除外）
    if prev_blank && g.iter().all(|l| l.starts_with("    ") || l.starts_with('\t')) {
        if has_han(&joined) {
            cx.hit("spaces", g.len());
            return Some(g.iter().map(|l| txt(l.trim_start())).collect());
        }
        cx.hit("code", 1);
        let body: Vec<String> = g.iter().map(|l| l.strip_prefix("    ").or_else(|| l.strip_prefix('\t')).unwrap_or(l).to_string()).collect();
        let lang = guess_lang(&body).unwrap_or("");
        return Some(fenced(lang, body));
    }
    // 命令行 / 明显的代码
    if looks_like_code(g) {
        cx.hit("code", 1);
        let body: Vec<String> = g.to_vec();
        let lang = guess_lang(&body).unwrap_or("");
        return Some(fenced(lang, body));
    }
    None
}

fn display_math(s: &str) -> Option<String> {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix("\\[").and_then(|x| x.strip_suffix("\\]")) {
        return Some(inner.trim().to_string());
    }
    if t.starts_with("$$") || t.contains('$') || has_han(t) || t.lines().count() > 6 {
        return None;
    }
    const CMDS: &[&str] = &["\\frac", "\\sum", "\\int", "\\sqrt", "\\alpha", "\\beta", "\\theta", "\\lambda", "\\pi", "\\cdot", "\\times", "\\leq", "\\geq", "\\infty", "\\partial", "\\begin{", "\\mathbf", "\\left", "\\prod", "\\lim"];
    let n = CMDS.iter().filter(|c| t.contains(*c)).count();
    let words = t.split_whitespace().filter(|w| w.chars().all(|c| c.is_ascii_alphabetic()) && w.len() > 3).count();
    if n >= 1 && words <= 2 && (t.contains('=') || t.contains('^') || t.contains('_') || n >= 2) {
        return Some(t.to_string());
    }
    None
}

fn looks_like_code(g: &[String]) -> bool {
    if g.len() < 2 || has_han(&g.join("")) {
        return false;
    }
    if g.iter().all(|l| l.trim_start().starts_with("$ ") || l.trim_start().starts_with(">>> ")) {
        return true;
    }
    let codeish = g
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.ends_with(';') || t.ends_with('{') || t == "}" || t.ends_with("};") || t.ends_with("):")
                || ["import ", "from ", "def ", "class ", "function ", "const ", "let ", "var ", "return ", "#include", "public ", "fn ", "SELECT ", "if (", "for ("]
                    .iter()
                    .any(|k| t.starts_with(k))
        })
        .count();
    codeish * 10 >= g.len() * 6
}

// ── 表格 ──

fn split_cells(line: &str, sep: &str) -> Vec<String> {
    line.split(sep).map(|c| c.trim().to_string()).collect()
}

fn split_multi_space(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut spaces = 0;
    for c in line.trim().chars() {
        if c == ' ' {
            spaces += 1;
            continue;
        }
        if spaces >= 2 {
            cells.push(std::mem::take(&mut cur));
        } else if spaces == 1 {
            cur.push(' ');
        }
        spaces = 0;
        cur.push(c);
    }
    if !cur.is_empty() {
        cells.push(cur);
    }
    cells
}

fn table_rows(g: &[String]) -> Option<Vec<Vec<String>>> {
    if g.len() < 2 {
        return None;
    }
    // 制表符（Excel / 网页表格粘贴）
    if g.iter().all(|l| l.contains('\t')) {
        let rows: Vec<Vec<String>> = g.iter().map(|l| split_cells(l.trim_end_matches('\t'), "\t")).collect();
        let n = rows[0].len();
        if n >= 2 && rows.iter().all(|r| r.len() == n) {
            return Some(rows);
        }
    }
    // 管道符但缺分隔行（含全角｜）
    if g.iter().all(|l| l.contains('|') || l.contains('｜')) && !g.iter().any(|l| is_sep_row(l)) {
        let rows: Vec<Vec<String>> = g.iter().map(|l| pipe_cells(l)).collect();
        let n = rows[0].len();
        if n >= 2 && rows.iter().all(|r| r.len() == n) {
            return Some(rows);
        }
    }
    // 多空格对齐（至少 3 行、各行列数一致、单元格都不长）
    if g.len() >= 3 {
        let rows: Vec<Vec<String>> = g.iter().map(|l| split_multi_space(l)).collect();
        let n = rows[0].len();
        if n >= 2 && rows.iter().all(|r| r.len() == n) && rows.iter().flatten().all(|c| width(c) <= 40) {
            return Some(rows);
        }
    }
    // CSV：≥3 行 ≥3 列，无句读，单元格短
    if g.len() >= 3 && !g.iter().any(|l| l.contains('。') || l.contains("， ")) {
        let rows: Vec<Vec<String>> = g.iter().map(|l| split_cells(l, ",")).collect();
        let n = rows[0].len();
        if n >= 3 && rows.iter().all(|r| r.len() == n) && rows.iter().flatten().all(|c| !c.is_empty() && width(c) <= 30) {
            return Some(rows);
        }
    }
    None
}

fn pipe_cells(l: &str) -> Vec<String> {
    let t = l.trim().replace('｜', "|");
    let t = t.trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_sep_row(l: &str) -> bool {
    let t = l.trim().replace('｜', "|");
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '：' | '—'))
}

fn md_table(rows: &[Vec<String>]) -> Vec<L> {
    let n = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let fmt = |r: &Vec<String>| {
        let mut cells: Vec<String> = r.iter().map(|c| c.replace('|', "\\|")).collect();
        cells.resize(n, String::new());
        format!("| {} |", cells.join(" | "))
    };
    let mut v = vec![txt("")];
    v.push(txt(fmt(&rows[0])));
    v.push(txt(format!("|{}", " --- |".repeat(n))));
    for r in &rows[1..] {
        v.push(txt(fmt(r)));
    }
    v.push(txt(""));
    v
}

/// 已有管道表格：补分隔行、列数对齐、全角｜。
fn fix_pipe_table(g: &[String], cx: &mut Ctx) -> Option<Vec<L>> {
    if g.len() < 2 {
        return None;
    }
    let has_sep = g.len() >= 2 && is_sep_row(&g[1]);
    let body: Vec<&String> = g.iter().enumerate().filter(|(k, _)| !(has_sep && *k == 1)).map(|(_, l)| l).collect();
    let rows: Vec<Vec<String>> = body.iter().map(|l| pipe_cells(l)).collect();
    let n = rows[0].len();
    // 行比表头多列：多半是单元格里含 | ，不动
    if rows.iter().any(|r| r.len() > n) {
        return None;
    }
    let ragged = rows.iter().any(|r| r.len() != n);
    let fullwidth = g.iter().any(|l| l.contains('｜'));
    let bad_sep = has_sep && pipe_cells(&g[1]).len() != n;
    if has_sep && !ragged && !fullwidth && !bad_sep && g.iter().all(|l| l.trim_start().starts_with('|')) {
        return None;
    }
    if n < 2 && !ragged {
        return None;
    }
    cx.hit("table", 1);
    let mut v = md_table(&rows);
    v.retain(|l| !l.t.is_empty());
    Some(v)
}

// ───────────────────────── 4. 标题层级 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum HPat {
    Chapter,
    CnNum,
    ParenCn,
    Dec2,
    Dec3,
}

const CN_NUM: &str = "一二三四五六七八九十百零〇";

fn heading_pattern(t: &str) -> Option<(HPat, usize)> {
    let t = t.trim();
    if t.is_empty() || width(t) > 60 || t.chars().count() < 2 {
        return None;
    }
    if t.trim_end().chars().last().map_or(false, |c| "。，,；;！？!?".contains(c)) {
        return None;
    }
    let chars: Vec<char> = t.chars().collect();
    // 第X章 / 第X节 / 第X部分
    if chars[0] == '第' {
        let k = chars[1..].iter().take_while(|c| CN_NUM.contains(**c) || c.is_ascii_digit()).count();
        if k > 0 && chars.get(1 + k).map_or(false, |c| "章节部篇讲".contains(*c)) {
            return Some((HPat::Chapter, 0));
        }
    }
    // 一、标题
    let k = chars.iter().take_while(|c| CN_NUM.contains(**c)).count();
    if k > 0 && chars.get(k) == Some(&'、') && chars.len() > k + 1 {
        return Some((HPat::CnNum, k + 1));
    }
    // （一）标题
    if matches!(chars[0], '（' | '(') {
        let k = chars[1..].iter().take_while(|c| CN_NUM.contains(**c)).count();
        if k > 0 && matches!(chars.get(1 + k), Some('）') | Some(')')) && chars.len() > k + 2 {
            return Some((HPat::ParenCn, 0));
        }
    }
    // 1.1 标题 / 1.1.1 标题（须有空格或中文紧随，排除 1.5 倍这类数字）
    let num: String = chars.iter().take_while(|c| c.is_ascii_digit() || **c == '.').collect();
    let dots = num.matches('.').count();
    if (1..=2).contains(&dots) && !num.ends_with('.') && num.split('.').all(|p| !p.is_empty() && p.len() <= 2) {
        let rest: String = chars[num.chars().count()..].iter().collect();
        if rest.starts_with(' ') && rest.trim().chars().next().map_or(false, |c| !c.is_ascii_digit()) && width(rest.trim()) <= 50 {
            return Some((if dots == 1 { HPat::Dec2 } else { HPat::Dec3 }, 0));
        }
    }
    None
}

/// 文档里出现了哪些编号体系 → 依次分配 ## / ### / ####。
fn heading_levels(lines: &[L]) -> Vec<(HPat, u8)> {
    let order = [HPat::Chapter, HPat::CnNum, HPat::ParenCn, HPat::Dec2, HPat::Dec3];
    let present: Vec<HPat> = order
        .iter()
        .copied()
        .filter(|p| lines.iter().any(|l| !l.code && heading_pattern(&l.t).map(|x| x.0) == Some(*p)))
        .collect();
    present.iter().enumerate().map(|(i, p)| (*p, (2 + i as u8).min(4))).collect()
}

// ───────────────────────── 5. 行内修复 ─────────────────────────

fn ordered_marker(t: &str) -> Option<(u32, usize)> {
    // 返回 (序号, 标记占用字节数)；支持 1. 1、 1） 1) 1． (1) （1） ①
    let chars: Vec<char> = t.chars().collect();
    if let Some(&c) = chars.first() {
        if ('①'..='⑳').contains(&c) {
            return Some((c as u32 - '①' as u32 + 1, c.len_utf8()));
        }
    }
    let (open, start) = match chars.first() {
        Some('(') | Some('（') => (true, 1),
        _ => (false, 0),
    };
    let k = chars[start..].iter().take_while(|c| c.is_ascii_digit()).count();
    if k == 0 || k > 3 {
        return None;
    }
    let n: u32 = chars[start..start + k].iter().collect::<String>().parse().ok()?;
    let after = chars.get(start + k)?;
    let ok = if open { matches!(after, ')' | '）') } else { matches!(after, '.' | '、' | '．' | ')' | '）') };
    if !ok {
        return None;
    }
    // 1.5 / 1.1 这类数字或小节号不是列表
    if !open && *after == '.' && chars.get(start + k + 1).map_or(false, |c| c.is_ascii_digit()) {
        return None;
    }
    if !open && *after == '．' && chars.get(start + k + 1).map_or(false, |c| c.is_ascii_digit()) {
        return None;
    }
    let bytes: usize = chars[..start + k + 1].iter().map(|c| c.len_utf8()).sum();
    Some((n, bytes))
}

const BULLETS: &str = "•●▪▫◦■□◆◇►▶➢✓✔·○";

fn fix_line(line: String, levels: &[(HPat, u8)], opts: &TidyOptions, cx: &mut Ctx) -> Vec<L> {
    let trimmed_end = line.trim_end().to_string();
    if trimmed_end.trim().is_empty() {
        return vec![txt("")];
    }
    let indent: String = trimmed_end.chars().take_while(|c| *c == ' ').collect();
    let mut t = trimmed_end.trim_start().to_string();

    // 表格行：只修全角
    if t.starts_with('|') {
        return vec![txt(trimmed_end)];
    }
    // $$ 行原样
    if t == "$$" {
        return vec![txt(t)];
    }

    // ## 标题缺空格
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (2..=6).contains(&hashes) && t[hashes..].chars().next().map_or(false, |c| !c.is_whitespace() && c != '#') {
        t = format!("{} {}", &t[..hashes], &t[hashes..]);
        cx.hit("heading", 1);
    }

    // 项目符号
    if let Some(c) = t.chars().next() {
        if BULLETS.contains(c) {
            let rest = t[c.len_utf8()..].trim_start().to_string();
            if !rest.is_empty() {
                t = format!("- {rest}");
                cx.hit("list-marker", 1);
            }
        } else if t.starts_with("+ ") {
            t = format!("- {}", &t[2..]);
            cx.hit("list-marker", 1);
        }
    }

    // 中文编号标题（独占一行、无句末标点）
    let mut heading_level: Option<u8> = None;
    if !t.starts_with('#') {
        if let Some((p, _)) = heading_pattern(&t) {
            if let Some((_, lv)) = levels.iter().find(|(q, _)| *q == p) {
                heading_level = Some(*lv);
            }
        }
    }

    // 有序列表标记规范化（标题模式优先）
    if heading_level.is_none() && !t.starts_with('#') {
        if let Some((n, len)) = ordered_marker(&t) {
            let rest = t[len..].trim_start();
            let canonical = t.starts_with(&format!("{n}. "));
            if !rest.is_empty() && !canonical {
                t = format!("{n}. {rest}");
                cx.hit("list-marker", 1);
            }
        }
    }

    // 行内：全角数字字母、字间空格、多空格、强调、链接、盘古
    let prefix_len = md_prefix_len(&t);
    t = format!("{}{}", &t[..prefix_len], fix_inline(&t[prefix_len..], opts, cx));

    if let Some(lv) = heading_level {
        cx.hit("heading", 1);
        let h = format!("{} {}", "#".repeat(lv as usize), t.trim());
        return vec![txt(""), txt(h), txt("")];
    }
    let keep_indent = if t.starts_with("- ") || ordered_marker(&t).is_some() { indent } else { String::new() };
    vec![txt(format!("{keep_indent}{t}"))]
}

/// 行首 Markdown 标记（"## " / "- " / "3. " / "> "）的字节长度——行内规则不碰它。
fn md_prefix_len(t: &str) -> usize {
    let h = t.chars().take_while(|c| *c == '#').count();
    if h > 0 && t[h..].starts_with(' ') {
        return h + 1;
    }
    if t.starts_with("- ") || t.starts_with("> ") || t.starts_with("* ") {
        return 2;
    }
    if let Some((a, _)) = t.split_once(". ") {
        if !a.is_empty() && a.len() <= 3 && a.chars().all(|c| c.is_ascii_digit()) {
            return a.len() + 2;
        }
    }
    0
}

fn fix_inline(s: &str, opts: &TidyOptions, cx: &mut Ctx) -> String {
    // 行内代码段不碰
    let mut out = String::new();
    for (k, seg) in s.split('`').enumerate() {
        if k > 0 {
            out.push('`');
        }
        if k % 2 == 1 {
            out.push_str(seg);
        } else {
            out.push_str(&fix_text_seg(seg, opts, cx));
        }
    }
    out
}

fn fix_text_seg(s: &str, opts: &TidyOptions, cx: &mut Ctx) -> String {
    let mut t = s.to_string();

    // 全角数字字母 → 半角
    let mut fw = 0;
    t = t
        .chars()
        .map(|c| {
            if ('０'..='９').contains(&c) || ('Ａ'..='Ｚ').contains(&c) || ('ａ'..='ｚ').contains(&c) {
                fw += 1;
                char::from_u32(c as u32 - 0xFEE0).unwrap_or(c)
            } else {
                c
            }
        })
        .collect();
    cx.hit("fullwidth", fw);

    // 字间空格：「每 个 文 字」——单空格夹在两个中文之间的占比过半才动
    let chars: Vec<char> = t.chars().collect();
    let mut spaced = 0;
    let mut adjacent = 0;
    for w in chars.windows(3) {
        if is_tight(w[0]) && w[1] == ' ' && is_tight(w[2]) {
            spaced += 1;
        }
    }
    for w in chars.windows(2) {
        if is_tight(w[0]) && is_tight(w[1]) {
            adjacent += 1;
        }
    }
    if spaced >= 3 && spaced * 2 >= spaced + adjacent {
        let mut o = String::new();
        let mut n = 0;
        for (k, c) in chars.iter().enumerate() {
            if *c == ' ' && k > 0 && k + 1 < chars.len() && is_tight(chars[k - 1]) && (is_tight(chars[k + 1]) || chars[k + 1] == ' ') {
                n += 1;
                continue;
            }
            o.push(*c);
        }
        t = o;
        cx.hit("cjk-spaces", n);
    }

    // 行内多空格 → 一个；中文与中文标点之间的空格去掉
    let chars: Vec<char> = t.chars().collect();
    let mut o = String::new();
    let mut n = 0;
    let mut k = 0;
    while k < chars.len() {
        let c = chars[k];
        if c == ' ' {
            let run = chars[k..].iter().take_while(|c| **c == ' ').count();
            let prev = if k > 0 { Some(chars[k - 1]) } else { None };
            let next = chars.get(k + run).copied();
            // 「文字 ，」「（ 文字」「。 下一句」：全角标点两侧的空格去掉
            let tight = next.map_or(false, |c| END_P.contains(c))
                || prev.map_or(false, |c| OPEN_P.contains(c))
                || (prev.map_or(false, |c| END_P.contains(c)) && next.map_or(false, is_han));
            if prev.is_some() && next.is_some() && tight {
                n += 1;
            } else if run >= 2 && prev.is_some() && next.is_some() {
                o.push(' ');
                n += 1;
            } else {
                for _ in 0..run {
                    o.push(' ');
                }
            }
            k += run;
            continue;
        }
        o.push(c);
        k += 1;
    }
    t = o;
    cx.hit("spaces", n);

    // 强调：** 文字 ** → **文字**
    let (t2, n) = fix_emphasis(&t);
    t = t2;
    cx.hit("emphasis", n);

    // 链接
    let (t2, n) = fix_links(&t);
    t = t2;
    cx.hit("link", n);

    // 行内公式：\( … \) → $…$
    if t.contains("\\(") && t.contains("\\)") {
        let n = t.matches("\\(").count();
        t = t.replace("\\(", "$").replace("\\)", "$");
        cx.hit("math", n);
    }

    if opts.pangu {
        let (t2, n) = pangu(&t);
        t = t2;
        cx.hit("pangu", n);
    }
    t
}

fn fix_emphasis(s: &str) -> (String, usize) {
    let mut n = 0;
    let mut out = String::new();
    let parts: Vec<&str> = s.split("**").collect();
    if parts.len() < 3 || parts.len() % 2 == 0 {
        return (s.to_string(), 0);
    }
    for (k, p) in parts.iter().enumerate() {
        if k > 0 {
            out.push_str("**");
        }
        if k % 2 == 1 && p.trim() != *p && !p.trim().is_empty() {
            n += 1;
            out.push_str(p.trim());
        } else {
            out.push_str(p);
        }
    }
    (out, n)
}

fn fix_links(s: &str) -> (String, usize) {
    let mut t = s.to_string();
    let mut n = 0;
    // [文字] (url) / [文字]（url）
    for (bad, open, close) in [("] (http", "] (", ")"), ("]（http", "]（", "）")] {
        while let Some(p) = t.find(bad) {
            let head = &t[..p + 1];
            let rest = &t[p + open.len()..];
            match rest.find(close) {
                Some(e) => {
                    let url = rest[..e].trim().to_string();
                    t = format!("{head}({url}){}", &rest[e + close.len()..]);
                    n += 1;
                }
                None => break,
            }
        }
    }
    // 裸 www. → https://www.
    let mut o = String::new();
    let chars: Vec<char> = t.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        let head: String = chars[k..chars.len().min(k + 4)].iter().collect();
        let prev = if k > 0 { Some(chars[k - 1]) } else { None };
        if head == "www." && prev.map_or(true, |c| c.is_whitespace() || is_cjk(c) || c == '(') {
            o.push_str("https://");
            n += 1;
        }
        o.push(chars[k]);
        k += 1;
    }
    t = o;
    // URL 紧贴中文：补空格，避免中文被吞进链接
    let mut o = String::new();
    let chars: Vec<char> = t.chars().collect();
    let mut in_url = false;
    let mut in_paren_link = false;
    for (k, c) in chars.iter().enumerate() {
        let head: String = chars[k..chars.len().min(k + 8)].iter().collect();
        if !in_url && (head.starts_with("http://") || head.starts_with("https://")) {
            in_url = true;
            in_paren_link = k > 0 && chars[k - 1] == '(';
        }
        if in_url && (c.is_whitespace() || *c == ')' && in_paren_link || *c == '>' || "，。；！？）】」、".contains(*c)) {
            in_url = false;
        }
        if in_url && is_han(*c) {
            if !in_paren_link {
                o.push(' ');
                n += 1;
            }
            in_url = false;
        }
        o.push(*c);
    }
    (o, n)
}

fn pangu(s: &str) -> (String, usize) {
    let chars: Vec<char> = s.chars().collect();
    let mut o = String::new();
    let mut n = 0;
    for (k, c) in chars.iter().enumerate() {
        if k > 0 {
            let p = chars[k - 1];
            let ascii = |x: char| x.is_ascii_alphanumeric();
            if (is_han(p) && ascii(*c)) || (ascii(p) && is_han(*c)) {
                o.push(' ');
                n += 1;
            }
        }
        o.push(*c);
    }
    (o, n)
}

// ───────────────────────── 6. 条目挤在一段 ─────────────────────────

fn split_run_on(lines: Vec<L>, cx: &mut Ctx) -> Vec<L> {
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        let tt = l.t.trim_start();
        if l.code || tt.starts_with('#') || tt.starts_with('|') || tt.starts_with('>') || tt.starts_with("- ") || ordered_num(tt).map_or(false, |(n, _)| n != 1 || indent_of(&l.t) > 0) {
            out.push(l);
            continue;
        }
        match split_enumeration(&l.t) {
            Some(parts) => {
                cx.hit("run-on", 1);
                out.push(txt(""));
                out.extend(parts.into_iter().map(txt));
                out.push(txt(""));
            }
            None => out.push(l),
        }
    }
    out
}

/// 「要点：1. 甲 2. 乙 3. 丙」/「① 甲 ② 乙 ③ 丙」/「• 甲 • 乙」→ 拆成列表行。
fn split_enumeration(s: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = s.chars().collect();
    // 候选标记位置
    let mut marks: Vec<(usize, u32, usize)> = Vec::new(); // (char 起点, 序号, char 长度)
    for k in 0..chars.len() {
        let prev_ok = k == 0 || chars[k - 1].is_whitespace() || "，。；;：:、！？!?)）".contains(chars[k - 1]);
        if !prev_ok {
            continue;
        }
        let rest: String = chars[k..chars.len().min(k + 6)].iter().collect();
        if let Some((n, bytes)) = ordered_marker(&rest) {
            let clen = rest[..bytes].chars().count();
            marks.push((k, n, clen));
        }
    }
    // 找最长的 1,2,3… 连续序列
    let mut best: Vec<(usize, u32, usize)> = Vec::new();
    for (si, m) in marks.iter().enumerate() {
        if m.1 != 1 {
            continue;
        }
        let mut seq = vec![*m];
        for m2 in &marks[si + 1..] {
            if m2.1 == seq.last().unwrap().1 + 1 && m2.0 > seq.last().unwrap().0 + seq.last().unwrap().2 {
                seq.push(*m2);
            }
        }
        if seq.len() > best.len() {
            best = seq;
        }
    }
    if best.len() >= 3 || (best.len() == 2 && best[0].0 > 0) {
        let mut parts = Vec::new();
        let pre: String = chars[..best[0].0].iter().collect();
        let pre = pre.trim().to_string();
        if !pre.is_empty() {
            parts.push(pre);
            parts.push(String::new());
        }
        for (i, m) in best.iter().enumerate() {
            let end = best.get(i + 1).map(|x| x.0).unwrap_or(chars.len());
            let body: String = chars[m.0 + m.2..end].iter().collect();
            let body = body.trim().trim_end_matches(|c| c == '；' || c == ';').trim().to_string();
            if body.is_empty() {
                return None;
            }
            parts.push(format!("{}. {}", m.1, body));
        }
        return Some(parts);
    }
    // 行内项目符号 ≥2 个
    let bullets: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|(k, c)| BULLETS.contains(**c) && **c != '·' && (*k == 0 || chars[k - 1].is_whitespace() || is_cjk_punct(chars[k - 1])))
        .map(|(k, _)| k)
        .collect();
    let inner = bullets.iter().filter(|k| **k > 0).count();
    if bullets.len() >= 2 && inner >= 1 {
        let mut parts = Vec::new();
        let pre: String = chars[..bullets[0]].iter().collect();
        if !pre.trim().is_empty() {
            parts.push(pre.trim().to_string());
            parts.push(String::new());
        }
        for (i, b) in bullets.iter().enumerate() {
            let end = bullets.get(i + 1).copied().unwrap_or(chars.len());
            let body: String = chars[b + 1..end].iter().collect();
            if !body.trim().is_empty() {
                parts.push(format!("- {}", body.trim()));
            }
        }
        return Some(parts);
    }
    None
}

// ───────────────────────── 7. 段落换行 ─────────────────────────

fn is_block_line(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with('#') || t.starts_with("- ") || t.starts_with("* ") || t.starts_with('|') || t.starts_with('>') || t == "$$"
        || t == "---" || ordered_marker(t).map_or(false, |_| t.split_once(". ").map_or(false, |(a, _)| a.chars().all(|c| c.is_ascii_digit())))
}

/// 连续多行纯文本：Markdown 会把单换行吞成一段。
/// - 像「一行一条」（行短 / 以句末标点结尾）→ 每行成段；
/// - 像 PDF 硬换行（行宽相近、行尾不收句）→ 合并成一句。
fn fix_paragraph_breaks(lines: Vec<L>, cx: &mut Ctx) -> Vec<L> {
    let mut out: Vec<L> = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut in_math = false;
    while i < lines.len() {
        let l = &lines[i];
        if l.t.trim() == "$$" && !l.code {
            in_math = !in_math;
        }
        if l.code || in_math || l.t.trim().is_empty() || is_block_line(&l.t) {
            out.push(l.clone());
            i += 1;
            continue;
        }
        let mut j = i;
        while j < lines.len() && !lines[j].code && !lines[j].t.trim().is_empty() && !is_block_line(&lines[j].t) && lines[j].t.trim() != "$$" {
            j += 1;
        }
        let run: Vec<String> = lines[i..j].iter().map(|l| l.t.trim().to_string()).collect();
        if run.len() == 1 {
            out.push(txt(run[0].clone()));
        } else {
            // 逐行决定：与下一行「接上」还是「断开」
            // - 行尾是逗号/顿号，或行宽接近满行且不收句 → 硬换行，接上；
            // - 其余（收句、短行）→ 本该独立成行，断开成段。
            let maxw = run.iter().map(|s| width(s)).max().unwrap_or(0);
            let mut acc = run[0].clone();
            for k in 1..run.len() {
                let prev = &run[k - 1];
                let last = prev.chars().last().unwrap_or(' ');
                let comma = "，,、".contains(last);
                let full = maxw >= 40 && width(prev) * 100 >= maxw * 75 && !ends_sentence(prev);
                if comma || full {
                    if let (Some(a), Some(b)) = (acc.chars().last(), run[k].chars().next()) {
                        if !(is_cjk(a) || is_cjk(b)) {
                            acc.push(' ');
                        }
                    }
                    acc.push_str(&run[k]);
                    cx.hit("hard-wrap", 1);
                } else {
                    out.push(txt(std::mem::take(&mut acc)));
                    out.push(txt(""));
                    acc = run[k].clone();
                    cx.hit("line-breaks", 1);
                }
            }
            out.push(txt(acc));
        }
        i = j;
    }
    out
}

// ───────────────────────── 8. 有序列表 ─────────────────────────

fn indent_of(t: &str) -> usize {
    t.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

fn ordered_num(t: &str) -> Option<(u32, String)> {
    let t2 = t.trim_start();
    let (a, b) = t2.split_once(". ")?;
    if a.is_empty() || a.len() > 3 || !a.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((a.parse().ok()?, b.to_string()))
}

/// 被空行隔开的有序项合并为一个列表并顺序编号（否则每项都从 1 开始）。
fn fix_ordered_lists(lines: Vec<L>, cx: &mut Ctx) -> Vec<L> {
    let mut out: Vec<L> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].code || ordered_num(&lines[i].t).is_none() {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        // 收集一段「有序项（中间只隔空行）」
        let mut items: Vec<(u32, String)> = Vec::new();
        let ind = indent_of(&lines[i].t);
        let pad: String = lines[i].t.chars().take(ind).collect();
        let mut j = i;
        let mut gaps = 0;
        loop {
            if j < lines.len() && !lines[j].code && indent_of(&lines[j].t) == ind {
                if let Some(it) = ordered_num(&lines[j].t) {
                    items.push(it);
                    j += 1;
                    continue;
                }
            }
            // 空行后紧跟下一个有序项 → 吞掉空行
            let mut k = j;
            while k < lines.len() && !lines[k].code && lines[k].t.trim().is_empty() {
                k += 1;
            }
            if k > j && k < lines.len() && !lines[k].code && indent_of(&lines[k].t) == ind && ordered_num(&lines[k].t).is_some() {
                gaps += 1;
                j = k;
                continue;
            }
            break;
        }
        let start = items[0].0.max(1);
        let renumbered = items.iter().enumerate().any(|(k, (n, _))| *n != start + k as u32);
        if gaps > 0 || renumbered {
            cx.hit("list-number", gaps.max(1));
        }
        if ind == 0 && out.last().map_or(false, |l: &L| !l.t.trim().is_empty()) {
            out.push(txt(""));
        }
        for (k, (_, body)) in items.iter().enumerate() {
            out.push(txt(format!("{pad}{}. {}", start + k as u32, body)));
        }
        i = j;
    }
    out
}

// ───────────────────────── 9. 标题 ─────────────────────────

fn promote_title(mut lines: Vec<L>, cx: &mut Ctx) -> Vec<L> {
    if lines.iter().any(|l| !l.code && l.t.starts_with("# ")) {
        return lines;
    }
    // frontmatter 之后第一条非空行
    let Some(idx) = lines.iter().position(|l| !l.code && !l.t.trim().is_empty()) else { return lines };
    if lines[..idx].iter().any(|l| l.code && !l.t.trim().is_empty() && l.t.trim() != "---") {
        return lines; // 前面是代码块，不猜
    }
    let t = lines[idx].t.trim().to_string();
    let rest_has_content = lines[idx + 1..].iter().any(|l| !l.t.trim().is_empty());
    if let Some(h) = t.strip_prefix("## ") {
        // 全文最高只有二级标题且首行就是 → 视为标题
        if !lines[idx + 1..].iter().any(|l| !l.code && l.t.starts_with("## ")) {
            lines[idx].t = format!("# {h}");
            cx.hit("title", 1);
        }
        return lines;
    }
    if is_block_line(&t) || !rest_has_content || ends_sentence(&t) || width(&t) > 60 || t.contains("http") {
        return lines;
    }
    lines[idx].t = format!("# {t}");
    lines.insert(idx + 1, txt(""));
    cx.hit("title", 1);
    lines
}

// ───────────────────────── 10. 收尾 ─────────────────────────

fn finish(lines: Vec<L>, cx: &mut Ctx) -> String {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut blank_run = 0;
    let mut big_runs = 0;
    for l in &lines {
        if l.code {
            if blank_run >= 3 {
                big_runs += 1;
            }
            blank_run = 0;
            out.push(l.t.clone());
            continue;
        }
        if l.t.trim().is_empty() {
            blank_run += 1;
            if blank_run == 1 && !out.is_empty() {
                out.push(String::new());
            }
            continue;
        }
        if blank_run >= 3 {
            big_runs += 1;
        }
        blank_run = 0;
        // 块之间需要空行：段落后紧跟列表 / 表格 / 公式，否则会被吸进段落
        let t = l.t.clone();
        let nested = indent_of(&t) > 0 && (ordered_num(&t).is_some() || t.trim_start().starts_with("- "));
        if nested {
            out.push(t);
            continue;
        }
        if let Some(prev) = out.last().cloned() {
            let pt = prev.trim_start();
            let starts_block = ordered_num(&t).is_some() || t.starts_with("- ") || t.starts_with('|') || t == "$$" || t.starts_with('#');
            let prev_same_kind = (ordered_num(pt).is_some() && ordered_num(&t).is_some())
                || (pt.starts_with("- ") && t.starts_with("- "))
                || (pt.starts_with('|') && t.starts_with('|'))
                || (indent_of(&prev) > 0 && (t.starts_with("- ") || ordered_num(&t).is_some()));
            let prev_is_math_open = pt == "$$" && out.iter().filter(|x| x.trim() == "$$").count() % 2 == 1;
            let closing_math = t == "$$" && out.iter().filter(|x| x.trim() == "$$").count() % 2 == 1;
            let need_a = !prev.is_empty() && starts_block && !prev_same_kind && !prev_is_math_open && !closing_math;
            // 标题后、列表/表格结束后接正文也补空行
            let prev_block_end = pt.starts_with('#') || ((pt.starts_with('|') || ordered_num(pt).is_some() || pt.starts_with("- ")) && !starts_block);
            let need_b = !prev.is_empty() && prev_block_end && !prev_same_kind && !starts_block && !prev_is_math_open;
            if need_a || need_b {
                out.push(String::new());
            }
        }
        out.push(t);
    }
    cx.hit("blank-lines", big_runs);
    while out.last().map_or(false, |l| l.is_empty()) {
        out.pop();
    }
    while out.first().map_or(false, |l| l.is_empty()) {
        out.remove(0);
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TidyReport {
        tidy(s, &TidyOptions::default())
    }
    fn kinds(r: &TidyReport) -> Vec<String> {
        r.issues.iter().map(|i| i.kind.clone()).collect()
    }

    #[test]
    fn idempotent_and_clean_doc_untouched() {
        let clean = "# 标题\n\n第一段。\n\n## 小节\n\n- 甲\n- 乙\n\n1. 一\n2. 二\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n";
        let r = t(clean);
        assert_eq!(r.markdown, clean, "{:?}", r.issues);
        assert_eq!(r.total, 0);
        let messy = "GEO 渠道稿件\n一、背景\n这是 第 一 段 文 字 内 容。\n第二行也该独立。\n\n\n\n\n要点：1. 甲 2. 乙 3. 丙\n姓名\t年龄\n张三\t18\n";
        let once = t(messy).markdown;
        assert_eq!(t(&once).markdown, once, "二次运行应无变化");
    }

    #[test]
    fn lines_stuck_in_one_paragraph_get_split() {
        let r = t("# 文档\n\n第一条：完成。\n第二条：进行中。\n第三条：未开始。\n");
        assert!(r.markdown.contains("完成。\n\n第二条"), "{}", r.markdown);
        assert!(kinds(&r).contains(&"line-breaks".to_string()));
    }

    #[test]
    fn pdf_hard_wrap_is_joined() {
        let src = "# 文档\n\nThis is a long sentence copied out of a PDF file that\nwas wrapped at a fixed column width by the layout engine and\nnow it breaks in the middle of the sentence when pasted.\n";
        let r = t(src);
        assert!(r.markdown.contains("file that was wrapped"), "{}", r.markdown);
        assert!(kinds(&r).contains(&"hard-wrap".to_string()));
    }

    #[test]
    fn lost_headings_restored_with_levels() {
        let r = t("# 报告\n\n一、背景\n正文内容。\n（一）现状\n正文。\n二、方案\n正文。\n");
        assert!(r.markdown.contains("\n## 一、背景\n"), "{}", r.markdown);
        assert!(r.markdown.contains("\n### （一）现状\n"), "{}", r.markdown);
        assert!(r.markdown.contains("\n## 二、方案\n"), "{}", r.markdown);
    }

    #[test]
    fn title_promoted_from_first_short_line() {
        let r = t("GEO 渠道稿件\n正文第一段写了很多内容。\n");
        assert!(r.markdown.starts_with("# GEO 渠道稿件\n\n"), "{}", r.markdown);
        let r = t("这是一句完整的话。\n下一句。\n");
        assert!(!r.markdown.starts_with("# "), "整句不当标题");
    }

    #[test]
    fn run_on_enumeration_becomes_list() {
        let r = t("# x\n\n要点：1. 降本 2. 增效 3. 提质\n");
        assert!(r.markdown.contains("要点：\n\n1. 降本\n2. 增效\n3. 提质"), "{}", r.markdown);
        let r = t("# x\n\n① 甲；② 乙；③ 丙\n");
        assert!(r.markdown.contains("1. 甲\n2. 乙\n3. 丙"), "{}", r.markdown);
        // 小数不是序号
        let r = t("# x\n\n增长 1.5 倍，2.3 倍，3.1 倍。\n");
        assert!(!kinds(&r).contains(&"run-on".to_string()), "{}", r.markdown);
    }

    #[test]
    fn ordered_numbering_continues() {
        let r = t("# x\n\n1. 甲\n\n2. 乙\n\n3. 丙\n");
        assert!(r.markdown.contains("1. 甲\n2. 乙\n3. 丙"), "{}", r.markdown);
        let r = t("# x\n\n1、甲\n2）乙\n（3）丙\n");
        assert!(r.markdown.contains("1. 甲\n2. 乙\n3. 丙"), "{}", r.markdown);
    }

    #[test]
    fn tables_recognised() {
        let r = t("# x\n\n姓名\t年龄\t城市\n张三\t18\t北京\n李四\t20\t上海\n");
        assert!(r.markdown.contains("| 姓名 | 年龄 | 城市 |\n| --- | --- | --- |\n| 张三 | 18 | 北京 |"), "{}", r.markdown);
        let r = t("# x\n\n名称 | 数值\nA | 1\nB | 2\n");
        assert!(r.markdown.contains("| 名称 | 数值 |\n| --- | --- |"), "{}", r.markdown);
        // 列数不齐的表格补齐
        let r = t("# x\n\n| a | b | c |\n| --- | --- | --- |\n| 1 | 2 |\n");
        assert!(r.markdown.contains("| 1 | 2 |  |"), "{}", r.markdown);
    }

    #[test]
    fn json_mermaid_code_math() {
        let r = t("# x\n\n{\"name\":\"鹿蕊\",\"tags\":[\"a\",\"b\"],\"n\":1}\n");
        assert!(r.markdown.contains("```json\n{\n  \"name\": \"鹿蕊\",\n  \"tags\": [\n    \"a\",\n    \"b\"\n  ],\n  \"n\": 1\n}\n```"), "{}", r.markdown);
        let r = t("# x\n\ngraph TD\nA-->B\nB-->C\n");
        assert!(r.markdown.contains("```mermaid\ngraph TD\nA-->B"), "{}", r.markdown);
        let r = t("# x\n\n```\nflowchart LR\nA-->B\n```\n");
        assert!(r.markdown.contains("```mermaid\nflowchart LR"), "{}", r.markdown);
        let r = t("# x\n\nimport os\ndef main():\n    print(os.name)\n");
        assert!(r.markdown.contains("```python\nimport os"), "{}", r.markdown);
        let r = t("# x\n\n\\[ E = mc^2 \\]\n\n面积公式 \\(\\pi r^2\\) 很常见。\n");
        assert!(r.markdown.contains("$$\nE = mc^2\n$$"), "{}", r.markdown);
        assert!(r.markdown.contains("$\\pi r^2$"), "{}", r.markdown);
    }

    #[test]
    fn spaces_invisible_blank_lines_artifacts() {
        let r = t("# x\n\n每 个 文 字 之 间 都 有 空 格。\n\n\n\n\n第二段\u{200b}有零宽\u{00a0}字符，  多个空格。\n\n第 3 页 共 10 页\n");
        assert!(r.markdown.contains("每个文字之间都有空格。"), "{}", r.markdown);
        assert!(r.markdown.contains("第二段有零宽 字符，多个空格。"), "{}", r.markdown);
        assert!(!r.markdown.contains("\n\n\n"));
        assert!(!r.markdown.contains("第 3 页"));
        let k = kinds(&r);
        for want in ["cjk-spaces", "invisible", "blank-lines", "artifact", "spaces"] {
            assert!(k.contains(&want.to_string()), "缺 {want}: {k:?}");
        }
        // 英文词间单空格不动
        assert_eq!(t("# x\n\nHello world, nice day.\n").total, 0);
    }

    #[test]
    fn links_bullets_emphasis_headings() {
        let r = t("# x\n\n• 甲\n• 乙\n\n看 [官网] (https://a.com) 和 www.b.com，详见https://c.com获取。\n\n这是 ** 重点 ** 内容。\n\n##小节\n");
        assert!(r.markdown.contains("- 甲\n- 乙"), "{}", r.markdown);
        assert!(r.markdown.contains("[官网](https://a.com)"), "{}", r.markdown);
        assert!(r.markdown.contains("https://www.b.com"), "{}", r.markdown);
        assert!(r.markdown.contains("https://c.com 获取"), "{}", r.markdown);
        assert!(r.markdown.contains("**重点**"), "{}", r.markdown);
        assert!(r.markdown.contains("## 小节"), "{}", r.markdown);
    }

    #[test]
    fn code_fences_are_never_touched() {
        let src = "# x\n\n```python\na  =  1   # 多 个 空 格\n\n\n\nb = 2\n```\n";
        assert_eq!(t(src).markdown, src);
    }

    #[test]
    fn output_parses_into_expected_blocks() {
        let r = t("周报\n本周完成：1. 接口联调 2. 埋点验收 3. 灰度发布\n数据\t数值\nDAU\t1200\n");
        let doc = crate::from_markdown(&r.markdown).unwrap();
        assert_eq!(doc.title.as_deref(), Some("周报"), "{}", r.markdown);
        let kinds: Vec<&str> = doc.blocks.iter().map(|b| b.kind()).collect();
        assert!(kinds.contains(&"list") && kinds.contains(&"table"), "{kinds:?}\n{}", r.markdown);
    }
}
