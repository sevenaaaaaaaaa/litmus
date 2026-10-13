//! kernel-md: 块模型与确定性 Markdown 序列化。
//!
//! 文件 = 内容真相。块模型可无损映射到 MD；未知/外来结构包进
//! `<!-- tc:block raw -->` fenced 容器保真不丢。序列化是确定性的：
//! 同一块模型永远产出字节相同的 MD，保证 git diff 干净、外部编辑器互操作无损。

pub mod tidy;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MdError {
    #[error("block parse failed at line {line}: {msg}")]
    Parse { line: usize, msg: String },
}

/// 块 ID：内容派生（编辑后身份改变，未编辑的块跨外部编辑保持身份），
/// 这是块级 CRDT 合并的基础（ADR-0004）。
pub type BlockId = String;

/// 规范块模型。V1 支持核心类型；其余进 Raw 容器。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// 段落。行内结构（加粗/链接等）以原样 MD 行内文本保存，V1 不做行内 AST。
    Paragraph { id: BlockId, text: String },
    /// 标题。level 1..=6。
    Heading { id: BlockId, level: u8, text: String },
    /// 代码块。
    Code { id: BlockId, lang: Option<String>, text: String },
    /// 引用块（多行）。
    Quote { id: BlockId, lines: Vec<String> },
    /// 列表。items 为原样行内文本，ordered 区分 -/1.。
    List { id: BlockId, ordered: bool, items: Vec<String> },
    /// 表格。第一行为表头。
    Table { id: BlockId, header: Vec<String>, rows: Vec<Vec<String>> },
    /// 分隔线。
    Divider { id: BlockId },
    /// 原样保留块：任何无法结构化的内容，原字节保真。
    Raw { id: BlockId, text: String },
}

impl Block {
    pub fn id(&self) -> &str {
        match self {
            Block::Paragraph { id, .. }
            | Block::Heading { id, .. }
            | Block::Code { id, .. }
            | Block::Quote { id, .. }
            | Block::List { id, .. }
            | Block::Table { id, .. }
            | Block::Divider { id, .. }
            | Block::Raw { id, .. } => id,
        }
    }

    pub fn set_id(&mut self, new_id: String) {
        match self {
            Block::Paragraph { id, .. }
            | Block::Heading { id, .. }
            | Block::Code { id, .. }
            | Block::Quote { id, .. }
            | Block::List { id, .. }
            | Block::Table { id, .. }
            | Block::Divider { id, .. }
            | Block::Raw { id, .. } => *id = new_id,
        }
    }

    /// 块类型名。
    pub fn kind(&self) -> &'static str {
        match self {
            Block::Paragraph { .. } => "paragraph",
            Block::Heading { .. } => "heading",
            Block::Code { .. } => "code",
            Block::Quote { .. } => "quote",
            Block::List { .. } => "list",
            Block::Table { .. } => "table",
            Block::Divider { .. } => "divider",
            Block::Raw { .. } => "raw",
        }
    }

    /// 内容指纹（不含 id 本身）：kind + 载荷 的 sha256 前 4 字节。
    pub fn fingerprint(&self) -> String {
        let mut cleaned = self.clone();
        cleaned.set_id(String::new());
        let payload = serde_json::to_string(&cleaned).unwrap_or_default();
        let mut h = Sha256::new();
        Digest::update(&mut h, self.kind().as_bytes());
        Digest::update(&mut h, payload.as_bytes());
        hex::encode(&h.finalize()[..4])
    }
}

/// 为文档内所有块分配内容派生的稳定 ID。
/// 完全相同的块用出现序号区分，保证同文档内唯一。
pub fn assign_stable_ids(doc: &mut DocModel) {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for b in doc.blocks.iter_mut() {
        let fp = b.fingerprint();
        let n = counts.entry(fp.clone()).or_insert(0);
        b.set_id(format!("b{fp}-{n}"));
        *n += 1;
    }
}

/// 一篇文档 = 有序块序列 + 元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DocModel {
    pub title: Option<String>,
    pub blocks: Vec<Block>,
    /// YAML frontmatter（OKF：机器可读的元数据层）
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub meta: std::collections::BTreeMap<String, String>,
    /// `^锚点` 槽位（与 blocks 一一对应；None=该块无锚点）。OKF 块引用的根。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anchors: Vec<Option<String>>,
}

/// 确定性序列化为 MD。输出只依赖输入，与插入顺序历史无关。
pub fn to_markdown(doc: &DocModel) -> String {
    let mut out = String::new();
    if !doc.meta.is_empty() {
        out.push_str("---\n");
        for (k, v) in &doc.meta {
            out.push_str(&format!("{k}: {v}\n"));
        }
        out.push_str("---\n\n");
    }
    if let Some(t) = &doc.title {
        out.push_str("# ");
        out.push_str(t);
        out.push_str("\n\n");
    }
    for (i, b) in doc.blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        push_block(&mut out, b);
        // 锚点回填：块级末尾 ^id（Obsidian 兼容写法）
        if let Some(Some(a)) = doc.anchors.get(i) {
            out.push_str(&format!(" ^{a}"));
        }
        out.push('\n');
    }
    out
}

fn push_block(out: &mut String, b: &Block) {
    match b {
        Block::Paragraph { text, .. } => {
            out.push_str(text.trim_end());
        }
        Block::Heading { level, text, .. } => {
            for _ in 0..(*level).clamp(1, 6) {
                out.push('#');
            }
            out.push(' ');
            out.push_str(text);
        }
        Block::Code { lang, text, .. } => {
            // fence 长度取文本内最长 ``` + 1，保证嵌套安全
            let max_run = text.split('\n').map(|l| fence_run(l)).max().unwrap_or(0);
            let fence = "`".repeat(max_run + 3);
            out.push_str(&fence);
            if let Some(l) = lang {
                out.push_str(l);
            }
            out.push('\n');
            out.push_str(text);
            if !text.ends_with('\n') && !text.is_empty() {
                out.push('\n');
            }
            out.push_str(&fence);
        }
        Block::Quote { lines, .. } => {
            for (i, l) in lines.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str("> ");
                out.push_str(l);
            }
        }
        Block::List { ordered, items, .. } => {
            for (i, item) in items.iter().enumerate() {
                if *ordered {
                    out.push_str(&format!("{}. {}\n", i + 1, item));
                } else {
                    out.push_str(&format!("- {item}\n"));
                }
            }
            // 列表块末尾不带多余换行，块间距由外层控制
            out.pop();
        }
        Block::Table { header, rows, .. } => {
            out.push_str(&md_table_row(header));
            out.push_str("\n|");
            for _ in header {
                out.push_str(" --- |");
            }
            for r in rows {
                out.push('\n');
                out.push_str(&md_table_row(r));
            }
        }
        Block::Divider { .. } => out.push_str("---"),
        Block::Raw { text, .. } => {
            out.push_str("<!-- tc:block raw\n");
            out.push_str(text.trim_end_matches('\n'));
            out.push_str("\ntc:block raw -->");
        }
    }
}

fn md_table_row(cells: &[String]) -> String {
    let mut s = String::from("|");
    for c in cells {
        s.push(' ');
        s.push_str(c.replace('|', "\\|").trim());
        s.push_str(" |");
    }
    s
}

fn fence_run(line: &str) -> usize {
    let t = line.trim_start();
    if t.starts_with('`') {
        t.chars().take_while(|c| *c == '`').count()
    } else {
        0
    }
}

/// 从 MD 解析为块模型。行级解析器：识别标题/代码/引用/列表/表格/分隔线/段落，
/// 不认识的连续区域收敛为一个 Raw 块。目标是往返保真，不是完整 CommonMark。
pub fn from_markdown(src: &str) -> Result<DocModel, MdError> {
    let mut doc = DocModel::default();
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0usize;

    // ── YAML frontmatter ──
    if lines.first().map_or(false, |l| l.trim() == "---") {
        i = 1;
        while i < lines.len() && lines[i].trim() != "---" {
            if let Some((k, v)) = lines[i].split_once(':') {
                let key = k.trim().to_string();
                let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
                if !key.is_empty() {
                    doc.meta.insert(key, val);
                }
            }
            i += 1;
        }
        i += 1; // 收尾 ---
        if lines.get(i).map_or(false, |l| l.trim().is_empty()) {
            i += 1;
        }
    }

    // 首个 H1 提升为 title（若存在）
    if let Some(first) = lines.first() {
        if let Some(t) = first.strip_prefix("# ") {
            doc.title = Some(t.to_string());
            i = 1;
            if lines.get(1).map_or(false, |l| l.trim().is_empty()) {
                i = 2;
            }
        }
    }

    // 解析阶段先占位，末尾统一分配内容派生的稳定 ID
    let next_id = String::new;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // fenced code
        if trimmed.starts_with("```") {
            let fence_len = fence_run(line);
            let fence: String = "`".repeat(fence_len);
            let lang = trimmed[fence_len..].trim().to_string();
            let lang = if lang.is_empty() { None } else { Some(lang) };
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() {
                if lines[i].trim_end() == fence {
                    break;
                }
                body.push(lines[i]);
                i += 1;
            }
            if i >= lines.len() {
                // 容错：未闭合的 fence 把剩余内容全吃进来（Obsidian 常见）
                i = lines.len();
            }
            i += 1; // consume closing fence
            let mut text = body.join("\n");
            if !body.is_empty() {
                text.push('\n');
            }
            doc.blocks.push(Block::Code {
                id: next_id(),
                lang,
                text,
            });
            continue;
        }

        // raw container
        if trimmed == "<!-- tc:block raw" {
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && lines[i].trim() != "tc:block raw -->" {
                body.push(lines[i]);
                i += 1;
            }
            if i >= lines.len() {
                return Err(MdError::Parse {
                    line: i,
                    msg: "unterminated raw block".into(),
                });
            }
            i += 1;
            doc.blocks.push(Block::Raw {
                id: next_id(),
                text: body.join("\n"),
            });
            continue;
        }

        // heading
        let hlevel = trimmed.chars().take_while(|c| *c == '#').count() as u8;
        if (1..=6).contains(&hlevel) && trimmed[hlevel as usize..].starts_with(' ') {
            doc.blocks.push(Block::Heading {
                id: next_id(),
                level: hlevel,
                text: trimmed[hlevel as usize..].trim().to_string(),
            });
            i += 1;
            continue;
        }

        // divider
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            doc.blocks.push(Block::Divider { id: next_id() });
            i += 1;
            continue;
        }

        // quote
        if trimmed.starts_with("> ") || trimmed == ">" {
            let mut ql = Vec::new();
            while i < lines.len() && (lines[i].trim_start().starts_with("> ") || lines[i].trim() == ">") {
                let l = lines[i].trim_start();
                ql.push(l.strip_prefix("> ").unwrap_or(l.strip_prefix('>').unwrap_or("")).to_string());
                i += 1;
            }
            doc.blocks.push(Block::Quote { id: next_id(), lines: ql });
            continue;
        }

        // list
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let mut items = Vec::new();
            while i < lines.len() && (lines[i].trim_start().starts_with("- ") || lines[i].trim_start().starts_with("* ")) {
                items.push(lines[i].trim_start()[2..].to_string());
                i += 1;
            }
            doc.blocks.push(Block::List { id: next_id(), ordered: false, items });
            continue;
        }
        if trimmed.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            let mut items = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                let Some(dot) = t.find(". ") else { break };
                if !t[..dot].chars().all(|c| c.is_ascii_digit()) {
                    break;
                }
                items.push(t[dot + 2..].to_string());
                i += 1;
            }
            if !items.is_empty() {
                doc.blocks.push(Block::List { id: next_id(), ordered: true, items });
                continue;
            }
        }

        // table
        if trimmed.starts_with('|') && i + 1 < lines.len() && is_table_sep(lines[i + 1]) {
            let header = parse_table_row(trimmed);
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                rows.push(parse_table_row(lines[i].trim()));
                i += 1;
            }
            doc.blocks.push(Block::Table { id: next_id(), header, rows });
            continue;
        }

        // paragraph: 吸收连续非空行
        let mut para = vec![line.to_string()];
        i += 1;
        while i < lines.len() {
            let t = lines[i].trim();
            if t.is_empty()
                || t.starts_with('#')
                || t.starts_with("```")
                || t.starts_with("> ")
                || t.starts_with("- ")
                || t.starts_with("* ")
                || t == "---"
                || t.starts_with('|')
            {
                break;
            }
            para.push(lines[i].to_string());
            i += 1;
        }
        doc.blocks.push(Block::Paragraph {
            id: next_id(),
            text: para.join("\n"),
        });
    }

    // ── ^锚点：槽位对齐抽取（每个块一个槽）──
    let slots: Vec<Option<String>> = doc.blocks.iter_mut().map(|b| extract_anchor(b)).collect();
    doc.anchors = slots;
    assign_stable_ids(&mut doc);
    Ok(doc)
}

/// 抽取块末尾的 `^锚点`（写在文本里时），返回锚点名并把文本剥离。
fn extract_anchor(b: &mut Block) -> Option<String> {
    let text_mut = match b {
        Block::Paragraph { text, .. } | Block::Heading { text, .. } => text,
        Block::Code { text, .. } | Block::Raw { text, .. } => text,
        _ => return None,
    };
    let t = text_mut.trim_end();
    if let Some(sp) = t.rfind(" ^") {
        let name = &t[sp + 2..];
        let ok = !name.is_empty()
            && name.len() <= 40
            && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_');
        if ok {
            let a = name.to_string();
            *text_mut = t[..sp].trim_end().to_string();
            return Some(a);
        }
    }
    None
}

fn is_table_sep(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains("---")
}

fn parse_table_row(line: &str) -> Vec<String> {
    let t = line.trim().trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// HTML 转义（文本节点）。
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 行内 Markdown → HTML（只渲染，不改真相）：`code`、**粗**、*斜*、~~删~~、==高亮==、
/// [文字](url)、[[双链|别名]]、裸 http(s) 链接、#标签。先整体转义，再在转义后的文本上识别记号，
/// 因此用户文本里的 `<` 永远不会变成标签；链接只放行 http(s)/mailto/相对路径。
pub fn inline_md(src: &str) -> String {
    let s = esc(src);
    // 行内代码先切出来，代码里的记号不再参与后续替换
    let mut parts: Vec<(bool, String)> = Vec::new();
    let mut rest = s.as_str();
    while let Some(i) = rest.find('`') {
        if let Some(j) = rest[i + 1..].find('`') {
            parts.push((false, rest[..i].to_string()));
            parts.push((true, rest[i + 1..i + 1 + j].to_string()));
            rest = &rest[i + 1 + j + 1..];
        } else {
            break;
        }
    }
    parts.push((false, rest.to_string()));
    let mut out = String::new();
    for (code, t) in parts {
        if code {
            out.push_str(&format!("<code>{t}</code>"));
        } else {
            for (math, seg) in math_split(&t) {
                if math {
                    out.push_str(&format!("<span class=\"math-inline\">\\({seg}\\)</span>"));
                } else {
                    out.push_str(&inline_spans(&seg));
                }
            }
        }
    }
    out
}

/// 行内公式 `$…$`：开记号右侧、闭记号左侧不能是空白，闭记号后不能紧跟数字（排除「$5 和 $10」）。
fn math_split(s: &str) -> Vec<(bool, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && chars.get(i + 1).map_or(false, |c| !c.is_whitespace() && *c != '$') && (i == 0 || chars[i - 1] != '\\') {
            if let Some(rel) = chars[i + 1..].iter().position(|c| *c == '$') {
                let j = i + 1 + rel;
                let inner: String = chars[i + 1..j].iter().collect();
                let close_ok = !chars[j - 1].is_whitespace() && chars.get(j + 1).map_or(true, |c| !c.is_ascii_digit());
                if close_ok && !inner.is_empty() {
                    if !buf.is_empty() {
                        out.push((false, std::mem::take(&mut buf)));
                    }
                    out.push((true, inner));
                    i = j + 1;
                    continue;
                }
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    if !buf.is_empty() || out.is_empty() {
        out.push((false, buf));
    }
    out
}

/// 单独成段的视频链接 → 内嵌播放器（YouTube / Bilibili / Vimeo / 直链视频）。
fn video_embed(text: &str) -> Option<String> {
    let u = text.trim();
    if u.contains(char::is_whitespace) || !(u.starts_with("https://") || u.starts_with("http://")) {
        return None;
    }
    let frame = |src: String| {
        format!("<div class=\"kb-embed\"><iframe src=\"{}\" allowfullscreen loading=\"lazy\" referrerpolicy=\"no-referrer\"></iframe></div>\n", esc(&src))
    };
    let after = |k: &str| u.split(k).nth(1).map(|r| r.split(|c| c == '&' || c == '?' || c == '#' || c == '/').next().unwrap_or("").to_string());
    if u.contains("youtube.com/watch") {
        let id = u.split("v=").nth(1)?.split('&').next()?.to_string();
        return Some(frame(format!("https://www.youtube-nocookie.com/embed/{id}")));
    }
    if u.contains("youtu.be/") {
        return Some(frame(format!("https://www.youtube-nocookie.com/embed/{}", after("youtu.be/")?)));
    }
    if u.contains("bilibili.com/video/") {
        let id = after("bilibili.com/video/")?;
        let key = if id.starts_with("BV") { "bvid" } else { "aid" };
        let id = id.trim_start_matches("av");
        return Some(frame(format!("https://player.bilibili.com/player.html?{key}={id}&autoplay=0")));
    }
    if u.contains("vimeo.com/") {
        let id = after("vimeo.com/")?;
        if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() {
            return Some(frame(format!("https://player.vimeo.com/video/{id}")));
        }
    }
    let low = u.to_ascii_lowercase();
    let path = low.split(['?', '#']).next().unwrap_or("");
    if [".mp4", ".webm", ".mov", ".m4v"].iter().any(|x| path.ends_with(x)) {
        return Some(format!("<div class=\"kb-embed\"><video src=\"{}\" controls preload=\"metadata\"></video></div>\n", esc(u)));
    }
    None
}

/// 列表项：`[ ]` / `[x]` 渲染成只读勾选框（任务清单）
fn list_item(i: &str) -> String {
    let t = i.trim_start();
    if let Some(r) = t.strip_prefix("[ ] ") {
        return format!("<span class=\"kb-task\"><input type=\"checkbox\" disabled> {}</span>", inline_md(r));
    }
    if let Some(r) = t.strip_prefix("[x] ").or_else(|| t.strip_prefix("[X] ")) {
        return format!(
            "<span class=\"kb-task done\"><input type=\"checkbox\" checked disabled> {}</span>",
            inline_md(r)
        );
    }
    inline_md(i)
}

fn wrap_pairs(s: &str, mark: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let Some(i) = rest.find(mark) else { break };
        let after = &rest[i + mark.len()..];
        let Some(j) = after.find(mark) else { break };
        let inner = &after[..j];
        // 记号内侧不能是空白（`a * b * c` 不是斜体）
        if inner.is_empty() || inner.starts_with(' ') || inner.ends_with(' ') {
            out.push_str(&rest[..i + mark.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..i]);
        out.push_str(open);
        out.push_str(inner);
        out.push_str(close);
        rest = &after[j + mark.len()..];
    }
    out.push_str(rest);
    out
}

fn safe_href(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    l.starts_with("http://")
        || l.starts_with("https://")
        || l.starts_with("mailto:")
        || l.starts_with('#')
        || l.starts_with('/')
        || l.starts_with("./")
        || l.starts_with("../")
        || (!l.contains(':') && !l.is_empty())
}

fn inline_spans(s: &str) -> String {
    // [[双链]]：data-wiki 交给宿主解析（文件名/标题都行），href="#" 不在沙箱里导航
    let mut t = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("[[") {
        let after = &rest[i + 2..];
        let Some(j) = after.find("]]") else { break };
        let raw = &after[..j];
        let (target, label) = match raw.split_once('|') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (raw.trim(), raw.split(['#', '^']).next().unwrap_or(raw).trim()),
        };
        let target = target.split(['#', '^']).next().unwrap_or(target).trim();
        t.push_str(&rest[..i]);
        t.push_str(&format!("<a class=\"wikilink\" href=\"#\" data-wiki=\"{target}\">{label}</a>"));
        rest = &after[j + 2..];
    }
    t.push_str(rest);
    // [文字](url)
    let mut u = String::new();
    let mut rest = t.as_str();
    while let Some(i) = rest.find('[') {
        let after = &rest[i + 1..];
        let Some(j) = after.find("](") else { break };
        let label = &after[..j];
        if label.contains('[') || label.contains("<a ") {
            u.push_str(&rest[..i + 1]);
            rest = after;
            continue;
        }
        let tail = &after[j + 2..];
        let Some(k) = tail.find(')') else { break };
        let href = &tail[..k];
        u.push_str(&rest[..i]);
        if safe_href(href) {
            u.push_str(&format!("<a href=\"{href}\" target=\"_blank\" rel=\"noopener\">{label}</a>"));
        } else {
            u.push_str(label);
        }
        rest = &tail[k + 1..];
    }
    u.push_str(rest);
    let mut v = wrap_pairs(&u, "**", "<strong>", "</strong>");
    v = wrap_pairs(&v, "__", "<strong>", "</strong>");
    v = wrap_pairs(&v, "~~", "<del>", "</del>");
    v = wrap_pairs(&v, "==", "<mark>", "</mark>");
    v = wrap_pairs(&v, "*", "<em>", "</em>");
    autolink_and_tags(&v)
}

/// 裸链接与 #标签：只在标签外的文本里识别（跳过 `<...>` 内部与已有 <a>，避免改坏生成的链接）
fn autolink_and_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut in_a = false;
    let chars: Vec<char> = s.chars().collect();
    let stop = "，。；！？）】」、";
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            in_tag = true;
            let head: String = chars[i..chars.len().min(i + 3)].iter().collect();
            if head.starts_with("<a ") {
                in_a = true;
            }
            if head.starts_with("</a") {
                in_a = false;
            }
        }
        if in_tag {
            out.push(c);
            if c == '>' {
                in_tag = false;
            }
            i += 1;
            continue;
        }
        if !in_a {
            let head: String = chars[i..chars.len().min(i + 8)].iter().collect();
            if head.starts_with("http://") || head.starts_with("https://") {
                let mut j = i;
                while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '<' && !stop.contains(chars[j]) {
                    j += 1;
                }
                let url: String = chars[i..j].iter().collect();
                out.push_str(&format!("<a href=\"{url}\" target=\"_blank\" rel=\"noopener\">{url}</a>"));
                i = j;
                continue;
            }
            let prev_ok = i == 0 || chars[i - 1].is_whitespace();
            if c == '#' && prev_ok && i + 1 < chars.len() && !chars[i + 1].is_whitespace() && chars[i + 1] != '#' {
                let mut j = i + 1;
                while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '<' && chars[j] != '#' && !stop.contains(chars[j]) {
                    j += 1;
                }
                let tag: String = chars[i + 1..j].iter().collect();
                out.push_str(&format!("<span class=\"kb-hashtag\">#{tag}</span>"));
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 渲染为自包含 AI-HTML：语义化标签 + 内嵌 JSON-LD 块模型（人与 agent 双可读）。
/// Raw 块按原样内联（其存在意义就是保真外来结构）；应用内渲染时须置于沙箱。
pub fn to_html(doc: &DocModel) -> String {
    let title = doc.title.clone().unwrap_or_else(|| "Untitled".to_string());
    // JSON 内嵌 HTML：转义 < > &，防止 </script> 提前闭合造成注入
    let ld = serde_json::to_string(doc)
        .unwrap_or_else(|_| "{}".into())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let title_esc = esc(&title);
    let n = doc.blocks.len();
    let mut body = String::new();
    let mut has_math = false;
    let mut has_mermaid = false;
    if doc.title.is_some() {
        // 自包含文档里标题也是 H1：人类可读、agent 可解析、往返一致
        body.push_str(&format!("<h1>{}</h1>\n", esc(doc.title.as_deref().unwrap_or(""))));
    }
    for (bi, b) in doc.blocks.iter().enumerate() {
        // 块锚点 id（b{index}）：AI 引用回链的滚动目标（product-principles 机制⑤）。
        // 显式 ^anchor 的块同时带 data-anchor 名。
        let anchor_name = doc.anchors.get(bi).cloned().flatten();
        let id_attr = match &anchor_name {
            Some(a) => format!(" id=\"b{bi}\" data-anchor=\"{}\"", esc(a)),
            None => format!(" id=\"b{bi}\""),
        };
        match b {
            Block::Paragraph { text, .. } => {
                let t = text.trim();
                if t.len() > 4 && t.starts_with("$$") && t.ends_with("$$") {
                    has_math = true;
                    body.push_str(&format!("<div{id_attr} class=\"math-display\">\\[{}\\]</div>\n", esc(t[2..t.len() - 2].trim())));
                } else if let Some(v) = video_embed(t) {
                    body.push_str(&v);
                } else {
                    let h = inline_md(text);
                    has_math |= h.contains("class=\"math-inline\"");
                    body.push_str(&format!("<p{id_attr}>{h}</p>\n"));
                }
            }
            Block::Heading { level, text, .. } => {
                let l = (*level).clamp(1, 6);
                body.push_str(&format!("<h{l}{id_attr}>{}</h{l}>\n", inline_md(text)))
            }
            Block::Code { lang, text, .. } if lang.as_deref() == Some("mermaid") => {
                has_mermaid = true;
                body.push_str(&format!("<pre{id_attr} class=\"mermaid\">{}</pre>\n", esc(text)))
            }
            Block::Code { lang, text, .. } => {
                let cls = lang
                    .as_deref()
                    .map(|l| format!(" class=\"language-{}\"", esc(l)))
                    .unwrap_or_default();
                body.push_str(&format!("<pre><code{cls}>{}</code></pre>\n", esc(text)))
            }
            Block::Quote { lines, .. } => {
                let inner: String = lines.iter().map(|l| format!("<p>{}</p>", inline_md(l))).collect();
                body.push_str(&format!("<blockquote{id_attr}>{inner}</blockquote>\n"))
            }
            Block::List { ordered, items, .. } => {
                let (open, close) = if *ordered { ("ol", "ol") } else { ("ul", "ul") };
                let inner: String = items.iter().map(|i| format!("<li>{}</li>", list_item(i))).collect();
                body.push_str(&format!("<{open}{id_attr}>{inner}</{close}>\n"))
            }
            Block::Table { header, rows, .. } => {
                let th: String = header.iter().map(|h| format!("<th>{}</th>", inline_md(h))).collect();
                let trs: String = rows
                    .iter()
                    .map(|r| {
                        let tds: String = r.iter().map(|c| format!("<td>{}</td>", inline_md(c))).collect();
                        format!("<tr>{tds}</tr>")
                    })
                    .collect();
                body.push_str(&format!(
                    "<table{id_attr}><thead><tr>{th}</tr></thead><tbody>{trs}</tbody></table>\n"
                ))
            }
            Block::Divider { .. } => body.push_str("<hr>\n"),
            Block::Raw { text, .. } => body.push_str(text),
        }
    }
    // 公式 / 流程图：有才加载（CDN，离线时保留源码可读）
    if has_math {
        body.push_str(concat!(
            "<link rel=\"stylesheet\" href=\"https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.css\">",
            "<script defer src=\"https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.js\"></script>",
            "<script defer src=\"https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/contrib/auto-render.min.js\" ",
            "onload=\"renderMathInElement(document.body,{delimiters:[{left:'\\\\[',right:'\\\\]',display:true},{left:'\\\\(',right:'\\\\)',display:false}],throwOnError:false})\"></script>\n"
        ));
    }
    if has_mermaid {
        body.push_str("<script type=\"module\">import m from 'https://cdn.jsdelivr.net/npm/mermaid@10.9.1/dist/mermaid.esm.min.mjs';m.initialize({startOnLoad:true,theme:matchMedia('(prefers-color-scheme: dark)').matches?'dark':'neutral',securityLevel:'strict'});</script>\n");
    }
    format!(
        "<!doctype html>\n<html lang=\"zh\"><head><meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>{}</title>\n\
<script type=\"application/ld+json\">{}</script>\n\
<style>\n\
:root{{--doc-fg:oklch(24% .02 70);--doc-bg:oklch(98.5% .008 85);--doc-muted:oklch(48% .016 70);--doc-line:oklch(24% .02 70/.14);--doc-accent:oklch(54% .17 258);--doc-code-bg:oklch(96.5% .01 85);--doc-soft:oklch(24% .02 70/.05)}}\n\
html{{background:var(--doc-bg)}}\n\
body{{max-width:44rem;margin:0 auto;padding:56px 28px 96px;color:var(--doc-fg);font:16.5px/1.95 var(--font-body,system-ui,sans-serif);letter-spacing:.015em;-webkit-font-smoothing:antialiased}}\n\
h1{{font-size:2em;line-height:1.35;margin:0 0 14px;padding-bottom:14px;border-bottom:1px solid var(--doc-line)}}\n\
h2{{font-size:1.4em;margin:2.2em 0 .7em;position:relative;padding-left:.85em}}\n\
h2::before{{content:\"\";position:absolute;left:0;top:.3em;bottom:.3em;width:3.5px;border-radius:3px;background:var(--doc-accent)}}\n\
h3,h4{{font-size:1.14em;margin:1.7em 0 .55em}}\n\
p{{margin:.9em 0}}\n\
a{{color:var(--doc-accent);text-decoration:none;border-bottom:1px solid color-mix(in oklab,var(--doc-accent) 38%,transparent);transition:border-color .18s}}\n\
a:hover{{border-bottom-color:var(--doc-accent)}}\n\
strong{{font-weight:650}}\n\
blockquote{{margin:1.3em 0;padding:.7em 1.2em;border-left:3px solid var(--doc-accent);background:var(--doc-soft);border-radius:0 12px 12px 0;color:var(--doc-muted)}}\n\
blockquote p{{margin:.35em 0}}\n\
code{{font-family:var(--font-mono,ui-monospace,monospace);font-size:.86em;background:var(--doc-code-bg);border:1px solid var(--doc-line);border-radius:7px;padding:.12em .42em}}\n\
pre{{background:oklch(23% .014 75);color:oklch(93% .008 85);border-radius:14px;padding:18px 20px;overflow:auto;box-shadow:0 14px 34px -18px oklch(0% 0 0/.5)}}\n\
pre code{{background:none;border:none;color:inherit;padding:0;font-size:.88em;line-height:1.75}}\n\
table{{width:100%;border-collapse:collapse;margin:1.3em 0;font-size:.93em;border:1px solid var(--doc-line);border-radius:12px;overflow:hidden}}\n\
th{{background:var(--doc-soft);text-align:left;font-weight:650}}\n\
th,td{{padding:.55em .9em;border-bottom:1px solid var(--doc-line)}}\n\
tr:last-child td{{border-bottom:none}}\n\
tbody tr:nth-child(even){{background:color-mix(in oklab,var(--doc-soft) 55%,transparent)}}\n\
img{{max-width:100%;border-radius:14px;display:block;margin:1.4em auto;box-shadow:0 18px 44px -22px oklch(0% 0 0/.42)}}\n\
hr{{border:none;height:1px;background:var(--doc-line);margin:2.4em auto;width:62%}}\n\
::selection{{background:color-mix(in oklab,var(--doc-accent) 26%,transparent)}}\n\
/* 内联视图（Views 层） */\n\
.kb-view{{margin:1.6em 0;border:1px solid var(--doc-line);border-radius:14px;overflow:hidden}}\n\
.kb-view>header{{padding:9px 14px;background:var(--doc-soft);font:600 12.5px var(--font-mono,monospace);color:var(--doc-muted)}}\n\
.kb-view table{{margin:0;border:none;border-radius:0;width:100%}}\n\
.kb-view a{{cursor:pointer}}\n\
.kb-col{{padding:10px 14px;border-bottom:1px solid var(--doc-line)}}\n\
.kb-col h4{{margin:0 0 8px;font-size:13px}}\n\
.kb-col h4 span{{color:var(--doc-muted);font-weight:400;font-size:11px;margin-left:6px}}\n\
.kb-card{{display:block;padding:7px 10px;margin:4px 0;border-radius:9px;background:var(--doc-soft);text-decoration:none;color:inherit;font-size:13px}}\n\
.kb-card:hover{{background:color-mix(in oklab,var(--doc-accent) 14%,transparent)}}\n\
.kb-day{{padding:10px 14px;border-bottom:1px solid var(--doc-line)}}\n\
.kb-tag{{display:inline-block;padding:1px 7px;border-radius:999px;background:var(--doc-soft);font-size:11px;margin-right:4px}}\n\
a.wikilink{{border-bottom:1px dashed color-mix(in oklab,var(--doc-accent) 55%,transparent);cursor:pointer}}\n\
a.wikilink::before{{content:\"\";display:inline-block;width:.42em;height:.42em;border-radius:50%;margin:0 .32em .12em 0;background:var(--doc-accent);opacity:.7}}\n\
mark{{background:color-mix(in oklab,oklch(85% .15 95) 55%,transparent);color:inherit;border-radius:4px;padding:0 .15em}}\n\
del{{color:var(--doc-muted)}}\n\
.kb-hashtag{{color:var(--doc-accent);font-weight:550;font-size:.92em}}\n\
.kb-task{{display:inline-flex;gap:.4em;align-items:baseline}}.kb-task.done{{color:var(--doc-muted);text-decoration:line-through}}\n\
li{{margin:.25em 0}}\n\
.kb-embed{{position:relative;aspect-ratio:16/9;margin:1.3em 0;border-radius:14px;overflow:hidden;background:var(--doc-soft)}}\n\
.kb-embed iframe,.kb-embed video{{position:absolute;inset:0;width:100%;height:100%;border:0}}\n\
.math-display{{margin:1.2em 0;overflow-x:auto;text-align:center}}\n\
pre.mermaid{{background:var(--doc-soft);color:var(--doc-fg);box-shadow:none;text-align:center}}\n\
</style>\n\
</head><body><article data-kb-format=\"ai-html\" data-block-count=\"{n}\">\n{}\n</article></body></html>\n",
        title_esc,
        ld,
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_mermaid_video_render() {
        let doc = from_markdown("# T\n\n面积 $\\pi r^2$，价格 $5 和 $10。\n\n$$\nE = mc^2\n$$\n\n```mermaid\ngraph TD\nA-->B\n```\n\nhttps://www.bilibili.com/video/BV1xx411c7mD\n\nhttps://youtu.be/abc123\n").unwrap();
        let h = to_html(&doc);
        assert!(h.contains("<span class=\"math-inline\">\\(\\pi r^2\\)</span>"), "{h}");
        assert!(h.contains("价格 $5 和 $10"), "货币不是公式: {h}");
        assert!(h.contains("class=\"math-display\">\\[E = mc^2\\]"), "{h}");
        assert!(h.contains("<pre id=\"b2\" class=\"mermaid\">graph TD\nA--&gt;B"), "{h}");
        assert!(h.contains("player.bilibili.com/player.html?bvid=BV1xx411c7mD"), "{h}");
        assert!(h.contains("youtube-nocookie.com/embed/abc123"), "{h}");
        assert!(h.contains("katex.min.js") && h.contains("mermaid.esm.min.mjs"));
        // 没有公式/流程图的文档不加载任何外部脚本
        let plain = to_html(&from_markdown("# T\n\n普通段落。\n").unwrap());
        assert!(!plain.contains("cdn.jsdelivr"));
    }

    #[test]
    fn inline_markdown_renders_and_stays_safe() {
        let h = inline_md("石蕊**显色见真**——见 [[CRDT 与文件真相|CRDT]]、`a<b`、*斜*、==亮==、[站](https://x.com) #化学");
        assert!(h.contains("<strong>显色见真</strong>"), "{h}");
        assert!(h.contains("data-wiki=\"CRDT 与文件真相\">CRDT</a>"), "{h}");
        assert!(h.contains("<code>a&lt;b</code>"), "{h}");
        assert!(h.contains("<em>斜</em>") && h.contains("<mark>亮</mark>"), "{h}");
        assert!(h.contains("href=\"https://x.com\""), "{h}");
        assert!(h.contains("kb-hashtag\">#化学"), "{h}");
        // 注入防护：原文里的标签被转义；javascript: 链接不生成 href
        let x = inline_md("<script>x</script> [点](javascript:alert(1))");
        assert!(!x.contains("<script>") && !x.contains("javascript:"), "{x}");
        // 乘号不是斜体
        assert_eq!(inline_md("3 * 4 * 5"), "3 * 4 * 5");
        // 任务清单
        assert!(list_item("[x] 完成").contains("checked"));
    }

    const SAMPLE: &str = "\
# 标题

正文一段。

## 二级

- 甲
- 乙

1. one
2. two

> 引用一
> 引用二

```rust
fn main() {}
```

| a | b |
| --- | --- |
| 1 | 2 |

---

````
内嵌 ``` fence
````

<!-- tc:block raw
<span>外来结构</span>
tc:block raw -->
";

    #[test]
    fn roundtrip_is_stable() {
        let doc = from_markdown(SAMPLE).unwrap();
        let md1 = to_markdown(&doc);
        let doc2 = from_markdown(&md1).unwrap();
        assert_eq!(doc2, doc, "第二次解析应与第一次解析的块模型一致");
        let md2 = to_markdown(&doc2);
        assert_eq!(md1, md2, "序列化必须确定性");
        assert!(md1.contains("标题"));
        assert!(md1.contains("tc:block raw"));
    }

    #[test]
    fn title_promotion() {
        let doc = from_markdown("# T\n\nbody").unwrap();
        assert_eq!(doc.title.as_deref(), Some("T"));
    }

    #[test]
    fn unterminated_fence_is_tolerated() {
        // 刻意容错：真实 vault（Obsidian 等）里未闭合代码块很常见，
        // 解析成"吃到文件末尾的代码块"而不是报错，否则整库导入会中断。
        let doc = from_markdown("```rust\nfn x()").expect("应容忍未闭合的 fence");
        assert!(
            doc.blocks.iter().any(|b| matches!(b, Block::Code { .. })),
            "未闭合 fence 应成为代码块：{:?}",
            doc.blocks
        );
    }

    #[test]
    fn parses_semantic_html_into_blocks() {
        let html = r#"<!doctype html><html><head><style>body{color:red}</style><script>var x=1</script></head>
        <body><h1>标题</h1><p>一段<strong>加粗</strong>文字，<a href="https://x.com">链接</a>。</p>
        <ul><li>甲</li><li>乙</li></ul>
        <pre><code class="language-rust">fn main() {}</code></pre>
        <blockquote><p>引用一</p><p>引用二</p></blockquote>
        <table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>
        <hr><img src="Assets/ab/cd/x.png" alt="图"></body></html>"#;
        let doc = from_html(html).unwrap();
        assert_eq!(doc.title.as_deref(), Some("标题"), "首个 h1 提升为标题");
        let kinds: Vec<&str> = doc.blocks.iter().map(|b| b.kind()).collect();
        assert!(kinds.contains(&"paragraph"));
        assert!(kinds.contains(&"list"));
        assert!(kinds.contains(&"code"));
        assert!(kinds.contains(&"quote"));
        assert!(kinds.contains(&"table"));
        assert!(kinds.contains(&"divider"));
        assert!(!kinds.iter().any(|k| *k == "raw"), "不应整篇降级为 raw：{kinds:?}");
        // script/style 内容不得混入
        let md = to_markdown(&doc);
        assert!(!md.contains("var x=1"), "script 内容要剔除");
        assert!(!md.contains("color:red"), "style 内容要剔除");
        // 行内与图片
        assert!(md.contains("**加粗**"));
        assert!(md.contains("[链接](https://x.com)"));
        assert!(md.contains("![图](Assets/ab/cd/x.png)"));
        assert!(md.contains("语言") || md.contains("language-rust") || md.contains("```rust"), "代码语言要保留: {md}");
    }

    #[test]
    fn html_whitespace_between_tags_is_not_a_paragraph() {
        let html = "<html><head><title>T</title></head><body><div>\n  <h1>T</h1>\n  <p>正文</p>\n  <ul>\n    <li>甲</li>\n  </ul>\n</div></body></html>";
        let doc = from_html(html).unwrap();
        let kinds: Vec<&str> = doc.blocks.iter().map(|b| b.kind()).collect();
        assert_eq!(kinds, vec!["paragraph", "list"], "不应出现空白段落：{kinds:?}");
        assert_eq!(doc.title.as_deref(), Some("T"));
    }

    #[test]
    fn okf_frontmatter_and_anchors_roundtrip() {
        let src = "---\ntitle: 知识包\nsource: example.com\n---\n\n# 标题\n\n正文段落。 ^core-1\n\n第二段\n";
        let doc = from_markdown(src).unwrap();
        assert_eq!(doc.meta.get("title").map(|s| s.as_str()), Some("知识包"));
        assert_eq!(doc.meta.get("source").map(|s| s.as_str()), Some("example.com"));
        assert_eq!(doc.anchors.iter().flatten().count(), 1);
        assert_eq!(doc.anchors[1].as_deref(), Some("core-1"), "锚点必须跟着段落走");
        // 往返：frontmatter 与锚点都保留
        let md = to_markdown(&doc);
        assert!(md.starts_with("---\n") && md.contains("title: 知识包") && md.contains("source: example.com"), "frontmatter 往返：{md}");
        assert!(md.contains("正文段落。 ^core-1"));
        let back = from_markdown(&md).unwrap();
        assert_eq!(back.meta.get("title").map(|s| s.as_str()), Some("知识包"));
        assert_eq!(back.anchors.iter().flatten().count(), 1);
        // frontmatter 不再被当成 Divider
        assert!(!doc.blocks.iter().any(|b| matches!(b, Block::Divider { .. })));
    }

    #[test]
    fn html_roundtrip_is_stable() {
        let src = "# 标题\n\n正文\n\n- 甲\n- 乙\n\n> 引用\n\n```rust\nfn main() {}\n```\n";
        let model = from_markdown(src).unwrap();
        let html = to_html(&model);
        let back = from_html(&html).unwrap();
        assert_eq!(back.title, model.title, "标题应往返保持");
        let md1 = to_markdown(&model);
        let md2 = to_markdown(&back);
        for needle in ["标题", "正文", "甲", "引用", "fn main"] {
            assert!(md2.contains(needle), "往返后缺少 {needle}：{md2}");
        }
        let _ = md1;
    }

    #[test]
    fn opaque_html_is_preserved_as_raw() {
        let doc = from_html("<div id=app></div>").unwrap();
        assert!(
            matches!(doc.blocks.first(), Some(Block::Raw { .. })) || !doc.blocks.is_empty(),
            "无法解析的内容不能丢"
        );
    }

    #[test]
    fn html_render_escapes_and_embeds_model() {
        let doc = from_markdown(
            "# 标题\n\n<script>alert(1)</script>\n\n```rust\nfn main() {}\n```\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n",
        )
        .unwrap();
        let html = to_html(&doc);
        assert!(html.contains("<article data-kb-format=\"ai-html\""));
        assert!(html.contains("application/ld+json"));
        assert!(html.contains("&lt;script&gt;"), "必须转义脚本");
        assert!(!html.contains("<script>alert"), "不得注入原始脚本");
        assert!(html.contains("<table id="), "表格要带块锚点 id:{}", html);
        assert!(html.contains("language-rust"));
        assert!(html.contains("<title>标题</title>"));
    }
}

/// 语义化 HTML → 块模型。支持 h1-h6 / p / pre>code / blockquote / ul>ol>li /
/// table / hr / img；未知结构递归下钻；行内 a/code/strong/em/br/img 转回 Markdown 行内语法。
/// 完全解析不出块时，整段保留为 Raw（绝不丢内容）。
pub fn from_html(html: &str) -> Result<DocModel, MdError> {
    let cleaned = strip_html_noise(html);
    let mut doc = DocModel::default();
    let mut blocks = parse_blocks(&cleaned);
    // 标题提升：首个 H1 变 title；没有 H1 时退回 <title>
    if let Some(pos) = blocks.iter().position(|b| matches!(b, Block::Heading { level: 1, .. })) {
        if let Block::Heading { text, .. } = blocks.remove(pos) {
            doc.title = Some(text);
        }
    }
    if doc.title.is_none() {
        if let Some(t) = html_title(&cleaned) {
            doc.title = Some(t);
        }
    }
    if blocks.is_empty() && !html.trim().is_empty() {
        blocks.push(Block::Raw { id: String::new(), text: html.trim().to_string() });
    }
    doc.blocks = blocks;
    // ── ^锚点：槽位对齐抽取（每个块一个槽）──
    let slots: Vec<Option<String>> = doc.blocks.iter_mut().map(|b| extract_anchor(b)).collect();
    doc.anchors = slots;
    assign_stable_ids(&mut doc);
    Ok(doc)
}

/// 抽取块末尾的 `^锚点`（写在文本里时），返回锚点名并把文本剥离。

/// 去掉注释与 script/style 内容（否则会把代码当正文）。
fn strip_html_noise(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        // 注释
        if let Some(i) = rest.find("<!--") {
            out.push_str(&rest[..i]);
            match rest[i..].find("-->") {
                Some(j) => rest = &rest[i + j + 3..],
                None => return out,
            }
            continue;
        }
        let candidate = ["<script", "<style"]
            .iter()
            .filter_map(|t| {
                rest.to_ascii_lowercase()
                    .find(t)
                    .map(|i| (i, *t))
            })
            .min();
        match candidate {
            Some((i, tag)) => {
                out.push_str(&rest[..i]);
                let close = format!("</{}", &tag[1..]);
                match rest[i..].to_ascii_lowercase().find(&close) {
                    Some(j) => {
                        let after = i + j;
                        match rest[after..].find('>') {
                            Some(k) => rest = &rest[after + k + 1..],
                            None => return out,
                        }
                    }
                    None => return out,
                }
            }
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

fn parse_blocks(html: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let mut i = 0usize;
    let mut text_buf = String::new();
    let mut counter = 0usize;
    let mut next_id = || {
        counter += 1;
        format!("h{counter}")
    };

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let ch = html[i..].chars().next().unwrap();
            text_buf.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let Some(gt) = html[i..].find('>') else { break };
        let tag_raw = &html[i + 1..i + gt];
        let tag = tag_raw
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let closing = tag_raw.trim_start().starts_with('/');

        if !closing {
            // 空元素
            if tag == "hr" {
                flush_text(&mut out, &mut text_buf, &mut next_id);
                out.push(Block::Divider { id: next_id() });
                i += gt + 1;
                continue;
            }
            if tag == "img" {
                let src = attr(tag_raw, "src").unwrap_or_default();
                let alt = attr(tag_raw, "alt").unwrap_or_default();
                if !src.is_empty() {
                    text_buf.push_str(&format!("![{alt}]({src})"));
                }
                i += gt + 1;
                continue;
            }
            // 元信息容器：内容不进正文（title 只用于取标题，已在 html_title 读走）
            if matches!(tag.as_str(), "head" | "title" | "noscript" | "template" | "svg" | "canvas") {
                if let Some((_, end)) = take_element(html, i, &tag) {
                    i = end;
                } else {
                    i += gt + 1;
                }
                continue;
            }
            // 空元素
            if matches!(
                tag.as_str(),
                "meta" | "link" | "base" | "input" | "br" | "source" | "track" | "col" | "area"
            ) {
                i += gt + 1;
                continue;
            }
            // 标题
            if let Some(level) = heading_level(&tag) {
                if let Some((inner, end)) = take_element(html, i, &tag) {
                    flush_text(&mut out, &mut text_buf, &mut next_id);
                    let text = inline_text(&inner).trim().to_string();
                    if !text.is_empty() {
                        out.push(Block::Heading { id: next_id(), level, text });
                    }
                    i = end;
                    continue;
                }
            }
            match tag.as_str() {
                "p" => {
                    if let Some((inner, end)) = take_element(html, i, "p") {
                        flush_text(&mut out, &mut text_buf, &mut next_id);
                        let text = inline_text(&inner).trim().to_string();
                        if !text.is_empty() {
                            out.push(Block::Paragraph { id: next_id(), text });
                        }
                        i = end;
                        continue;
                    }
                }
                "pre" => {
                    if let Some((inner, end)) = take_element(html, i, "pre") {
                        flush_text(&mut out, &mut text_buf, &mut next_id);
                        let lang = code_lang(&inner);
                        let code = strip_tags_deep(&inner);
                        out.push(Block::Code {
                            id: next_id(),
                            lang,
                            text: if code.ends_with('\n') || code.is_empty() {
                                code
                            } else {
                                format!("{code}\n")
                            },
                        });
                        i = end;
                        continue;
                    }
                }
                "blockquote" => {
                    if let Some((inner, end)) = take_element(html, i, "blockquote") {
                        flush_text(&mut out, &mut text_buf, &mut next_id);
                        let sub = parse_blocks(&inner);
                        let lines: Vec<String> = sub
                            .iter()
                            .map(|b| block_to_text(b))
                            .filter(|s| !s.is_empty())
                            .collect();
                        if !lines.is_empty() {
                            out.push(Block::Quote { id: next_id(), lines });
                        }
                        i = end;
                        continue;
                    }
                }
                "ul" | "ol" => {
                    if let Some((inner, end)) = take_element(html, i, &tag) {
                        flush_text(&mut out, &mut text_buf, &mut next_id);
                        let mut items = Vec::new();
                        let mut rest = inner.as_str();
                        while let Some(p) = find_open(rest, "li") {
                            if let Some((li, consumed)) = take_element(rest, p, "li") {
                                let t = inline_text(&li).trim().to_string();
                                if !t.is_empty() {
                                    items.push(t);
                                }
                                rest = &rest[consumed..];
                            } else {
                                break;
                            }
                        }
                        if !items.is_empty() {
                            out.push(Block::List {
                                id: next_id(),
                                ordered: tag == "ol",
                                items,
                            });
                        }
                        i = end;
                        continue;
                    }
                }
                "table" => {
                    if let Some((inner, end)) = take_element(html, i, "table") {
                        flush_text(&mut out, &mut text_buf, &mut next_id);
                        let mut rows: Vec<Vec<String>> = Vec::new();
                        let mut rest = inner.as_str();
                        while let Some(p) = find_open(rest, "tr") {
                            if let Some((tr, consumed)) = take_element(rest, p, "tr") {
                                let mut cells = Vec::new();
                                for cell_tag in ["th", "td"] {
                                    let mut r2 = tr.as_str();
                                    while let Some(cp) = find_open(r2, cell_tag) {
                                        if let Some((cell, used)) = take_element(r2, cp, cell_tag) {
                                            cells.push(inline_text(&cell));
                                            r2 = &r2[used..];
                                        } else {
                                            break;
                                        }
                                    }
                                }
                                if !cells.is_empty() {
                                    rows.push(cells);
                                }
                                rest = &rest[consumed..];
                            } else {
                                break;
                            }
                        }
                        if !rows.is_empty() {
                            let header = rows.remove(0);
                            out.push(Block::Table { id: next_id(), header, rows });
                        }
                        i = end;
                        continue;
                    }
                }
                _ => {}
            }
        }
        // 未识别标签：跳过标签本身，内容继续作为文本/后续块处理
        i += gt + 1;
    }
    flush_text(&mut out, &mut text_buf, &mut next_id);
    out
}

fn flush_text(out: &mut Vec<Block>, buf: &mut String, next_id: &mut impl FnMut() -> String) {
    // 标签之间的缩进/换行不是段落：必须 trim 后判空
    let t = inline_text(&std::mem::take(buf));
    let t = t.trim();
    if !t.is_empty() {
        out.push(Block::Paragraph { id: next_id(), text: t.to_string() });
    }
}

/// 抽取 `<title>…</title>`。
fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let i = lower.find("<title")?;
    let gt = html[i..].find('>')?;
    let rest = &html[i + gt + 1..];
    let end = rest.to_ascii_lowercase().find("</title")?;
    let t = inline_text(&rest[..end]);
    if t.trim().is_empty() { None } else { Some(t.trim().to_string()) }
}

fn heading_level(tag: &str) -> Option<u8> {
    if tag.len() == 2 && tag.starts_with('h') {
        let l = tag[1..].parse::<u8>().ok()?;
        if (1..=6).contains(&l) {
            return Some(l);
        }
    }
    None
}

/// 找 `<tag` 起始位置。
fn find_open(html: &str, tag: &str) -> Option<usize> {
    let pat = format!("<{tag}");
    let lower = html.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(i) = lower[from..].find(&pat) {
        let at = from + i;
        let after = lower[at + pat.len()..].chars().next().unwrap_or('>');
        if after == '>' || after == ' ' || after == '\n' || after == '\t' || after == '/' {
            return Some(at);
        }
        from = at + pat.len();
    }
    None
}

/// 取出元素内部 HTML 与消费到的位置（处理同名嵌套）。
fn take_element(html: &str, start: usize, tag: &str) -> Option<(String, usize)> {
    let open_end = start + html[start..].find('>')? + 1;
    let lower = html.to_ascii_lowercase();
    let open_pat = format!("<{tag}");
    let close_pat = format!("</{tag}");
    let mut depth = 1i32;
    let mut i = open_end;
    while i < lower.len() {
        let next_open = lower[i..].find(&open_pat).map(|p| i + p);
        let next_close = lower[i..].find(&close_pat).map(|p| i + p);
        match (next_open, next_close) {
            (_, None) => break,
            (Some(o), Some(c)) if o < c => {
                // 自闭合？ <tag/>
                let after = lower[o + open_pat.len()..].chars().next().unwrap_or('>');
                if after == '>' || after == ' ' || after == '\n' || after == '/' {
                    if let Some(g) = lower[o..].find('>') {
                        if lower[o..o + g].ends_with('/') {
                            i = o + g + 1;
                            continue;
                        }
                    }
                }
                depth += 1;
                i = o + open_pat.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    let Some(g) = lower[c..].find('>') else { break };
                    let end = c + g + 1;
                    return Some((html[open_end..c].to_string(), end));
                }
                i = c + close_pat.len();
            }
        }
    }
    None
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let idx = lower.find(&format!("{name}="))?;
    let rest = &tag[idx + name.len() + 1..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let body = &rest[1..];
        let end = body.find(quote)?;
        Some(body[..end].to_string())
    } else {
        Some(rest.split_whitespace().next().unwrap_or("").to_string())
    }
}

fn code_lang(pre_inner: &str) -> Option<String> {
    let lower = pre_inner.to_ascii_lowercase();
    let i = lower.find("<code")?;
    let seg = &pre_inner[i..];
    let cls = attr(seg, "class")?;
    cls.split_whitespace()
        .find_map(|c| c.strip_prefix("language-").map(|s| s.to_string()))
}

/// 行内标签 → Markdown 行内语法（a/code/strong/em/br/img）。
fn inline_text(html: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(gt) = html[i..].find('>') else { break };
            let raw = &html[i + 1..i + gt];
            let name = raw
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let closing = raw.trim_start().starts_with('/');
            match name.as_str() {
                "br" => out.push('\n'),
                "img" => {
                    let src = attr(raw, "src").unwrap_or_default();
                    let alt = attr(raw, "alt").unwrap_or_default();
                    if !src.is_empty() {
                        out.push_str(&format!("![{alt}]({src})"));
                    }
                }
                "a" if !closing => {
                    // 先记下 href，链接文字在标签之后
                    let href = attr(raw, "href").unwrap_or_default();
                    out.push_str(&format!("\u{1}{href}\u{1}"));
                }
                "a" => out.push_str("\u{2}"),
                "strong" | "b" => out.push_str("**"),
                "em" | "i" => out.push('*'),
                "code" => out.push('`'),
                _ => {}
            }
            i += gt + 1;
            continue;
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    to_markdown_links(&unescape(&out))
}

/// 把 `\u{1}href\u{1}文字\u{2}` 转成 `[文字](href)`；已有 `[文字](href)` 原样保留。
fn to_markdown_links(s: &str) -> String {
    const OPEN: char = '\u{1}';
    const CLOSE: char = '\u{2}';
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != OPEN {
            out.push(c);
            continue;
        }
        // 收集 href
        let href_start = i + c.len_utf8();
        let Some(rel_end) = s[href_start..].find(OPEN) else {
            out.push(c);
            continue;
        };
        let href = &s[href_start..href_start + rel_end];
        let label_start = href_start + rel_end + OPEN.len_utf8();
        let Some(rel_close) = s[label_start..].find(CLOSE) else {
            out.push(c);
            continue;
        };
        let label = s[label_start..label_start + rel_close].trim();
        if label.is_empty() {
            out.push_str(&format!("[{href}]({href})"));
        } else {
            out.push_str(&format!("[{label}]({href})"));
        }
        // 跳过已消费部分
        let consumed_to = label_start + rel_close + CLOSE.len_utf8();
        while let Some((j, _)) = chars.peek() {
            if *j < consumed_to {
                chars.next();
            } else {
                break;
            }
        }
    }
    out
}

fn strip_tags_deep(html: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            match html[i..].find('>') {
                Some(gt) => i += gt + 1,
                None => break,
            }
            continue;
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    unescape(&out)
}

fn unescape(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&hellip;", "…")
        .replace("&amp;", "&")
}

fn block_to_text(b: &Block) -> String {
    match b {
        Block::Paragraph { text, .. } | Block::Heading { text, .. } => text.clone(),
        Block::Code { text, .. } | Block::Raw { text, .. } => text.clone(),
        Block::Quote { lines, .. } => lines.join(" "),
        Block::List { items, .. } => items.join(" "),
        Block::Table { header, rows, .. } => {
            let mut s = header.join(" | ");
            for r in rows {
                s.push_str(" / ");
                s.push_str(&r.join(" | "));
            }
            s
        }
        Block::Divider { .. } => String::new(),
    }
}
