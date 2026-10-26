//! kernel-a2ui: Google A2UI（Agent-to-User Interface）协议渲染器。
//!
//! 规范：a2ui.org，v0.8（legacy，当前 v0.9.1）。要点：
//! - 传输：JSONL（每行一条消息），可套 SSE
//! - 服务端→客户端：`surfaceUpdate` / `dataModelUpdate` / `beginRendering` / `deleteSurface`
//! - 组件：扁平邻接表 `[{id, component:{"<Type>":{...}}}]`，父子用字符串 ID 引用（LLM 易生成）
//! - 数据绑定：`BoundValue = {literalString|literalNumber|literalBoolean|literalArray|path}`
//! - 目录（Catalog）由客户端定义：协议只规定抽象组件，渲染器映射到原生实现
//!
//! 本实现把 A2UI 映射到 Litmus 的 HTML 契约（与 AI-HTML 同一套 token），
//! 未知组件降级为带标签的占位而不是报错——协议要求渐进渲染。

use serde_json::Value;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum A2uiError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no surface: beginRendering 未到达或 root 不存在")]
    NoSurface,
}

pub const STANDARD_CATALOG_V08: &str =
    "https://a2ui.org/specification/v0_8/standard_catalog_definition.json";

/// 一个 surface 的构建状态。
#[derive(Debug, Default, Clone)]
pub struct Surface {
    pub components: BTreeMap<String, Value>,
    pub data: Value,
    pub root: Option<String>,
    pub catalog_id: Option<String>,
    pub ready: bool,
}

/// 消费 A2UI 消息流，构建 surface。
#[derive(Debug, Default)]
pub struct A2uiStream {
    pub surfaces: BTreeMap<String, Surface>,
}

impl A2uiStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一行 JSONL。
    pub fn push_line(&mut self, line: &str) -> Result<(), A2uiError> {
        let t = line.trim();
        if t.is_empty() {
            return Ok(());
        }
        let msg: Value = serde_json::from_str(t)?;
        self.push(&msg);
        Ok(())
    }

    /// 喂入一条消息。
    pub fn push(&mut self, msg: &Value) {
        if let Some(su) = msg.get("surfaceUpdate") {
            let id = su
                .get("surfaceId")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            let s = self.surfaces.entry(id).or_default();
            if let Some(arr) = su.get("components").and_then(|c| c.as_array()) {
                for c in arr {
                    if let Some(cid) = c.get("id").and_then(|v| v.as_str()) {
                        s.components.insert(cid.to_string(), c.clone());
                    }
                }
            }
        } else if let Some(dm) = msg.get("dataModelUpdate") {
            let id = dm
                .get("surfaceId")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            let s = self.surfaces.entry(id).or_default();
            let path = dm.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let contents = dm.get("contents").cloned().unwrap_or(Value::Null);
            set_path(&mut s.data, path, contents_to_json(&contents));
        } else if let Some(br) = msg.get("beginRendering") {
            let id = br
                .get("surfaceId")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            let root = br.get("root").and_then(|v| v.as_str()).map(|s| s.to_string());
            let catalog = br.get("catalogId").and_then(|v| v.as_str()).map(|s| s.to_string());
            let s = self.surfaces.entry(id).or_default();
            s.root = root;
            s.catalog_id = catalog;
            s.ready = true;
        } else if let Some(ds) = msg.get("deleteSurface") {
            if let Some(id) = ds.get("surfaceId").and_then(|v| v.as_str()) {
                self.surfaces.remove(id);
            }
        }
    }

    /// 渲染全部就绪 surface 为 HTML 片段（不含 <html> 外壳）。
    pub fn render(&self) -> Result<String, A2uiError> {
        let mut out = String::new();
        let mut any = false;
        for (id, s) in &self.surfaces {
            if !s.ready {
                continue;
            }
            let Some(root) = &s.root else { continue };
            any = true;
            out.push_str(&format!(
                "<section class=\"a2ui-surface\" data-surface=\"{}\" data-catalog=\"{}\">\n",
                esc(id),
                esc(s.catalog_id.as_deref().unwrap_or(STANDARD_CATALOG_V08))
            ));
            out.push_str(&self.render_component(s, root, 0));
            out.push_str("</section>\n");
        }
        if !any {
            return Err(A2uiError::NoSurface);
        }
        Ok(out)
    }

    /// 渲染为自包含单页（可直接放进沙箱 iframe / 发布）。
    pub fn render_document(&self, title: &str) -> Result<String, A2uiError> {
        let body = self.render()?;
        Ok(format!(
            "<!doctype html>\n<html lang=\"zh\"><head><meta charset=\"utf-8\">\n\
<title>{}</title>\n<style>{}</style>\n</head><body class=\"a2ui\">\n{}</body></html>\n",
            esc(title),
            A2UI_CSS,
            body
        ))
    }

    fn render_component(&self, s: &Surface, id: &str, depth: usize) -> String {
        if depth > 40 {
            return String::new(); // 防御环
        }
        let Some(entry) = s.components.get(id) else {
            return format!("<div class=\"a2ui-missing\">missing:{}</div>", esc(id));
        };
        let Some(obj) = entry.get("component").and_then(|c| c.as_object()) else {
            return String::new();
        };
        let Some((ty, props)) = obj.iter().next() else {
            return String::new();
        };
        let p = |k: &str| props.get(k);
        let text = |k: &str| p(k).map(|v| self.resolve(s, v)).unwrap_or_default();
        let child = |k: &str| {
            p(k)
                .and_then(|v| v.as_str())
                .map(|c| self.render_component(s, c, depth + 1))
                .unwrap_or_default()
        };
        let children = |k: &str| {
            // children: {explicitList:[...]} 或 {template:{dataBinding,componentId}}
            let mut out = String::new();
            if let Some(ch) = p(k) {
                if let Some(list) = ch.get("explicitList").and_then(|v| v.as_array()) {
                    for c in list {
                        if let Some(cid) = c.as_str() {
                            out.push_str(&self.render_component(s, cid, depth + 1));
                        }
                    }
                } else if let Some(tpl) = ch.get("template") {
                    let path = tpl.get("dataBinding").and_then(|v| v.as_str()).unwrap_or("");
                    let cid = tpl.get("componentId").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(items) = s.data.pointer(path).and_then(|v| v.as_array()) {
                        for (i, item) in items.iter().enumerate() {
                            let mut sub = s.clone();
                            sub.data = item.clone();
                            let _ = i;
                            out.push_str(&self.render_component(&sub, cid, depth + 1));
                        }
                    }
                }
            }
            out
        };

        match ty.as_str() {
            "Column" => format!("<div class=\"a2ui-col\">{}</div>", children("children")),
            "Row" => format!("<div class=\"a2ui-row\">{}</div>", children("children")),
            "List" => format!("<div class=\"a2ui-list\">{}</div>", children("children")),
            "Card" => format!("<div class=\"a2ui-card\">{}</div>", child("child")),
            "Text" => {
                let t = esc(&text("text"));
                match p("usageHint").and_then(|v| v.as_str()).unwrap_or("body") {
                    "h1" => format!("<h1>{t}</h1>"),
                    "h2" => format!("<h2>{t}</h2>"),
                    "h3" => format!("<h3>{t}</h3>"),
                    "h4" => format!("<h4>{t}</h4>"),
                    "caption" => format!("<p class=\"a2ui-caption\">{t}</p>"),
                    _ => format!("<p>{t}</p>"),
                }
            }
            "Button" => {
                let label = child("child");
                let action = p("action")
                    .and_then(|a| a.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!(
                    "<button class=\"a2ui-btn\" data-action=\"{}\" data-component=\"{}\">{}</button>",
                    esc(action),
                    esc(id),
                    label
                )
            }
            // Link：库内回链（AI 引用来源卡）。宿主点击 data-doc 打开文档、
            // data-anchor（^anchor 名或 b{index}）滚动到块——协议级反幻觉的出口。
            "Link" => {
                let label = child("child");
                let doc = p("doc").map(|v| self.resolve(s, v)).unwrap_or_default();
                let anchor = p("anchor").map(|v| self.resolve(s, v)).unwrap_or_default();
                format!(
                    "<a class=\"a2ui-link\" href=\"#\" data-doc=\"{}\" data-anchor=\"{}\">{}</a>",
                    esc(&doc),
                    esc(&anchor),
                    label
                )
            }
            "TextField" => {
                let label = text("label");
                let value = text("text");
                format!(
                    "<label class=\"a2ui-field\"><span>{label}</span><input data-component=\"{}\" value=\"{}\"></label>",
                    esc(id),
                    esc(&value)
                )
            }
            "Image" => {
                let url = p("url").map(|v| self.resolve(s, v)).unwrap_or_default();
                format!("<img class=\"a2ui-img\" src=\"{}\" alt=\"\">", esc(&url))
            }
            "Divider" => "<hr class=\"a2ui-hr\">".to_string(),
            "Icon" => {
                let n = text("name");
                format!("<span class=\"a2ui-icon\">{}</span>", esc(&n))
            }
            other => format!(
                "<div class=\"a2ui-unknown\" data-type=\"{}\">{}</div>",
                esc(other),
                children("children")
            ),
        }
    }

    /// 解析 BoundValue：path 优先取数据模型，否则用 literal*。
    fn resolve(&self, s: &Surface, v: &Value) -> String {
        if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
            if let Some(found) = s.data.pointer(path) {
                return json_to_string(found);
            }
        }
        for key in ["literalString", "literalNumber", "literalBoolean"] {
            if let Some(lit) = v.get(key) {
                return json_to_string(lit);
            }
        }
        if let Some(arr) = v.get("literalArray").and_then(|a| a.as_array()) {
            return arr.iter().map(json_to_string).collect::<Vec<_>>().join(", ");
        }
        String::new()
    }
}

/// A2UI 消息流 → 完整 HTML 文档（给发布产物 / MCP resource / 沙箱预览用）。
pub fn render_jsonl(jsonl: &str, title: &str) -> Result<String, A2uiError> {
    let mut s = A2uiStream::new();
    for line in jsonl.lines() {
        s.push_line(line)?;
    }
    s.render_document(title)
}

/// A2UI 消息流 → HTML 片段（无外壳，供宿主页面 Shadow DOM 原生承载——A2UI-3 去 iframe）。
/// 样式由调用方随片段一并注入（`A2UI_CSS`）。
pub fn render_jsonl_fragment(jsonl: &str) -> Result<String, A2uiError> {
    let mut s = A2uiStream::new();
    for line in jsonl.lines() {
        s.push_line(line)?;
    }
    s.render()
}

/// contents 是邻接表：[{key, valueString|valueNumber|valueBoolean|valueMap}]。
/// 键全为连续数字时归一成数组——A2UI 用数字键表达列表，template.dataBinding 需要数组。
fn contents_to_json(contents: &Value) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(arr) = contents.as_array() {
        for e in arr {
            let Some(k) = e.get("key").and_then(|v| v.as_str()) else {
                continue;
            };
            for (vk, vv) in e.as_object().into_iter().flatten() {
                if vk == "key" {
                    continue;
                }
                let val = if vk == "valueMap" {
                    contents_to_json(vv)
                } else {
                    vv.clone()
                };
                map.insert(k.to_string(), val);
            }
        }
    }
    let all_numeric = !map.is_empty()
        && map.keys().all(|k| k.parse::<usize>().is_ok())
        && map.keys().map(|k| k.parse::<usize>().unwrap()).max().unwrap() == map.len() - 1;
    if all_numeric {
        let mut idx: Vec<usize> = map.keys().map(|k| k.parse().unwrap()).collect();
        idx.sort_unstable();
        return Value::Array(
            idx.into_iter()
                .map(|i| map.get(&i.to_string()).cloned().unwrap_or(Value::Null))
                .collect(),
        );
    }
    Value::Object(map)
}

fn set_path(root: &mut Value, path: &str, value: Value) {
    let path = path.trim_matches('/');
    if path.is_empty() {
        *root = value;
        return;
    }
    let parts: Vec<&str> = path.split('/').collect();
    if !root.is_object() {
        *root = Value::Object(serde_json::Map::new());
    }
    let mut cur = root;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            if let Some(obj) = cur.as_object_mut() {
                obj.insert(part.to_string(), value);
            }
            return;
        }
        let obj = cur.as_object_mut().unwrap();
        cur = obj
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
}

fn json_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 与 Litmus token 契约一致的 A2UI 渲染样式（零 hex）。
pub const A2UI_CSS: &str = r#"
.a2ui{font-family:var(--font-body,system-ui);color:var(--fg,oklch(24% 0 0));margin:0;padding:20px}
.a2ui-surface{display:block;margin:0 0 18px}
.a2ui-col{display:flex;flex-direction:column;gap:10px}
.a2ui-row{display:flex;flex-direction:row;gap:10px;align-items:center;flex-wrap:wrap}
.a2ui-list{display:flex;flex-direction:column;gap:8px}
.a2ui-card{background:var(--surface,oklch(100% 0 0/.86));border:1px solid var(--border,oklch(88% .012 82));
  border-radius:var(--r-md,18px);padding:16px;box-shadow:var(--shadow-sm,0 10px 26px -14px oklch(38% .05 80/.22))}
.a2ui-caption{color:var(--muted,oklch(51% 0 0));font-size:13px}
.a2ui-link{display:inline-block;border:1px solid var(--border,oklch(88% .012 82));background:var(--surface,oklch(100% 0 0/.86));color:var(--fg,oklch(24% 0 0));border-radius:10px;padding:6px 12px;font:500 12.5px var(--font-body,system-ui);cursor:pointer;text-decoration:none}
.a2ui-link:hover{border-color:var(--accent,oklch(54% .17 258));color:var(--accent,oklch(54% .17 258))}
.a2ui-btn{border:1px solid var(--border,oklch(88% .012 82));background:var(--accent,oklch(54% .17 258));
  color:var(--on-accent,oklch(100% 0 0));border-radius:12px;padding:8px 14px;font:600 13px var(--font-body,system-ui);cursor:pointer}
.a2ui-btn:hover{filter:brightness(1.06)}
.a2ui-field{display:flex;flex-direction:column;gap:5px;font-size:13px}
.a2ui-field input{padding:8px 10px;border-radius:10px;border:1px solid var(--border,oklch(88% .012 82));background:transparent;color:inherit}
.a2ui-img{max-width:100%;border-radius:12px;display:block}
.a2ui-hr{border:none;border-top:1px solid var(--border,oklch(88% .012 82));margin:8px 0}
.a2ui-icon{font-size:18px}
.a2ui-unknown{border:1px dashed var(--border-strong,oklch(79% .018 80));border-radius:12px;padding:10px;color:var(--muted,oklch(51% 0 0));font:12px var(--font-mono,monospace)}
.a2ui-missing{color:var(--danger,oklch(55% .2 25));font:12px var(--font-mono,monospace)}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE_JSONL: &str = r#"{"surfaceUpdate":{"components":[{"id":"root","component":{"Column":{"children":{"explicitList":["profile_card"]}}}}]}}
{"surfaceUpdate":{"components":[{"id":"profile_card","component":{"Card":{"child":"card_content"}}}]}}
{"surfaceUpdate":{"components":[{"id":"card_content","component":{"Column":{"children":{"explicitList":["header_row","bio_text"]}}}}]}}
{"surfaceUpdate":{"components":[{"id":"header_row","component":{"Row":{"children":{"explicitList":["name_text"]}}}}]}}
{"surfaceUpdate":{"components":[{"id":"name_text","component":{"Text":{"usageHint":"h3","text":{"literalString":"A2A Fan"}}}}]}}
{"surfaceUpdate":{"components":[{"id":"bio_text","component":{"Text":{"text":{"path":"/user/bio"}}}}]}}
{"dataModelUpdate":{"path":"user","contents":[{"key":"bio","valueString":"Building beautiful apps."}]}}
{"beginRendering":{"root":"root"}}"#;

    #[test]
    fn renders_adjacency_list_with_data_binding() {
        let mut s = A2uiStream::new();
        for line in PROFILE_JSONL.lines() {
            s.push_line(line).unwrap();
        }
        let html = s.render().unwrap();
        assert!(html.contains("a2ui-surface"));
        assert!(html.contains("<h3>A2A Fan</h3>"));
        assert!(html.contains("Building beautiful apps."), "数据绑定要解析路径");
        assert!(html.contains("a2ui-card"));
    }

    #[test]
    fn streams_progressively_before_rendering() {
        let mut s = A2uiStream::new();
        s.push_line(r#"{"surfaceUpdate":{"components":[{"id":"root","component":{"Text":{"text":{"literalString":"hi"}}}}]}}"#)
            .unwrap();
        // 未收到 beginRendering 时不应渲染（避免半截内容闪现）
        assert!(s.render().is_err());
        s.push_line(r#"{"beginRendering":{"root":"root"}}"#).unwrap();
        assert!(s.render().unwrap().contains("hi"));
    }

    #[test]
    fn template_list_renders_all_items() {
        let jsonl = r#"{"surfaceUpdate":{"components":[{"id":"root","component":{"List":{"children":{"template":{"dataBinding":"/items","componentId":"row"}}}}}]}}
{"surfaceUpdate":{"components":[{"id":"row","component":{"Text":{"text":{"path":"/name"}}}}]}}
{"dataModelUpdate":{"path":"items","contents":[{"key":"0","valueMap":[{"key":"name","valueString":"甲"}]},{"key":"1","valueMap":[{"key":"name","valueString":"乙"}]}]}}
{"beginRendering":{"root":"root"}}"#;
        let mut s = A2uiStream::new();
        for l in jsonl.lines() {
            s.push_line(l).unwrap();
        }
        let html = s.render().unwrap();
        assert!(html.contains("甲") && html.contains("乙"), "template 列表要展开：{html}");
    }

    #[test]
    fn unknown_component_degrades_gracefully_and_escapes() {
        let jsonl = r#"{"surfaceUpdate":{"components":[{"id":"root","component":{"SomeFutureWidget":{"children":{"explicitList":["t"]}}}}]}}
{"surfaceUpdate":{"components":[{"id":"t","component":{"Text":{"text":{"literalString":"<script>alert(1)</script>"}}}}]}}
{"beginRendering":{"root":"root"}}"#;
        let mut s = A2uiStream::new();
        for l in jsonl.lines() {
            s.push_line(l).unwrap();
        }
        let html = s.render().unwrap();
        assert!(html.contains("a2ui-unknown"), "未知组件要降级占位");
        assert!(html.contains("&lt;script&gt;"), "文本必须转义");
        assert!(!html.contains("<script>alert"));
    }

    #[test]
    fn full_document_has_styles_and_catalog() {
        let doc = render_jsonl(PROFILE_JSONL, "个人卡片").unwrap();
        assert!(doc.starts_with("<!doctype html>"));
        assert!(doc.contains("data-catalog=\"https://a2ui.org/specification/v0_8/"));
        assert!(doc.contains(".a2ui-card"));
        assert!(doc.contains("<title>个人卡片</title>"));
    }

    #[test]
    fn link_component_emits_doc_backlink() {
        let jsonl = r#"{"surfaceUpdate":{"components":[{"id":"root","component":{"Column":{"children":{"explicitList":["t","lnk"]}}}}]}}
{"surfaceUpdate":{"components":[{"id":"t","component":{"Text":{"usageHint":"h4","text":{"literalString":"来源 2 条"}}}}]}}
{"surfaceUpdate":{"components":[{"id":"lnk","component":{"Link":{"doc":{"literalString":"Notes/笔记.md"},"anchor":{"literalString":"b3"},"child":"lbl"}}}]}}
{"surfaceUpdate":{"components":[{"id":"lbl","component":{"Text":{"text":{"literalString":"打开原文"}}}}]}}
{"beginRendering":{"root":"root"}}"#;
        let mut st = A2uiStream::new();
        for l in jsonl.lines() { st.push_line(l).unwrap(); }
        let html = st.render().unwrap();
        assert!(html.contains("data-doc=\"Notes/笔记.md\""), "回链要带 data-doc：{html}");
        assert!(html.contains("data-anchor=\"b3\""));
        assert!(html.contains("打开原文"));
        assert!(!html.contains("href=\"Notes/"), "Link 不直接导航,由宿主接管");
    }

    #[test]
    fn fragment_has_no_document_shell() {
        let frag = render_jsonl_fragment(PROFILE_JSONL).unwrap();
        assert!(!frag.contains("<!doctype"), "片段不能带文档外壳");
        assert!(!frag.contains("<style>"), "样式由宿主注入，不在片段里");
        assert!(frag.contains("a2ui-surface"));
        assert!(frag.contains("Building beautiful apps."));
    }

    #[test]
    fn delete_surface_removes_it() {
        let jsonl = r#"{"surfaceUpdate":{"surfaceId":"a","components":[{"id":"root","component":{"Text":{"text":{"literalString":"gone"}}}}]}}
{"beginRendering":{"surfaceId":"a","root":"root"}}
{"deleteSurface":{"surfaceId":"a"}}"#;
        let mut s = A2uiStream::new();
        for l in jsonl.lines() {
            s.push_line(l).unwrap();
        }
        assert!(s.render().is_err(), "surface 删除后不应再渲染");
    }
}
