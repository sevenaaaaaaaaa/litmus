//! Markdown → .docx / .odt（原生生成，无外部工具）。
//!
//! 版式目标：在 Word / WPS / Pages / LibreOffice 里打开就是一篇排好的文档——
//! 标题样式（可生成目录）、正文 1.5 倍行距、中文字体、真列表（项目符号 / 编号自动续号）、
//! 带表头底色与边框的表格（表头跨页重复）、等宽底纹代码块、左边线引用、可点击超链接、
//! 内嵌图片（按页宽等比缩放）、分隔线。

use crate::xml::escape;
use crate::zip::ZipWriter;
use kernel_md::{Block, DocModel};

// ───────────────────────── 行内 ─────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RFmt {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub mark: bool,
    pub math: bool,
}

#[derive(Debug, Clone)]
pub enum Run {
    Text(String, RFmt),
    Link(Vec<Run>, String),
    Image(String, String),
    Break,
}

/// Markdown 行内 → runs。
pub fn parse_inline(s: &str) -> Vec<Run> {
    let mut out = Vec::new();
    inline_into(s, RFmt::default(), &mut out);
    merge(out)
}

fn merge(v: Vec<Run>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for r in v {
        match (out.last_mut(), &r) {
            (Some(Run::Text(a, fa)), Run::Text(b, fb)) if fa == fb => a.push_str(b),
            _ => out.push(r),
        }
    }
    out
}

fn find_close(s: &str, from: usize, mark: &str) -> Option<usize> {
    let rest = &s[from..];
    let j = rest.find(mark)?;
    let inner = &rest[..j];
    if inner.is_empty() || inner.starts_with(' ') || inner.ends_with(' ') {
        return None;
    }
    Some(from + j)
}

fn inline_into(s: &str, f: RFmt, out: &mut Vec<Run>) {
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, out: &mut Vec<Run>, f: &RFmt| {
        if !buf.is_empty() {
            out.push(Run::Text(std::mem::take(buf), f.clone()));
        }
    };
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().unwrap();
        // 行内代码
        if c == '`' {
            let ticks = rest.chars().take_while(|c| *c == '`').count();
            let fence = "`".repeat(ticks);
            if let Some(j) = rest[ticks..].find(&fence) {
                flush(&mut buf, out, &f);
                let mut cf = f.clone();
                cf.code = true;
                out.push(Run::Text(rest[ticks..ticks + j].trim().to_string(), cf));
                i += ticks + j + ticks;
                continue;
            }
        }
        // 图片
        if rest.starts_with("![") {
            if let Some(close) = rest.find("](") {
                if let Some(end) = rest[close + 2..].find(')') {
                    flush(&mut buf, out, &f);
                    let alt = rest[2..close].to_string();
                    let src = rest[close + 2..close + 2 + end].trim().to_string();
                    out.push(Run::Image(alt, src));
                    i += close + 2 + end + 1;
                    continue;
                }
            }
        }
        // 双链
        if rest.starts_with("[[") {
            if let Some(end) = rest.find("]]") {
                flush(&mut buf, out, &f);
                let raw = &rest[2..end];
                let label = raw.split_once('|').map(|(_, l)| l).unwrap_or(raw);
                out.push(Run::Text(label.to_string(), f.clone()));
                i += end + 2;
                continue;
            }
        }
        // 链接
        if c == '[' {
            if let Some(close) = rest.find("](") {
                if !rest[1..close].contains('[') {
                    if let Some(end) = rest[close + 2..].find(')') {
                        flush(&mut buf, out, &f);
                        let mut inner = Vec::new();
                        inline_into(&rest[1..close], f.clone(), &mut inner);
                        out.push(Run::Link(inner, rest[close + 2..close + 2 + end].trim().to_string()));
                        i += close + 2 + end + 1;
                        continue;
                    }
                }
            }
        }
        // 公式
        if c == '$' && !rest.starts_with("$$") {
            if let Some(j) = find_close(s, i + 1, "$") {
                flush(&mut buf, out, &f);
                let mut mf = f.clone();
                mf.math = true;
                out.push(Run::Text(latex_plain(&s[i + 1..j]), mf));
                i = j + 1;
                continue;
            }
        }
        // 强调
        let mut matched = false;
        for (mark, kind) in [("**", 0), ("__", 0), ("~~", 1), ("==", 2), ("*", 3)] {
            if rest.starts_with(mark) {
                if let Some(j) = find_close(s, i + mark.len(), mark) {
                    flush(&mut buf, out, &f);
                    let mut nf = f.clone();
                    match kind {
                        0 => nf.bold = true,
                        1 => nf.strike = true,
                        2 => nf.mark = true,
                        _ => nf.italic = true,
                    }
                    inline_into(&s[i + mark.len()..j], nf, out);
                    i = j + mark.len();
                    matched = true;
                    break;
                }
            }
        }
        if matched {
            continue;
        }
        // <br>
        if rest.starts_with("<br>") || rest.starts_with("<br/>") || rest.starts_with("<br />") {
            flush(&mut buf, out, &f);
            out.push(Run::Break);
            i += rest.find('>').unwrap() + 1;
            continue;
        }
        // 裸链接
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(|ch: char| ch.is_whitespace() || "，。；！？）】」、<>\"".contains(ch)).unwrap_or(rest.len());
            let url = rest[..end].trim_end_matches(['.', ',', ')']).to_string();
            flush(&mut buf, out, &f);
            out.push(Run::Link(vec![Run::Text(url.clone(), f.clone())], url.clone()));
            i += url.len();
            continue;
        }
        buf.push(c);
        i += c.len_utf8();
    }
    flush(&mut buf, out, &f);
}

/// LaTeX → 可读文本（Word 里退化为斜体公式文本）。
pub fn latex_plain(s: &str) -> String {
    let mut t = s.to_string();
    for (a, b) in [
        ("\\alpha", "α"), ("\\beta", "β"), ("\\gamma", "γ"), ("\\delta", "δ"), ("\\theta", "θ"), ("\\lambda", "λ"), ("\\mu", "μ"), ("\\pi", "π"),
        ("\\sigma", "σ"), ("\\omega", "ω"), ("\\Delta", "Δ"), ("\\Sigma", "Σ"), ("\\times", "×"), ("\\cdot", "·"), ("\\leq", "≤"), ("\\geq", "≥"),
        ("\\neq", "≠"), ("\\approx", "≈"), ("\\infty", "∞"), ("\\pm", "±"), ("\\sum", "∑"), ("\\int", "∫"), ("\\to", "→"), ("\\partial", "∂"),
        ("\\left", ""), ("\\right", ""), ("\\,", " "), ("\\;", " "),
    ] {
        t = t.replace(a, b);
    }
    // \frac{a}{b} → (a)/(b)；\sqrt{x} → √(x)
    while let Some(p) = t.find("\\frac{") {
        let Some((a, ra)) = brace(&t[p + 5..]) else { break };
        let Some((b, rb)) = brace(&t[p + 5 + ra..]) else { break };
        t = format!("{}({})/({}){}", &t[..p], a, b, &t[p + 5 + ra + rb..]);
    }
    while let Some(p) = t.find("\\sqrt{") {
        let Some((a, ra)) = brace(&t[p + 5..]) else { break };
        t = format!("{}√({}){}", &t[..p], a, &t[p + 5 + ra..]);
    }
    // 上下标：x^2 / x^{10} / a_1 → Unicode
    let sup = |c: char| "0123456789+-=()n".find(c).map(|i| "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿ".chars().nth(i).unwrap());
    let sub = |c: char| "0123456789+-=()".find(c).map(|i| "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎".chars().nth(i).unwrap());
    let chars: Vec<char> = t.chars().collect();
    let mut o = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c == '^' || c == '_') && i + 1 < chars.len() {
            let map: &dyn Fn(char) -> Option<char> = if c == '^' { &sup } else { &sub };
            let (body, next): (Vec<char>, usize) = if chars[i + 1] == '{' {
                let end = chars[i + 2..].iter().position(|x| *x == '}').map(|k| i + 2 + k).unwrap_or(chars.len());
                (chars[i + 2..end.min(chars.len())].to_vec(), end + 1)
            } else {
                (vec![chars[i + 1]], i + 2)
            };
            if !body.is_empty() && body.iter().all(|x| map(*x).is_some()) {
                o.extend(body.iter().map(|x| map(*x).unwrap()));
                i = next;
                continue;
            }
        }
        o.push(c);
        i += 1;
    }
    o.replace(['{', '}'], "")
}

fn brace(s: &str) -> Option<(String, usize)> {
    if !s.starts_with('{') {
        return None;
    }
    let mut d = 0;
    for (i, c) in s.char_indices() {
        match c {
            '{' => d += 1,
            '}' => {
                d -= 1;
                if d == 0 {
                    return Some((s[1..i].to_string(), i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn join_soft(t: &str) -> String {
    let lines: Vec<&str> = t.split('\n').map(|l| l.trim()).collect();
    let mut s = String::new();
    for l in lines {
        if l.is_empty() {
            continue;
        }
        if let (Some(a), Some(b)) = (s.chars().last(), l.chars().next()) {
            let cjk = |c: char| ('\u{2e80}'..='\u{9fff}').contains(&c) || ('\u{ff00}'..='\u{ffef}').contains(&c) || ('\u{3000}'..='\u{303f}').contains(&c);
            if !(cjk(a) || cjk(b)) {
                s.push(' ');
            }
        }
        s.push_str(l);
    }
    s
}

fn strip_tags(s: &str) -> String {
    let mut o = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => o.push(c),
            _ => {}
        }
    }
    o
}

// ───────────────────────── 图片尺寸 ─────────────────────────

/// (宽, 高) 像素；不认识的格式给个默认值。
pub fn image_size(b: &[u8]) -> (u32, u32) {
    if b.len() > 24 && &b[..8] == b"\x89PNG\r\n\x1a\n" {
        return (u32::from_be_bytes([b[16], b[17], b[18], b[19]]), u32::from_be_bytes([b[20], b[21], b[22], b[23]]));
    }
    if b.len() > 10 && &b[..6] == b"GIF89a" || b.len() > 10 && &b[..6] == b"GIF87a" {
        return (u16::from_le_bytes([b[6], b[7]]) as u32, u16::from_le_bytes([b[8], b[9]]) as u32);
    }
    if b.len() > 4 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let m = b[i + 1];
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC {
                return (u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32, u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32);
            }
            i += 2 + len;
        }
    }
    if b.len() > 30 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        if &b[12..16] == b"VP8X" {
            let w = 1 + (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16);
            let h = 1 + (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16);
            return (w, h);
        }
    }
    (800, 600)
}

fn image_ext(b: &[u8]) -> &'static str {
    if b.starts_with(b"\x89PNG") {
        "png"
    } else if b.starts_with(&[0xFF, 0xD8]) {
        "jpeg"
    } else if b.starts_with(b"GIF8") {
        "gif"
    } else if b.len() > 12 && &b[8..12] == b"WEBP" {
        "webp"
    } else if b.starts_with(b"<svg") || b.starts_with(b"<?xml") {
        "svg"
    } else {
        "png"
    }
}

/// 导出参数：标题 + 图片读取回调（库内相对路径 / data URI → 字节）。
pub struct ExportOpts<'a> {
    pub title: Option<String>,
    pub load_image: &'a dyn Fn(&str) -> Option<Vec<u8>>,
}

fn load_img(opts: &ExportOpts, src: &str) -> Option<Vec<u8>> {
    if let Some(d) = src.strip_prefix("data:") {
        let (_, b64) = d.split_once(";base64,")?;
        return decode_b64(b64);
    }
    let b = (opts.load_image)(src)?;
    // Word/ODT 不认 webp / svg：不嵌入
    matches!(image_ext(&b), "png" | "jpeg" | "gif").then_some(b)
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0);
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
    Some(out)
}

const CN_FONT: &str = "PingFang SC";
const CN_FONT_ALT: &str = "Microsoft YaHei";
const LATIN_FONT: &str = "Calibri";
const MONO_FONT: &str = "Menlo";

// ───────────────────────── DOCX ─────────────────────────

struct DocxCx<'a> {
    opts: &'a ExportOpts<'a>,
    /// 文档有标题（Title 样式）时，Markdown 的 ## 对应 Word 的「标题 1」
    shift: u8,
    rels: Vec<(String, String, bool)>, // (id, target, external)
    media: Vec<(String, Vec<u8>)>,
    nums: Vec<u32>, // 每个有序列表实例一个 num（自动续号 / 重新起编）
    img_id: u32,
}

impl DocxCx<'_> {
    fn rel(&mut self, target: &str, external: bool) -> String {
        if let Some((id, _, _)) = self.rels.iter().find(|(_, t, e)| t == target && *e == external) {
            return id.clone();
        }
        let id = format!("rId{}", 100 + self.rels.len());
        self.rels.push((id.clone(), target.to_string(), external));
        id
    }
}

pub fn to_docx(md: &str, opts: &ExportOpts) -> Result<Vec<u8>, String> {
    let doc = kernel_md::from_markdown(&mark_levels(md)).map_err(|e| e.to_string())?;
    to_docx_model(&doc, opts)
}

pub fn to_docx_model(doc: &DocModel, opts: &ExportOpts) -> Result<Vec<u8>, String> {
    let title = opts.title.clone().or(doc.title.clone());
    let mut cx = DocxCx { opts, shift: if title.is_some() { 1 } else { 0 }, rels: Vec::new(), media: Vec::new(), nums: Vec::new(), img_id: 0 };
    let mut body = String::new();
    if let Some(t) = &title {
        body.push_str(&format!("<w:p><w:pPr><w:pStyle w:val=\"Title\"/></w:pPr>{}</w:p>", runs_docx(&parse_inline(t), &RFmt::default(), &mut cx)));
    }
    for b in &doc.blocks {
        body.push_str(&block_docx(b, &mut cx));
    }
    body.push_str("<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/>\
<w:pgNumType w:start=\"1\"/><w:docGrid w:linePitch=\"312\"/></w:sectPr>");
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:document {NS}><w:body>{body}</w:body></w:document>"
    );

    let mut rels = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings\" Target=\"settings.xml\"/>\
<Relationship Id=\"rId4\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable\" Target=\"fontTable.xml\"/>");
    for (id, t, ext) in &cx.rels {
        if *ext {
            rels.push_str(&format!("<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" Target=\"{}\" TargetMode=\"External\"/>", escape(t)));
        } else {
            rels.push_str(&format!("<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"{}\"/>", escape(t)));
        }
    }
    rels.push_str("</Relationships>");

    let mut z = ZipWriter::new();
    z.add("[Content_Types].xml", CONTENT_TYPES.as_bytes(), false);
    z.add("_rels/.rels", ROOT_RELS.as_bytes(), false);
    z.add("word/document.xml", document.as_bytes(), false);
    z.add("word/_rels/document.xml.rels", rels.as_bytes(), false);
    z.add("word/styles.xml", styles_xml().as_bytes(), false);
    z.add("word/numbering.xml", numbering_xml(&cx.nums).as_bytes(), false);
    z.add("word/settings.xml", SETTINGS.as_bytes(), false);
    z.add("word/fontTable.xml", font_table().as_bytes(), false);
    for (name, bytes) in &cx.media {
        z.add(&format!("word/media/{name}"), bytes, true);
    }
    let t = escape(title.as_deref().unwrap_or(""));
    z.add(
        "docProps/core.xml",
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><dc:title>{t}</dc:title><dc:creator>鹿蕊 Litmus</dc:creator></cp:coreProperties>").as_bytes(),
        false,
    );
    z.add("docProps/app.xml", "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>鹿蕊 Litmus</Application></Properties>".as_bytes(), false);
    Ok(z.finish())
}

const NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"";

const CONTENT_TYPES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Default Extension=\"png\" ContentType=\"image/png\"/><Default Extension=\"jpeg\" ContentType=\"image/jpeg\"/><Default Extension=\"gif\" ContentType=\"image/gif\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/word/numbering.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/>\
<Override PartName=\"/word/settings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml\"/>\
<Override PartName=\"/word/fontTable.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>\
</Types>";

const ROOT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/>\
</Relationships>";

const SETTINGS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:defaultTabStop w:val=\"420\"/><w:characterSpacingControl w:val=\"compressPunctuation\"/><w:compat><w:compatSetting w:name=\"compatibilityMode\" w:uri=\"http://schemas.microsoft.com/office/word\" w:val=\"15\"/></w:compat></w:settings>";

fn font_table() -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:fonts xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:font w:name=\"{CN_FONT}\"><w:altName w:val=\"{CN_FONT_ALT}\"/><w:charset w:val=\"86\"/><w:family w:val=\"swiss\"/></w:font>\
<w:font w:name=\"{LATIN_FONT}\"><w:family w:val=\"swiss\"/></w:font><w:font w:name=\"{MONO_FONT}\"><w:altName w:val=\"Consolas\"/><w:family w:val=\"modern\"/><w:pitch w:val=\"fixed\"/></w:font></w:fonts>")
}

fn styles_xml() -> String {
    let fonts = format!("<w:rFonts w:ascii=\"{LATIN_FONT}\" w:hAnsi=\"{LATIN_FONT}\" w:eastAsia=\"{CN_FONT}\" w:cs=\"{LATIN_FONT}\"/>");
    let mono = format!("<w:rFonts w:ascii=\"{MONO_FONT}\" w:hAnsi=\"{MONO_FONT}\" w:eastAsia=\"{CN_FONT}\" w:cs=\"{MONO_FONT}\"/>");
    let ink = "2B2A28";
    let accent = "2F5BB7";
    let mut s = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:docDefaults><w:rPrDefault><w:rPr>{fonts}<w:color w:val=\"{ink}\"/><w:sz w:val=\"22\"/><w:szCs w:val=\"22\"/><w:lang w:val=\"en-US\" w:eastAsia=\"zh-CN\"/></w:rPr></w:rPrDefault>\
<w:pPrDefault><w:pPr><w:spacing w:after=\"120\" w:line=\"360\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault></w:docDefaults>\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Title\"><w:name w:val=\"Title\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:keepNext/><w:spacing w:before=\"0\" w:after=\"360\" w:line=\"240\" w:lineRule=\"auto\"/><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"10\" w:color=\"D9D6D0\"/></w:pBdr><w:outlineLvl w:val=\"0\"/></w:pPr>\
<w:rPr><w:b/><w:bCs/><w:sz w:val=\"44\"/><w:szCs w:val=\"44\"/></w:rPr></w:style>");
    let sizes = [36, 30, 26, 24, 22, 22];
    for (i, sz) in sizes.iter().enumerate() {
        let n = i + 1;
        let color = if n <= 2 { ink } else { "3A3936" };
        s.push_str(&format!("<w:style w:type=\"paragraph\" w:styleId=\"Heading{n}\"><w:name w:val=\"heading {n}\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:uiPriority w:val=\"9\"/><w:qFormat/>\
<w:pPr><w:keepNext/><w:keepLines/><w:spacing w:before=\"{}\" w:after=\"{}\" w:line=\"276\" w:lineRule=\"auto\"/><w:outlineLvl w:val=\"{}\"/></w:pPr>\
<w:rPr><w:b/><w:bCs/><w:color w:val=\"{color}\"/><w:sz w:val=\"{sz}\"/><w:szCs w:val=\"{sz}\"/></w:rPr></w:style>", if n == 1 { 480 } else { 360 }, if n <= 2 { 160 } else { 120 }, i));
    }
    s.push_str(&format!("<w:style w:type=\"paragraph\" w:styleId=\"Quote\"><w:name w:val=\"Quote\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:pBdr><w:left w:val=\"single\" w:sz=\"18\" w:space=\"10\" w:color=\"{accent}\"/></w:pBdr><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F4F3F0\"/><w:spacing w:before=\"60\" w:after=\"60\"/><w:ind w:left=\"360\" w:right=\"120\"/></w:pPr>\
<w:rPr><w:color w:val=\"5C5A55\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Code\"><w:name w:val=\"Code\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F3F2EF\"/><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"264\" w:lineRule=\"auto\"/><w:ind w:left=\"240\" w:right=\"240\"/><w:contextualSpacing/></w:pPr>\
<w:rPr>{mono}<w:sz w:val=\"19\"/><w:szCs w:val=\"19\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"ListParagraph\"><w:name w:val=\"List Paragraph\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/><w:pPr><w:spacing w:after=\"60\"/><w:ind w:left=\"720\"/><w:contextualSpacing/></w:pPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Caption\"><w:name w:val=\"caption\"/><w:basedOn w:val=\"Normal\"/><w:pPr><w:jc w:val=\"center\"/></w:pPr><w:rPr><w:color w:val=\"7A776F\"/><w:sz w:val=\"18\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Math\"><w:name w:val=\"Math\"/><w:basedOn w:val=\"Normal\"/><w:pPr><w:jc w:val=\"center\"/><w:spacing w:before=\"120\" w:after=\"120\"/></w:pPr><w:rPr><w:rFonts w:ascii=\"Cambria Math\" w:hAnsi=\"Cambria Math\"/><w:i/></w:rPr></w:style>\
<w:style w:type=\"character\" w:styleId=\"CodeChar\"><w:name w:val=\"Inline Code\"/><w:rPr>{mono}<w:sz w:val=\"20\"/><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F1EFEB\"/></w:rPr></w:style>\
<w:style w:type=\"character\" w:styleId=\"Hyperlink\"><w:name w:val=\"Hyperlink\"/><w:rPr><w:color w:val=\"{accent}\"/><w:u w:val=\"single\"/></w:rPr></w:style>\
<w:style w:type=\"table\" w:default=\"1\" w:styleId=\"TableNormal\"><w:name w:val=\"Normal Table\"/><w:tblPr><w:tblInd w:w=\"0\" w:type=\"dxa\"/><w:tblCellMar><w:top w:w=\"0\" w:type=\"dxa\"/><w:left w:w=\"108\" w:type=\"dxa\"/><w:bottom w:w=\"0\" w:type=\"dxa\"/><w:right w:w=\"108\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr></w:style>\
<w:style w:type=\"table\" w:styleId=\"LitmusTable\"><w:name w:val=\"Litmus Table\"/><w:basedOn w:val=\"TableNormal\"/>\
<w:pPr><w:spacing w:before=\"40\" w:after=\"40\" w:line=\"276\" w:lineRule=\"auto\"/></w:pPr>\
<w:tblPr><w:tblBorders><w:top w:val=\"single\" w:sz=\"4\" w:color=\"D9D6D0\"/><w:left w:val=\"single\" w:sz=\"4\" w:color=\"D9D6D0\"/><w:bottom w:val=\"single\" w:sz=\"4\" w:color=\"D9D6D0\"/><w:right w:val=\"single\" w:sz=\"4\" w:color=\"D9D6D0\"/><w:insideH w:val=\"single\" w:sz=\"4\" w:color=\"E6E3DE\"/><w:insideV w:val=\"single\" w:sz=\"4\" w:color=\"E6E3DE\"/></w:tblBorders>\
<w:tblCellMar><w:top w:w=\"80\" w:type=\"dxa\"/><w:left w:w=\"120\" w:type=\"dxa\"/><w:bottom w:w=\"80\" w:type=\"dxa\"/><w:right w:w=\"120\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr>\
<w:tblStylePr w:type=\"firstRow\"><w:rPr><w:b/><w:bCs/></w:rPr><w:tcPr><w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"F1EFEB\"/></w:tcPr></w:tblStylePr></w:style>\
</w:styles>"));
    s
}

fn numbering_xml(nums: &[u32]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:numbering xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">");
    // 0：项目符号
    s.push_str("<w:abstractNum w:abstractNumId=\"0\"><w:multiLevelType w:val=\"hybridMultilevel\"/>");
    let bullets = ["•", "◦", "▪"];
    for l in 0..9 {
        s.push_str(&format!("<w:lvl w:ilvl=\"{l}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"{}\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"{}\" w:hanging=\"360\"/></w:pPr></w:lvl>", bullets[l % 3], 720 + 360 * l));
    }
    s.push_str("</w:abstractNum>");
    // 1：编号（1. → a. → i.）
    s.push_str("<w:abstractNum w:abstractNumId=\"1\"><w:multiLevelType w:val=\"hybridMultilevel\"/>");
    let fmts = [("decimal", "%1."), ("lowerLetter", "%2."), ("lowerRoman", "%3.")];
    for l in 0..9 {
        let (f, _) = fmts[l % 3];
        s.push_str(&format!("<w:lvl w:ilvl=\"{l}\"><w:start w:val=\"1\"/><w:numFmt w:val=\"{f}\"/><w:lvlText w:val=\"%{}.\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"{}\" w:hanging=\"360\"/></w:pPr></w:lvl>", l + 1, 720 + 360 * l));
    }
    s.push_str("</w:abstractNum>");
    s.push_str("<w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num>");
    for (k, start) in nums.iter().enumerate() {
        s.push_str(&format!("<w:num w:numId=\"{}\"><w:abstractNumId w:val=\"1\"/><w:lvlOverride w:ilvl=\"0\"><w:startOverride w:val=\"{start}\"/></w:lvlOverride></w:num>", k + 2));
    }
    s.push_str("</w:numbering>");
    s
}

fn rpr_docx(f: &RFmt) -> String {
    let mut r = String::new();
    if f.code {
        r.push_str("<w:rStyle w:val=\"CodeChar\"/>");
    }
    if f.bold {
        r.push_str("<w:b/><w:bCs/>");
    }
    if f.italic || f.math {
        r.push_str("<w:i/><w:iCs/>");
    }
    if f.math {
        r.push_str("<w:rFonts w:ascii=\"Cambria Math\" w:hAnsi=\"Cambria Math\"/>");
    }
    if f.strike {
        r.push_str("<w:strike/>");
    }
    if f.mark {
        r.push_str("<w:highlight w:val=\"yellow\"/>");
    }
    if r.is_empty() { String::new() } else { format!("<w:rPr>{r}</w:rPr>") }
}

fn t_docx(text: &str) -> String {
    format!("<w:t xml:space=\"preserve\">{}</w:t>", escape(text))
}

fn runs_docx(runs: &[Run], base: &RFmt, cx: &mut DocxCx) -> String {
    let mut s = String::new();
    for r in runs {
        match r {
            Run::Text(t, f) => {
                let mut f2 = f.clone();
                f2.bold |= base.bold;
                f2.italic |= base.italic;
                s.push_str(&format!("<w:r>{}{}</w:r>", rpr_docx(&f2), t_docx(t)));
            }
            Run::Break => s.push_str("<w:r><w:br/></w:r>"),
            Run::Link(inner, url) => {
                let inner_xml: String = inner
                    .iter()
                    .map(|x| match x {
                        Run::Text(t, f) => {
                            let mut rp = rpr_docx(f);
                            rp = if rp.is_empty() { "<w:rPr><w:rStyle w:val=\"Hyperlink\"/></w:rPr>".into() } else { rp.replacen("<w:rPr>", "<w:rPr><w:rStyle w:val=\"Hyperlink\"/>", 1) };
                            format!("<w:r>{rp}{}</w:r>", t_docx(t))
                        }
                        _ => String::new(),
                    })
                    .collect();
                if url.starts_with('#') {
                    s.push_str(&format!("<w:hyperlink w:anchor=\"{}\">{inner_xml}</w:hyperlink>", escape(&url[1..])));
                } else if url.starts_with("http") || url.starts_with("mailto:") {
                    let id = cx.rel(url, true);
                    s.push_str(&format!("<w:hyperlink r:id=\"{id}\">{inner_xml}</w:hyperlink>"));
                } else {
                    s.push_str(&inner_xml);
                }
            }
            Run::Image(alt, src) => s.push_str(&image_docx(alt, src, cx)),
        }
    }
    s
}

fn image_docx(alt: &str, src: &str, cx: &mut DocxCx) -> String {
    let Some(bytes) = load_img(cx.opts, src) else {
        return format!("<w:r><w:rPr><w:color w:val=\"7A776F\"/></w:rPr>{}</w:r>", t_docx(&format!("[图片：{}]", if alt.is_empty() { src } else { alt })));
    };
    let (w, h) = image_size(&bytes);
    let ext = image_ext(&bytes);
    cx.img_id += 1;
    let n = cx.img_id;
    let name = format!("image{n}.{ext}");
    let rid = cx.rel(&format!("media/{name}"), false);
    cx.media.push((name.clone(), bytes));
    // 页宽 6.27 英寸（A4 去边距）；96 dpi → EMU
    let max_w: u64 = 5_731_510;
    let mut cx_emu = w as u64 * 9525;
    let mut cy_emu = h as u64 * 9525;
    if cx_emu > max_w {
        cy_emu = cy_emu * max_w / cx_emu.max(1);
        cx_emu = max_w;
    }
    let alt = escape(alt);
    format!("<w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"{cx_emu}\" cy=\"{cy_emu}\"/><wp:docPr id=\"{n}\" name=\"图片 {n}\" descr=\"{alt}\"/>\
<wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect=\"1\"/></wp:cNvGraphicFramePr><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:pic>\
<pic:nvPicPr><pic:cNvPr id=\"{n}\" name=\"{name}\" descr=\"{alt}\"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx_emu}\" cy=\"{cy_emu}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>")
}

fn para_docx(style: Option<&str>, extra_ppr: &str, inner: &str) -> String {
    let st = style.map(|s| format!("<w:pStyle w:val=\"{s}\"/>")).unwrap_or_default();
    let ppr = if st.is_empty() && extra_ppr.is_empty() { String::new() } else { format!("<w:pPr>{st}{extra_ppr}</w:pPr>") };
    format!("<w:p>{ppr}{inner}</w:p>")
}

fn block_docx(b: &Block, cx: &mut DocxCx) -> String {
    match b {
        Block::Heading { level, text, .. } => {
            let l = level.saturating_sub(cx.shift).clamp(1, 6);
            para_docx(Some(&format!("Heading{l}")), "", &runs_docx(&parse_inline(text), &RFmt::default(), cx))
        }
        Block::Paragraph { text, .. } => {
            let t = text.trim();
            if t.starts_with("$$") && t.ends_with("$$") && t.len() > 4 {
                let m = latex_plain(t[2..t.len() - 2].trim());
                return para_docx(Some("Math"), "", &format!("<w:r>{}</w:r>", t_docx(&m)));
            }
            let runs = parse_inline(&join_soft(text));
            // 独占一段的图片：居中
            let only_img = runs.len() == 1 && matches!(runs[0], Run::Image(..));
            para_docx(None, if only_img { "<w:jc w:val=\"center\"/>" } else { "" }, &runs_docx(&runs, &RFmt::default(), cx))
        }
        Block::Code { text, lang, .. } => {
            let mut s = String::new();
            let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
            let n = lines.len();
            for (i, l) in lines.iter().enumerate() {
                // 块首尾留白：首行上距 / 末行下距
                let sp = match (i == 0, i + 1 == n) {
                    (true, true) => "<w:spacing w:before=\"120\" w:after=\"160\"/>",
                    (true, false) => "<w:spacing w:before=\"120\"/>",
                    (false, true) => "<w:spacing w:after=\"160\"/>",
                    _ => "",
                };
                let _ = lang;
                s.push_str(&para_docx(Some("Code"), sp, &format!("<w:r>{}</w:r>", t_docx(&l.replace('\t', "    ")))));
            }
            s
        }
        Block::Quote { lines, .. } => lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| para_docx(Some("Quote"), "", &runs_docx(&parse_inline(l), &RFmt::default(), cx)))
            .collect(),
        Block::List { ordered, items, .. } => {
            let num = if *ordered {
                cx.nums.push(1);
                cx.nums.len() as u32 + 1
            } else {
                1
            };
            items
                .iter()
                .map(|it| {
                    let (lvl, text) = list_level(it);
                    let (task, text) = task_prefix(text);
                    let ppr = format!("<w:numPr><w:ilvl w:val=\"{lvl}\"/><w:numId w:val=\"{num}\"/></w:numPr>");
                    let mut inner = String::new();
                    if let Some(done) = task {
                        inner.push_str(&format!("<w:r>{}</w:r>", t_docx(if done { "☑ " } else { "☐ " })));
                    }
                    inner.push_str(&runs_docx(&parse_inline(text), &RFmt::default(), cx));
                    para_docx(Some("ListParagraph"), &ppr, &inner)
                })
                .collect()
        }
        Block::Table { header, rows, .. } => {
            let n = header.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0)).max(1);
            let col_w = 9000 / n;
            let mut s = format!("<w:tbl><w:tblPr><w:tblStyle w:val=\"LitmusTable\"/><w:tblW w:w=\"5000\" w:type=\"pct\"/><w:tblLook w:val=\"04A0\" w:firstRow=\"1\" w:lastRow=\"0\" w:firstColumn=\"0\" w:lastColumn=\"0\" w:noHBand=\"1\" w:noVBand=\"1\"/></w:tblPr><w:tblGrid>{}</w:tblGrid>", format!("<w:gridCol w:w=\"{col_w}\"/>").repeat(n));
            let row_xml = |cells: &[String], head: bool, cx: &mut DocxCx| {
                let mut r = String::from("<w:tr>");
                if head {
                    r.push_str("<w:trPr><w:tblHeader/><w:cantSplit/></w:trPr>");
                } else {
                    r.push_str("<w:trPr><w:cantSplit/></w:trPr>");
                }
                for k in 0..n {
                    let c = cells.get(k).map(|s| s.as_str()).unwrap_or("");
                    let base = RFmt { bold: head, ..Default::default() };
                    let paras: String = c
                        .split("<br>")
                        .map(|p| para_docx(None, "<w:spacing w:before=\"0\" w:after=\"0\"/>", &runs_docx(&parse_inline(p.trim()), &base, cx)))
                        .collect();
                    r.push_str(&format!("<w:tc><w:tcPr><w:tcW w:w=\"{col_w}\" w:type=\"dxa\"/></w:tcPr>{paras}</w:tc>"));
                }
                r.push_str("</w:tr>");
                r
            };
            s.push_str(&row_xml(header, true, cx));
            for r in rows {
                s.push_str(&row_xml(r, false, cx));
            }
            s.push_str("</w:tbl>");
            // 表后留一点距离（Word 里表格紧贴下一段很难看）
            s.push_str(&para_docx(None, "<w:spacing w:before=\"0\" w:after=\"60\"/>", ""));
            s
        }
        Block::Divider { .. } => para_docx(None, "<w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" w:color=\"D9D6D0\"/></w:pBdr><w:spacing w:before=\"120\" w:after=\"240\"/>", ""),
        Block::Raw { text, .. } => {
            let t = strip_tags(text);
            t.lines().filter(|l| !l.trim().is_empty()).map(|l| para_docx(None, "", &format!("<w:r>{}</w:r>", t_docx(l.trim())))).collect()
        }
    }
}

/// kernel-md 把嵌套列表摊平：导出前把缩进层级编码进条目文本（\u{2}N\u{2}），这里解回来。
fn list_level(item: &str) -> (usize, &str) {
    if let Some(rest) = item.strip_prefix('\u{2}') {
        if let Some((n, tail)) = rest.split_once('\u{2}') {
            return (n.parse().unwrap_or(0), tail);
        }
    }
    let ind = item.len() - item.trim_start().len();
    ((ind / 2).min(8), item.trim_start())
}

fn mark_levels(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let ind: usize = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum();
        let t = line.trim_start();
        let is_item = t.starts_with("- ") || t.starts_with("* ") || t.split_once(". ").map_or(false, |(a, _)| !a.is_empty() && a.len() <= 3 && a.chars().all(|c| c.is_ascii_digit()));
        if !in_fence && ind >= 2 && is_item {
            let lvl = (ind / 2).min(8);
            let (marker, rest) = if t.starts_with("- ") || t.starts_with("* ") { (&t[..2], &t[2..]) } else { let k = t.find(". ").unwrap() + 2; (&t[..k], &t[k..]) };
            out.push_str(&format!("{marker}\u{2}{lvl}\u{2}{rest}\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn task_prefix(t: &str) -> (Option<bool>, &str) {
    if let Some(r) = t.strip_prefix("[ ] ") {
        (Some(false), r)
    } else if let Some(r) = t.strip_prefix("[x] ").or_else(|| t.strip_prefix("[X] ")) {
        (Some(true), r)
    } else {
        (None, t)
    }
}

// ───────────────────────── ODT ─────────────────────────

struct OdtCx<'a> {
    opts: &'a ExportOpts<'a>,
    shift: u8,
    pics: Vec<(String, Vec<u8>)>,
}

pub fn to_odt(md: &str, opts: &ExportOpts) -> Result<Vec<u8>, String> {
    let doc = kernel_md::from_markdown(&mark_levels(md)).map_err(|e| e.to_string())?;
    let title = opts.title.clone().or(doc.title.clone());
    let mut cx = OdtCx { opts, shift: if title.is_some() { 1 } else { 0 }, pics: Vec::new() };
    let mut body = String::new();
    if let Some(t) = &title {
        body.push_str(&format!("<text:p text:style-name=\"Title\">{}</text:p>", spans_odt(&parse_inline(t), &mut cx)));
    }
    let mut tables = 0;
    for b in &doc.blocks {
        body.push_str(&block_odt(b, &mut cx, &mut tables));
    }
    let content = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<office:document-content {ODF_NS} office:version=\"1.3\">\
<office:automatic-styles>\
<style:style style:name=\"T_b\" style:family=\"text\"><style:text-properties fo:font-weight=\"bold\" style:font-weight-asian=\"bold\"/></style:style>\
<style:style style:name=\"T_i\" style:family=\"text\"><style:text-properties fo:font-style=\"italic\" style:font-style-asian=\"italic\"/></style:style>\
<style:style style:name=\"T_bi\" style:family=\"text\"><style:text-properties fo:font-weight=\"bold\" fo:font-style=\"italic\" style:font-weight-asian=\"bold\"/></style:style>\
<style:style style:name=\"T_s\" style:family=\"text\"><style:text-properties style:text-line-through-style=\"solid\"/></style:style>\
<style:style style:name=\"T_m\" style:family=\"text\"><style:text-properties fo:background-color=\"#fff3a8\"/></style:style>\
<style:style style:name=\"T_code\" style:family=\"text\"><style:text-properties style:font-name=\"{MONO_FONT}\" fo:font-size=\"9.5pt\" fo:background-color=\"#f1efeb\"/></style:style>\
<style:style style:name=\"Tbl\" style:family=\"table\"><style:table-properties style:width=\"17cm\" table:align=\"margins\" fo:margin-bottom=\"0.25cm\"/></style:style>\
<style:style style:name=\"TblCell\" style:family=\"table-cell\"><style:table-cell-properties fo:padding=\"0.12cm\" fo:border=\"0.5pt solid #d9d6d0\"/></style:style>\
<style:style style:name=\"TblHead\" style:family=\"table-cell\"><style:table-cell-properties fo:padding=\"0.12cm\" fo:border=\"0.5pt solid #d9d6d0\" fo:background-color=\"#f1efeb\"/></style:style>\
<style:style style:name=\"P_center\" style:family=\"paragraph\" style:parent-style-name=\"Text_20_body\"><style:paragraph-properties fo:text-align=\"center\"/></style:style>\
<style:style style:name=\"fr\" style:family=\"graphic\"><style:graphic-properties style:wrap=\"none\" style:vertical-pos=\"top\" style:horizontal-pos=\"center\" style:horizontal-rel=\"paragraph\"/></style:style>\
</office:automatic-styles><office:body><office:text>{body}</office:text></office:body></office:document-content>");

    let mut manifest = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" manifest:version=\"1.3\">\
<manifest:file-entry manifest:full-path=\"/\" manifest:media-type=\"application/vnd.oasis.opendocument.text\"/>\
<manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"/>\
<manifest:file-entry manifest:full-path=\"styles.xml\" manifest:media-type=\"text/xml\"/>\
<manifest:file-entry manifest:full-path=\"meta.xml\" manifest:media-type=\"text/xml\"/>");
    for (name, b) in &cx.pics {
        manifest.push_str(&format!("<manifest:file-entry manifest:full-path=\"Pictures/{name}\" manifest:media-type=\"image/{}\"/>", image_ext(b)));
    }
    manifest.push_str("</manifest:manifest>");
    let meta = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<office:document-meta {ODF_NS} office:version=\"1.3\"><office:meta><dc:title>{}</dc:title><meta:generator>鹿蕊 Litmus</meta:generator></office:meta></office:document-meta>", escape(title.as_deref().unwrap_or("")));

    let mut z = ZipWriter::new();
    z.add("mimetype", b"application/vnd.oasis.opendocument.text", true);
    z.add("META-INF/manifest.xml", manifest.as_bytes(), false);
    z.add("content.xml", content.as_bytes(), false);
    z.add("styles.xml", odt_styles().as_bytes(), false);
    z.add("meta.xml", meta.as_bytes(), false);
    for (name, b) in &cx.pics {
        z.add(&format!("Pictures/{name}"), b, true);
    }
    Ok(z.finish())
}

const ODF_NS: &str = "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:meta=\"urn:oasis:names:tc:opendocument:xmlns:meta:1.0\" xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" xmlns:loext=\"urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0\"";

fn odt_styles() -> String {
    let mut s = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<office:document-styles {ODF_NS} office:version=\"1.3\">\
<office:font-face-decls><style:font-face style:name=\"{LATIN_FONT}\" svg:font-family=\"{LATIN_FONT}\"/><style:font-face style:name=\"{CN_FONT}\" svg:font-family=\"'{CN_FONT}', '{CN_FONT_ALT}', 'Noto Sans CJK SC'\"/><style:font-face style:name=\"{MONO_FONT}\" svg:font-family=\"{MONO_FONT}, Consolas, 'DejaVu Sans Mono'\" style:font-pitch=\"fixed\"/></office:font-face-decls>\
<office:styles>\
<style:default-style style:family=\"paragraph\"><style:paragraph-properties fo:line-height=\"150%\"/><style:text-properties style:font-name=\"{LATIN_FONT}\" style:font-name-asian=\"{CN_FONT}\" fo:font-size=\"11pt\" style:font-size-asian=\"11pt\" fo:color=\"#2b2a28\" fo:language=\"zh\" fo:country=\"CN\" style:language-asian=\"zh\" style:country-asian=\"CN\"/></style:default-style>\
<style:style style:name=\"Standard\" style:family=\"paragraph\" style:class=\"text\"/>\
<style:style style:name=\"Text_20_body\" style:display-name=\"Text body\" style:family=\"paragraph\" style:parent-style-name=\"Standard\" style:class=\"text\"><style:paragraph-properties fo:margin-top=\"0cm\" fo:margin-bottom=\"0.21cm\"/></style:style>\
<style:style style:name=\"Title\" style:family=\"paragraph\" style:parent-style-name=\"Standard\" style:next-style-name=\"Text_20_body\" style:class=\"chapter\"><style:paragraph-properties fo:margin-bottom=\"0.6cm\" fo:padding-bottom=\"0.25cm\" fo:border-bottom=\"0.5pt solid #d9d6d0\" fo:keep-with-next=\"always\"/><style:text-properties fo:font-size=\"22pt\" style:font-size-asian=\"22pt\" fo:font-weight=\"bold\" style:font-weight-asian=\"bold\"/></style:style>\
<style:style style:name=\"Heading\" style:family=\"paragraph\" style:parent-style-name=\"Standard\" style:next-style-name=\"Text_20_body\" style:class=\"text\"><style:paragraph-properties fo:margin-top=\"0.5cm\" fo:margin-bottom=\"0.2cm\" fo:keep-with-next=\"always\" fo:line-height=\"125%\"/><style:text-properties fo:font-weight=\"bold\" style:font-weight-asian=\"bold\"/></style:style>");
    for (i, sz) in [18, 15, 13, 12, 11, 11].iter().enumerate() {
        let n = i + 1;
        s.push_str(&format!("<style:style style:name=\"Heading_20_{n}\" style:display-name=\"Heading {n}\" style:family=\"paragraph\" style:parent-style-name=\"Heading\" style:next-style-name=\"Text_20_body\" style:default-outline-level=\"{n}\" style:class=\"text\"><style:text-properties fo:font-size=\"{sz}pt\" style:font-size-asian=\"{sz}pt\"/></style:style>"));
    }
    s.push_str(&format!("<style:style style:name=\"List_20_Contents\" style:display-name=\"List Contents\" style:family=\"paragraph\" style:parent-style-name=\"Standard\" style:class=\"list\"><style:paragraph-properties fo:margin-top=\"0cm\" fo:margin-bottom=\"0.08cm\"/></style:style>\
<style:style style:name=\"Quotations\" style:family=\"paragraph\" style:parent-style-name=\"Text_20_body\" style:class=\"html\"><style:paragraph-properties fo:margin-top=\"0.2cm\" fo:margin-left=\"0.6cm\" fo:margin-right=\"0.3cm\" fo:padding-left=\"0.3cm\" fo:border-left=\"2pt solid #2f5bb7\" fo:background-color=\"#f4f3f0\"/><style:text-properties fo:color=\"#5c5a55\"/></style:style>\
<style:style style:name=\"Preformatted_20_Text\" style:display-name=\"Preformatted Text\" style:family=\"paragraph\" style:parent-style-name=\"Standard\" style:class=\"html\"><style:paragraph-properties fo:margin-top=\"0cm\" fo:margin-bottom=\"0cm\" fo:line-height=\"115%\" fo:background-color=\"#f3f2ef\" fo:padding-left=\"0.3cm\" fo:padding-right=\"0.3cm\"/><style:text-properties style:font-name=\"{MONO_FONT}\" fo:font-size=\"9.5pt\" style:font-size-asian=\"9.5pt\"/></style:style>\
<style:style style:name=\"Table_20_Contents\" style:display-name=\"Table Contents\" style:family=\"paragraph\" style:parent-style-name=\"Standard\" style:class=\"extra\"><style:paragraph-properties fo:line-height=\"130%\"/></style:style>\
<style:style style:name=\"Table_20_Heading\" style:display-name=\"Table Heading\" style:family=\"paragraph\" style:parent-style-name=\"Table_20_Contents\" style:class=\"extra\"><style:text-properties fo:font-weight=\"bold\" style:font-weight-asian=\"bold\"/></style:style>\
<style:style style:name=\"Horizontal_20_Line\" style:display-name=\"Horizontal Line\" style:family=\"paragraph\" style:parent-style-name=\"Standard\"><style:paragraph-properties fo:margin-top=\"0.2cm\" fo:margin-bottom=\"0.4cm\" fo:border-bottom=\"0.5pt solid #d9d6d0\" fo:padding=\"0cm\"/><style:text-properties fo:font-size=\"4pt\"/></style:style>\
<style:style style:name=\"Internet_20_link\" style:display-name=\"Internet link\" style:family=\"text\"><style:text-properties fo:color=\"#2f5bb7\" style:text-underline-style=\"solid\" style:text-underline-width=\"auto\" style:text-underline-color=\"font-color\"/></style:style>\
<text:list-style style:name=\"L_bullet\">"));
    for l in 1..=10 {
        s.push_str(&format!("<text:list-level-style-bullet text:level=\"{l}\" text:bullet-char=\"{}\"><style:list-level-properties text:list-level-position-and-space-mode=\"label-alignment\"><style:list-level-label-alignment text:label-followed-by=\"listtab\" fo:text-indent=\"-0.5cm\" fo:margin-left=\"{:.2}cm\"/></style:list-level-properties></text:list-level-style-bullet>", ["•", "◦", "▪"][(l - 1) % 3], 0.75 + 0.6 * (l - 1) as f32));
    }
    s.push_str("</text:list-style><text:list-style style:name=\"L_number\">");
    for l in 1..=10 {
        s.push_str(&format!("<text:list-level-style-number text:level=\"{l}\" style:num-suffix=\".\" style:num-format=\"{}\"><style:list-level-properties text:list-level-position-and-space-mode=\"label-alignment\"><style:list-level-label-alignment text:label-followed-by=\"listtab\" fo:text-indent=\"-0.6cm\" fo:margin-left=\"{:.2}cm\"/></style:list-level-properties></text:list-level-style-number>", ["1", "a", "i"][(l - 1) % 3], 0.85 + 0.6 * (l - 1) as f32));
    }
    s.push_str("</text:list-style></office:styles>\
<office:automatic-styles><style:page-layout style:name=\"pm1\"><style:page-layout-properties fo:page-width=\"21cm\" fo:page-height=\"29.7cm\" fo:margin-top=\"2.54cm\" fo:margin-bottom=\"2.54cm\" fo:margin-left=\"2.54cm\" fo:margin-right=\"2.54cm\"/></style:page-layout></office:automatic-styles>\
<office:master-styles><style:master-page style:name=\"Standard\" style:page-layout-name=\"pm1\"/></office:master-styles></office:document-styles>");
    s
}

fn span_style(f: &RFmt) -> Option<&'static str> {
    if f.code {
        return Some("T_code");
    }
    match (f.bold, f.italic || f.math) {
        (true, true) => Some("T_bi"),
        (true, false) => Some("T_b"),
        (false, true) => Some("T_i"),
        _ if f.strike => Some("T_s"),
        _ if f.mark => Some("T_m"),
        _ => None,
    }
}

fn text_odt(t: &str) -> String {
    // 连续空格要用 text:s 才不被折叠
    let mut o = String::new();
    let mut spaces = 0;
    for c in t.chars() {
        if c == ' ' {
            spaces += 1;
            continue;
        }
        if spaces > 0 {
            o.push(' ');
            if spaces > 1 {
                o.push_str(&format!("<text:s text:c=\"{}\"/>", spaces - 1));
            }
            spaces = 0;
        }
        match c {
            '\t' => o.push_str("<text:tab/>"),
            c => o.push_str(&escape(&c.to_string())),
        }
    }
    if spaces > 0 {
        o.push(' ');
        if spaces > 1 {
            o.push_str(&format!("<text:s text:c=\"{}\"/>", spaces - 1));
        }
    }
    o
}

fn spans_odt(runs: &[Run], cx: &mut OdtCx) -> String {
    let mut s = String::new();
    for r in runs {
        match r {
            Run::Text(t, f) => match span_style(f) {
                Some(st) => s.push_str(&format!("<text:span text:style-name=\"{st}\">{}</text:span>", text_odt(t))),
                None => s.push_str(&text_odt(t)),
            },
            Run::Break => s.push_str("<text:line-break/>"),
            Run::Link(inner, url) => s.push_str(&format!("<text:a xlink:type=\"simple\" xlink:href=\"{}\" text:style-name=\"Internet_20_link\">{}</text:a>", escape(url), spans_odt(inner, cx))),
            Run::Image(alt, src) => match load_img(cx.opts, src) {
                Some(b) => {
                    let (w, h) = image_size(&b);
                    let name = format!("img{}.{}", cx.pics.len() + 1, image_ext(&b));
                    let (mut wc, mut hc) = (w as f64 * 2.54 / 96.0, h as f64 * 2.54 / 96.0);
                    if wc > 16.0 {
                        hc = hc * 16.0 / wc;
                        wc = 16.0;
                    }
                    s.push_str(&format!("<draw:frame draw:style-name=\"fr\" draw:name=\"{name}\" text:anchor-type=\"as-char\" svg:width=\"{wc:.2}cm\" svg:height=\"{hc:.2}cm\"><draw:image xlink:href=\"Pictures/{name}\" xlink:type=\"simple\" xlink:show=\"embed\" xlink:actuate=\"onLoad\"/><svg:title>{}</svg:title></draw:frame>", escape(alt)));
                    cx.pics.push((name, b));
                }
                None => s.push_str(&text_odt(&format!("[图片：{alt}]"))),
            },
        }
    }
    s
}

fn block_odt(b: &Block, cx: &mut OdtCx, tables: &mut usize) -> String {
    match b {
        Block::Heading { level, text, .. } => {
            let l = level.saturating_sub(cx.shift).clamp(1, 6);
            format!("<text:h text:style-name=\"Heading_20_{l}\" text:outline-level=\"{l}\">{}</text:h>", spans_odt(&parse_inline(text), cx))
        }
        Block::Paragraph { text, .. } => {
            let t = text.trim();
            if t.starts_with("$$") && t.ends_with("$$") && t.len() > 4 {
                return format!("<text:p text:style-name=\"P_center\"><text:span text:style-name=\"T_i\">{}</text:span></text:p>", text_odt(&latex_plain(t[2..t.len() - 2].trim())));
            }
            let runs = parse_inline(&join_soft(text));
            let only_img = runs.len() == 1 && matches!(runs[0], Run::Image(..));
            format!("<text:p text:style-name=\"{}\">{}</text:p>", if only_img { "P_center" } else { "Text_20_body" }, spans_odt(&runs, cx))
        }
        Block::Code { text, .. } => text
            .trim_end_matches('\n')
            .split('\n')
            .map(|l| format!("<text:p text:style-name=\"Preformatted_20_Text\">{}</text:p>", text_odt(l)))
            .collect(),
        Block::Quote { lines, .. } => lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("<text:p text:style-name=\"Quotations\">{}</text:p>", spans_odt(&parse_inline(l), cx)))
            .collect(),
        Block::List { ordered, items, .. } => {
            // 按层级嵌套：深一级 → 在当前 list-item 里开子 list
            let mut s = format!("<text:list text:style-name=\"{}\">", if *ordered { "L_number" } else { "L_bullet" });
            let mut depth = 0usize;
            let mut open_item = false;
            for it in items {
                let (lvl, text) = list_level(it);
                let lvl = lvl.min(depth + 1);
                let (task, text) = task_prefix(text);
                let mark = match task {
                    Some(true) => "☑ ",
                    Some(false) => "☐ ",
                    None => "",
                };
                while depth > lvl {
                    s.push_str("</text:list-item></text:list>");
                    depth -= 1;
                }
                if lvl > depth && open_item {
                    s.push_str("<text:list>");
                    depth = lvl;
                } else if open_item {
                    s.push_str("</text:list-item>");
                }
                s.push_str(&format!("<text:list-item><text:p text:style-name=\"List_20_Contents\">{}{}</text:p>", mark, spans_odt(&parse_inline(text), cx)));
                open_item = true;
            }
            while depth > 0 {
                s.push_str("</text:list-item></text:list>");
                depth -= 1;
            }
            if open_item {
                s.push_str("</text:list-item>");
            }
            s.push_str("</text:list>");
            s
        }
        Block::Table { header, rows, .. } => {
            *tables += 1;
            let n = header.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0)).max(1);
            let mut s = format!("<table:table table:name=\"表格{}\" table:style-name=\"Tbl\"><table:table-column table:number-columns-repeated=\"{n}\"/>", tables);
            let row = |cells: &[String], head: bool, cx: &mut OdtCx| {
                let mut r = String::from("<table:table-row>");
                for k in 0..n {
                    let c = cells.get(k).map(|s| s.as_str()).unwrap_or("");
                    let ps: String = c
                        .split("<br>")
                        .map(|p| format!("<text:p text:style-name=\"{}\">{}</text:p>", if head { "Table_20_Heading" } else { "Table_20_Contents" }, spans_odt(&parse_inline(p.trim()), cx)))
                        .collect();
                    r.push_str(&format!("<table:table-cell table:style-name=\"{}\" office:value-type=\"string\">{ps}</table:table-cell>", if head { "TblHead" } else { "TblCell" }));
                }
                r.push_str("</table:table-row>");
                r
            };
            s.push_str("<table:table-header-rows>");
            s.push_str(&row(header, true, cx));
            s.push_str("</table:table-header-rows>");
            for r in rows {
                s.push_str(&row(r, false, cx));
            }
            s.push_str("</table:table>");
            s
        }
        Block::Divider { .. } => "<text:p text:style-name=\"Horizontal_20_Line\"/>".into(),
        Block::Raw { text, .. } => strip_tags(text).lines().filter(|l| !l.trim().is_empty()).map(|l| format!("<text:p text:style-name=\"Text_20_body\">{}</text:p>", text_odt(l.trim()))).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_parse() {
        let r = parse_inline("看 **重点** 与 *斜* 、`code`、[站](https://a.com)、![图](Assets/a.png) 和 $\\frac{a}{b}$");
        let kinds: Vec<&str> = r.iter().map(|x| match x { Run::Text(_, f) if f.bold => "b", Run::Text(_, f) if f.italic && !f.math => "i", Run::Text(_, f) if f.code => "c", Run::Text(_, f) if f.math => "m", Run::Text(..) => "t", Run::Link(..) => "l", Run::Image(..) => "img", Run::Break => "br" }).collect();
        assert!(kinds.contains(&"b") && kinds.contains(&"i") && kinds.contains(&"c") && kinds.contains(&"l") && kinds.contains(&"img") && kinds.contains(&"m"), "{kinds:?}");
        assert_eq!(latex_plain("\\frac{a}{b} + \\sqrt{x} \\times \\pi"), "(a)/(b) + √(x) × π");
    }
}
