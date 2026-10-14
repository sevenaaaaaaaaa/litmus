//! kernel-convert：办公 / 开源文档格式 ↔ Markdown。
//!
//! 导入（结构保真，零外部工具）：docx · odt · pptx · odp · xlsx · ods · epub · rtf · html
//! 导出：docx · odt（样式 / 编号 / 表格 / 图片 / 中文字体）
//!
//! 只依赖 flate2：zip 与 xml 都是仓内最小实现（见 [`zip`] / [`xml`]）。

pub mod docx;
pub mod epub;
pub mod export;
pub mod mdw;
pub mod odf;
pub mod rtf;
pub mod sheets;
pub mod slides;
pub mod xml;
pub mod zip;

/// 导入结果：Markdown 正文（不含标题行）+ 标题 + 内嵌图片。
/// 图片在正文里以 `kc-img://<n>` 占位，调用方存成附件后用 [`Converted::resolve_images`] 换成真实路径。
#[derive(Debug, Clone, Default)]
pub struct Converted {
    pub title: Option<String>,
    pub markdown: String,
    pub images: Vec<Image>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub token: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

pub(crate) fn push_image(images: &mut Vec<Image>, name: String, bytes: Vec<u8>) -> String {
    // 同一张图（同内容）只存一次
    if let Some(i) = images.iter().find(|i| i.bytes == bytes) {
        return i.token.clone();
    }
    let token = format!("kc-img://{}", images.len());
    images.push(Image { token: token.clone(), name, bytes });
    token
}

impl Converted {
    /// 用回调把图片存成附件（返回库内路径），替换正文占位符。存失败的图片去掉占位。
    pub fn resolve_images(&mut self, mut save: impl FnMut(&Image) -> Option<String>) {
        for img in &self.images {
            match save(img) {
                Some(path) => self.markdown = self.markdown.replace(&format!("({})", img.token), &format!("({path})")),
                None => self.markdown = self.markdown.replace(&format!("({})", img.token), "()"),
            }
        }
    }

    /// 完整文档：`# 标题` + 正文。
    pub fn document(&self, fallback_title: &str) -> String {
        let t = self.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| fallback_title.to_string());
        let body = self.markdown.trim();
        if body.starts_with("# ") {
            return format!("{body}\n");
        }
        format!("# {}\n\n{}\n", t.trim(), body)
    }
}

/// 能原生转换的扩展名。
pub fn supports(ext: &str) -> bool {
    matches!(ext, "docx" | "docm" | "dotx" | "odt" | "ott" | "fodt" | "pptx" | "odp" | "xlsx" | "xlsm" | "ods" | "epub" | "rtf" | "html" | "htm" | "xhtml")
}

/// 按扩展名转换。
pub fn convert(ext: &str, bytes: &[u8]) -> Result<Converted, String> {
    let mut c = match ext {
        "docx" | "docm" | "dotx" => docx::convert(bytes)?,
        "odt" | "ott" => odf::convert_text(bytes)?,
        "fodt" => odf::convert_flat(&String::from_utf8_lossy(bytes))?,
        "pptx" => slides::pptx(bytes)?,
        "odp" => slides::odp(bytes)?,
        "xlsx" | "xlsm" => sheets::xlsx(bytes)?,
        "ods" => sheets::ods(bytes)?,
        "epub" => epub::convert(bytes)?,
        "rtf" => rtf::convert(&String::from_utf8_lossy(bytes))?,
        "html" | "htm" | "xhtml" => html(&decode_text(bytes))?,
        other => return Err(format!("不支持的格式：{other}")),
    };
    normalize_headings(&mut c);
    c.markdown = polish(&c.markdown);
    Ok(c)
}

/// 标题层级收口：正文首行一级标题当文档标题；其余一级标题整体降一级，给文档标题让位。
fn normalize_headings(c: &mut Converted) {
    let body = c.markdown.trim_start().to_string();
    let mut lines: Vec<String> = body.lines().map(String::from).collect();
    if let Some(first) = lines.first().cloned() {
        if let Some(h) = first.strip_prefix("# ") {
            let h = h.trim().trim_matches('*').trim().to_string();
            match &c.title {
                None => {
                    c.title = Some(h);
                    lines.remove(0);
                }
                Some(t) if t.trim() == h => {
                    lines.remove(0);
                }
                _ => {}
            }
        }
    }
    let mut in_fence = false;
    let has_h1 = lines.iter().any(|l| {
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        !in_fence && l.starts_with("# ")
    });
    let mut in_fence = false;
    c.markdown = lines
        .into_iter()
        .map(|l| {
            if l.trim_start().starts_with("```") {
                in_fence = !in_fence;
            }
            let hashes = l.chars().take_while(|ch| *ch == '#').count();
            if in_fence || hashes == 0 || hashes > 6 || !l[hashes..].starts_with(' ') {
                return l;
            }
            // 标题里不留整段强调记号
            let text = l[hashes + 1..].trim().trim_matches('*').trim().to_string();
            let lv = if has_h1 { (hashes + 1).min(6) } else { hashes };
            format!("{} {}", "#".repeat(lv), text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    c.markdown.push('\n');
}

/// HTML → Markdown（复用 kernel-md 的 from_html：结构化块 + 行内语法）。
pub fn html(src: &str) -> Result<Converted, String> {
    let doc = kernel_md::from_html(src).map_err(|e| e.to_string())?;
    let title = doc.title.clone();
    let mut d2 = doc.clone();
    d2.title = None;
    Ok(Converted { title, markdown: kernel_md::to_markdown(&d2), images: Vec::new(), warnings: Vec::new() })
}

/// 文本解码：UTF-8 优先，含 BOM 的 UTF-16 也认。
pub fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        let u: Vec<u16> = bytes[2..].chunks(2).filter(|c| c.len() == 2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        return String::from_utf16_lossy(&u);
    }
    let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    String::from_utf8_lossy(b).into_owned()
}

/// 转换后的收口：用排版规则统一空行 / 表格 / 序号（不提升标题——标题由转换器决定）。
fn polish(md: &str) -> String {
    let opts = kernel_md::tidy::TidyOptions { promote_title: false, pangu: false };
    kernel_md::tidy::tidy(md, &opts).markdown
}

#[cfg(test)]
mod roundtrip {
    use super::*;

    const MD: &str = "# 测试文档\n\n## 第一节\n\n这是**加粗**、*斜体*、~~删除~~、`代码`与[链接](https://example.com)。\n\n- 甲\n- 乙\n  - 乙一\n- 丙\n\n1. 一\n2. 二\n3. 三\n\n| 名称 | 数值 |\n| --- | --- |\n| A | 1 |\n| B | 2 |\n\n```rust\nfn main() {}\n```\n\n> 引用一句。\n\n## 第二节\n\n![图](Assets/a.png)\n\n最后一段，公式 $x^2$。\n";

    fn png() -> Vec<u8> {
        // 1x1 PNG
        vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4, 0, 9, 251, 3, 253, 167, 98, 97, 21, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130]
    }

    fn check(c: &Converted) {
        let md = &c.markdown;
        assert_eq!(c.title.as_deref(), Some("测试文档"), "{md}");
        for want in ["## 第一节", "**加粗**", "*斜体*", "~~删除~~", "`代码`", "[链接](https://example.com)", "- 甲", "  - 乙一", "1. 一\n2. 二\n3. 三", "| 名称 | 数值 |", "```", "fn main() {}", "> 引用一句。", "## 第二节", "kc-img://0"] {
            assert!(md.contains(want), "缺少 {want:?}:\n{md}");
        }
        assert_eq!(c.images.len(), 1);
    }

    #[test]
    fn docx_and_odt_roundtrip() {
        let img = png();
        let load = move |s: &str| if s == "Assets/a.png" { Some(img.clone()) } else { None };
        let opts = export::ExportOpts { title: None, load_image: &load };
        let docx = export::to_docx(MD, &opts).unwrap();
        check(&convert("docx", &docx).unwrap());
        let odt = export::to_odt(MD, &opts).unwrap();
        check(&convert("odt", &odt).unwrap());
    }

    #[test]
    fn rtf_basic() {
        let rtf = r#"{\rtf1\ansi{\fonttbl{\f0 Arial;}{\f1\fmodern Courier New;}}{\stylesheet{\s1 heading 1;}}{\info{\title \u40575?\u34122?}}
\pard\s1 \u31532?\u19968?\u31456?\par
\pard Hello {\b bold} and {\field{\*\fldinst{HYPERLINK "https://a.com"}}{\fldrslt{link}}}.\par
\pard{\listtext \bullet\tab}item one\par
\pard{\listtext \bullet\tab}item two\par
\pard\intbl a\cell b\cell\row
\pard\intbl 1\cell 2\cell\row
\pard{\f1 let x = 1;}\par
}"#;
        let c = convert("rtf", rtf.as_bytes()).unwrap();
        assert_eq!(c.title.as_deref(), Some("鹿蕊"));
        let m = &c.markdown;
        for want in ["第一章", "**bold**", "[link](https://a.com)", "- item one\n- item two", "| a | b |", "let x = 1;"] {
            assert!(m.contains(want), "缺少 {want:?}:\n{m}");
        }
    }
}
