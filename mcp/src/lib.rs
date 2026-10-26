//! thirdc-mcp: 把知识库作为 MCP server 暴露给任意 agent（出口侧）。
//!
//! 协议：JSON-RPC 2.0，传输与协议解耦——`handle()` 是纯函数式的消息处理，
//! stdio（本地 agent）与后续 HTTP/SSE（远程 agent）复用同一实现。
//!
//! 工具：search_vault / list_docs / read_doc / write_doc / delete_doc / put_asset / vault_status
//! 资源：knowledge://Notes/xxx.md（markdown + AI-HTML 双表示）

use base64::Engine;
use kernel_core::{Kernel, Vault, is_safe_doc_path, to_html, to_markdown};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub mod client;

/// 本端支持的最新协议版本；客户端请求的版本在 SUPPORTED 内则回显。
pub const LATEST_PROTOCOL: &str = "2025-06-18";
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

pub struct McpServer {
    pub kernel: Arc<Mutex<Kernel>>,
}

impl McpServer {
    pub fn new(kernel: Arc<Mutex<Kernel>>) -> Self {
        Self { kernel }
    }

    /// 从库路径构建。
    pub fn open(vault: Vault) -> anyhow::Result<Self> {
        Ok(Self::new(Arc::new(Mutex::new(Kernel::open(vault)?))))
    }

    /// 供 agent / HTTP 层复用的工具调用入口。
    pub fn call_tool_public(&self, name: &str, args: Value) -> Result<Value, String> {
        self.tools_call(&serde_json::json!({ "name": name, "arguments": args }))
            .map_err(|(_, m)| m)
    }

    /// 工具定义转换为 OpenAI function calling 格式（含已启用插件的 contributions.tools）。
    pub fn tools_for_llm(&self) -> Value {
        let defs = tool_definitions();
        let mut arr: Vec<Value> = defs
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t["name"],
                        "description": t["description"],
                        "parameters": t["inputSchema"],
                    }
                })
            })
            .collect();
        arr.extend(self.plugin_tools_for_llm());
        Value::Array(arr)
    }

    /// 处理一条 JSON-RPC 消息。通知返回 None（无需响应）。
    pub fn handle(&self, raw: &str) -> Option<String> {        let msg: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_response(Value::Null, -32700, &format!("parse error: {e}")));
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // 通知（无 id）：不回响应
        if id.is_none() {
            return None;
        }
        let id = id.unwrap();

        let result = match method {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.tools_list()),
            "tools/call" => self.tools_call(&params),
            "resources/list" => self.resources_list(),
            "resources/read" => self.resources_read(&params),
            _ => Err((-32601, format!("method not found: {method}"))),
        };

        Some(match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }).to_string(),
            Err((code, m)) => error_response(id, code, &m),
        })
    }

    fn initialize(&self, params: &Value) -> Result<Value, (i32, String)> {
        let requested = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(LATEST_PROTOCOL);
        let version = if SUPPORTED_PROTOCOLS.contains(&requested) {
            requested
        } else {
            LATEST_PROTOCOL
        };
        Ok(json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": false, "listChanged": false }
            },
            "serverInfo": { "name": "thirdc", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "ThirdC 知识库。文档真相在 Notes/ 下的 Markdown；资源以 knowledge://Notes/x.md 暴露，同时提供 text/markdown 与 AI-HTML(text/html) 表示。写入前建议先 search_vault 查看现有内容。"
        }))
    }

    fn tools_list(&self) -> Value {
        json!({ "tools": tool_definitions() })
    }

    fn tools_call(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        if name.starts_with("plugin_") {
            return self.call_plugin_tool(name, args).map_err(|e| (-32602, e));
        }
        match name {
            "search_vault" => self.t_search_vault(args),
            "search_semantic" => self.t_search_semantic(args),
            "save_memory" => self.t_save_memory(args),
            "recall_memory" => self.t_recall_memory(args),
            "list_docs" => self.t_list_docs(args),
            "read_doc" => self.t_read_doc(args),
            "write_doc" => self.t_write_doc(args),
            "delete_doc" => self.t_delete_doc(args),
            "put_asset" => self.t_put_asset(args),
            "vault_status" => self.t_status(args),
            "import_design" => self.t_import_design(args),
            "list_designs" => self.t_list_designs(args),
            "set_design" => self.t_set_design(args),
            "import_page_design" => self.t_import_page_design(args),
            _ => Err((-32602, format!("unknown tool: {name}"))),
        }
    }

    fn with_kernel<T>(&self, f: impl FnOnce(&mut Kernel) -> anyhow::Result<T>) -> Result<T, (i32, String)> {
        let mut k = self
            .kernel
            .lock()
            .map_err(|_| (-32603, "kernel lock poisoned".to_string()))?;
        f(&mut k).map_err(|e| (-32603, e.to_string()))
    }

    fn t_search_vault(&self, args: Value) -> Result<Value, (i32, String)> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'query'".to_string()))?
            .to_string();
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;
        self.with_kernel(|k| {
            k.sync_throttled()?;
            let hits = k.search(&query)?;
            let arr: Vec<Value> = hits
                .into_iter()
                .take(limit)
                .map(|(path, rank)| json!({ "path": path, "rank": rank }))
                .collect();
            // 文本里直接列出路径：模型可引用，用户可见可跳转
            let lines: String = arr
                .iter()
                .enumerate()
                .map(|(i, h)| format!("{}. {}", i + 1, h["path"].as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(tool_result(
                format!("{} hit(s) for {:?}\n{}", arr.len(), query, lines),
                json!({ "query": query, "hits": arr }),
            ))
        })
    }

    fn t_search_semantic(&self, args: Value) -> Result<Value, (i32, String)> {
        let query = args.get("query").and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'query'".to_string()))?.to_string();
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        self.with_kernel(|k| {
            k.sync_throttled()?;
            let merged = k.search_hybrid(&query, limit)?;
            let arr: Vec<Value> = merged.iter()
                .map(|(p, s)| json!({"path": p, "score": s})).collect();
            let lines: String = arr.iter().enumerate()
                .map(|(i, h)| format!("{}. {}", i + 1, h["path"].as_str().unwrap_or("")))
                .collect::<Vec<_>>().join("\n");
            Ok(tool_result(
                format!("混合检索 {} hit(s)\n{}", merged.len(), lines),
                json!({"query": query, "hits": arr}),
            ))
        })
    }

    /// 写入长期记忆（跨会话保留，用户可编辑）
    fn t_save_memory(&self, args: Value) -> Result<Value, (i32, String)> {
        let text = args.get("text").and_then(|t| t.as_str())
            .ok_or((-32602, "missing 'text'".to_string()))?.to_string();
        let kind = args.get("kind").and_then(|k| k.as_str()).unwrap_or("事实").to_string();
        self.with_kernel(|k| {
            let rel = k.save_memory(&kind, &text)?;
            Ok(tool_result(
                format!("已记住 [{kind}] {text}"),
                json!({ "saved": rel, "kind": kind, "text": text }),
            ))
        })
    }

    /// 回忆：在历史对话与记忆中检索
    fn t_recall_memory(&self, args: Value) -> Result<Value, (i32, String)> {
        let query = args.get("query").and_then(|q| q.as_str())
            .ok_or((-32602, "missing 'query'".to_string()))?.to_string();
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(5) as usize;
        self.with_kernel(|k| {
            let hits = k.recall(&query, limit)?;
            let arr: Vec<Value> = hits.iter()
                .map(|(p, s, sn)| json!({"path": p, "score": s, "snippet": sn}))
                .collect();
            Ok(tool_result(
                format!("回忆到 {} 条", arr.len()),
                json!({ "query": query, "hits": arr }),
            ))
        })
    }

    fn t_list_docs(&self, _args: Value) -> Result<Value, (i32, String)> {
        self.with_kernel(|k| {
            k.sync_throttled()?;
            let docs = kernel_core::list_docs(&k.vault)?;
            let arr: Vec<Value> = docs
                .iter()
                .filter_map(|p| p.to_str())
                .map(|p| {
                    let title = k
                        .get_doc(p)
                        .ok()
                        .and_then(|m| m.title)
                        .unwrap_or_default();
                    json!({ "path": p, "title": title })
                })
                .collect();
            Ok(tool_result(
                format!("{} document(s)", arr.len()),
                json!({ "docs": arr }),
            ))
        })
    }

    fn t_read_doc(&self, args: Value) -> Result<Value, (i32, String)> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'path'".to_string()))?
            .to_string();
        if !is_safe_doc_path(&path) {
            return Err((-32602, "path must be under Notes/".into()));
        }
        self.with_kernel(|k| {
            k.sync_throttled()?;
            let model = k.get_doc(&path)?;
            let md = to_markdown(&model);
            let source = std::fs::read_to_string(k.vault.root.join(&path)).unwrap_or_default();
            let format = if kernel_core::is_html_rel(&path) { "html" } else { "markdown" };
            Ok(tool_result(
                md.clone(),
                json!({
                    "path": path,
                    "title": model.title,
                    "format": format,
                    "source": source,
                    "markdown": md,
                    "html": to_html(&model),
                    "blocks": model.blocks,
                }),
            ))
        })
    }

    fn t_write_doc(&self, args: Value) -> Result<Value, (i32, String)> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'path'".to_string()))?
            .to_string();
        // 两种内容：markdown（默认）或 html（HTML 一等文档，路径需 .html/.htm）
        let content = args
            .get("markdown")
            .or_else(|| args.get("html"))
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'markdown' or 'html'".to_string()))?
            .to_string();
        if args.get("html").is_some() && !kernel_core::is_html_rel(&path) {
            return Err((-32602, "html 内容需要 Notes/xxx.html 路径".into()));
        }
        if !is_safe_doc_path(&path) {
            return Err((-32602, "path must be under Notes/".into()));
        }
        self.with_kernel(|k| {
            k.put_doc(&path, &content)?;
            let format = if kernel_core::is_html_rel(&path) { "html" } else { "markdown" };
            Ok(tool_result(
                format!("written: {path} ({format})"),
                json!({ "path": path, "format": format, "written": true }),
            ))
        })
    }

    fn t_delete_doc(&self, args: Value) -> Result<Value, (i32, String)> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'path'".to_string()))?
            .to_string();
        if !is_safe_doc_path(&path) {
            return Err((-32602, "path must be under Notes/".into()));
        }
        self.with_kernel(|k| {
            let abs = k.vault.root.join(&path);
            std::fs::remove_file(&abs)?;
            k.sync_all()?;
            Ok(tool_result(
                format!("deleted: {path}"),
                json!({ "path": path, "deleted": true }),
            ))
        })
    }

    fn t_put_asset(&self, args: Value) -> Result<Value, (i32, String)> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'name'".to_string()))?
            .to_string();
        let b64 = args
            .get("content_base64")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'content_base64'".to_string()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| (-32602, format!("invalid base64: {e}")))?;
        self.with_kernel(|k| {
            let meta = k.put_asset(&name, &bytes)?;
            let md = Kernel::asset_markdown(&meta, &name);
            Ok(tool_result(
                md.clone(),
                json!({ "hash": meta.hash, "path": meta.rel, "mime": meta.mime, "markdown": md }),
            ))
        })
    }

    fn t_status(&self, _args: Value) -> Result<Value, (i32, String)> {        self.with_kernel(|k| {
            k.sync_all()?;
            let docs = kernel_core::list_docs(&k.vault)?.len();
            let indexed = k.indexed_count().unwrap_or(0);
            let assets = k.assets_count().unwrap_or(0);
            Ok(tool_result(
                format!("{} docs, {} indexed, {} assets", docs, indexed, assets),
                json!({
                    "vault": k.vault.config.name,
                    "docs": docs, "indexed": indexed, "assets": assets
                }),
            ))
        })
    }

    fn t_import_design(&self, args: Value) -> Result<Value, (i32, String)> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'name'".to_string()))?
            .to_string();
        self.with_kernel(|k| {
            let profile = if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                k.import_design_path(std::path::Path::new(path), Some(&name))?
            } else {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("需要 content 或 path"))?;
                k.import_design_text(&name, content)?
            };
            Ok(tool_result(
                profile.summary(),
                json!({
                    "name": profile.name, "kind": profile.kind,
                    "tokens": profile.tokens, "fonts": profile.fonts,
                    "rules": profile.rules, "archetypes": profile.archetypes,
                }),
            ))
        })
    }

    fn t_list_designs(&self, _args: Value) -> Result<Value, (i32, String)> {
        self.with_kernel(|k| {
            let list = k.designs()?;
            let active = k.active_design().map(|p| p.name).unwrap_or_default();
            let arr: Vec<Value> = list
                .iter()
                .map(|p| json!({ "name": p.name, "kind": p.kind, "tokens": p.tokens.len(), "active": p.name == active }))
                .collect();
            Ok(tool_result(
                format!("{} 个设计规范，当前激活：{}", arr.len(), if active.is_empty() { "（无，使用内置契约）" } else { &active }),
                json!({ "designs": arr, "active": active }),
            ))
        })
    }

    fn t_set_design(&self, args: Value) -> Result<Value, (i32, String)> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'name'".to_string()))?
            .to_string();
        self.with_kernel(|k| {
            if name.is_empty() {
                k.clear_active_design()?;
            } else {
                k.set_active_design(&name)?;
            }
            Ok(tool_result(
                format!("已激活设计规范：{}", if name.is_empty() { "内置契约" } else { &name }),
                json!({ "active": name }),
            ))
        })
    }

    /// 消化一个真实页面的排版（ego-lite → headless Chromium → 直接抓取）。
    fn t_import_page_design(&self, args: Value) -> Result<Value, (i32, String)> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'url'".to_string()))?
            .to_string();
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| url.trim_end_matches('/').rsplit('/').next().unwrap_or("page").to_string());
        let via = args.get("via").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
        self.with_kernel(|k| {
            let ego = k.vault.config.browser.ego.clone();
            let chrome = k.vault.config.browser.chrome.clone();
            let to = std::time::Duration::from_secs(60);
            let mut tried: Vec<String> = Vec::new();

            if via == "auto" || via == "ego" {
                match kernel_core::ego_digest(&url, ego.as_deref(), to) {
                    Ok(d) => {
                        let p = k.import_design_digest(&name, &url, &d)?;
                        return Ok(tool_result(
                            format!("已用 ego-lite 消化 {url}\n{}", p.summary()),
                            json!({ "name": p.name, "backend": "ego", "fonts": p.fonts, "rules": p.rules, "tokens": p.tokens.len() }),
                        ));
                    }
                    Err(e) => tried.push(format!("ego: {e}")),
                }
            }
            if via == "auto" || via == "render" || via == "headless" {
                match kernel_core::dump_dom(&url, chrome.as_deref(), to) {
                    Ok(html) => {
                        let p = k.import_design_html(&name, &url, &html)?;
                        return Ok(tool_result(
                            format!("已用 headless Chromium 消化 {url}\n{}", p.summary()),
                            json!({ "name": p.name, "backend": "render", "rules": p.rules }),
                        ));
                    }
                    Err(e) => tried.push(format!("render: {e}")),
                }
            }
            match kernel_core::fetch_html(&url, to) {
                Ok(html) => {
                    let p = k.import_design_html(&name, &url, &html)?;
                    Ok(tool_result(
                        format!("已直接抓取 {url}\n{}", p.summary()),
                        json!({ "name": p.name, "backend": "http", "rules": p.rules }),
                    ))
                }
                Err(e) => {
                    tried.push(format!("http: {e}"));
                    Err(anyhow::anyhow!("全部后端失败：{}", tried.join("; ")))
                }
            }
        })
    }

    // ---------- resources ----------

    fn resources_list(&self) -> Result<Value, (i32, String)> {
        self.with_kernel(|k| {
            k.sync_all()?;
            let docs = kernel_core::list_docs(&k.vault)?;
            let resources: Vec<Value> = docs
                .iter()
                .filter_map(|p| p.to_str())
                .map(|p| {
                    let title = k.get_doc(p).ok().and_then(|m| m.title);
                    json!({
                        "uri": format!("knowledge://{p}"),
                        "name": p,
                        "title": title,
                        "mimeType": "text/markdown"
                    })
                })
                .collect();
            let mut resources = resources;
            if let Some(p) = k.active_design() {
                resources.insert(0, json!({
                    "uri": "design://active",
                    "name": format!("design:{}", p.name),
                    "title": format!("设计规范 · {}", p.name),
                    "mimeType": "text/plain"
                }));
            }
            Ok(json!({ "resources": resources }))
        })
    }

    fn resources_read(&self, params: &Value) -> Result<Value, (i32, String)> {
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "missing 'uri'".to_string()))?;
        if uri == "design://active" {
            return self.with_kernel(|k| match k.active_design() {
                Some(p) => Ok(json!({ "contents": [{
                    "uri": "design://active",
                    "mimeType": "text/plain",
                    "text": format!("{}\n\n--- CSS 变量 ---\n{}", p.summary(), p.to_css())
                }] })),
                None => Ok(json!({ "contents": [{
                    "uri": "design://active",
                    "mimeType": "text/plain",
                    "text": "尚未导入设计规范，渲染使用内置契约。可用 import_design 导入 design.md / SKILL 目录 / tokens.css / 页面 HTML。"
                }] })),
            });
        }
        let path = uri
            .strip_prefix("knowledge://")
            .ok_or((-32602, "uri must be knowledge://<path>".to_string()))?
            .to_string();
        if !is_safe_doc_path(&path) {
            return Err((-32602, "path must be under Notes/".into()));
        }
        self.with_kernel(|k| {
            k.sync_throttled()?;
            let model = k.get_doc(&path)?;
            Ok(json!({
                "contents": [
                    { "uri": uri, "mimeType": "text/markdown", "text": to_markdown(&model) },
                    { "uri": format!("{uri}#html"), "mimeType": "text/html", "text": to_html(&model) }
                ]
            }))
        })
    }
}

/// 从工具结果中提取可读文本（content[].text）。
pub fn extract_tool_text(result: &Value) -> String {
    if let Some(arr) = result.get("content").and_then(|c| c.as_array()) {
        let joined: Vec<String> = arr
            .iter()
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
            .collect();
        if !joined.is_empty() {
            return joined.join("\n");
        }
        if let Some(sc) = result.get("structuredContent") {
            return serde_json::to_string_pretty(sc).unwrap_or_default();
        }
    }
    result.to_string()
}

/// MCP tools/call 结果：文本 + 结构化内容双份。
fn tool_result(text: impl Into<String>, structured: Value) -> Value {
    json!({
        "content": [ { "type": "text", "text": text.into() } ],
        "structuredContent": structured,
        "isError": false
    })
}

fn error_response(id: Value, code: i32, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "search_vault",
            "description": "全文检索知识库（FTS5，中文子串可查，bm25 排序）。返回命中文档路径与相关度。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索词" },
                    "limit": { "type": "integer", "description": "返回条数，默认 10" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "search_semantic",
            "description": "混合语义检索（TF-IDF + FTS5 RRF 合并），比纯关键词更智能。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "description": "默认 10" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "save_memory",
            "description": "写入长期记忆（跨会话保留）。kind 用 [偏好]/[事实]/[决策]。用户可在 Notes/Agent/memory.md 编辑或删除。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要记住的内容，一句话" },
                    "kind": { "type": "string", "description": "偏好 | 事实 | 决策" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "recall_memory",
            "description": "回忆：检索过去对话与长期记忆，找回上下文。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "list_docs",
            "description": "列出库中全部文档（路径 + 标题）。",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "read_doc",
            "description": "读取一篇文档。返回格式（markdown/html）、源文、Markdown、AI-HTML 渲染与块模型；HTML 一等文档同样可读。",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "如 Notes/foo.md" } },
                "required": ["path"]
            }
        },
        {
            "name": "write_doc",
            "description": "创建或覆盖一篇文档（写文件 + op-log + 索引）。内容用 markdown，或对 Notes/xxx.html 路径用 html（HTML 一等文档，会被解析成块模型并保留原文）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "如 Notes/foo.md，必须在 Notes/ 下" },
                    "markdown": { "type": "string", "description": "Markdown 内容" },
                    "html": { "type": "string", "description": "HTML 内容（路径需 .html/.htm）" }
                },
                "required": ["path", "markdown"]
            }
        },
        {
            "name": "delete_doc",
            "description": "删除一篇文档（不可恢复；文件真相被移除）。",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        },
        {
            "name": "put_asset",
            "description": "上传附件（图片等）并返回可直接粘贴的 Markdown 引用。内容寻址去重。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "文件名，如 photo.png" },
                    "content_base64": { "type": "string" }
                },
                "required": ["name", "content_base64"]
            }
        },
        {
            "name": "import_design",
            "description": "导入设计规范（design.md / 设计 SKILL 目录 / tokens.css / 任意页面 HTML 的排版摘要），保存为可复用规范。之后渲染与生成都会遵循它。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "规范名，如 openflow" },
                    "content": { "type": "string", "description": "规范文本（markdown/css/html）" },
                    "path": { "type": "string", "description": "或从本地路径导入（文件或目录）" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "import_page_design",
            "description": "消化一个真实网页的排版并导入为设计规范。优先用 ego-lite（复用已登录会话，拿到 computed style 级的字体/配色/容器宽/字阶），失败则退回 headless Chromium 或直接抓取。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "name": { "type": "string", "description": "规范名，缺省取 URL 末段" },
                    "via": { "type": "string", "description": "auto | ego | render | http" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "list_designs",
            "description": "列出已导入的设计规范及当前激活项。",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "set_design",
            "description": "激活/切换设计规范（传空字符串恢复内置契约）。",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "vault_status",
            "description": "库状态：文档数、已索引数、附件数。",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_core::Vault;

    fn server() -> (McpServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path(), "mcp-test").unwrap();
        (McpServer::open(vault).unwrap(), dir)
    }

    /// 在临时库中安装一个带 tools 的示例插件（manifest + installed 登记）。
    fn seed_tool_plugin(dir: &tempfile::TempDir, tool_url: &str, tools_disabled: Value) -> String {
        let pdir = dir.path().join(".thirdc/plugins/demo-tools");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("manifest.json"),
            json!({
                "id": "demo-tools", "name": "示例工具包", "version": "1.0.0",
                "contributions": { "tools": [
                    { "id": "lookup", "title": "外部查询", "description": "查询外部接口",
                      "type": "http", "method": "GET", "url": tool_url,
                      "args": { "kw": { "type": "string", "description": "关键词", "required": true } } },
                    { "id": "find", "title": "库内检索", "type": "kb-search" }
                ]}
            }).to_string(),
        ).unwrap();
        std::fs::create_dir_all(dir.path().join(".thirdc/events")).unwrap();
        std::fs::write(
            dir.path().join(".thirdc/plugins/installed.json"),
            json!({ "demo-tools": { "enabled": true, "installedAt": "t", "tools": tools_disabled } }).to_string(),
        ).unwrap();
        "demo-tools".to_string()
    }

    #[test]
    fn conflow_digest_extracts_structure() {
        let (s, dir) = server();
        // 造一篇真实格式的对话文档
        let conv = dir.path().join("Notes/Agent");
        std::fs::create_dir_all(&conv).unwrap();
        std::fs::write(
            conv.join("2026-10-01 对话.md"),
            "---\ndate: 2026-10-01\ntype: conversation\n---\n\n# 2026-10-01 对话\n\n## 09:00 我\n\n讨论一下导出功能的方案，待办：先做长图\n\n## 09:01 studio\n\n好的。\n\n## 09:05 我\n\n决定：卡片按块分页\n\n## 09:06 studio\n\n明白。\n",
        ).unwrap();
        // 知识卡生成
        let r = conflow::digest(&s, "").unwrap();
        assert_eq!(r["source"], "Notes/Agent/2026-10-01 对话.md");
        assert_eq!(r["digest"], "Notes/Conflow/2026-10-01-知识卡.md");
        assert_eq!(r["turns"], 4);
        assert_eq!(r["points"], 2);
        // 知识卡内容：要点 / 决定 / 待办 三段齐备
        let md = std::fs::read_to_string(dir.path().join("Notes/Conflow/2026-10-01-知识卡.md")).unwrap();
        assert!(md.contains("## 讨论要点"));
        assert!(md.contains("讨论一下导出功能的方案"));
        assert!(md.contains("决定：卡片按块分页"));
        assert!(md.contains("待办：先做长图"));
        // 列表：最新一篇排序在前
        let list = conflow::list_conversations(&s).unwrap();
        assert_eq!(list[0]["date"], "2026-10-01");
        // 命令模式动词可达
        let out = agent::command_mode(&s, "总结对话");
        assert!(out.reply.contains("知识卡已生成"), "got: {}", out.reply);
    }

    #[test]
    fn plugin_tools_register_and_kbsearch_roundtrip() {
        let (s, dir) = server();
        seed_tool_plugin(&dir, "https://example.com/api?q={kw}", json!({}));
        // 注册可见
        let defs = s.plugin_tools();
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|(n, d, _)| n == "plugin_demo-tools_find" && d.contains("示例工具包")));
        assert!(s.tools_for_llm().as_array().unwrap().iter().any(|t| t["function"]["name"] == "plugin_demo-tools_find"));
        // 往库中写一篇文档 → kb-search 命中
        s.call_tool_public("write_doc", json!({ "path": "Notes/t.md", "markdown": "# T\n\n插件工具测试内容 xyzzy\n" })).unwrap();
        let r = s.call_tool_public("plugin_demo-tools_find", json!({ "query": "xyzzy" })).unwrap();
        assert_eq!(r["hits"].as_array().unwrap().len(), 1);
        // 审计落盘
        let audit = std::fs::read_to_string(dir.path().join(".thirdc/events/plugin-tools.jsonl")).unwrap();
        assert!(audit.contains("plugin_demo-tools_find"));
    }

    #[test]
    fn plugin_tool_disabled_and_unknown_rejected() {
        let (s, dir) = server();
        // 内网 URL：lookup 未停用但会被 SSRF 拒；find 被工具级停用 → 先于 URL 校验被拒
        seed_tool_plugin(&dir, "http://127.0.0.1:7700/x?q={kw}", json!({ "find": false }));
        let e1 = s.call_tool_public("plugin_demo-tools_find", json!({ "query": "x" })).unwrap_err();
        assert!(e1.contains("停用"), "got: {e1}");
        let e2 = s.call_tool_public("plugin_demo-tools_lookup", json!({ "kw": "x" })).unwrap_err();
        assert!(e2.contains("内网"), "got: {e2}");
        // 不存在的工具名
        assert!(s.call_tool_public("plugin_nope_missing", json!({})).is_err());
    }

    #[test]
    fn plugin_tool_http_ssrf_guard_and_templating() {
        let (s, dir) = server();
        // 内网地址在调用前被拒（URL 模板已正确填充才能到达 guard）
        seed_tool_plugin(&dir, "http://127.0.0.1:7700/x?q={kw}", json!({}));
        let err = s.call_tool_public("plugin_demo-tools_lookup", json!({ "kw": "a b" })).unwrap_err();
        assert!(err.contains("内网"), "got: {err}");
        // 未提供的 {参数} 被识别
        let (s2, dir2) = server();
        seed_tool_plugin(&dir2, "https://example.com/api?q={kw}&extra={missing}", json!({}));
        let err2 = s2.call_tool_public("plugin_demo-tools_lookup", json!({ "kw": "a" })).unwrap_err();
        assert!(err2.contains("missing"), "got: {err2}");
    }

    fn call(s: &McpServer, method: &str, params: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let raw = s.handle(&req.to_string()).expect("should respond");
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn initialize_negotiates_protocol() {
        let (s, _d) = server();
        let r = call(
            &s,
            "initialize",
            json!({ "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name":"t","version":"0"} }),
        );
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "thirdc");
        assert!(r["result"]["capabilities"]["tools"].is_object());

        let r2 = call(&s, "initialize", json!({ "protocolVersion": "9999-01-01" }));
        assert_eq!(r2["result"]["protocolVersion"], LATEST_PROTOCOL);
    }

    #[test]
    fn notifications_get_no_response() {
        let (s, _d) = server();
        let n = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(s.handle(&n.to_string()).is_none());
    }

    #[test]
    fn tools_list_and_unknown_method() {
        let (s, _d) = server();
        let r = call(&s, "tools/list", json!({}));
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"search_vault"));
        assert!(names.contains(&"write_doc"));
        assert!(names.contains(&"put_asset"));

        let bad = call(&s, "no/such", json!({}));
        assert_eq!(bad["error"]["code"], -32601);
    }

    #[test]
    fn write_read_search_via_tools() {
        let (s, _d) = server();
        let w = call(
            &s,
            "tools/call",
            json!({ "name": "write_doc", "arguments": {
                "path": "Notes/agent.md",
                "markdown": "# Agent 写入\n\n这是通过 MCP 工具创建的内容。\n"
            }}),
        );
        assert_eq!(w["result"]["isError"], false);

        let r = call(
            &s,
            "tools/call",
            json!({ "name": "read_doc", "arguments": { "path": "Notes/agent.md" } }),
        );
        assert_eq!(r["result"]["structuredContent"]["title"], "Agent 写入");
        assert!(r["result"]["structuredContent"]["html"]
            .as_str()
            .unwrap()
            .contains("ai-html"));

        let sres = call(
            &s,
            "tools/call",
            json!({ "name": "search_vault", "arguments": { "query": "MCP 工具" } }),
        );
        assert_eq!(
            sres["result"]["structuredContent"]["hits"][0]["path"],
            "Notes/agent.md"
        );
    }

    #[test]
    fn resources_expose_markdown_and_ai_html() {
        let (s, _d) = server();
        call(
            &s,
            "tools/call",
            json!({ "name": "write_doc", "arguments": {
                "path": "Notes/r.md", "markdown": "# 资源\n\n正文\n" }}),
        );
        let list = call(&s, "resources/list", json!({}));
        let resources = list["result"]["resources"].as_array().unwrap();
        assert!(resources.iter().any(|r| r["uri"] == "knowledge://Notes/r.md"));

        let read = call(&s, "resources/read", json!({ "uri": "knowledge://Notes/r.md" }));
        let contents = read["result"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["mimeType"], "text/markdown");
        assert_eq!(contents[1]["mimeType"], "text/html");
    }

    #[test]
    fn put_asset_returns_markdown_ref() {
        let (s, _d) = server();
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG fake");
        let r = call(
            &s,
            "tools/call",
            json!({ "name": "put_asset", "arguments": { "name": "pic.png", "content_base64": png } }),
        );
        let md = r["result"]["structuredContent"]["markdown"].as_str().unwrap();
        assert!(md.starts_with("![pic.png](Assets/"));
    }

    #[test]
    fn security_and_errors() {
        let (s, _d) = server();
        // 目录穿越
        let t = call(
            &s,
            "tools/call",
            json!({ "name": "read_doc", "arguments": { "path": "Notes/../../etc/passwd" } }),
        );
        assert_eq!(t["error"]["code"], -32602);

        // 坏 JSON
        let raw = s.handle("{not json").unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["error"]["code"], -32700);

        // 缺参数
        let m = call(&s, "tools/call", json!({ "name": "search_vault", "arguments": {} }));
        assert_eq!(m["error"]["code"], -32602);

        // 未知工具
        let u = call(&s, "tools/call", json!({ "name": "nope", "arguments": {} }));
        assert_eq!(u["error"]["code"], -32602);

        // 坏 base64
        let b = call(
            &s,
            "tools/call",
            json!({ "name": "put_asset", "arguments": { "name": "x.png", "content_base64": "!!!" } }),
        );
        assert_eq!(b["error"]["code"], -32602);
    }
}

/// 入口侧高层操作：连接配置 → 采集到库。
pub mod pull {
    use super::client::{McpClient, extract_mime, extract_text};
    use kernel_core::{ConnectionConfig, Kernel};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    pub struct PullReport {
        pub server: String,
        pub tools: Vec<String>,
        pub imported: Vec<(String, String)>, // (uri, rel)
        pub skipped: Vec<String>,
    }

    /// 探测：握手 + 列出 tools / resources。
    pub fn probe(cfg: &ConnectionConfig) -> anyhow::Result<(String, Vec<String>, Vec<String>)> {
        let mut c = McpClient::connect(cfg)?;
        let tools = c
            .list_tools()?
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
            .collect();
        let resources = c
            .list_resources()?
            .iter()
            .filter_map(|r| r.get("uri").and_then(|n| n.as_str()).map(|s| s.to_string()))
            .collect();
        Ok((c.server_name(), tools, resources))
    }

    /// 通用工具调用（结果文本直接返回）。
    pub fn call(cfg: &ConnectionConfig, tool: &str, args: Value) -> anyhow::Result<Value> {
        let mut c = McpClient::connect(cfg)?;
        c.call_tool(tool, args)
    }

    /// 把该连接的**全部 resources** 采集入库（同一 uri 重复采集即更新原文档）。
    pub fn pull_resources(
        cfg: &ConnectionConfig,
        kernel: &Arc<Mutex<Kernel>>,
    ) -> anyhow::Result<PullReport> {
        let mut c = McpClient::connect(cfg)?;
        let server = c.server_name();
        let tools = c
            .list_tools()?
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
            .collect();
        let resources = c.list_resources()?;

        let mut imported = Vec::new();
        let mut skipped = Vec::new();
        for r in resources {
            let uri = match r.get("uri").and_then(|u| u.as_str()) {
                Some(u) => u.to_string(),
                None => continue,
            };
            let read = match c.read_resource(&uri) {
                Ok(v) => v,
                Err(e) => {
                    skipped.push(format!("{uri}: {e}"));
                    continue;
                }
            };
            let Some(text) = extract_text(&read) else {
                skipped.push(format!("{uri}: no text content"));
                continue;
            };
            let mime = extract_mime(&read);
            let title = r
                .get("title")
                .or_else(|| r.get("name"))
                .and_then(|t| t.as_str());
            let mut k = kernel.lock().map_err(|_| anyhow::anyhow!("kernel lock poisoned"))?;
            let rel = k.import_capture(&cfg.name, &uri, title, &text, &mime)?;
            imported.push((uri, rel));
        }
        Ok(PullReport {
            server,
            tools,
            imported,
            skipped,
        })
    }
}

#[cfg(test)]
mod client_tests {
    use super::client::{McpClient, extract_text};
    use super::pull;
    use kernel_core::{ConnectionConfig, Kernel, Vault};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    const MOCK_SERVER: &str = r##"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    if not line.strip(): continue
    try: msg = json.loads(line)
    except Exception: continue
    if "id" not in msg: continue
    rid = msg["id"]; m = msg.get("method")
    if m == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"mock","version":"0.0"}}})
    elif m == "tools/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}})
    elif m == "tools/call":
        a = msg.get("params",{}).get("arguments",{})
        send({"jsonrpc":"2.0","id":rid,"result":{"content":[{"type":"text","text":"echo: " + str(a.get("text",""))}]}})
    elif m == "resources/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"resources":[{"uri":"mock://note/1","name":"note1","title":"模拟笔记","mimeType":"text/markdown"}]}})
    elif m == "resources/read":
        send({"jsonrpc":"2.0","id":rid,"result":{"contents":[{"uri":"mock://note/1","mimeType":"text/markdown","text":"# 模拟笔记\n\n来自模拟 MCP server 的内容。\n"}]}})
    else:
        send({"jsonrpc":"2.0","id":rid,"error":{"code":-32601,"message":"method not found"}})
"##;

    struct Fixture {
        _dir: tempfile::TempDir,
        cfg: ConnectionConfig,
        kernel: Arc<Mutex<Kernel>>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mock_mcp.py");
        std::fs::write(&script, MOCK_SERVER).unwrap();
        let vault = Vault::init(dir.path().join("vault"), "mcp-entry").unwrap();
        let cfg = ConnectionConfig {
            name: "mock".into(),
            command: "python3".into(),
            args: vec![script.to_string_lossy().into_owned()],
            env: Default::default(),
        };
        Fixture {
            _dir: dir,
            cfg,
            kernel: Arc::new(Mutex::new(Kernel::open(vault).unwrap())),
        }
    }

    #[test]
    fn connect_and_call_tool() {
        let f = fixture();
        let mut c = McpClient::connect(&f.cfg).unwrap();
        assert_eq!(c.server_name(), "mock");
        assert_eq!(c.protocol_version, "2024-11-05");

        let tools = c.list_tools().unwrap();
        assert_eq!(tools[0]["name"], "echo");

        let r = c.call_tool("echo", json!({ "text": "hi" })).unwrap();
        assert_eq!(extract_text(&r).unwrap(), "echo: hi");
    }

    #[test]
    fn probe_lists_tools_and_resources() {
        let f = fixture();
        let (server, tools, resources) = pull::probe(&f.cfg).unwrap();
        assert_eq!(server, "mock");
        assert_eq!(tools, vec!["echo".to_string()]);
        assert_eq!(resources, vec!["mock://note/1".to_string()]);
    }

    #[test]
    fn pull_imports_resource_into_vault_and_is_idempotent() {
        let f = fixture();
        let report = pull::pull_resources(&f.cfg, &f.kernel).unwrap();
        assert_eq!(report.imported.len(), 1);
        let (uri, rel) = &report.imported[0];
        assert_eq!(uri, "mock://note/1");
        assert_eq!(rel, "Notes/Sources/mock/模拟笔记.md", "中文标题应保留为可读文件名");

        // 文档落盘、带来源行、可检索
        let k = f.kernel.lock().unwrap();
        let text = std::fs::read_to_string(k.vault.root.join(rel)).unwrap();
        assert!(text.contains("mock://note/1"));
        assert!(text.contains("来自模拟 MCP server 的内容"));
        drop(k);

        // 再次采集：更新同一文档，不产生副本
        let report2 = pull::pull_resources(&f.cfg, &f.kernel).unwrap();
        assert_eq!(report2.imported[0].1, *rel);
        let k = f.kernel.lock().unwrap();
        assert_eq!(k.items_count().unwrap(), 1);
        let docs = kernel_core::list_docs(&k.vault).unwrap();
        assert_eq!(docs.len(), 1, "重复采集不应产生第二篇文档");
    }
}

/// 对话式 agent：LLM（OpenAI 兼容）驱动内核工具；未配置模型时退化为命令模式。
/* ───────── 插件工具（v2）：contributions.tools 声明 → agent 可调用 ─────────
   harness 思路：能力即声明，宿主代执行。v2 支持两种声明式工具：
   - http：模板化调用外部 API（{arg} 占位符；SSRF 防护 + 10s 超时 + 1MB 截断）
   - kb-search：库内检索（可限定标签）
   插件代码不执行；每次执行写审计 .thirdc/events/plugin-tools.jsonl */

pub const PLUGIN_TOOL_PREFIX: &str = "plugin_";

impl McpServer {
    fn plugins_dir(&self) -> Option<std::path::PathBuf> {
        let k = self.kernel.lock().ok()?;
        Some(k.vault.root.join(".thirdc").join("plugins"))
    }

    fn plugin_installed(&self) -> serde_json::Map<String, Value> {
        let Some(dir) = self.plugins_dir() else { return Default::default() };
        std::fs::read_to_string(dir.join("installed.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    /// 汇总已启用插件的 tools 声明（含工具级停用检查），name = plugin_<pid>_<tid>。
    pub fn plugin_tools(&self) -> Vec<(String, String, Value)> {
        let mut out = Vec::new();
        let Some(dir) = self.plugins_dir() else { return out };
        let installed = self.plugin_installed();
        let Ok(rd) = std::fs::read_dir(&dir) else { return out };
        for e in rd.filter_map(|e| e.ok()) {
            let pdir = e.path();
            if !pdir.is_dir() { continue; }
            let Ok(mraw) = std::fs::read_to_string(pdir.join("manifest.json")) else { continue };
            let Ok(m) = serde_json::from_str::<Value>(&mraw) else { continue };
            let pid = m.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if pid.is_empty() || pid.contains('_') { continue; }
            let Some(st) = installed.get(&pid) else { continue };
            if st.get("enabled").and_then(|x| x.as_bool()) != Some(true) { continue }
            let tool_overrides = st.get("tools").and_then(|x| x.as_object()).cloned().unwrap_or_default();
            let pname = m.get("name").and_then(|x| x.as_str()).unwrap_or(&pid).to_string();
            let Some(tools) = m.get("contributions").and_then(|c| c.get("tools")).and_then(|x| x.as_array()) else { continue };
            for t in tools {
                let tid = t.get("id").and_then(|x| x.as_str()).unwrap_or("");
                if tid.is_empty() || tid.contains('_') { continue }
                if tool_overrides.get(tid).and_then(|x| x.as_bool()) == Some(false) { continue }
                let name = format!("{PLUGIN_TOOL_PREFIX}{pid}_{tid}");
                let desc = format!(
                    "【插件 {}】{}",
                    pname,
                    t.get("description").and_then(|x| x.as_str()).unwrap_or_else(|| t.get("title").and_then(|x| x.as_str()).unwrap_or(tid))
                );
                let schema = match t.get("type").and_then(|x| x.as_str()) {
                    Some("kb-search") => json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "检索关键词" },
                            "limit": { "type": "integer", "description": "返回条数（默认 5）" }
                        },
                        "required": ["query"]
                    }),
                    _ => {
                        // http：args 声明 → JSON schema
                        let mut props = serde_json::Map::new();
                        let mut required = Vec::new();
                        if let Some(a) = t.get("args").and_then(|x| x.as_object()) {
                            for (k, v) in a {
                                props.insert(k.clone(), v.clone());
                                if v.get("required").and_then(|x| x.as_bool()) == Some(true) {
                                    required.push(json!(k));
                                }
                            }
                        }
                        json!({ "type": "object", "properties": props, "required": required })
                    }
                };
                out.push((name, desc, schema));
            }
        }
        out
    }

    pub fn plugin_tools_for_llm(&self) -> Vec<Value> {
        self.plugin_tools()
            .into_iter()
            .map(|(name, desc, schema)| json!({
                "type": "function",
                "function": { "name": name, "description": desc, "parameters": schema }
            }))
            .collect()
    }

    fn plugin_tool_audit(&self, line: Value) {
        let Some(dir) = self.plugins_dir() else { return };
        let ev = dir.join("..").join("events");
        let _ = std::fs::create_dir_all(&ev);
        let mut s = line.to_string();
        s.push('\n');
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(ev.join("plugin-tools.jsonl")).and_then(|mut f| std::io::Write::write_all(&mut f, s.as_bytes()));
    }

    /// 执行插件工具（宿主代执行；声明式，不运行插件代码）。
    pub fn call_plugin_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        let mut hit: Option<(String, Value)> = None; // (plugin_id, tool json)
        'outer: {
            let Some(dir) = self.plugins_dir() else { break 'outer };
            let installed = self.plugin_installed();
            let Ok(rd) = std::fs::read_dir(&dir) else { break 'outer };
            for e in rd.filter_map(|e| e.ok()) {
                let pdir = e.path();
                if !pdir.is_dir() { continue }
                let Ok(mraw) = std::fs::read_to_string(pdir.join("manifest.json")) else { continue };
                let Ok(m) = serde_json::from_str::<Value>(&mraw) else { continue };
                let pid = m.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let Some(st) = installed.get(&pid) else { continue };
                if st.get("enabled").and_then(|x| x.as_bool()) != Some(true) { continue }
                let overrides = st.get("tools").and_then(|x| x.as_object()).cloned().unwrap_or_default();
                let Some(tools) = m.get("contributions").and_then(|c| c.get("tools")).and_then(|x| x.as_array()) else { continue };
                for t in tools {
                    let tid = t.get("id").and_then(|x| x.as_str()).unwrap_or("");
                    if format!("{PLUGIN_TOOL_PREFIX}{pid}_{tid}") == name
                        && overrides.get(tid).and_then(|x| x.as_bool()) != Some(false)
                    {
                        hit = Some((pid.clone(), t.clone()));
                        break 'outer;
                    }
                }
            }
        }
        let Some((pid, tool)) = hit else {
            return Err(format!("未知或已停用的插件工具：{name}"));
        };
        let ttype = tool.get("type").and_then(|x| x.as_str()).unwrap_or("http").to_string();
        let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let result = match ttype.as_str() {
            "kb-search" => self.plugin_tool_kb_search(&args),
            "http" => self.plugin_tool_http(&args, &tool),
            other => Err(format!("不支持的插件工具类型：{other}")),
        };
        let ok = result.is_ok();
        self.plugin_tool_audit(json!({
            "ts": started, "plugin": pid, "tool": name, "type": ttype, "ok": ok,
            "args": args,
        }));
        result
    }

    fn plugin_tool_kb_search(&self, args: &Value) -> Result<Value, String> {
        let query = args.get("query").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Err("缺少 query".into());
        }
        let limit = args.get("limit").and_then(|x| x.as_u64()).unwrap_or(5).clamp(1, 20) as usize;
        let hits = self
            .with_kernel(|k| k.search(&query).map_err(|e| anyhow::anyhow!(e)))
            .map_err(|(_, m)| m)?
            .into_iter()
            .take(limit)
            .collect::<Vec<_>>();
        Ok(json!({ "query": query, "hits": hits.iter().map(|(p, r)| json!({ "path": p, "rank": r })).collect::<Vec<_>>() }))
    }

    fn plugin_tool_http(&self, args: &Value, tool: &Value) -> Result<Value, String> {
        let raw_url = tool.get("url").and_then(|x| x.as_str()).unwrap_or("");
        if !raw_url.starts_with("http://") && !raw_url.starts_with("https://") {
            return Err("url 必须是 http(s)".into());
        }
        // 模板替换：{key} → urlencoded(args[key])
        let mut url = raw_url.to_string();
        let mut body_extra = serde_json::Map::new();
        if let Some(a) = args.as_object() {
            for (k, v) in a {
                let placeholder = format!("{{{k}}}");
                let textval = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if url.contains(&placeholder) {
                    url = url.replace(&placeholder, &urlencode(&textval));
                } else {
                    body_extra.insert(k.clone(), v.clone());
                }
            }
        }
        // 收集未被替换的 {占位符}
        let missing: Vec<String> = {
            let mut v = Vec::new();
            let mut rest = url.as_str();
            while let Some(i) = rest.find('{') {
                if let Some(j) = rest[i..].find('}') {
                    v.push(rest[i + 1..i + j].to_string());
                    rest = &rest[i + j + 1..];
                } else { break }
            }
            v.retain(|p| !p.is_empty());
            v
        };
        if !missing.is_empty() {
            return Err(format!("url 存在未提供的参数：{}", missing.join(", ")));
        }
        if url.len() > 2000 {
            return Err("url 过长".into());
        }
        ssrf_guard(&url)?;
        let method = tool.get("method").and_then(|x| x.as_str()).unwrap_or("GET").to_uppercase();
        let client = reqwest::blocking::Client::builder()
            .user_agent("thirdc-plugin-tool")
            .timeout(std::time::Duration::from_secs(10))
            .build().map_err(|e| e.to_string())?;
        let mut req = match method.as_str() {
            "POST" => {
                let mut body = tool.get("body").cloned().unwrap_or(json!({}));
                if let Some(bm) = body.as_object_mut() {
                    for (k, v) in body_extra.clone() { bm.insert(k, v); }
                }
                client.post(&url).json(&body)
            }
            _ => client.get(&url),
        };
        if let Some(hdrs) = tool.get("headers").and_then(|x| x.as_object()) {
            for (k, v) in hdrs {
                if let (Some(k), Some(vs)) = (Some(k.as_str()), v.as_str()) {
                    req = req.header(k.to_string(), vs.to_string());
                }
            }
        }
        let resp = req.send().map_err(|e| format!("请求失败：{e}"))?;
        let status = resp.status();
        let text = resp.text().map_err(|e| e.to_string())?;
        let truncated: String = text.chars().take(1500).collect();
        if !status.is_success() {
            return Err(format!("HTTP {status}：{truncated}"));
        }
        Ok(json!({ "status": status.as_u16(), "text": truncated, "truncated": text.chars().count() > 1500 }))
    }
}

/// 极简 percent-encoding（保留 RFC3986 unreserved）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// SSRF 防护：拒绝内网 / 回环目标。
fn ssrf_guard(url: &str) -> Result<(), String> {
    let host = url
        .strip_prefix("https://").or_else(|| url.strip_prefix("http://"))
        .and_then(|r| r.split('/').next())
        .and_then(|h| h.rsplit('@').next())
        .and_then(|h| h.split(':').next())
        .unwrap_or("")
        .trim_start_matches('[')
        .to_lowercase();
    let blocked = host.is_empty()
        || host == "localhost" || host.ends_with(".localhost") || host == "::1" || host == "0.0.0.0"
        || host.starts_with("127.") || host.starts_with("10.")
        || host.starts_with("192.168.") || host.starts_with("169.254.");
    if blocked {
        return Err(format!("禁止访问内网地址：{host}"));
    }
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() == 4 && octets.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        if let (Ok(a), Ok(b)) = (octets[0].parse::<u8>(), octets[1].parse::<u8>()) {
            if a == 172 && (16..=31).contains(&b) {
                return Err(format!("禁止访问内网地址：{host}"));
            }
        }
    }
    Ok(())
}

/* ───────── ConFlow 对话流（矩阵产品 · 内置整合）─────────
   独立开发、定期整合：ConFlow 的知识卡逻辑独立成模块，知识卡落在独立命名空间
   Notes/Conflow/，未来独立部署版经 MCP 连接器替换本模块即可，ThirdC 侧无感。 */
pub mod conflow {
    use crate::McpServer;
    use serde_json::{json, Value};

    pub const DIR: &str = "Notes/Conflow";

    /// 列出库内对话文档（Notes/Agent 下含「对话」且 type: conversation 的文档）。
    pub fn list_conversations(server: &McpServer) -> Result<Vec<Value>, String> {
        let mut k = server.kernel.lock().map_err(|_| "内核锁异常".to_string())?;
        let _ = k.sync_throttled();
        let mut out = Vec::new();
        for p in kernel_core::list_docs(&k.vault).map_err(|e| e.to_string())? {
            let rel = p.to_string_lossy().into_owned();
            if !rel.starts_with("Notes/Agent/") || !rel.contains("对话") || !rel.ends_with(".md") { continue }
            let text = std::fs::read_to_string(k.vault.root.join(&p)).unwrap_or_default();
            if !text.contains("type: conversation") { continue }
            let turns = text.matches("\n## ").count();
            let date = rel.trim_end_matches(" 对话.md").rsplit('/').next().unwrap_or("").to_string();
            out.push(json!({ "path": rel, "date": date, "turns": turns, "bytes": text.len() }));
        }
        out.sort_by(|a, b| {
            let da = a.get("date").and_then(|x| x.as_str()).unwrap_or("");
            let db = b.get("date").and_then(|x| x.as_str()).unwrap_or("");
            db.cmp(da)
        });
        Ok(out)
    }

    /// 生成知识卡（规则提炼，无需模型）：要点 = 我方各轮首句；决定 / 待办 = 关键词行。
    /// 空路径 = 最新一篇；传日期 = Notes/Agent/<日期> 对话.md。
    pub fn digest(server: &McpServer, rel_or_date: &str) -> Result<Value, String> {
        let rel = if rel_or_date.trim().is_empty() {
            list_conversations(server)?
                .first()
                .and_then(|c| c.get("path")).and_then(|p| p.as_str())
                .ok_or("库内还没有对话记录")?.to_string()
        } else if rel_or_date.contains('/') {
            rel_or_date.trim().to_string()
        } else {
            format!("Notes/Agent/{} 对话.md", rel_or_date.trim())
        };

        let mut k = server.kernel.lock().map_err(|_| "内核锁异常".to_string())?;
        let text = std::fs::read_to_string(k.vault.root.join(&rel))
            .map_err(|_| format!("对话不存在：{rel}"))?;
        let date = rel.trim_end_matches(" 对话.md").rsplit('/').next().unwrap_or("").to_string();

        // 解析轮次：## HH:MM role → (time, role, body)
        let mut turns: Vec<(String, String, String)> = Vec::new();
        let mut cur: Option<(String, String)> = None;
        let mut buf = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("## ") {
                if let Some((t, r)) = cur.take() { turns.push((t, r, std::mem::take(&mut buf))); }
                let mut it = rest.splitn(2, ' ');
                let time = it.next().unwrap_or("").to_string();
                let role = it.next().unwrap_or("").to_string();
                cur = Some((time, role));
            } else if cur.is_some() {
                buf.push_str(line);
                buf.push('\n');
            }
        }
        if let Some((t, r)) = cur.take() { turns.push((t, r, buf)); }

        let my_turns: Vec<&(String, String, String)> = turns.iter().filter(|(_, r, _)| r == "我").collect();
        let points: Vec<String> = my_turns.iter()
            .filter_map(|(_, _, x)| x.lines().find(|l| !l.trim().is_empty()))
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| if l.chars().count() > 60 { format!("{}…", l.chars().take(60).collect::<String>()) } else { l.to_string() })
            .collect();
        let mut actions: Vec<String> = Vec::new();
        let mut decisions: Vec<String> = Vec::new();
        for (_, _, x) in &turns {
            for l in x.lines() {
                let ll = l.trim();
                if ll.is_empty() { continue }
                let is_action = ll.starts_with("- [ ]") || ll.contains("待办") || ll.contains("TODO") || ll.contains("后续：");
                let is_decision = ll.contains("决定") || ll.contains("结论：") || ll.contains("结论:");
                if is_action && !actions.contains(&ll.to_string()) { actions.push(ll.to_string()); }
                if is_decision && !decisions.contains(&ll.to_string()) && !is_action { decisions.push(ll.to_string()); }
            }
        }
        let duration = if turns.len() >= 2 {
            format!("{} → {}", turns.first().unwrap().0, turns.last().unwrap().0)
        } else { "—".to_string() };

        let mut md = format!("# ConFlow 知识卡 · {date}\n\n> 来源：`{rel}` · {} 轮 · {duration}\n\n## 讨论要点\n", my_turns.len());
        if points.is_empty() { md.push_str("- （无有效讨论）\n"); } else { for p in &points { md.push_str(&format!("- {p}\n")); } }
        md.push_str("\n## 决定\n");
        if decisions.is_empty() { md.push_str("- （未识别到明确决定）\n"); } else { for d in &decisions { md.push_str(&format!("- {d}\n")); } }
        md.push_str("\n## 待办\n");
        if actions.is_empty() {
            md.push_str("- [ ] （未识别到待办，可手动补充）\n");
        } else {
            for a in &actions {
                let clean = a.trim_start_matches("- [ ]").trim().to_string();
                md.push_str(&format!("- [ ] {clean}\n"));
            }
        }
        md.push_str("\n> 由 ConFlow 对话流自动提炼（规则模式）。配好 AI 后在对话中说「用 AI 重新总结这篇对话」可获得语义级摘要。\n");

        let digest_rel = format!("{DIR}/{date}-知识卡.md");
        k.put_doc(&digest_rel, &md).map_err(|e| e.to_string())?;
        Ok(json!({ "source": rel, "digest": digest_rel, "turns": turns.len(),
                   "points": points.len(), "decisions": decisions.len(), "actions": actions.len() }))
    }
}

pub mod agent {
    use crate::McpServer;
    use kernel_core::AiConfig;
    use serde_json::{Value, json};

    pub struct Step {
        pub tool: String,
        pub args: Value,
        pub ok: bool,
        pub summary: String,
    }

    pub struct Outcome {
        pub mode: &'static str,
        pub reply: String,
        pub steps: Vec<Step>,
        /// 模型用量（usage.total_tokens 累计；命令模式为 0）。
        pub tokens: u64,
    }

    /// 命令模式：无需模型即可驱动（"搜索 X" / "列出" / "新建 X" / "采集 <连接>" / "同步"）。
    /// 动词按前缀匹配，不要求空格（"列出文档" 也能识别）。
    pub fn command_mode(server: &McpServer, message: &str) -> Outcome {
        let msg = message.trim();
        let raw = msg.trim_start_matches('/');
        let verbs: &[(&str, &str)] = &[
            ("搜索", "search"), ("查找", "search"), ("找", "search"), ("search", "search"),
            ("列出", "list"), ("列表", "list"), ("ls", "list"), ("list", "list"),
            ("新建", "new"), ("创建", "new"), ("new", "new"),
            ("采集", "pull"), ("拉取", "pull"), ("pull", "pull"),
            ("同步", "sync"), ("sync", "sync"), ("状态", "status"), ("status", "status"),
            ("总结对话", "digest"), ("对话总结", "digest"), ("对话纪要", "digest"), ("digest", "digest"),
        ];
        let (kind, rest) = verbs
            .iter()
            .find(|(v, _)| raw.to_lowercase().starts_with(v))
            .map(|(v, k)| (*k, raw[v.len()..].trim()))
            .unwrap_or(("search", raw));

        // 不需要模型的本地动作
        match kind {
            "digest" => {
                // ConFlow 对话流：rest 可为日期（2026-09-25）或空（最新一篇）
                return match crate::conflow::digest(server, rest) {
                    Ok(r) => Outcome {
                        mode: "command",
                        reply: format!(
                            "知识卡已生成：{}（{} 轮 · 要点 {} · 决定 {} · 待办 {}）",
                            r["digest"].as_str().unwrap_or(""),
                            r["turns"], r["points"], r["decisions"], r["actions"]
                        ),
                        steps: vec![Step {
                            tool: "conflow_digest".into(),
                            args: json!({ "source": r["source"] }),
                            ok: true,
                            summary: r["digest"].as_str().unwrap_or("").to_string(),
                        }],
                        tokens: 0,
                    },
                    Err(e) => Outcome {
                        mode: "command",
                        reply: format!("总结失败：{e}"),
                        steps: vec![Step { tool: "conflow_digest".into(), args: json!({}), ok: false, summary: e }],
                        tokens: 0,
                    },
                };
            }
            "new" if !rest.is_empty() => {
                let slug = kernel_core::slugify(rest);
                let path = if slug.is_empty() {
                    format!("Notes/note-{}.md", &kernel_core::Cas::hash_hex(rest.as_bytes())[..8])
                } else {
                    format!("Notes/{slug}.md")
                };
                let body = format!("# {rest}\n\n");
                let args = json!({ "path": path, "markdown": body });
                return match server.call_tool_public("write_doc", args.clone()) {
                    Ok(v) => Outcome {
                        mode: "command",
                        reply: format!("已创建 {path}"),
                        steps: vec![Step { tool: "write_doc".into(), args, ok: true, summary: crate::extract_tool_text(&v) }],
                        tokens: 0,
                    },
                    Err(e) => Outcome {
                        mode: "command",
                        reply: format!("创建失败：{e}"),
                        steps: vec![Step { tool: "write_doc".into(), args, ok: false, summary: e }],
                        tokens: 0,
                    },
                };
            }
            "pull" => {
                let (cfg, _) = {
                    let k = match server.kernel.lock() {
                        Ok(k) => k,
                        Err(_) => return Outcome { mode: "command", reply: "内核锁异常".into(), steps: vec![], tokens: 0 },
                    };
                    let found = k
                        .vault
                        .config
                        .connections
                        .iter()
                        .find(|c| rest.is_empty() || c.name == rest)
                        .cloned();
                    (found, ())
                };
                let Some(cfg) = cfg else {
                    let names: Vec<String> = server
                        .kernel
                        .lock()
                        .map(|k| k.vault.config.connections.iter().map(|c| c.name.clone()).collect())
                        .unwrap_or_default();
                    return Outcome {
                        mode: "command",
                        reply: if names.is_empty() {
                            "未配置任何连接。在 thirdc.toml 的 [[connections]] 里添加 MCP server。".into()
                        } else {
                            format!("指定要采集的连接：{}", names.join(" / "))
                        },
                        steps: vec![],
                        tokens: 0,
                    };
                };
                let kernel_arc = server.kernel.clone();
                return match crate::pull::pull_resources(&cfg, &kernel_arc) {
                    Ok(r) => Outcome {
                        mode: "command",
                        reply: format!("采集完成：{} 个资源入库", r.imported.len()),
                        steps: r
                            .imported
                            .iter()
                            .map(|(uri, rel)| Step {
                                tool: "resources/read".into(),
                                args: json!({ "uri": uri }),
                                ok: true,
                                summary: rel.clone(),
                            })
                            .collect(),
                        tokens: 0,
                    },
                    Err(e) => Outcome { mode: "command", reply: format!("采集失败：{e}"), steps: vec![], tokens: 0 },
                };
            }
            _ => {}
        }

        let (tool, args, label): (&str, Value, String) = match kind {
            "list" => ("list_docs", json!({}), "列出全部文档".into()),
            "sync" | "status" => ("vault_status", json!({}), "查看库状态".into()),
            _ => ("search_vault", json!({ "query": if rest.is_empty() { raw } else { rest } }),
                  format!("检索「{}」", if rest.is_empty() { raw } else { rest })),
        };
        let mut steps = Vec::new();
        let reply = match server.call_tool_public(tool, args.clone()) {
            Ok(v) => {
                let text = crate::extract_tool_text(&v);
                steps.push(Step { tool: tool.into(), args, ok: true, summary: label });
                text
            }
            Err(e) => {
                steps.push(Step { tool: tool.into(), args, ok: false, summary: e.clone() });
                format!("执行失败：{e}")
            }
        };
        Outcome { mode: "command", reply, steps, tokens: 0 }
    }

    /// 纯文本补全：不给工具、不碰库——排版 / 起标题这类「只变形不行动」的任务用它，
    /// 杜绝模型顺手调用 write_doc 之类的副作用。
    pub async fn complete(cfg: &AiConfig, system: &str, user: &str) -> anyhow::Result<String> {
        let key = cfg.resolved_key();
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(180)).build()?;
        let mut req = client.post(&url).json(&json!({
            "model": cfg.model,
            "temperature": 0.1,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        }));
        if !key.is_empty() {
            req = req.bearer_auth(&key);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("模型接口返回 {status}: {}", body);
        }
        Ok(body["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
    }

    /// 发一次 chat/completions，要求流式（`stream: true`）。正文增量逐段回调 `on_delta`；
    /// 工具调用按 index 拼回完整的 `tool_calls`。服务商不支持流式、直接回整块 JSON 时也能用
    /// （整段正文一次回调）。返回拼好的 assistant message 与 token 数（流式时服务商可能不给，记 0）。
    pub async fn chat_stream(
        client: &reqwest::Client,
        cfg: &AiConfig,
        mut body: Value,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> anyhow::Result<(Value, u64)> {
        let key = cfg.resolved_key();
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        body["stream"] = json!(true);
        let mut req = client.post(&url).json(&body);
        if !key.is_empty() {
            req = req.bearer_auth(&key);
        }
        let mut resp = req.send().await?;
        let status = resp.status();
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("event-stream"))
            .unwrap_or(false);
        if !status.is_success() || !is_sse {
            let text = resp.text().await?;
            let v: Value = serde_json::from_str(&text).unwrap_or(json!({ "raw": text }));
            if !status.is_success() {
                anyhow::bail!("模型接口返回 {status}: {}", v);
            }
            let msg = v["choices"][0]["message"].clone();
            if let Some(c) = msg["content"].as_str() {
                if !c.is_empty() {
                    on_delta(c);
                }
            }
            return Ok((msg, v["usage"]["total_tokens"].as_u64().unwrap_or(0)));
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut calls: Vec<Value> = Vec::new();
        let mut tokens = 0u64;
        let mut done = false;
        while !done {
            let Some(chunk) = resp.chunk().await? else { break };
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                if let Some(t) = v["usage"]["total_tokens"].as_u64() {
                    tokens = t;
                }
                let delta = &v["choices"][0]["delta"];
                if let Some(c) = delta["content"].as_str() {
                    if !c.is_empty() {
                        content.push_str(c);
                        on_delta(c);
                    }
                }
                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let i = tc["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
                        while calls.len() <= i {
                            calls.push(json!({ "id": "", "type": "function", "function": { "name": "", "arguments": "" } }));
                        }
                        let c = &mut calls[i];
                        if let Some(id) = tc["id"].as_str() {
                            c["id"] = json!(id);
                        }
                        if let Some(n) = tc["function"]["name"].as_str() {
                            let cur = c["function"]["name"].as_str().unwrap_or("").to_string();
                            c["function"]["name"] = json!(cur + n);
                        }
                        if let Some(a) = tc["function"]["arguments"].as_str() {
                            let cur = c["function"]["arguments"].as_str().unwrap_or("").to_string();
                            c["function"]["arguments"] = json!(cur + a);
                        }
                    }
                }
            }
        }
        let mut msg = json!({ "role": "assistant", "content": if content.is_empty() && !calls.is_empty() { Value::Null } else { json!(content) } });
        if !calls.is_empty() {
            msg["tool_calls"] = json!(calls);
        }
        Ok((msg, tokens))
    }

    /// 纯文本补全的流式版：正文边生成边回调（结晶答案用）。
    pub async fn complete_stream(
        cfg: &AiConfig,
        system: &str,
        user: &str,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> anyhow::Result<String> {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(180)).build()?;
        let (msg, _) = chat_stream(
            &client,
            cfg,
            json!({
                "model": cfg.model,
                "temperature": 0.1,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": user },
                ],
            }),
            on_delta,
        )
        .await?;
        Ok(msg["content"].as_str().unwrap_or("").to_string())
    }

    /// 带工具的 Agent，流式版：每一步工具调用回调 `on_step`，正文增量回调 `on_delta`。
    /// 一轮里先吐出的正文若随后跟着工具调用，那是「我先查一下」之类的过渡话，
    /// 调用方收到 step 时应当把这段草稿清掉。
    pub async fn run_ai_streaming(
        server: &McpServer,
        cfg: &AiConfig,
        message: &str,
        on_step: &mut (dyn FnMut(&Step) + Send),
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> anyhow::Result<Outcome> {
        let tools = server.tools_for_llm();
        let mut messages = vec![
            json!({ "role": "system", "content":
                "你是 ThirdC 知识库里的协作者。优先用工具了解库中现有内容再回答；\
                 写文档前先 search_vault 查重。回答简洁，用中文。\
                 当用户要求整理/汇总时，用 write_doc 把结果沉淀成文档。" }),
            json!({ "role": "user", "content": message }),
        ];
        let client = reqwest::Client::new();
        let mut steps = Vec::new();
        let mut tokens = 0u64;
        for _ in 0..cfg.max_steps.max(1) {
            let (msg, t) = chat_stream(
                &client,
                cfg,
                json!({ "model": cfg.model, "messages": messages, "tools": tools, "tool_choice": "auto" }),
                on_delta,
            )
            .await?;
            tokens += t;
            let tool_calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
            if tool_calls.is_empty() {
                let reply = msg["content"].as_str().filter(|s| !s.is_empty()).unwrap_or("(模型没有返回内容)").to_string();
                return Ok(Outcome { mode: "ai", reply, steps, tokens });
            }
            messages.push(msg.clone());
            for call in tool_calls {
                let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let args: Value = call["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));
                let (ok, summary) = match server.call_tool_public(&name, args.clone()) {
                    Ok(v) => (true, crate::extract_tool_text(&v)),
                    Err(e) => (false, format!("失败：{e}")),
                };
                let step = Step { tool: name.clone(), args: args.clone(), ok, summary: summary.clone() };
                on_step(&step);
                steps.push(step);
                messages.push(json!({ "role": "tool", "tool_call_id": call["id"].clone(), "content": summary }));
            }
        }
        Ok(Outcome {
            mode: "ai",
            reply: "已达到工具调用步数上限，未得到最终答复。可细化问题后重试。".into(),
            steps,
            tokens,
        })
    }

    /// AI 模式：模型选择工具 → 执行 → 回灌结果，直到给出最终答复。
    pub async fn run_ai(
        server: &McpServer,
        cfg: &AiConfig,
        message: &str,
    ) -> anyhow::Result<Outcome> {
        run_ai_with(server, cfg, message, |_| {}).await
    }

    /// 同上，但每一步工具调用都会回调（供 SSE 实时上屏）。
    pub async fn run_ai_with(
        server: &McpServer,
        cfg: &AiConfig,
        message: &str,
        mut on_step: impl FnMut(&Step),
    ) -> anyhow::Result<Outcome> {
        let tools = server.tools_for_llm();
        let mut messages = vec![
            json!({ "role": "system", "content":
                "你是 ThirdC 知识库里的协作者。优先用工具了解库中现有内容再回答；\
                 写文档前先 search_vault 查重。回答简洁，用中文。\
                 当用户要求整理/汇总时，用 write_doc 把结果沉淀成文档。" }),
            json!({ "role": "user", "content": message }),
        ];
        let key = cfg.resolved_key();
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let mut steps = Vec::new();
        let mut tokens = 0u64;

        for _ in 0..cfg.max_steps.max(1) {
            let mut req = client.post(&url).json(&json!({
                "model": cfg.model,
                "messages": messages,
                "tools": tools,
                "tool_choice": "auto",
            }));
            if !key.is_empty() {
                req = req.bearer_auth(&key);
            }
            let resp = req.send().await?;
            let status = resp.status();
            let body: Value = resp.json().await?;
            if !status.is_success() {
                anyhow::bail!("模型接口返回 {status}: {}", body);
            }
            tokens += body["usage"]["total_tokens"].as_u64().unwrap_or(0);
            let choice = body["choices"].get(0).cloned().unwrap_or(Value::Null);
            let msg = choice["message"].clone();
            let tool_calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();

            if tool_calls.is_empty() {
                let reply = msg["content"].as_str().unwrap_or("(模型没有返回内容)").to_string();
                return Ok(Outcome { mode: "ai", reply, steps, tokens });
            }

            messages.push(msg.clone());
            for call in tool_calls {
                let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let args: Value = call["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));
                let (ok, summary) = match server.call_tool_public(&name, args.clone()) {
                    Ok(v) => (true, crate::extract_tool_text(&v)),
                    Err(e) => (false, format!("失败：{e}")),
                };
                let step = Step { tool: name.clone(), args: args.clone(), ok, summary: summary.clone() };
                on_step(&step);
                steps.push(step);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call["id"].clone(),
                    "content": summary,
                }));
            }
        }
        Ok(Outcome {
            mode: "ai",
            reply: "已达到工具调用步数上限，未得到最终答复。可细化问题后重试。".into(),
            steps,
            tokens,
        })
    }
}
