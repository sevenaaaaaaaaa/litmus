//! 演示文稿（.pptx / .odp）→ 每页一节：标题 + 要点（保留层级）+ 表格 + 图片 + 备注。

use crate::mdw::{self, Fmt, Piece};
use crate::xml::{self, Node};
use crate::zip::ZipReader;
use crate::{Converted, Image};
use std::collections::HashMap;

fn rels_of(zip: &ZipReader, path: &str) -> HashMap<String, String> {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    let rp = format!("{dir}/_rels/{file}.rels");
    zip.read_string(&rp)
        .map(|s| {
            let r = xml::parse(&s);
            let mut v = Vec::new();
            r.find_all("Relationship", &mut v);
            v.iter().filter_map(|x| Some((x.attr("Id")?.to_string(), resolve(dir, x.attr("Target")?)))).collect()
        })
        .unwrap_or_default()
}

fn resolve(dir: &str, target: &str) -> String {
    if target.starts_with("http") || target.starts_with("mailto:") {
        return target.to_string();
    }
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

pub fn pptx(bytes: &[u8]) -> Result<Converted, String> {
    let zip = ZipReader::new(bytes).map_err(|e| e.to_string())?;
    let pres = zip.read_string("ppt/presentation.xml").ok_or("缺少 ppt/presentation.xml")?;
    let prels = rels_of(&zip, "ppt/presentation.xml");
    let px = xml::parse(&pres);
    let mut ids = Vec::new();
    px.find_all("p:sldId", &mut ids);
    let mut slide_paths: Vec<String> = ids.iter().filter_map(|s| s.attr("r:id").and_then(|r| prels.get(r).cloned())).collect();
    if slide_paths.is_empty() {
        slide_paths = zip.names().iter().filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml")).cloned().collect();
    }
    let mut images: Vec<Image> = Vec::new();
    let mut blocks = Vec::new();
    let mut deck_title = None;
    for (i, sp) in slide_paths.iter().enumerate() {
        let Some(sx) = zip.read_string(sp) else { continue };
        let rels = rels_of(&zip, sp);
        let root = xml::parse(&sx);
        let Some(tree) = root.find("p:spTree") else { continue };
        let mut shapes: Vec<(i64, i64, &Node)> = Vec::new();
        collect_shapes(tree, &mut shapes);
        shapes.sort_by_key(|(y, x, _)| (*y / 50_000, *x));
        let mut title = String::new();
        let mut body: Vec<String> = Vec::new();
        for (_, _, sh) in &shapes {
            let ph = sh.find("p:ph").and_then(|p| p.attr("type")).unwrap_or("");
            match sh.name.as_str() {
                "p:sp" => {
                    let Some(tx) = sh.child("p:txBody") else { continue };
                    if (ph == "title" || ph == "ctrTitle") && title.is_empty() {
                        title = tx.text().split_whitespace().collect::<Vec<_>>().join(" ");
                        continue;
                    }
                    if matches!(ph, "sldNum" | "dt" | "ftr") {
                        continue;
                    }
                    let is_body = ph == "body" || ph.is_empty() || ph == "obj" || ph == "subTitle";
                    for p in tx.elems().filter(|e| e.is("a:p")) {
                        let lvl: usize = p.child("a:pPr").and_then(|x| x.attr("lvl")).and_then(|v| v.parse().ok()).unwrap_or(0);
                        let t = a_inline(p, &rels);
                        if t.trim().is_empty() {
                            continue;
                        }
                        let numbered = p.child("a:pPr").map_or(false, |x| x.child("a:buAutoNum").is_some());
                        let no_bullet = p.child("a:pPr").map_or(false, |x| x.child("a:buNone").is_some()) || ph == "subTitle";
                        let pad = "  ".repeat(lvl);
                        body.push(if !is_body || no_bullet { t.trim().to_string() } else if numbered { format!("{pad}1. {}", t.trim()) } else { format!("{pad}- {}", t.trim()) });
                    }
                }
                "p:graphicFrame" => {
                    if let Some(tbl) = sh.find("a:tbl") {
                        let grid: Vec<Vec<String>> = tbl
                            .elems()
                            .filter(|e| e.is("a:tr"))
                            .map(|tr| tr.elems().filter(|e| e.is("a:tc")).map(|tc| tc.text().split_whitespace().collect::<Vec<_>>().join(" ")).collect())
                            .collect();
                        body.push(mdw::table(&grid));
                    }
                }
                "p:pic" => {
                    if let Some(id) = sh.find("a:blip").and_then(|b| b.attr("r:embed")) {
                        if let Some(path) = rels.get(id) {
                            if let Some(bytes) = zip.read(path) {
                                let alt = sh.find("p:cNvPr").and_then(|c| c.attr("descr").filter(|d| !d.is_empty()).or(c.attr("name"))).unwrap_or("图片").to_string();
                                let tok = crate::push_image(&mut images, path.rsplit('/').next().unwrap_or("img.png").to_string(), bytes);
                                body.push(format!("![{alt}]({tok})"));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // 备注
        let notes = rels
            .values()
            .find(|p| p.contains("notesSlide"))
            .and_then(|p| zip.read_string(p))
            .map(|s| {
                let r = xml::parse(&s);
                let mut sps = Vec::new();
                r.find_all("p:sp", &mut sps);
                sps.iter()
                    .filter(|s| s.find("p:ph").and_then(|p| p.attr("type")) == Some("body"))
                    .map(|s| s.text().trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if i == 0 && !title.is_empty() {
            deck_title = Some(title.clone());
        }
        blocks.push(format!("## {}", if title.is_empty() { format!("第 {} 页", i + 1) } else { format!("{}. {}", i + 1, title) }));
        blocks.extend(fix_numbers(body));
        if !notes.is_empty() {
            blocks.push(format!("> 备注：{notes}"));
        }
    }
    Ok(Converted { title: deck_title, markdown: mdw::join_blocks(&blocks), images, warnings: Vec::new() })
}

/// 同级「1.」占位改成顺序编号。
fn fix_numbers(lines: Vec<String>) -> Vec<String> {
    let mut counters: Vec<u32> = vec![0; 10];
    lines
        .into_iter()
        .map(|l| {
            let ind = l.chars().take_while(|c| *c == ' ').count();
            let lvl = ind / 2;
            if let Some(rest) = l.trim_start().strip_prefix("1. ") {
                counters[lvl.min(9)] += 1;
                for c in counters.iter_mut().skip(lvl + 1) {
                    *c = 0;
                }
                format!("{}{}. {rest}", " ".repeat(ind), counters[lvl.min(9)])
            } else {
                if ind == 0 {
                    counters.iter_mut().for_each(|c| *c = 0);
                }
                l
            }
        })
        .collect()
}

fn collect_shapes<'a>(n: &'a Node, out: &mut Vec<(i64, i64, &'a Node)>) {
    for c in n.elems() {
        match c.name.as_str() {
            "p:sp" | "p:graphicFrame" | "p:pic" => {
                let off = c.find("a:off");
                let y = off.and_then(|o| o.attr("y")).and_then(|v| v.parse().ok()).unwrap_or(0);
                let x = off.and_then(|o| o.attr("x")).and_then(|v| v.parse().ok()).unwrap_or(0);
                out.push((y, x, c));
            }
            "p:grpSp" => collect_shapes(c, out),
            _ => {}
        }
    }
}

fn a_inline(p: &Node, rels: &HashMap<String, String>) -> String {
    let mut pieces = Vec::new();
    for r in p.elems() {
        match r.name.as_str() {
            "a:r" | "a:fld" => {
                let rpr = r.child("a:rPr");
                let f = Fmt {
                    bold: rpr.and_then(|x| x.attr("b")) == Some("1"),
                    italic: rpr.and_then(|x| x.attr("i")) == Some("1"),
                    strike: rpr.and_then(|x| x.attr("strike")).map_or(false, |s| s != "noStrike"),
                    ..Default::default()
                };
                let t = r.child("a:t").map(|t| t.text()).unwrap_or_default();
                let link = rpr.and_then(|x| x.child("a:hlinkClick")).and_then(|h| h.attr("r:id")).and_then(|id| rels.get(id).cloned());
                match link {
                    Some(u) => pieces.push(Piece::Link(vec![Piece::Text(t, f)], u)),
                    None => pieces.push(Piece::Text(t, f)),
                }
            }
            "a:br" => pieces.push(Piece::Text(" ".into(), Fmt::default())),
            _ => {}
        }
    }
    mdw::inline(&pieces)
}

pub fn odp(bytes: &[u8]) -> Result<Converted, String> {
    let zip = ZipReader::new(bytes).map_err(|e| e.to_string())?;
    let content = zip.read_string("content.xml").ok_or("缺少 content.xml")?;
    let root = xml::parse(&content);
    let mut pages = Vec::new();
    root.find_all("draw:page", &mut pages);
    let mut blocks = Vec::new();
    let mut images = Vec::new();
    let mut deck_title = None;
    for (i, pg) in pages.iter().enumerate() {
        let mut title = String::new();
        let mut body = Vec::new();
        for fr in pg.elems().filter(|e| e.is("draw:frame") || e.is("draw:custom-shape")) {
            let class = fr.attr("presentation:class").unwrap_or("");
            if class == "title" && title.is_empty() {
                title = fr.text().split_whitespace().collect::<Vec<_>>().join(" ");
                continue;
            }
            if matches!(class, "page-number" | "footer" | "date-time") {
                continue;
            }
            if let Some(img) = fr.child("draw:image").and_then(|im| im.attr("xlink:href")) {
                if let Some(b) = zip.read(img) {
                    let tok = crate::push_image(&mut images, img.rsplit('/').next().unwrap_or("img.png").to_string(), b);
                    body.push(format!("![图片]({tok})"));
                    continue;
                }
            }
            if let Some(tb) = fr.child("draw:text-box").or(Some(fr)) {
                odp_text(tb, 0, &mut body);
            }
        }
        if i == 0 && !title.is_empty() {
            deck_title = Some(title.clone());
        }
        blocks.push(format!("## {}", if title.is_empty() { format!("第 {} 页", i + 1) } else { format!("{}. {}", i + 1, title) }));
        blocks.extend(body);
        if let Some(n) = pg.child("presentation:notes") {
            let t = n.text().split_whitespace().collect::<Vec<_>>().join(" ");
            if !t.is_empty() {
                blocks.push(format!("> 备注：{t}"));
            }
        }
    }
    Ok(Converted { title: deck_title, markdown: mdw::join_blocks(&blocks), images, warnings: Vec::new() })
}

fn odp_text(n: &Node, depth: usize, out: &mut Vec<String>) {
    for c in n.elems() {
        match c.name.as_str() {
            "text:p" => {
                let t = c.text().split_whitespace().collect::<Vec<_>>().join(" ");
                if !t.is_empty() {
                    out.push(if depth == 0 { t } else { format!("{}- {t}", "  ".repeat(depth - 1)) });
                }
            }
            "text:list" => {
                for it in c.elems().filter(|e| e.is("text:list-item")) {
                    odp_text(it, depth + 1, out);
                }
            }
            "table:table" => {
                let grid: Vec<Vec<String>> = {
                    let mut rows = Vec::new();
                    c.find_all("table:table-row", &mut rows);
                    rows.iter().map(|r| r.elems().filter(|e| e.is("table:table-cell")).map(|tc| tc.text().split_whitespace().collect::<Vec<_>>().join(" ")).collect()).collect()
                };
                out.push(mdw::table(&grid));
            }
            _ => {}
        }
    }
}
