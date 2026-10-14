//! DOCX → Markdown（结构保真）。
//!
//! 覆盖：标题层级（样式名 / outlineLvl / 继承链）、标题自动编号、列表（项目符号 / 多级编号 /
//! 中文编号）、表格（文档序、合并单元格、版式用的单格表拆回正文）、超链接（关系 / 域代码）、
//! 粗斜体删除线高亮行内代码、上下标、图片（含文本框）、脚注、公式（OMML → LaTeX）、
//! 代码段（等宽字体 / Code 样式）、引用样式、修订（删除内容不导入）、目录域跳过。

use crate::mdw::{self, Fmt, Piece};
use crate::xml::{self, Node};
use crate::zip::ZipReader;
use crate::{Converted, Image};
use std::collections::HashMap;

#[derive(Default, Clone)]
struct Style {
    name: String,
    based_on: Option<String>,
    outline: Option<u8>,
    mono: bool,
    num: Option<(String, u8)>,
    bold: bool,
    italic: bool,
}

#[derive(Clone)]
struct Lvl {
    fmt: String,
    text: String,
    start: u32,
}

struct Ctx<'a> {
    zip: &'a ZipReader<'a>,
    styles: HashMap<String, Style>,
    rels: HashMap<String, String>,
    /// numId → (abstractId, 起始覆盖)
    nums: HashMap<String, (String, HashMap<u8, u32>)>,
    abstracts: HashMap<String, Vec<Lvl>>,
    counters: HashMap<String, Vec<u32>>,
    images: Vec<Image>,
    footnotes: Vec<String>,
    foot_src: HashMap<String, Node>,
    has_title_style: bool,
    title: Option<String>,
}

pub fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let zip = ZipReader::new(bytes).map_err(|e| e.to_string())?;
    let doc_xml = zip.read_string("word/document.xml").ok_or("缺少 word/document.xml（不是 Word 文档？）")?;
    let mut cx = Ctx {
        zip: &zip,
        styles: HashMap::new(),
        rels: HashMap::new(),
        nums: HashMap::new(),
        abstracts: HashMap::new(),
        counters: HashMap::new(),
        images: Vec::new(),
        footnotes: Vec::new(),
        foot_src: HashMap::new(),
        has_title_style: false,
        title: None,
    };
    load_styles(&mut cx);
    load_rels(&mut cx);
    load_numbering(&mut cx);
    load_footnotes(&mut cx);
    let core_title = zip
        .read_string("docProps/core.xml")
        .and_then(|s| xml::parse(&s).find_local("title").map(|n| n.text().trim().to_string()))
        .filter(|t| !t.is_empty());

    let root = xml::parse(&doc_xml);
    let body = root.find("w:body").ok_or("文档没有正文")?;
    cx.has_title_style = uses_title_style(body, &cx);
    let mut blocks: Vec<String> = Vec::new();
    walk_body(body, &mut cx, &mut blocks);
    let blocks = merge_code(blocks);

    // 标题：Title 样式 > 唯一的一级标题且位于开头 > core.xml
    let mut title = cx.title.clone();
    let mut blocks = blocks;
    if title.is_none() {
        let h1: Vec<usize> = blocks.iter().enumerate().filter(|(_, b)| b.starts_with("# ")).map(|(i, _)| i).collect();
        if h1.len() == 1 && h1[0] == 0 {
            title = Some(blocks.remove(0)[2..].trim().to_string());
        } else if !h1.is_empty() {
            // 多个一级标题：整体降一级，给文档标题让位
            blocks = blocks.into_iter().map(|b| if b.starts_with('#') { format!("#{b}") } else { b }).collect();
        }
    }
    if title.is_none() {
        title = core_title;
    }
    let mut md = mdw::join_blocks(&blocks);
    if !cx.footnotes.is_empty() {
        md.push('\n');
        for (i, f) in cx.footnotes.iter().enumerate() {
            md.push_str(&format!("[^{}]: {}\n", i + 1, f));
        }
    }
    Ok(Converted { title, markdown: md, images: cx.images, warnings: Vec::new() })
}

// ───────────────────────── 元数据 ─────────────────────────

fn load_styles(cx: &mut Ctx) {
    let Some(s) = cx.zip.read_string("word/styles.xml") else { return };
    let root = xml::parse(&s);
    let mut all = Vec::new();
    root.find_all("w:style", &mut all);
    for st in all {
        let Some(id) = st.attr("w:styleId") else { continue };
        let mut style = Style {
            name: st.child("w:name").and_then(|n| n.attr("w:val")).unwrap_or("").to_string(),
            based_on: st.child("w:basedOn").and_then(|n| n.attr("w:val")).map(String::from),
            ..Default::default()
        };
        if let Some(ppr) = st.child("w:pPr") {
            style.outline = ppr.child("w:outlineLvl").and_then(|n| n.attr("w:val")).and_then(|v| v.parse().ok());
            if let Some(np) = ppr.child("w:numPr") {
                let nid = np.child("w:numId").and_then(|n| n.attr("w:val")).unwrap_or("").to_string();
                let il = np.child("w:ilvl").and_then(|n| n.attr("w:val")).and_then(|v| v.parse().ok()).unwrap_or(0);
                if !nid.is_empty() && nid != "0" {
                    style.num = Some((nid, il));
                }
            }
        }
        if let Some(rpr) = st.child("w:rPr") {
            style.mono = rpr.child("w:rFonts").map_or(false, |f| is_mono(f.attr("w:ascii").unwrap_or("")));
            style.bold = on(rpr.child("w:b"));
            style.italic = on(rpr.child("w:i"));
        }
        cx.styles.insert(id.to_string(), style);
    }
}

fn load_rels(cx: &mut Ctx) {
    let Some(s) = cx.zip.read_string("word/_rels/document.xml.rels") else { return };
    let root = xml::parse(&s);
    let mut all = Vec::new();
    root.find_all("Relationship", &mut all);
    for r in all {
        if let (Some(id), Some(t)) = (r.attr("Id"), r.attr("Target")) {
            cx.rels.insert(id.to_string(), t.to_string());
        }
    }
}

fn load_numbering(cx: &mut Ctx) {
    let Some(s) = cx.zip.read_string("word/numbering.xml") else { return };
    let root = xml::parse(&s);
    let mut abs = Vec::new();
    root.find_all("w:abstractNum", &mut abs);
    for a in abs {
        let Some(id) = a.attr("w:abstractNumId") else { continue };
        let mut lvls = vec![Lvl { fmt: "bullet".into(), text: "•".into(), start: 1 }; 9];
        for l in a.elems().filter(|e| e.is("w:lvl")) {
            let il: usize = l.attr("w:ilvl").and_then(|v| v.parse().ok()).unwrap_or(0);
            if il < 9 {
                lvls[il] = Lvl {
                    fmt: l.child("w:numFmt").and_then(|n| n.attr("w:val")).unwrap_or("decimal").to_string(),
                    text: l.child("w:lvlText").and_then(|n| n.attr("w:val")).unwrap_or("").to_string(),
                    start: l.child("w:start").and_then(|n| n.attr("w:val")).and_then(|v| v.parse().ok()).unwrap_or(1),
                };
            }
        }
        cx.abstracts.insert(id.to_string(), lvls);
    }
    let mut nums = Vec::new();
    root.find_all("w:num", &mut nums);
    for n in nums {
        let (Some(id), Some(aid)) = (n.attr("w:numId"), n.child("w:abstractNumId").and_then(|x| x.attr("w:val"))) else { continue };
        let mut over = HashMap::new();
        for o in n.elems().filter(|e| e.is("w:lvlOverride")) {
            let il: u8 = o.attr("w:ilvl").and_then(|v| v.parse().ok()).unwrap_or(0);
            if let Some(s) = o.child("w:startOverride").and_then(|x| x.attr("w:val")).and_then(|v| v.parse().ok()) {
                over.insert(il, s);
            }
        }
        cx.nums.insert(id.to_string(), (aid.to_string(), over));
    }
}

fn load_footnotes(cx: &mut Ctx) {
    let Some(s) = cx.zip.read_string("word/footnotes.xml") else { return };
    let root = xml::parse(&s);
    let mut all = Vec::new();
    root.find_all("w:footnote", &mut all);
    for f in all {
        if let Some(id) = f.attr("w:id") {
            cx.foot_src.insert(id.to_string(), f.clone());
        }
    }
}

fn on(n: Option<&Node>) -> bool {
    n.map_or(false, |n| !matches!(n.attr("w:val"), Some("0") | Some("false") | Some("none")))
}

fn is_mono(font: &str) -> bool {
    let f = font.to_ascii_lowercase();
    ["consolas", "courier", "menlo", "monaco", "mono", "source code", "fira code", "jetbrains", "sf mono", "inconsolata", "lucida console"]
        .iter()
        .any(|m| f.contains(m))
}

/// 样式链上解析：标题级别（1 起）/ 等宽 / 编号。
fn style_heading(cx: &Ctx, id: &str) -> Option<u8> {
    let mut cur = Some(id.to_string());
    let mut guard = 0;
    while let Some(sid) = cur {
        guard += 1;
        if guard > 12 {
            break;
        }
        let st = cx.styles.get(&sid)?;
        let name = st.name.to_lowercase();
        if name == "title" || name == "标题" {
            return Some(0);
        }
        for p in ["heading ", "标题 ", "heading", "标题"] {
            if let Some(n) = name.strip_prefix(p).and_then(|r| r.trim().parse::<u8>().ok()) {
                return Some(n.clamp(1, 6));
            }
        }
        if let Some(o) = st.outline {
            if o < 9 {
                return Some((o + 1).min(6));
            }
        }
        cur = st.based_on.clone();
    }
    None
}

fn style_flag(cx: &Ctx, id: &str, f: impl Fn(&Style) -> bool) -> bool {
    let mut cur = Some(id.to_string());
    let mut guard = 0;
    while let Some(sid) = cur {
        guard += 1;
        if guard > 12 {
            break;
        }
        let Some(st) = cx.styles.get(&sid) else { break };
        if f(st) {
            return true;
        }
        cur = st.based_on.clone();
    }
    false
}

fn style_num(cx: &Ctx, id: &str) -> Option<(String, u8)> {
    let mut cur = Some(id.to_string());
    let mut guard = 0;
    while let Some(sid) = cur {
        guard += 1;
        if guard > 12 {
            break;
        }
        let st = cx.styles.get(&sid)?;
        if st.num.is_some() {
            return st.num.clone();
        }
        cur = st.based_on.clone();
    }
    None
}

fn uses_title_style(body: &Node, cx: &Ctx) -> bool {
    let mut ps = Vec::new();
    body.find_all("w:pStyle", &mut ps);
    ps.iter().any(|p| p.attr("w:val").map_or(false, |v| style_heading(cx, v) == Some(0)))
}

// ───────────────────────── 正文 ─────────────────────────

fn walk_body(node: &Node, cx: &mut Ctx, out: &mut Vec<String>) {
    for c in node.elems() {
        match c.name.as_str() {
            "w:p" => {
                if let Some(b) = paragraph(c, cx, out) {
                    out.push(b);
                }
            }
            "w:tbl" => table(c, cx, out),
            "w:sdt" => {
                // 目录控件整块跳过
                let is_toc = c.find("w:docPartGallery").and_then(|g| g.attr("w:val")).map_or(false, |v| v.contains("Table of Contents"));
                if !is_toc {
                    if let Some(sc) = c.child("w:sdtContent") {
                        walk_body(sc, cx, out);
                    }
                }
            }
            "w:customXml" | "w:ins" | "w:smartTag" => walk_body(c, cx, out),
            _ => {}
        }
    }
}

/// 段落 → 一个块（标题 / 列表项 / 段落 / 代码行 / 公式）；空段落返回 None。
/// 段落里的文本框内容作为额外块推入 `extra`。
fn paragraph(p: &Node, cx: &mut Ctx, extra: &mut Vec<String>) -> Option<String> {
    let ppr = p.child("w:pPr");
    let style = ppr.and_then(|x| x.child("w:pStyle")).and_then(|x| x.attr("w:val")).unwrap_or("").to_string();
    let mut heading = if style.is_empty() { None } else { style_heading(cx, &style) };
    if heading.is_none() {
        heading = ppr.and_then(|x| x.child("w:outlineLvl")).and_then(|x| x.attr("w:val")).and_then(|v| v.parse::<u8>().ok()).filter(|o| *o < 9).map(|o| (o + 1).min(6));
    }
    let mono_style = !style.is_empty() && style_flag(cx, &style, |s| s.mono || s.name.to_lowercase().contains("code") || s.name.to_lowercase().contains("preformatted") || s.name.contains("代码"));
    let quote_style = !style.is_empty() && style_flag(cx, &style, |s| { let n = s.name.to_lowercase(); n.contains("quot") || n.contains("引用") || n == "block text" });

    // 编号：段落 numPr > 样式 numPr
    let mut num = ppr.and_then(|x| x.child("w:numPr")).and_then(|np| {
        let nid = np.child("w:numId").and_then(|n| n.attr("w:val"))?.to_string();
        let il = np.child("w:ilvl").and_then(|n| n.attr("w:val")).and_then(|v| v.parse().ok()).unwrap_or(0u8);
        Some((nid, il))
    });
    if num.is_none() && !style.is_empty() {
        num = style_num(cx, &style);
    }
    let num = num.filter(|(nid, _)| nid != "0");

    // 段落内容
    let mut pieces = Vec::new();
    let mut boxes = Vec::new();
    let mut st = RunState::default();
    runs(p, cx, &mut pieces, &mut boxes, &mut st);
    let all_mono = !pieces.is_empty() && pieces.iter().all(|x| match x { Piece::Text(t, f) => f.code || t.trim().is_empty(), _ => false });
    let text_plain: String = pieces.iter().map(|x| if let Piece::Text(t, _) = x { t.as_str() } else { "" }).collect();

    // 代码行：保留原文，不做行内记号
    if (mono_style || all_mono) && heading.is_none() && num.is_none() && !text_plain.trim().is_empty() {
        for b in boxes {
            extra.push(b);
        }
        return Some(format!("\u{0}CODE{}", text_plain.trim_end()));
    }
    if all_mono {
        pieces = pieces.into_iter().map(|x| if let Piece::Text(t, mut f) = x { f.code = false; Piece::Text(t, f) } else { x }).collect();
    }
    let mut text = mdw::inline(&pieces);
    // 段内软换行 → 分段（渲染端单换行会被吞）
    text = text.split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n\n");
    let text = text.trim().to_string();
    for b in boxes {
        extra.push(b);
    }
    if text.is_empty() {
        return None;
    }
    // 整段公式
    if text.starts_with("$$") {
        return Some(text);
    }

    // 编号文本
    let label = num.as_ref().and_then(|(nid, il)| number_label(cx, nid, *il));

    if let Some(h) = heading {
        let plain = text.replace("**", "");
        if h == 0 {
            cx.title.get_or_insert(plain.clone());
            return None;
        }
        let lvl = if cx.has_title_style { (h + 1).min(6) } else { h };
        let prefix = match &label {
            Some(Label::Ordered(l, _)) | Some(Label::Other(l)) => format!("{l} "),
            _ => String::new(),
        };
        return Some(format!("{} {}{}", "#".repeat(lvl as usize), prefix, plain));
    }
    if let Some(l) = label {
        let il = num.as_ref().map(|x| x.1).unwrap_or(0) as usize;
        let pad = "  ".repeat(il);
        return Some(match l {
            Label::Bullet => format!("{pad}- {text}"),
            Label::Ordered(_, n) => format!("{pad}{n}. {text}"),
            Label::Other(l) => format!("{pad}- {l} {text}"),
        });
    }
    if quote_style {
        return Some(text.split("\n\n").map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n>\n"));
    }
    Some(text)
}

enum Label {
    Bullet,
    /// (原样标签, 序号)
    Ordered(String, u32),
    Other(String),
}

fn number_label(cx: &mut Ctx, nid: &str, il: u8) -> Option<Label> {
    let (aid, over) = cx.nums.get(nid)?.clone();
    let lvls = cx.abstracts.get(&aid)?.clone();
    let il = (il as usize).min(8);
    let lv = lvls.get(il)?;
    if lv.fmt == "bullet" {
        return Some(Label::Bullet);
    }
    if lv.fmt == "none" {
        return None;
    }
    // 计数：同一 abstract 共享（Word 的语义），深层归零
    let cnt = cx.counters.entry(aid.clone()).or_insert_with(|| vec![0; 9]);
    for (k, c) in cnt.iter_mut().enumerate() {
        if k > il {
            *c = 0;
        }
    }
    let start = over.get(&(il as u8)).copied().unwrap_or(lv.start);
    cnt[il] = if cnt[il] == 0 { start } else { cnt[il] + 1 };
    // 上层未计数的按起始值补
    for k in 0..il {
        if cnt[k] == 0 {
            cnt[k] = lvls[k].start;
        }
    }
    let snapshot = cnt.clone();
    let mut text = lv.text.clone();
    for k in 0..=il {
        text = text.replace(&format!("%{}", k + 1), &mdw::format_num(snapshot[k], &lvls[k].fmt));
    }
    let n = snapshot[il];
    let plain_decimal = matches!(lv.fmt.as_str(), "decimal" | "decimalZero") && (lv.text == format!("%{}.", il + 1) || lv.text == format!("%{}", il + 1) || lv.text == format!("%{})", il + 1) || lv.text == format!("%{}、", il + 1));
    if plain_decimal {
        Some(Label::Ordered(text, n))
    } else if text.trim().is_empty() {
        Some(Label::Bullet)
    } else {
        Some(Label::Other(text))
    }
}

#[derive(Default)]
struct RunState {
    /// 复杂域：Some(指令) 期间
    field: Option<String>,
    in_result: bool,
    skip_result: bool,
    link: Option<(String, Vec<Piece>)>,
}

fn runs(node: &Node, cx: &mut Ctx, out: &mut Vec<Piece>, boxes: &mut Vec<String>, st: &mut RunState) {
    for c in node.elems() {
        match c.name.as_str() {
            "w:r" => run(c, cx, out, boxes, st),
            "w:hyperlink" => {
                let url = c
                    .attr("r:id")
                    .and_then(|id| cx.rels.get(id).cloned())
                    .or_else(|| c.attr("w:anchor").map(|a| format!("#{a}")))
                    .unwrap_or_default();
                let mut inner = Vec::new();
                runs(c, cx, &mut inner, boxes, st);
                out.push(Piece::Link(inner, url));
            }
            "w:fldSimple" => {
                let instr = c.attr("w:instr").unwrap_or("").trim().to_string();
                let mut inner = Vec::new();
                runs(c, cx, &mut inner, boxes, st);
                if let Some(url) = hyperlink_instr(&instr) {
                    out.push(Piece::Link(inner, url));
                } else if !instr.starts_with("TOC") {
                    out.extend(inner);
                }
            }
            "w:ins" | "w:smartTag" | "w:customXml" | "w:sdtContent" | "w:sdt" | "w:moveTo" => runs(c, cx, out, boxes, st),
            "m:oMathPara" => {
                let tex: Vec<String> = c.elems().filter(|e| e.is("m:oMath")).map(omml).collect();
                out.push(Piece::Raw(format!("$$\n{}\n$$", tex.join(" \\\\ "))));
            }
            "m:oMath" => out.push(Piece::Raw(format!("${}$", omml(c).trim()))),
            // w:del / w:moveFrom：修订里删掉的内容不导入
            _ => {}
        }
    }
}

fn hyperlink_instr(instr: &str) -> Option<String> {
    let rest = instr.trim().strip_prefix("HYPERLINK")?.trim();
    if let Some(q) = rest.split('"').nth(1) {
        if rest.contains("\\l") && !rest.trim_start().starts_with('"') {
            return Some(format!("#{q}"));
        }
        return Some(q.to_string());
    }
    rest.split_whitespace().next().map(String::from)
}

fn run(r: &Node, cx: &mut Ctx, out: &mut Vec<Piece>, boxes: &mut Vec<String>, st: &mut RunState) {
    let rpr = r.child("w:rPr");
    let rstyle = rpr.and_then(|x| x.child("w:rStyle")).and_then(|x| x.attr("w:val")).unwrap_or("").to_string();
    let mut f = Fmt::default();
    if let Some(rp) = rpr {
        f.bold = on(rp.child("w:b"));
        f.italic = on(rp.child("w:i"));
        f.strike = on(rp.child("w:strike")) || on(rp.child("w:dstrike"));
        f.mark = rp.child("w:highlight").map_or(false, |h| h.attr("w:val") != Some("none"));
        f.code = rp.child("w:rFonts").map_or(false, |x| is_mono(x.attr("w:ascii").or(x.attr("w:hAnsi")).unwrap_or("")));
        match rp.child("w:vertAlign").and_then(|v| v.attr("w:val")) {
            Some("superscript") => f.sup = true,
            Some("subscript") => f.sub = true,
            _ => {}
        }
    }
    if !rstyle.is_empty() {
        f.bold |= style_flag(cx, &rstyle, |s| s.bold || s.name.to_lowercase() == "strong");
        f.italic |= style_flag(cx, &rstyle, |s| s.italic || s.name.to_lowercase() == "emphasis");
        f.code |= style_flag(cx, &rstyle, |s| s.mono || s.name.to_lowercase().contains("code"));
        if style_flag(cx, &rstyle, |s| s.name.to_lowercase().contains("hyperlink")) {
            // 超链接字符样式本身不加格式
        }
    }
    for c in r.elems() {
        match c.name.as_str() {
            "w:fldChar" => match c.attr("w:fldCharType") {
                Some("begin") => {
                    st.field = Some(String::new());
                    st.in_result = false;
                }
                Some("separate") => {
                    st.in_result = true;
                    let instr = st.field.clone().unwrap_or_default();
                    let instr = instr.trim();
                    st.skip_result = instr.starts_with("TOC") || instr.starts_with("PAGE") || instr.starts_with("NUMPAGES");
                    if let Some(url) = hyperlink_instr(instr) {
                        st.link = Some((url, Vec::new()));
                    }
                }
                Some("end") => {
                    if let Some((url, inner)) = st.link.take() {
                        out.push(Piece::Link(inner, url));
                    }
                    st.field = None;
                    st.in_result = false;
                    st.skip_result = false;
                }
                _ => {}
            },
            "w:instrText" => {
                if let Some(fi) = st.field.as_mut() {
                    fi.push_str(&c.text());
                }
            }
            "w:t" | "w:delText" if c.is("w:t") => {
                if st.field.is_some() && !st.in_result {
                    continue;
                }
                if st.skip_result {
                    continue;
                }
                let piece = Piece::Text(c.text(), f.clone());
                match st.link.as_mut() {
                    Some((_, inner)) => inner.push(piece),
                    None => out.push(piece),
                }
            }
            "w:tab" | "w:ptab" => out.push(Piece::Text(" ".into(), Fmt::default())),
            "w:br" | "w:cr" => {
                if c.attr("w:type") != Some("page") && c.attr("w:type") != Some("column") {
                    out.push(Piece::Text("\n".into(), Fmt::default()));
                }
            }
            "w:noBreakHyphen" => out.push(Piece::Text("-".into(), f.clone())),
            "w:sym" => {
                if let Some(ch) = c.attr("w:char").and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32) {
                    out.push(Piece::Text(ch.to_string(), f.clone()));
                }
            }
            "w:footnoteReference" | "w:endnoteReference" => {
                if let Some(id) = c.attr("w:id") {
                    if let Some(fnode) = cx.foot_src.get(id).cloned() {
                        let mut ps = Vec::new();
                        let mut b2 = Vec::new();
                        let mut st2 = RunState::default();
                        for p in fnode.elems().filter(|e| e.is("w:p")) {
                            runs(p, cx, &mut ps, &mut b2, &mut st2);
                            ps.push(Piece::Text(" ".into(), Fmt::default()));
                        }
                        cx.footnotes.push(mdw::inline(&ps).trim().to_string());
                        out.push(Piece::Raw(format!("[^{}]", cx.footnotes.len())));
                    }
                }
            }
            "w:drawing" | "w:pict" | "mc:AlternateContent" | "w:object" => {
                drawing(c, cx, out, boxes);
            }
            _ => {}
        }
    }
}

/// 图片 / 文本框。
fn drawing(d: &Node, cx: &mut Ctx, out: &mut Vec<Piece>, boxes: &mut Vec<String>) {
    // AlternateContent：优先 Choice，避免 Fallback 重复
    if d.is("mc:AlternateContent") {
        if let Some(ch) = d.child("mc:Choice").or_else(|| d.child("mc:Fallback")) {
            drawing(ch, cx, out, boxes);
        }
        return;
    }
    let mut blips = Vec::new();
    d.find_all("a:blip", &mut blips);
    let mut ids: Vec<String> = blips.iter().filter_map(|b| b.attr("r:embed").map(String::from)).collect();
    let mut vml = Vec::new();
    d.find_all("v:imagedata", &mut vml);
    ids.extend(vml.iter().filter_map(|v| v.attr("r:id").map(String::from)));
    let alt = d
        .find("wp:docPr")
        .and_then(|n| n.attr("descr").filter(|s| !s.is_empty()).or(n.attr("title")).or(n.attr("name")))
        .unwrap_or("图片")
        .replace(['[', ']', '\n'], " ");
    for id in ids {
        let Some(target) = cx.rels.get(&id).cloned() else { continue };
        let path = if target.starts_with('/') { target.trim_start_matches('/').to_string() } else { format!("word/{}", target.trim_start_matches("./")) };
        if let Some(bytes) = cx.zip.read(&path) {
            let name = path.rsplit('/').next().unwrap_or("image.png").to_string();
            let token = crate::push_image(&mut cx.images, name, bytes);
            out.push(Piece::Raw(format!("![{}]({token})", alt.trim())));
        }
    }
    // 文本框正文
    let mut txbx = Vec::new();
    d.find_all("w:txbxContent", &mut txbx);
    for t in txbx.into_iter().take(1) {
        let mut blocks = Vec::new();
        walk_body(&t.clone(), cx, &mut blocks);
        boxes.extend(blocks);
    }
}

fn table(t: &Node, cx: &mut Ctx, out: &mut Vec<String>) {
    let rows: Vec<&Node> = t.elems().filter(|e| e.is("w:tr")).collect();
    // 版式表（单行单列 / 单列多段落）：拆回正文
    let single_cell = rows.len() == 1 && rows[0].elems().filter(|e| e.is("w:tc")).count() == 1;
    if single_cell {
        let tc = rows[0].elems().find(|e| e.is("w:tc")).unwrap();
        let mut inner = Vec::new();
        walk_body(tc, cx, &mut inner);
        // 单格表通常是「提示框」：用引用承载
        let quoted: Vec<String> = inner.into_iter().map(|b| if b.starts_with('\u{0}') || b.starts_with('#') || b.starts_with('|') { b } else { b.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n") }).collect();
        out.extend(quoted);
        return;
    }
    let mut grid: Vec<Vec<String>> = Vec::new();
    for tr in rows {
        let mut row = Vec::new();
        for tc in tr.elems().filter(|e| e.is("w:tc")) {
            let tcpr = tc.child("w:tcPr");
            let span: usize = tcpr.and_then(|p| p.child("w:gridSpan")).and_then(|g| g.attr("w:val")).and_then(|v| v.parse().ok()).unwrap_or(1);
            let vmerge_cont = tcpr.and_then(|p| p.child("w:vMerge")).map_or(false, |v| v.attr("w:val").map_or(true, |x| x == "continue"));
            let text = if vmerge_cont {
                String::new()
            } else {
                let mut inner = Vec::new();
                walk_body(tc, cx, &mut inner);
                inner
                    .into_iter()
                    .map(|b| b.trim_start_matches('\u{0}').trim_start_matches("CODE").trim_start_matches('#').trim().to_string())
                    .filter(|b| !b.is_empty())
                    .collect::<Vec<_>>()
                    .join("<br>")
            };
            row.push(text);
            for _ in 1..span {
                row.push(String::new());
            }
        }
        grid.push(row);
    }
    grid.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    if grid.is_empty() {
        return;
    }
    // 表头整格加粗是样式，不是内容
    if let Some(h) = grid.first_mut() {
        for c in h.iter_mut() {
            if c.starts_with("**") && c.ends_with("**") && c.len() > 4 && !c[2..c.len() - 2].contains("**") {
                *c = c[2..c.len() - 2].to_string();
            }
        }
    }
    out.push(mdw::table(&grid));
}

/// 连续代码行合并为一个 fence。
fn merge_code(blocks: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    let flush = |buf: &mut Vec<String>, out: &mut Vec<String>| {
        if buf.is_empty() {
            return;
        }
        let body = buf.join("\n");
        let lang = guess(&body);
        out.push(format!("```{lang}\n{body}\n```"));
        buf.clear();
    };
    for b in blocks {
        if let Some(code) = b.strip_prefix("\u{0}CODE") {
            buf.push(code.to_string());
        } else {
            flush(&mut buf, &mut out);
            out.push(b);
        }
    }
    flush(&mut buf, &mut out);
    out
}

fn guess(body: &str) -> &'static str {
    let t = body.trim_start();
    if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde_json::Value>(t).is_ok() {
        return "json";
    }
    if body.contains("def ") || body.contains("import ") && !body.contains(';') {
        return "python";
    }
    if body.contains("function ") || body.contains("const ") || body.contains("=>") {
        return "javascript";
    }
    if body.to_uppercase().contains("SELECT ") && body.to_uppercase().contains(" FROM ") {
        return "sql";
    }
    ""
}

// ───────────────────────── OMML → LaTeX ─────────────────────────

pub fn omml(n: &Node) -> String {
    let mut s = String::new();
    for c in n.elems() {
        s.push_str(&omml_el(c));
    }
    s
}

fn arg(n: &Node, name: &str) -> String {
    n.child(name).map(omml).unwrap_or_default()
}

fn omml_el(c: &Node) -> String {
    match c.name.as_str() {
        "m:r" => c.elems().filter(|e| e.is("m:t") || e.is("w:t")).map(|t| tex_text(&t.text())).collect(),
        "m:f" => format!("\\frac{{{}}}{{{}}}", arg(c, "m:num"), arg(c, "m:den")),
        "m:sSup" => format!("{{{}}}^{{{}}}", arg(c, "m:e"), arg(c, "m:sup")),
        "m:sSub" => format!("{{{}}}_{{{}}}", arg(c, "m:e"), arg(c, "m:sub")),
        "m:sSubSup" => format!("{{{}}}_{{{}}}^{{{}}}", arg(c, "m:e"), arg(c, "m:sub"), arg(c, "m:sup")),
        "m:rad" => {
            let deg = arg(c, "m:deg");
            if deg.trim().is_empty() { format!("\\sqrt{{{}}}", arg(c, "m:e")) } else { format!("\\sqrt[{deg}]{{{}}}", arg(c, "m:e")) }
        }
        "m:nary" => {
            let chr = c.find("m:chr").and_then(|x| x.attr("m:val")).unwrap_or("∫");
            let op = match chr { "∑" => "\\sum", "∏" => "\\prod", "∮" => "\\oint", "⋃" => "\\bigcup", "⋂" => "\\bigcap", _ => "\\int" };
            let sub = arg(c, "m:sub");
            let sup = arg(c, "m:sup");
            format!("{op}{}{} {}", if sub.is_empty() { String::new() } else { format!("_{{{sub}}}") }, if sup.is_empty() { String::new() } else { format!("^{{{sup}}}") }, arg(c, "m:e"))
        }
        "m:d" => {
            let beg = c.find("m:begChr").and_then(|x| x.attr("m:val")).unwrap_or("(");
            let end = c.find("m:endChr").and_then(|x| x.attr("m:val")).unwrap_or(")");
            let inner: Vec<String> = c.elems().filter(|e| e.is("m:e")).map(omml).collect();
            let fix = |s: &str| match s { "{" => "\\{", "}" => "\\}", x => x }.to_string();
            format!("\\left{}{}\\right{}", fix(beg), inner.join(","), fix(end))
        }
        "m:func" => format!("{}{{{}}}", arg(c, "m:fName"), arg(c, "m:e")),
        "m:bar" => format!("\\overline{{{}}}", arg(c, "m:e")),
        "m:acc" => format!("\\hat{{{}}}", arg(c, "m:e")),
        "m:limLow" => format!("{}_{{{}}}", arg(c, "m:e"), arg(c, "m:lim")),
        "m:limUpp" => format!("{}^{{{}}}", arg(c, "m:e"), arg(c, "m:lim")),
        "m:m" => {
            let rows: Vec<String> = c.elems().filter(|e| e.is("m:mr")).map(|r| r.elems().filter(|e| e.is("m:e")).map(omml).collect::<Vec<_>>().join(" & ")).collect();
            format!("\\begin{{matrix}}{}\\end{{matrix}}", rows.join(" \\\\ "))
        }
        "m:eqArr" => c.elems().filter(|e| e.is("m:e")).map(omml).collect::<Vec<_>>().join(" \\\\ "),
        "m:box" | "m:borderBox" | "m:groupChr" | "m:phant" | "m:e" | "m:oMath" => omml(c),
        _ if c.name.ends_with("Pr") => String::new(),
        _ => omml(c),
    }
}

fn tex_text(s: &str) -> String {
    let mut o = String::new();
    for ch in s.chars() {
        let rep = match ch {
            'α' => "\\alpha ", 'β' => "\\beta ", 'γ' => "\\gamma ", 'δ' => "\\delta ", 'ε' => "\\epsilon ", 'θ' => "\\theta ", 'λ' => "\\lambda ",
            'μ' => "\\mu ", 'π' => "\\pi ", 'σ' => "\\sigma ", 'φ' => "\\phi ", 'ω' => "\\omega ", 'Δ' => "\\Delta ", 'Σ' => "\\Sigma ", 'Ω' => "\\Omega ",
            '×' => "\\times ", '÷' => "\\div ", '±' => "\\pm ", '≤' => "\\leq ", '≥' => "\\geq ", '≠' => "\\neq ", '≈' => "\\approx ", '∞' => "\\infty ",
            '→' => "\\to ", '∂' => "\\partial ", '·' => "\\cdot ", '∈' => "\\in ",
            _ => {
                o.push(ch);
                continue;
            }
        };
        o.push_str(rep);
    }
    o
}

#[allow(dead_code)]
fn unused(_: &Node) {
    let _ = xml::escape("");
}
