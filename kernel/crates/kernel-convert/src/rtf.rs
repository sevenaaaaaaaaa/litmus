//! RTF → Markdown：段落 / 标题（样式表 heading N、\outlinelevel）/ 列表（\listtext）/ 表格（\cell \row）/
//! 粗斜体删除线 / 超链接（HYPERLINK 域）/ Unicode（\uN）/ PNG·JPEG 图片（\pict）。

use crate::mdw::{self, Fmt, Piece};
use crate::{Converted, Image};
use std::collections::HashMap;

#[derive(Clone, Default)]
struct G {
    f: Fmt,
    skip: bool,
    uc: usize,
    /// 当前组类型
    dest: &'static str,
    /// dest 是在本组设定的（子组继承 dest 但不负责收尾）
    owner: bool,
}

#[derive(Default)]
struct P {
    pieces: Vec<Piece>,
    heading: Option<u8>,
    list: Option<String>,
    level: usize,
    in_table: bool,
    li: i32,
}

pub fn convert(src: &str) -> Result<Converted, String> {
    if !src.trim_start().starts_with("{\\rtf") {
        return Err("不是 RTF 文件".into());
    }
    let b = src.as_bytes();
    let mut i = 0;
    let mut stack: Vec<G> = vec![G { uc: 1, ..Default::default() }];
    let mut styles: HashMap<i32, String> = HashMap::new();
    let mut style_buf = String::new();
    let mut style_id: Option<i32> = None;
    let mut cur = P::default();
    let mut blocks: Vec<String> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cells: Vec<String> = Vec::new();
    let mut grid: Vec<Vec<String>> = Vec::new();
    let mut title = None;
    let mut info_title = String::new();
    let mut listtext = String::new();
    let mut fld_inst = String::new();
    let mut fld_result: Option<(String, Vec<Piece>)> = None;
    let mut pict = String::new();
    let mut pict_kind = "";
    let mut images: Vec<Image> = Vec::new();
    let mut skip_chars = 0usize;
    let mut font_id: Option<i32> = None;
    let mut font_buf = String::new();
    let mut font_modern = false;
    let mut mono_fonts: std::collections::HashSet<i32> = std::collections::HashSet::new();

    let flush_para = |cur: &mut P, blocks: &mut Vec<String>, row: &mut Vec<String>| {
        let all_code = !cur.pieces.is_empty() && cur.pieces.iter().all(|p| matches!(p, Piece::Text(t, f) if f.code || t.trim().is_empty()));
        if all_code && !cur.in_table && cur.heading.is_none() {
            let raw: String = cur.pieces.iter().map(|p| if let Piece::Text(t, _) = p { t.as_str() } else { "" }).collect();
            blocks.push(format!("\u{0}CODE{}", raw.trim_end()));
            let keep_table = cur.in_table;
            *cur = P { in_table: keep_table, ..Default::default() };
            return;
        }
        let mut text = mdw::inline(&cur.pieces).trim().to_string();
        // 字面量项目符号（「•\t内容」）
        if cur.list.is_none() {
            if let Some(c) = text.chars().next().filter(|c| "•·▪◦○●–-".contains(*c)) {
                let rest = text[c.len_utf8()..].to_string();
                if rest.starts_with(char::is_whitespace) {
                    cur.list = Some(c.to_string());
                    text = rest.trim().to_string();
                }
            }
        }
        if cur.list.is_some() && cur.level == 0 && cur.li > 720 {
            cur.level = ((cur.li - 360) / 360).max(0) as usize;
        }
        if cur.in_table {
            // 单元格中的段落：先进格缓冲（多段以 <br> 连接），\cell 时落格
            if !text.is_empty() {
                if !row.is_empty() {
                    row.push("<br>".into());
                }
                row.push(text);
            }
        } else if !text.is_empty() {
            let pad = "  ".repeat(cur.level);
            let line = if let Some(h) = cur.heading {
                format!("{} {}", "#".repeat(h as usize), text.replace("**", ""))
            } else if let Some(l) = &cur.list {
                let l = l.trim();
                if l.chars().next().map_or(false, |c| c.is_ascii_digit()) && l.trim_end_matches(['.', ')', '）', '、']).chars().all(|c| c.is_ascii_digit()) {
                    format!("{pad}{}. {text}", l.trim_end_matches(['.', ')', '）', '、']))
                } else if l.chars().all(|c| "•·▪◦●○■-–*".contains(c) || c.is_whitespace()) || l.is_empty() {
                    format!("{pad}- {text}")
                } else {
                    format!("{pad}- {l} {text}")
                }
            } else {
                text
            };
            blocks.push(line);
        }
        let keep_table = cur.in_table;
        *cur = P { in_table: keep_table, ..Default::default() };
    };

    while i < b.len() {
        let c = b[i];
        let g = stack.last().cloned().unwrap_or_default();
        match c {
            b'{' => {
                let mut ng = g.clone();
                ng.owner = false;
                if g.dest == "stylesheet" {
                    ng.dest = "stylesheet-entry";
                    ng.owner = true;
                } else if g.dest == "fonttbl" {
                    ng.dest = "font-entry";
                    ng.owner = true;
                }
                stack.push(ng);
                i += 1;
            }
            b'}' => {
                let ended = stack.pop().unwrap_or_default();
                match if ended.owner { ended.dest } else { "" } {
                    "font-entry" => {
                        if let Some(id) = font_id.take() {
                            let n = font_buf.to_lowercase();
                            if font_modern || ["courier", "mono", "consolas", "menlo", "monaco"].iter().any(|m| n.contains(m)) {
                                mono_fonts.insert(id);
                            }
                        }
                        font_buf.clear();
                        font_modern = false;
                    }
                    "stylesheet-entry" => {
                        if let Some(id) = style_id.take() {
                            styles.insert(id, style_buf.trim().trim_end_matches(';').to_lowercase());
                        }
                        style_buf.clear();
                    }
                    "title" => {
                        // \upr 双写：ANSI 版常是一串 ?，取 Unicode 版
                        let t = info_title.trim().to_string();
                        if !t.is_empty() && !t.chars().all(|c| c == '?') {
                            title = Some(t);
                        }
                        info_title.clear();
                    }
                    "fldinst" => {
                        let inst = fld_inst.trim().to_string();
                        fld_inst.clear();
                        if let Some(rest) = inst.strip_prefix("HYPERLINK") {
                            let url = rest.split('"').nth(1).map(String::from).unwrap_or_else(|| rest.trim().to_string());
                            fld_result = Some((url, Vec::new()));
                        }
                    }
                    "fldrslt" => {
                        if let Some((url, inner)) = fld_result.take() {
                            cur.pieces.push(Piece::Link(inner, url));
                        }
                    }
                    "pict" => {
                        let bytes = hex_bytes(&pict);
                        if !bytes.is_empty() && !pict_kind.is_empty() {
                            let tok = crate::push_image(&mut images, format!("image.{pict_kind}"), bytes);
                            cur.pieces.push(Piece::Raw(format!("![图片]({tok})")));
                        }
                        pict.clear();
                        pict_kind = "";
                    }
                    "listtext" => {
                        cur.list = Some(listtext.trim().to_string());
                        listtext.clear();
                    }
                    _ => {}
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= b.len() {
                    break;
                }
                let n = b[i];
                if !n.is_ascii_alphabetic() {
                    i += 1;
                    match n {
                        b'\'' => {
                            let hex = std::str::from_utf8(&b[i..(i + 2).min(b.len())]).unwrap_or("");
                            i += 2;
                            if skip_chars > 0 {
                                skip_chars -= 1;
                                continue;
                            }
                            if let Ok(v) = u8::from_str_radix(hex, 16) {
                                // 只认 Latin-1（CJK 在现代 RTF 里走 \uN）
                                emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, &(v as char).to_string());
                            }
                        }
                        b'~' => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, " "),
                        b'_' => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, "-"),
                        b'*' => {
                            if let Some(t) = stack.last_mut() {
                                t.skip = true;
                            }
                        }
                        b'\\' | b'{' | b'}' => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, &(n as char).to_string()),
                        b'\n' | b'\r' => flush_para(&mut cur, &mut blocks, &mut row),
                        _ => {}
                    }
                    continue;
                }
                let ws = i;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let word = std::str::from_utf8(&b[ws..i]).unwrap_or("");
                let ns = i;
                if i < b.len() && (b[i] == b'-' || b[i].is_ascii_digit()) {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let param: Option<i32> = std::str::from_utf8(&b[ns..i]).ok().and_then(|s| s.parse().ok());
                if i < b.len() && b[i] == b' ' {
                    i += 1;
                }
                let top = stack.last_mut().unwrap();
                match word {
                    "colortbl" | "header" | "footer" | "headerl" | "headerr" | "footerl" | "footerr" | "xmlnstbl" | "listtable" | "listoverridetable" | "rsidtbl" | "generator" | "themedata" | "colorschememapping" | "latentstyles" | "datastore" | "pgdsctbl" | "operator" | "author" | "company" => top.skip = true,
                    "info" => {}
                    "title" => { top.dest = "title"; top.owner = true; }
                    "stylesheet" => { top.dest = "stylesheet"; top.owner = true; }
                    "s" | "cs" | "ds" if top.dest == "stylesheet-entry" => {
                        style_id = param;
                    }
                    "s" => {
                        if let Some(name) = param.and_then(|p| styles.get(&p)) {
                            for pre in ["heading ", "标题 "] {
                                if let Some(n) = name.strip_prefix(pre).and_then(|r| r.trim().parse::<u8>().ok()) {
                                    cur.heading = Some(n.clamp(1, 6));
                                }
                            }
                            if name == "title" {
                                cur.heading = Some(1);
                            }
                        }
                    }
                    "outlinelevel" => cur.heading = param.map(|p| (p as u8 + 1).min(6)),
                    "ilvl" => cur.level = param.unwrap_or(0).max(0) as usize,
                    "listtext" => { top.dest = "listtext"; top.owner = true; }
                    "pntext" => { top.dest = "listtext"; top.owner = true; }
                    "fldinst" => { top.dest = "fldinst"; top.owner = true; }
                    "fldrslt" => { top.dest = "fldrslt"; top.owner = true; }
                    "pict" => {
                        top.dest = "pict";
                        top.owner = true;
                    }
                    "fonttbl" => {
                        top.dest = "fonttbl";
                        top.owner = true;
                    }
                    "f" if top.dest == "font-entry" => font_id = param,
                    "fmodern" if top.dest == "font-entry" => font_modern = true,
                    "f" => top.f.code = param.map_or(false, |p| mono_fonts.contains(&p)),
                    "li" | "lin" if top.dest == "" || top.dest == "fldrslt" => cur.li = param.unwrap_or(0),
                    "pngblip" => pict_kind = "png",
                    "jpegblip" => pict_kind = "jpg",
                    "nonshppict" => top.skip = true,
                    "par" | "sect" => {
                        if !cur.in_table && !grid.is_empty() {
                            blocks.push(mdw::table(&std::mem::take(&mut grid)));
                        }
                        flush_para(&mut cur, &mut blocks, &mut row)
                    }
                    "line" => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, "\n"),
                    "tab" => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, " "),
                    "pard" if top.dest == "listtext" => {}
                    "pard" => {
                        cur.li = 0;
                        cur.heading = None;
                        cur.level = 0;
                        cur.in_table = false;
                    }
                    "intbl" => cur.in_table = true,
                    "trowd" => {}
                    "cell" => {
                        cur.in_table = true;
                        flush_para(&mut cur, &mut blocks, &mut row);
                        cells.push(std::mem::take(&mut row).concat());
                    }
                    "row" => {
                        while cells.last().map_or(false, |c| c.is_empty()) {
                            cells.pop();
                        }
                        grid.push(std::mem::take(&mut cells));
                        cur.in_table = false;
                    }
                    "b" => top.f.bold = param != Some(0),
                    "i" => top.f.italic = param != Some(0),
                    "strike" | "striked" => top.f.strike = param != Some(0),
                    "super" => top.f.sup = true,
                    "sub" => top.f.sub = true,
                    "nosupersub" => {
                        top.f.sup = false;
                        top.f.sub = false;
                    }
                    "plain" => top.f = Fmt::default(),
                    "uc" => top.uc = param.unwrap_or(1).max(0) as usize,
                    "u" => {
                        let mut v = param.unwrap_or(0);
                        if v < 0 {
                            v += 65536;
                        }
                        let uc = top.uc;
                        if let Some(ch) = char::from_u32(v as u32) {
                            emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, &ch.to_string());
                        }
                        skip_chars = uc;
                    }
                    "emdash" => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, "—"),
                    "endash" => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, "–"),
                    "bullet" => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, "•"),
                    "lquote" | "rquote" => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, "'"),
                    "ldblquote" | "rdblquote" => emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, "\""),
                    _ => {}
                }
                let _ = &styles;
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                let s = i;
                while i < b.len() && !matches!(b[i], b'\\' | b'{' | b'}' | b'\r' | b'\n') {
                    i += 1;
                }
                let mut t = String::from_utf8_lossy(&b[s..i]).into_owned();
                if skip_chars > 0 {
                    let k = skip_chars.min(t.chars().count());
                    t = t.chars().skip(k).collect();
                    skip_chars -= k;
                }
                if g.dest == "pict" {
                    pict.push_str(&t);
                } else if g.dest == "font-entry" {
                    font_buf.push_str(&t);
                } else {
                    emit(&mut cur, &g, &mut fld_result, &mut listtext, &mut style_buf, &mut info_title, &mut fld_inst, &t);
                }
            }
        }
    }
    flush_para(&mut cur, &mut blocks, &mut row);
    if !grid.is_empty() {
        blocks.push(mdw::table(&grid));
    }
    // 连续等宽段落 → 代码块
    let mut merged: Vec<String> = Vec::new();
    let mut code: Vec<String> = Vec::new();
    for b in blocks {
        if let Some(c) = b.strip_prefix("\u{0}CODE") {
            code.push(c.to_string());
            continue;
        }
        if !code.is_empty() {
            merged.push(format!("```\n{}\n```", code.join("\n")));
            code.clear();
        }
        merged.push(b);
    }
    if !code.is_empty() {
        merged.push(format!("```\n{}\n```", code.join("\n")));
    }
    let blocks = merged;
    Ok(Converted { title, markdown: mdw::join_blocks(&blocks), images, warnings: Vec::new() })
}

#[allow(clippy::too_many_arguments)]
fn emit(cur: &mut P, g: &G, fld: &mut Option<(String, Vec<Piece>)>, listtext: &mut String, style_buf: &mut String, info_title: &mut String, fld_inst: &mut String, t: &str) {
    match g.dest {
        "listtext" => return listtext.push_str(t),
        "stylesheet-entry" => return style_buf.push_str(t),
        "font-entry" | "fonttbl" => return,
        "stylesheet" => return,
        "title" => return info_title.push_str(t),
        "fldinst" => return fld_inst.push_str(t),
        "pict" => return,
        _ => {}
    }
    if g.skip {
        return;
    }
    let piece = Piece::Text(t.to_string(), g.f.clone());
    match fld.as_mut() {
        Some((_, inner)) => inner.push(piece),
        None => cur.pieces.push(piece),
    }
}

fn hex_bytes(s: &str) -> Vec<u8> {
    let h: Vec<u8> = s.bytes().filter(|c| c.is_ascii_hexdigit()).collect();
    h.chunks(2).filter(|c| c.len() == 2).filter_map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok()).collect()
}
