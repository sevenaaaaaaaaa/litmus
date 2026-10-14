//! 表格文件（.xlsx / .ods）→ 每个工作表一节 Markdown 表格。

use crate::mdw;
use crate::xml::{self, Node};
use crate::zip::ZipReader;
use crate::Converted;
use std::collections::HashMap;

const MAX_ROWS: usize = 500;
const MAX_COLS: usize = 40;

pub fn xlsx(bytes: &[u8]) -> Result<Converted, String> {
    let zip = ZipReader::new(bytes).map_err(|e| e.to_string())?;
    let wb = zip.read_string("xl/workbook.xml").ok_or("缺少 xl/workbook.xml")?;
    let shared: Vec<String> = zip
        .read_string("xl/sharedStrings.xml")
        .map(|s| {
            let r = xml::parse(&s);
            let mut si = Vec::new();
            r.find_all("si", &mut si);
            si.iter()
                .map(|n| {
                    // 富文本：只取 t，跳过拼音 rPh
                    let mut ts = Vec::new();
                    collect_t(n, &mut ts);
                    ts.concat()
                })
                .collect()
        })
        .unwrap_or_default();
    let rels: HashMap<String, String> = zip
        .read_string("xl/_rels/workbook.xml.rels")
        .map(|s| {
            let r = xml::parse(&s);
            let mut v = Vec::new();
            r.find_all("Relationship", &mut v);
            v.iter().filter_map(|x| Some((x.attr("Id")?.to_string(), x.attr("Target")?.to_string()))).collect()
        })
        .unwrap_or_default();
    let wbx = xml::parse(&wb);
    let mut sheets = Vec::new();
    wbx.find_all("sheet", &mut sheets);
    let mut blocks = Vec::new();
    let multi = sheets.len() > 1;
    for (idx, sh) in sheets.iter().enumerate() {
        let name = sh.attr("name").unwrap_or("Sheet").to_string();
        let target = sh
            .attr("r:id")
            .and_then(|id| rels.get(id).cloned())
            .unwrap_or_else(|| format!("worksheets/sheet{}.xml", idx + 1));
        let path = if target.starts_with('/') { target.trim_start_matches('/').to_string() } else { format!("xl/{}", target.trim_start_matches("./")) };
        let Some(sx) = zip.read_string(&path) else { continue };
        let root = xml::parse(&sx);
        let mut rows_n = Vec::new();
        root.find_all("row", &mut rows_n);
        let mut grid: Vec<Vec<String>> = Vec::new();
        let total = rows_n.len();
        for r in rows_n.iter().take(MAX_ROWS) {
            let mut row: Vec<String> = Vec::new();
            for c in r.elems().filter(|e| e.local() == "c") {
                let col = c.attr("r").map(col_index).unwrap_or(row.len());
                if col >= MAX_COLS {
                    continue;
                }
                while row.len() < col {
                    row.push(String::new());
                }
                let t = c.attr("t").unwrap_or("n");
                let v = c.child_local("v").map(|v| v.text()).unwrap_or_default();
                let val = match t {
                    "s" => v.trim().parse::<usize>().ok().and_then(|i| shared.get(i).cloned()).unwrap_or_default(),
                    "inlineStr" => c.child_local("is").map(|n| n.text()).unwrap_or_default(),
                    "b" => if v.trim() == "1" { "TRUE".into() } else { "FALSE".into() },
                    _ => tidy_number(&v),
                };
                row.push(val);
            }
            grid.push(row);
        }
        push_sheet(&mut blocks, &name, grid, total, multi);
    }
    Ok(Converted { title: None, markdown: mdw::join_blocks(&blocks), images: Vec::new(), warnings: Vec::new() })
}

fn collect_t(n: &Node, out: &mut Vec<String>) {
    for c in n.elems() {
        if c.local() == "rPh" {
            continue;
        }
        if c.local() == "t" {
            out.push(c.text());
        } else {
            collect_t(c, out);
        }
    }
}

fn col_index(r: &str) -> usize {
    let mut n = 0usize;
    for ch in r.chars().take_while(|c| c.is_ascii_alphabetic()) {
        n = n * 26 + (ch.to_ascii_uppercase() as usize - 'A' as usize + 1);
    }
    n.saturating_sub(1)
}

/// 0.30000000000000004 → 0.3（浮点噪声）
fn tidy_number(v: &str) -> String {
    let v = v.trim();
    if let Ok(f) = v.parse::<f64>() {
        if v.contains('.') && v.len() > 12 {
            let s = format!("{:.10}", f);
            return s.trim_end_matches('0').trim_end_matches('.').to_string();
        }
    }
    v.to_string()
}

fn push_sheet(blocks: &mut Vec<String>, name: &str, mut grid: Vec<Vec<String>>, total: usize, multi: bool) {
    grid.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    if grid.is_empty() {
        return;
    }
    // 去掉全空列
    let n = grid.iter().map(|r| r.len()).max().unwrap_or(0);
    let keep: Vec<usize> = (0..n).filter(|&k| grid.iter().any(|r| r.get(k).map_or(false, |c| !c.trim().is_empty()))).collect();
    let grid: Vec<Vec<String>> = grid.iter().map(|r| keep.iter().map(|&k| r.get(k).cloned().unwrap_or_default()).collect()).collect();
    if multi {
        blocks.push(format!("## {name}"));
    }
    blocks.push(mdw::table(&grid));
    if total > MAX_ROWS {
        blocks.push(format!("（仅导入前 {MAX_ROWS} 行，全表共 {total} 行；原件已存为附件）"));
    }
}

pub fn ods(bytes: &[u8]) -> Result<Converted, String> {
    let zip = ZipReader::new(bytes).map_err(|e| e.to_string())?;
    let content = zip.read_string("content.xml").ok_or("缺少 content.xml")?;
    let root = xml::parse(&content);
    let mut tables = Vec::new();
    root.find_all("table:table", &mut tables);
    let multi = tables.len() > 1;
    let mut blocks = Vec::new();
    for t in tables {
        let name = t.attr("table:name").unwrap_or("Sheet").to_string();
        let mut rows = Vec::new();
        collect_rows(t, &mut rows);
        let total = rows.len();
        let mut grid = Vec::new();
        for r in rows {
            let rep_rows: usize = r.attr("table:number-rows-repeated").and_then(|v| v.parse().ok()).unwrap_or(1);
            let mut row = Vec::new();
            for c in r.elems() {
                if !(c.is("table:table-cell") || c.is("table:covered-table-cell")) {
                    continue;
                }
                let rep: usize = c.attr("table:number-columns-repeated").and_then(|v| v.parse().ok()).unwrap_or(1);
                let text: Vec<String> = c.elems().filter(|p| p.is("text:p")).map(|p| p.text()).collect();
                let val = if text.is_empty() { c.attr("office:value").unwrap_or("").to_string() } else { text.join(" ") };
                for _ in 0..rep.min(MAX_COLS) {
                    if row.len() < MAX_COLS {
                        row.push(val.clone());
                    }
                }
            }
            // 末尾海量重复空行（ODS 常见）不展开
            let empty = row.iter().all(|c| c.trim().is_empty());
            for _ in 0..if empty { 1 } else { rep_rows.min(MAX_ROWS) } {
                if grid.len() < MAX_ROWS {
                    grid.push(row.clone());
                }
            }
        }
        push_sheet(&mut blocks, &name, grid, total, multi);
    }
    Ok(Converted { title: None, markdown: mdw::join_blocks(&blocks), images: Vec::new(), warnings: Vec::new() })
}

fn collect_rows<'a>(n: &'a Node, out: &mut Vec<&'a Node>) {
    for c in n.elems() {
        if c.is("table:table-row") {
            out.push(c);
        } else if c.local().starts_with("table-") && c.local() != "table-cell" {
            collect_rows(c, out);
        }
    }
}
