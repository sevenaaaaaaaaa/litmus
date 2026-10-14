//! EPUB → Markdown：按 spine 顺序合并章节（XHTML 经 kernel-md::from_html），图片入库。

use crate::xml;
use crate::zip::ZipReader;
use crate::{Converted, Image};

fn dir_of(p: &str) -> &str {
    p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn join(dir: &str, href: &str) -> String {
    let href = href.split('#').next().unwrap_or(href);
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in href.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/").replace("%20", " ")
}

pub fn convert(bytes: &[u8]) -> Result<Converted, String> {
    let zip = ZipReader::new(bytes).map_err(|e| e.to_string())?;
    let container = zip.read_string("META-INF/container.xml").ok_or("缺少 META-INF/container.xml（不是 EPUB？）")?;
    let opf_path = xml::parse(&container).find("rootfile").and_then(|r| r.attr("full-path").map(String::from)).ok_or("找不到 OPF")?;
    let opf = zip.read_string(&opf_path).ok_or("OPF 读取失败")?;
    let ox = xml::parse(&opf);
    let base = dir_of(&opf_path).to_string();
    let title = ox.find_local("title").map(|t| t.text().trim().to_string()).filter(|t| !t.is_empty());
    let mut items = std::collections::HashMap::new();
    let mut mani = Vec::new();
    ox.find_all("item", &mut mani);
    if mani.is_empty() {
        ox.find_all("opf:item", &mut mani);
    }
    for it in mani {
        if let (Some(id), Some(h)) = (it.attr("id"), it.attr("href")) {
            items.insert(id.to_string(), join(&base, h));
        }
    }
    let mut refs = Vec::new();
    ox.find_all("itemref", &mut refs);
    if refs.is_empty() {
        ox.find_all("opf:itemref", &mut refs);
    }
    let mut images: Vec<Image> = Vec::new();
    let mut parts = Vec::new();
    for r in refs {
        if r.attr("linear") == Some("no") {
            continue;
        }
        let Some(path) = r.attr("idref").and_then(|i| items.get(i)) else { continue };
        let Some(html) = zip.read_string(path) else { continue };
        let html = rewrite_images(&html, dir_of(path), &zip, &mut images);
        // XHTML 序言 / DOCTYPE / <head> 交给 from_html 前去掉（只要 <body> 内容）
        let html = match (html.find("<body"), html.rfind("</body>")) {
            (Some(a), Some(b)) if b > a => {
                let inner_start = html[a..].find('>').map(|k| a + k + 1).unwrap_or(a);
                let head_title = html.find("<title>").and_then(|t| html[t + 7..].find("</title>").map(|e| html[t + 7..t + 7 + e].to_string()));
                let _ = head_title;
                format!("<html><body>{}</body></html>", &html[inner_start..b])
            }
            _ => html,
        };
        let Ok(doc) = kernel_md::from_html(&html) else { continue };
        let mut d2 = doc.clone();
        d2.title = None;
        let body = kernel_md::to_markdown(&d2);
        // 章节内标题整体降一级，章名做 ##
        let body: String = body
            .lines()
            .map(|l| if l.starts_with('#') && l.trim_start_matches('#').starts_with(' ') && l.len() < 300 { format!("#{l}") } else { l.to_string() })
            .collect::<Vec<_>>()
            .join("\n");
        let chapter = doc.title.clone().filter(|t| !t.trim().is_empty());
        let only_cover = body.trim().is_empty() || (body.trim().starts_with("![") && body.trim().lines().count() == 1 && chapter.is_none());
        if only_cover && parts.is_empty() && body.trim().is_empty() {
            continue;
        }
        match chapter {
            Some(c) => parts.push(format!("## {}\n\n{}", c.trim(), body.trim())),
            None => parts.push(body.trim().to_string()),
        }
    }
    Ok(Converted { title, markdown: parts.join("\n\n") + "\n", images, warnings: Vec::new() })
}

fn rewrite_images(html: &str, dir: &str, zip: &ZipReader, images: &mut Vec<Image>) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    for key in ["src=\""] {
        while let Some(i) = rest.find(key) {
            let (head, tail) = rest.split_at(i + key.len());
            out.push_str(head);
            let Some(end) = tail.find('"') else {
                rest = tail;
                break;
            };
            let src = &tail[..end];
            let path = join(dir, src);
            match zip.read(&path).filter(|_| !src.starts_with("http")) {
                Some(bytes) => {
                    let tok = crate::push_image(images, path.rsplit('/').next().unwrap_or("img").to_string(), bytes);
                    out.push_str(&tok);
                }
                None => out.push_str(src),
            }
            rest = &tail[end..];
        }
    }
    out.push_str(rest);
    out
}
