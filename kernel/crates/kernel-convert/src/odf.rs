//! OpenDocument 文本（.odt / .fodt）→ Markdown。
//! 标题（text:h / 标题样式）、列表（项目符号 / 编号，多级）、表格（合并单元格）、链接、
//! 粗斜体删除线高亮等宽、上下标、图片、脚注、代码段（等宽 / Preformatted 样式）、引用样式。

use crate::mdw::{self, Fmt, Piece};
use crate::xml::{self, Node};
use crate::zip::ZipReader;
use crate::{Converted, Image};
use std::collections::HashMap;

#[derive(Default, Clone)]
struct St {
    parent: Option<String>,
    display: String,
    outline: Option<u8>,
    #[allow(dead_code)]
    list_style: Option<String>,
    f: Fmt,
    mono: bool,
}

struct Ctx<'a> {
    zip: Option<&'a ZipReader<'a>>,
    styles: HashMap<String, St>,
    /// 列表样式名 → 每级是否编号
    lists: HashMap<String, Vec<bool>>,
    images: Vec<Image>,
    notes: Vec<String>,
}

pub fn convert_text(bytes: &[u8]) -> Result<Converted, String> {
    let zip = ZipReader::new(bytes).map_err(|e| e.to_string())?;
    let content = zip.read_string("content.xml").ok_or("缺少 content.xml（不是 OpenDocument？）")?;
    let mut cx = Ctx { zip: Some(&zip), styles: HashMap::new(), lists: HashMap::new(), images: Vec::new(), notes: Vec::new() };
    if let Some(s) = zip.read_string("styles.xml") {
        load_styles(&xml::parse(&s), &mut cx);
    }
    let root = xml::parse(&content);
    load_styles(&root, &mut cx);
    let title = zip.read_string("meta.xml").and_then(|m| xml::parse(&m).find("dc:title").map(|t| t.text().trim().to_string())).filter(|t| !t.is_empty());
    finish(&root, cx, title)
}

pub fn convert_flat(src: &str) -> Result<Converted, String> {
    let root = xml::parse(src);
    let mut cx = Ctx { zip: None, styles: HashMap::new(), lists: HashMap::new(), images: Vec::new(), notes: Vec::new() };
    load_styles(&root, &mut cx);
    let title = root.find("dc:title").map(|t| t.text().trim().to_string()).filter(|t| !t.is_empty());
    finish(&root, cx, title)
}

fn finish(root: &Node, mut cx: Ctx, meta_title: Option<String>) -> Result<Converted, String> {
    let text = root.find("office:text").ok_or("文档没有正文")?;
    let mut blocks = Vec::new();
    walk(text, &mut cx, &mut blocks, 0);
    let blocks = merge_code(blocks);
    let mut title = None;
    let mut blocks = blocks;
    // 「Title」样式 / 唯一开头一级标题 → 文档标题
    if let Some(i) = blocks.iter().position(|b| b.starts_with("\u{1}TITLE")) {
        title = Some(blocks.remove(i)[6..].trim().to_string());
    }
    if title.is_none() {
        let h1: Vec<usize> = blocks.iter().enumerate().filter(|(_, b)| b.starts_with("# ")).map(|(i, _)| i).collect();
        if h1.len() == 1 && h1[0] == 0 {
            title = Some(blocks.remove(0)[2..].trim().to_string());
        } else if !h1.is_empty() {
            blocks = blocks.into_iter().map(|b| if b.starts_with('#') { format!("#{b}") } else { b }).collect();
        }
    }
    let mut md = mdw::join_blocks(&blocks);
    if !cx.notes.is_empty() {
        md.push('\n');
        for (i, n) in cx.notes.iter().enumerate() {
            md.push_str(&format!("[^{}]: {}\n", i + 1, n));
        }
    }
    Ok(Converted { title: title.or(meta_title), markdown: md, images: cx.images, warnings: Vec::new() })
}

fn load_styles(root: &Node, cx: &mut Ctx) {
    let mut all = Vec::new();
    root.find_all("style:style", &mut all);
    for s in all {
        let Some(name) = s.attr("style:name") else { continue };
        let mut st = St {
            parent: s.attr("style:parent-style-name").map(String::from),
            display: s.attr("style:display-name").unwrap_or(name).to_string(),
            outline: s.attr("style:default-outline-level").and_then(|v| v.parse().ok()),
            list_style: s.attr("style:list-style-name").map(String::from),
            ..Default::default()
        };
        if let Some(tp) = s.child("style:text-properties") {
            st.f.bold = tp.attr("fo:font-weight").map_or(false, |w| w == "bold" || w.parse::<u32>().map_or(false, |n| n >= 600));
            st.f.italic = tp.attr("fo:font-style").map_or(false, |v| v == "italic" || v == "oblique");
            st.f.strike = tp.attr("style:text-line-through-style").map_or(false, |v| v != "none");
            st.f.mark = tp.attr("fo:background-color").map_or(false, |v| v != "transparent" && !v.is_empty());
            let pos = tp.attr("style:text-position").unwrap_or("");
            st.f.sup = pos.starts_with("super") || pos.starts_with("33%");
            st.f.sub = pos.starts_with("sub") || pos.starts_with("-33%");
            st.mono = tp.attr("style:font-name").map_or(false, mono_name) || tp.attr("fo:font-family").map_or(false, mono_name);
        }
        cx.styles.insert(name.to_string(), st);
    }
    let mut ls = Vec::new();
    root.find_all("text:list-style", &mut ls);
    for l in ls {
        let Some(name) = l.attr("style:name") else { continue };
        let mut levels = vec![false; 10];
        for lv in l.elems() {
            let k: usize = lv.attr("text:level").and_then(|v| v.parse().ok()).unwrap_or(1);
            if (1..=10).contains(&k) {
                levels[k - 1] = lv.local() == "list-level-style-number" && lv.attr("style:num-format").map_or(false, |f| !f.is_empty());
            }
        }
        cx.lists.insert(name.to_string(), levels);
    }
}

fn mono_name(s: &str) -> bool {
    let f = s.to_ascii_lowercase();
    ["mono", "courier", "consolas", "menlo", "monaco", "source code", "liberation mono", "dejavu sans mono"].iter().any(|m| f.contains(m))
}

fn chain<T>(cx: &Ctx, name: &str, f: impl Fn(&St) -> Option<T>) -> Option<T> {
    let mut cur = Some(name.to_string());
    let mut guard = 0;
    while let Some(n) = cur {
        guard += 1;
        if guard > 12 {
            break;
        }
        let st = cx.styles.get(&n)?;
        if let Some(v) = f(st) {
            return Some(v);
        }
        cur = st.parent.clone();
    }
    None
}

fn chain_names(cx: &Ctx, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Some(name.to_string());
    let mut guard = 0;
    while let Some(n) = cur {
        guard += 1;
        if guard > 12 {
            break;
        }
        let Some(st) = cx.styles.get(&n) else {
            out.push(n.replace("_20_", " "));
            break;
        };
        out.push(st.display.replace("_20_", " "));
        cur = st.parent.clone();
    }
    out
}

fn style_fmt(cx: &Ctx, name: &str) -> (Fmt, bool) {
    let mut f = Fmt::default();
    let mut mono = false;
    let mut cur = Some(name.to_string());
    let mut guard = 0;
    while let Some(n) = cur {
        guard += 1;
        if guard > 12 {
            break;
        }
        let Some(st) = cx.styles.get(&n) else { break };
        f.bold |= st.f.bold;
        f.italic |= st.f.italic;
        f.strike |= st.f.strike;
        f.mark |= st.f.mark;
        f.sup |= st.f.sup;
        f.sub |= st.f.sub;
        mono |= st.mono;
        cur = st.parent.clone();
    }
    (f, mono)
}

fn walk(node: &Node, cx: &mut Ctx, out: &mut Vec<String>, depth: usize) {
    for c in node.elems() {
        match c.name.as_str() {
            "text:h" => {
                let level: u8 = c.attr("text:outline-level").and_then(|v| v.parse().ok()).unwrap_or(1);
                let t = para_inline(c, cx);
                if !t.trim().is_empty() {
                    out.push(format!("{} {}", "#".repeat(level.clamp(1, 6) as usize), t.replace("**", "").trim()));
                }
            }
            "text:p" => {
                let style = c.attr("text:style-name").unwrap_or("").to_string();
                // 自动样式（P1…）挂在命名样式下：整条链的名字一起看
                let disp = chain_names(cx, &style).join("|").to_lowercase();
                let outline = chain(cx, &style, |s| s.outline);
                let (pf, mono) = style_fmt(cx, &style);
                let all_text = c.text();
                if disp.split('|').any(|d| d == "title" || d == "标题") {
                    out.push(format!("\u{1}TITLE{}", all_text.trim()));
                    continue;
                }
                if let Some(o) = outline.filter(|_| disp.contains("heading") || disp.contains("标题")) {
                    out.push(format!("{} {}", "#".repeat(o.clamp(1, 6) as usize), all_text.trim()));
                    continue;
                }
                if mono || disp.contains("preformatted") || disp.contains("source") || disp.contains("code") || disp.contains("代码") {
                    out.push(format!("\u{0}CODE{}", raw_text(c)));
                    continue;
                }
                let mut t = para_inline_fmt(c, cx, &pf);
                t = t.split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n\n");
                if t.trim().is_empty() {
                    continue;
                }
                if disp.contains("quot") || disp.contains("引用") {
                    out.push(t.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n"));
                } else {
                    out.push(t);
                }
                // 段落里的图片框也可能带说明文字：已在 para_inline 里处理
            }
            "text:list" => list(c, cx, out, depth, None),
            "table:table" => table(c, cx, out),
            "text:section" | "text:soft-page-break" | "office:text" | "text:index-body" => walk(c, cx, out, depth),
            "text:table-of-content" => {} // 目录不导入
            _ => {}
        }
    }
}

fn list(l: &Node, cx: &mut Ctx, out: &mut Vec<String>, depth: usize, inherited: Option<String>) {
    let style = l.attr("text:style-name").map(String::from).or(inherited);
    let numbered = style.as_ref().and_then(|s| cx.lists.get(s)).map(|v| v.get(depth).copied().unwrap_or(false)).unwrap_or(false);
    let mut n = l.attr("text:start-value").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1);
    for item in l.elems().filter(|e| e.is("text:list-item") || e.is("text:list-header")) {
        let mut first = true;
        for c in item.elems() {
            match c.name.as_str() {
                "text:p" | "text:h" => {
                    let t = para_inline(c, cx);
                    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
                    if t.is_empty() {
                        continue;
                    }
                    let pad = "  ".repeat(depth);
                    if first {
                        out.push(if numbered { format!("{pad}{n}. {t}") } else { format!("{pad}- {t}") });
                        first = false;
                    } else {
                        out.push(format!("{pad}  {t}"));
                    }
                }
                "text:list" => list(c, cx, out, depth + 1, style.clone()),
                "table:table" => table(c, cx, out),
                _ => {}
            }
        }
        if !first {
            n += 1;
        }
    }
}

fn table(t: &Node, cx: &mut Ctx, out: &mut Vec<String>) {
    let mut rows_nodes = Vec::new();
    collect_rows(t, &mut rows_nodes);
    let mut grid: Vec<Vec<String>> = Vec::new();
    for r in rows_nodes {
        let mut row = Vec::new();
        for c in r.elems() {
            match c.name.as_str() {
                "table:table-cell" => {
                    let rep: usize = c.attr("table:number-columns-repeated").and_then(|v| v.parse().ok()).unwrap_or(1).min(64);
                    let mut parts = Vec::new();
                    for p in c.elems() {
                        if p.is("text:p") || p.is("text:h") {
                            let s = para_inline(p, cx);
                            if !s.trim().is_empty() {
                                parts.push(s.trim().to_string());
                            }
                        } else if p.is("text:list") {
                            let mut b = Vec::new();
                            list(p, cx, &mut b, 0, None);
                            parts.extend(b.into_iter().map(|x| x.trim().to_string()));
                        }
                    }
                    let text = parts.join("<br>");
                    for _ in 0..rep {
                        row.push(text.clone());
                    }
                }
                "table:covered-table-cell" => row.push(String::new()),
                _ => {}
            }
        }
        while row.last().map_or(false, |c| c.is_empty()) {
            row.pop();
        }
        grid.push(row);
    }
    grid.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    if grid.is_empty() {
        return;
    }
    if grid.len() == 1 && grid[0].len() == 1 {
        out.push(format!("> {}", grid[0][0]));
        return;
    }
    out.push(mdw::table(&grid));
}

fn collect_rows<'a>(n: &'a Node, out: &mut Vec<&'a Node>) {
    for c in n.elems() {
        if c.is("table:table-row") {
            out.push(c);
        } else if c.is("table:table-header-rows") || c.is("table:table-rows") || c.is("table:table-row-group") {
            collect_rows(c, out);
        }
    }
}

fn raw_text(n: &Node) -> String {
    let mut s = String::new();
    for c in &n.children {
        match c {
            xml::Child::T(t) => s.push_str(t),
            xml::Child::E(e) => match e.name.as_str() {
                "text:s" => s.push_str(&" ".repeat(e.attr("text:c").and_then(|v| v.parse().ok()).unwrap_or(1))),
                "text:tab" => s.push('\t'),
                "text:line-break" => s.push('\n'),
                _ => s.push_str(&raw_text(e)),
            },
        }
    }
    s
}

fn para_inline(p: &Node, cx: &mut Ctx) -> String {
    para_inline_fmt(p, cx, &Fmt::default())
}

fn para_inline_fmt(p: &Node, cx: &mut Ctx, base: &Fmt) -> String {
    let mut pieces = Vec::new();
    spans(p, cx, base.clone(), &mut pieces);
    mdw::inline(&pieces)
}

fn spans(n: &Node, cx: &mut Ctx, f: Fmt, out: &mut Vec<Piece>) {
    for c in &n.children {
        match c {
            xml::Child::T(t) => {
                // ODF 里文本的空白折叠：多空格用 text:s 表达
                let t = t.replace(['\n', '\r'], "");
                if !t.is_empty() {
                    out.push(Piece::Text(t, f.clone()));
                }
            }
            xml::Child::E(e) => match e.name.as_str() {
                "text:span" => {
                    let (sf, mono) = style_fmt(cx, e.attr("text:style-name").unwrap_or(""));
                    let mut f2 = f.clone();
                    f2.bold |= sf.bold;
                    f2.italic |= sf.italic;
                    f2.strike |= sf.strike;
                    f2.mark |= sf.mark;
                    f2.sup |= sf.sup;
                    f2.sub |= sf.sub;
                    f2.code |= mono;
                    spans(e, cx, f2, out);
                }
                "text:a" => {
                    let url = e.attr("xlink:href").unwrap_or("").to_string();
                    let mut inner = Vec::new();
                    spans(e, cx, f.clone(), &mut inner);
                    out.push(Piece::Link(inner, url));
                }
                "text:s" => out.push(Piece::Text(" ".repeat(e.attr("text:c").and_then(|v| v.parse().ok()).unwrap_or(1).min(8)), f.clone())),
                "text:tab" => out.push(Piece::Text(" ".into(), f.clone())),
                "text:line-break" => out.push(Piece::Text("\n".into(), Fmt::default())),
                "text:note" => {
                    if let Some(body) = e.child("text:note-body") {
                        let mut ps = Vec::new();
                        for p in body.elems() {
                            spans(p, cx, Fmt::default(), &mut ps);
                            ps.push(Piece::Text(" ".into(), Fmt::default()));
                        }
                        cx.notes.push(mdw::inline(&ps).trim().to_string());
                        out.push(Piece::Raw(format!("[^{}]", cx.notes.len())));
                    }
                }
                "draw:frame" | "draw:a" => {
                    let mut imgs = Vec::new();
                    e.find_all("draw:image", &mut imgs);
                    let alt = e.find("svg:title").or_else(|| e.find("svg:desc")).map(|t| t.text()).unwrap_or_else(|| "图片".into());
                    let mut got = false;
                    for img in imgs {
                        let href = img.attr("xlink:href").unwrap_or("");
                        let bytes = if let Some(z) = cx.zip { z.read(href.trim_start_matches("./")) } else { None };
                        let bytes = bytes.or_else(|| img.child("office:binary-data").and_then(|b| b64(&b.text())));
                        if let Some(bytes) = bytes {
                            let name = href.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("image.png").to_string();
                            let token = crate::push_image(&mut cx.images, name, bytes);
                            out.push(Piece::Raw(format!("![{}]({token})", alt.trim())));
                            got = true;
                            break;
                        }
                    }
                    if !got {
                        // 文本框
                        if let Some(tb) = e.find("draw:text-box") {
                            for p in tb.elems() {
                                spans(p, cx, f.clone(), out);
                                out.push(Piece::Text("\n".into(), Fmt::default()));
                            }
                        }
                    }
                }
                "text:bookmark" | "text:bookmark-start" | "text:bookmark-end" | "text:soft-page-break" | "office:annotation" | "text:sequence-decls" => {}
                _ => spans(e, cx, f.clone(), out),
            },
        }
    }
}

fn b64(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let Some(v) = T.iter().position(|x| *x == c) else { continue };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn merge_code(blocks: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf: Vec<String> = Vec::new();
    for b in blocks {
        if let Some(code) = b.strip_prefix("\u{0}CODE") {
            buf.push(code.to_string());
            continue;
        }
        if !buf.is_empty() {
            out.push(format!("```\n{}\n```", buf.join("\n")));
            buf.clear();
        }
        out.push(b);
    }
    if !buf.is_empty() {
        out.push(format!("```\n{}\n```", buf.join("\n")));
    }
    out
}
