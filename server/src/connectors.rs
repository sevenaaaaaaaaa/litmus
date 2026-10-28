//! 对接（0.18 / 0.26）：GitHub、Notion（官方 MCP）、Sanity、OpenFlow、MFlow、WebsFlow、PayFlow（只读）、inFlow——接进来，也能发回去。
//!
//! - **接进来**：都是数据源的一种（`kind` = github / notion / sanity / openflow / mflow），内容原处索引、
//!   在场上出现、能搜能问，值得的再导入或让 AI 学。配置在这台设备（`.litmus/local/sources.json` 的 `conf`），
//!   密钥在 /凭证，配置里只写名字。
//! - **发回去**：把库里的一篇发到对方（`POST /connectors/publish`）。一律先进草稿或新建：GitHub 提交到指定分支的
//!   指定路径、Notion 在指定页面下新建子页面、Sanity 写成草稿（`drafts.` 开头，不上线）、OpenFlow 建草稿文章
//!   （勾了才发布）、MFlow 进选题队列。每次都要用户在壳里确认；发过的记在 `.litmus/local/outbound.json`，
//!   再发同一篇时 GitHub / Sanity 原地更新，其他新建一份。
//! - 远程 MCP（OpenFlow）走 `litmus_mcp::client` 的 HTTP 传输；Notion 用本机 `npx @notionhq/notion-mcp-server`。

use super::sources::{put_text_pub as put_text, secret_pub as secret, Item, Source, MAX_TEXT_PUB as MAX_TEXT};
use super::{audit_log, check_token, err, AppState};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

const MAX_ITEMS: usize = 3000;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn ymd(secs: i64) -> String {
    super::field::ymd(secs)
}
/// "2026-10-27T08:00:00Z" → unix 秒（解析不了就是现在）
fn parse_time(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 10 {
        return now_secs() as i64;
    }
    let num = |a: usize, n: usize| s.get(a..a + n).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d)) = (num(0, 4), num(5, 2), num(8, 2)) else { return now_secs() as i64 };
    let (hh, mm, ss) = (num(11, 2).unwrap_or(0), num(14, 2).unwrap_or(0), num(17, 2).unwrap_or(0));
    // 公历 → 天数（Howard Hinnant 算法）
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hh * 3600 + mm * 60 + ss
}

fn conf(s: &Source) -> Value {
    s.conf.clone().unwrap_or(json!({}))
}
fn cs<'a>(c: &'a Value, k: &str) -> Option<&'a str> {
    c.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty())
}
fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(40)).user_agent(concat!("Litmus/", env!("CARGO_PKG_VERSION"))).build().map_err(|e| e.to_string())
}
fn key_id(s: &str) -> String {
    kernel_core::Cas::hash_hex(s.as_bytes())[..16].to_string()
}
/// 去掉 frontmatter 和开头的一级标题（标题另作字段发出去，不在正文里重复）。
fn body_without_title(md: &str) -> String {
    let body = md.strip_prefix("---\n").and_then(|r| r.find("\n---\n").map(|i| &r[i + 5..])).unwrap_or(md);
    let mut lines = body.lines().skip_while(|l| l.trim().is_empty()).peekable();
    if lines.peek().map(|l| l.trim_start().starts_with("# ")).unwrap_or(false) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n").trim().to_string()
}

fn title_of_md(md: &str, fallback: &str) -> String {
    md.lines().find_map(|l| l.trim().strip_prefix("# ").map(|t| t.trim().to_string())).filter(|t| !t.is_empty()).unwrap_or_else(|| fallback.to_string())
}
fn item(root: &Path, s: &Source, id: &str, title: &str, text: &str, kind: &str, loc: &str, mtime: i64, etag: Option<String>) -> Item {
    let body = if text.trim_start().starts_with("# ") { text.to_string() } else { format!("# {title}\n\n{text}") };
    let hash = put_text(root, &body.chars().take(MAX_TEXT).collect::<String>());
    Item { key: format!("ext:{}/{id}", s.id), src: s.id.clone(), title: title.chars().take(120).collect(), kind: kind.into(), loc: loc.into(), date: ymd(mtime), mtime, size: text.len() as u64, hash, note: None, etag }
}

/// 壳里「添加」表单要的：每种对接的说明、要填的、要存的凭证、能不能发回去。
pub fn presets() -> Value {
    json!([
        { "kind": "github", "name": "GitHub", "about": "仓库里的文档：README、docs/ 下的 md、CLAUDE.md / AGENTS.md / SKILL.md。发回去 = 提交到仓库。",
          "secret": "github_token", "secret_hint": "GitHub 个人访问令牌（Fine-grained，Contents 读；要发回去再加 Contents 写）。不填时用本机 gh 登录的令牌",
          "fields": [["repos", "仓库（owner/repo，逗号分隔；留空 = 你自己的全部仓库）"], ["paths", "只要这些路径（可选，逗号分隔，如 docs/, README.md）"]],
          "write": [["repo", "发到哪个仓库（owner/repo）"], ["branch", "分支（留空 = 默认分支）"], ["dir", "放在哪个目录（如 docs/litmus）"]] },
        { "kind": "notion", "name": "Notion", "about": "经 Notion 官方 MCP（本机 npx @notionhq/notion-mcp-server）读集成能访问的页面。发回去 = 在指定页面下新建子页面。",
          "secret": "notion_token", "secret_hint": "Notion 集成令牌（notion.so/my-integrations 新建内部集成，把要读的页面「连接」到它）。需要本机有 Node",
          "fields": [],
          "write": [["parent", "发到哪个页面下（页面链接或 ID）"]] },
        { "kind": "sanity", "name": "Sanity", "about": "按 GROQ 读文档（Portable Text 转成 Markdown）。发回去 = 写成草稿（drafts.），不会直接上线。",
          "secret": "sanity_token", "secret_hint": "Sanity API 令牌（manage.sanity.io → API → Tokens；只读 Viewer 即可，发回去要 Editor）。公开数据集可不填",
          "fields": [["project", "Project ID"], ["dataset", "Dataset（默认 production）"], ["query", "GROQ（可选；默认全部已发布文档）"]],
          "write": [["type", "文档类型（_type，如 post）"], ["body_field", "正文字段（默认 body）"]] },
        { "kind": "openflow", "name": "OpenFlow", "about": "经 OpenFlow 的 MCP（HTTP）读文章与技能。发回去 = 新建草稿文章（勾了才发布）。",
          "secret": "openflow_api_key", "secret_hint": "OpenFlow 后台 → 开发者 → API Key（读文章要 articles 权限，发回去要写权限）",
          "fields": [["url", "MCP 地址（默认 https://nownexts.com/mcp-server.php）"]],
          "defaults": { "url": "https://nownexts.com/mcp-server.php" },
          "write": [["category", "文章分类（默认 insight）"]] },
        { "kind": "mflow", "name": "MFlow", "about": "读知识中台与报告（经 MFlow 的机器接口，只读）。发回去 = 进选题队列（服务器要开 MFLOW_MACHINE_WRITE=1）。",
          "secret": "mflow_token", "secret_hint": "MFlow 服务器 run/env.sh 里的 MFLOW_API_TOKEN",
          "fields": [["url", "MFlow 地址（默认 https://nownexts.com/mflow）"], ["only", "只要这些（可选，逗号分隔：知识库名或报告类别）"]],
          "defaults": { "url": "https://nownexts.com/mflow" },
          "write": [] },
        { "kind": "websflow", "name": "WebsFlow", "about": "读你的落地页：发布了的读整页正文，没发布的读模块摘要，再带上近 14 天的曝光、点击、线索和平台记下的投放经验。发回去 = 按这篇做一个新页面（默认不发布）。",
          "secret": "websflow_api_key", "secret_hint": "WebsFlow 控制台「设置 → API 密钥」，wfk_ 开头（读要 read，发回去要 write）",
          "fields": [["url", "WebsFlow 地址（默认 https://nownexts.com/webflow）"]],
          "defaults": { "url": "https://nownexts.com/webflow" },
          "write": [["mode", "做成什么（site 落地页 / h5 / ppt / story，默认 site）"]] },
        { "kind": "payflow", "name": "PayFlow", "about": "只读：在售商品目录，和近一段时间的经营概览（成交额、订单、订阅、渠道、佣金）。不读买家信息，也不会往 PayFlow 写任何东西。",
          "secret": "payflow_api_key", "secret_hint": "PayFlow 后台「API」页新建密钥，填 key_id.secret（用沙箱密钥也行，看板数据不含沙箱订单）",
          "fields": [["url", "PayFlow 地址（默认 https://nownexts.com/payflow）"], ["days", "经营概览看最近几天（默认 30）"]],
          "defaults": { "url": "https://nownexts.com/payflow" },
          "write": [] },
        { "kind": "inflow", "name": "inFlow", "about": "读洞察（带证据、置信度和推荐动作，忽略掉的不要）和工作区记忆。发回去 = 写进工作区记忆，再发同一篇会更新那一条。",
          "secret": "inflow_api_key", "secret_hint": "inFlow 的 API Key（服务器开了 INSFLOW_API_AUTH 才需要；读要 read，发回去要 write）",
          "fields": [["url", "inFlow 地址（默认 https://nownexts.com/inflow）"], ["workspace", "只要这些工作区（可选，逗号分隔的 id；留空 = 全部）"]],
          "defaults": { "url": "https://nownexts.com/inflow" },
          "write": [["workspace", "写进哪个工作区（id；留空 = 第一个）"]] }
    ])
}

fn list_of(c: &Value, k: &str) -> Vec<String> {
    match c.get(k) {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        Some(Value::String(s)) => s.split([',', '，', '\n']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => Vec::new(),
    }
}

// ═══════════ GitHub ═══════════

fn gh_token(root: &Path) -> Option<String> {
    secret(root, "github_token")
        .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()))
        .or_else(|| std::process::Command::new("gh").args(["auth", "token"]).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|t| !t.is_empty()))
}

fn gh_api(c: &Value) -> String {
    cs(c, "api_base").unwrap_or("https://api.github.com").trim_end_matches('/').to_string()
}

fn gh_get(client: &reqwest::blocking::Client, url: &str, token: Option<&str>) -> Result<Value, String> {
    let mut r = client.get(url).header("accept", "application/vnd.github+json").header("x-github-api-version", "2022-11-28");
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    let resp = r.send().map_err(|e| format!("连不上 GitHub：{e}"))?;
    let st = resp.status();
    if st.as_u16() == 401 {
        return Err("GitHub 令牌无效或过期（/凭证 里的 github_token）".into());
    }
    if st.as_u16() == 403 || st.as_u16() == 429 {
        return Err("GitHub 拒绝了：可能是没权限，或者超了调用频率（不带令牌每小时只有 60 次）".into());
    }
    if !st.is_success() {
        return Err(format!("GitHub 返回 {st}（{url}）"));
    }
    resp.json().map_err(|e| e.to_string())
}

/// 仓库里哪些算「文档」。
pub fn gh_wanted(path: &str, only: &[String]) -> bool {
    let p = path.to_ascii_lowercase();
    if !(p.ends_with(".md") || p.ends_with(".mdx") || p.ends_with(".markdown")) {
        return false;
    }
    if ["node_modules/", "vendor/", "third_party/", ".github/issue_template", "target/", "dist/", "build/"].iter().any(|x| p.contains(x)) {
        return false;
    }
    if !only.is_empty() {
        return only.iter().any(|o| {
            let o = o.to_ascii_lowercase();
            if o.ends_with('/') { p.starts_with(&o) || p.contains(&format!("/{o}")) } else { p == o || p.ends_with(&format!("/{o}")) }
        });
    }
    let name = p.rsplit('/').next().unwrap_or(&p);
    !p.contains('/') || p.starts_with("docs/") || p.contains("/docs/") || p.contains("skills/") || ["readme.md", "claude.md", "agents.md", "skill.md", "contributing.md", "changelog.md"].contains(&name)
}

fn scan_github(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let c = conf(s);
    let api = gh_api(&c);
    let token = gh_token(root);
    let client = http()?;
    let only = list_of(&c, "paths");
    let mut repos = list_of(&c, "repos");
    if repos.is_empty() {
        let t = token.as_deref().ok_or("要列出你自己的仓库得有令牌：在 /凭证 存 github_token，或在本机登录 gh；也可以直接填仓库名")?;
        for page in 1..=3 {
            let v = gh_get(&client, &format!("{api}/user/repos?affiliation=owner&per_page=100&sort=pushed&page={page}"), Some(t))?;
            let arr = v.as_array().cloned().unwrap_or_default();
            for r in &arr {
                if r["fork"].as_bool() == Some(true) || r["archived"].as_bool() == Some(true) {
                    continue;
                }
                if let Some(n) = r["full_name"].as_str() {
                    repos.push(n.to_string());
                }
            }
            if arr.len() < 100 {
                break;
            }
        }
    }
    let mut out: Vec<Item> = Vec::new();
    // 一个仓库出错（空仓库的 tree 是 409、私有仓库没权限）就跳过它，别让整次扫描白干；
    // 连着出错太多（多半是限流）就停下，没扫到的沿用上次的结果，不让它们从场上消失
    let (mut fails, mut first_err): (usize, Option<String>) = (0, None);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    'repos: for full in repos.iter().take(80) {
        let got = gh_get(&client, &format!("{api}/repos/{full}"), token.as_deref()).and_then(|meta| {
            let branch = cs(&c, "branch").map(String::from).or_else(|| meta["default_branch"].as_str().map(String::from)).unwrap_or_else(|| "main".into());
            let tree = gh_get(&client, &format!("{api}/repos/{full}/git/trees/{branch}?recursive=1"), token.as_deref())?;
            Ok((meta, branch, tree))
        });
        let (meta, branch, tree) = match got {
            Ok(x) => { fails = 0; x }
            Err(e) => {
                fails += 1;
                first_err.get_or_insert(format!("{full}：{e}"));
                if fails >= 5 { break 'repos; }
                continue;
            }
        };
        let pushed = meta["pushed_at"].as_str().map(parse_time).unwrap_or(now_secs() as i64);
        let files: Vec<(String, String)> = tree["tree"].as_array().into_iter().flatten().filter(|e| e["type"] == "blob" && e["size"].as_u64().unwrap_or(0) < 400_000).filter_map(|e| Some((e["path"].as_str()?.to_string(), e["sha"].as_str()?.to_string()))).filter(|(p, _)| gh_wanted(p, &only)).take(300).collect();
        for (path, sha) in files {
            if out.len() >= MAX_ITEMS {
                break;
            }
            if !super::sources::scan_go() {
                fails = 5;
                break 'repos;
            }
            let id = format!("{full}/{path}");
            let key = format!("ext:{}/{id}", s.id);
            seen.insert(key.clone());
            if let Some(old) = prev.get(&key).filter(|o| o.etag.as_deref() == Some(sha.as_str())) {
                out.push(old.clone());
                continue;
            }
            let blob = match gh_get(&client, &format!("{api}/repos/{full}/git/blobs/{sha}"), token.as_deref()) {
                Ok(b) => { fails = 0; b }
                Err(e) => {
                    fails += 1;
                    first_err.get_or_insert(format!("{full}/{path}：{e}"));
                    if let Some(old) = prev.get(&key) { out.push(old.clone()); }
                    if fails >= 5 { break 'repos; }
                    continue;
                }
            };
            let raw = blob["content"].as_str().unwrap_or("").replace(['\n', '\r'], "");
            let bytes = base64::engine::general_purpose::STANDARD.decode(raw).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).to_string();
            let fname = path.rsplit('/').next().unwrap_or(&path).to_string();
            let short = full.rsplit('/').next().unwrap_or(full);
            let title = title_of_md(&text, &format!("{short} · {}", fname.trim_end_matches(".md")));
            let loc = format!("https://github.com/{full}/blob/{branch}/{path}");
            out.push(item(root, s, &id, &title, &text, "md", &loc, pushed, Some(sha)));
        }
    }
    if let Some(e) = first_err.as_ref() {
        if out.is_empty() && prev.is_empty() {
            return Err(e.clone());
        }
    }
    // 半路停下（连着出错或你点了停下）：上次扫到、这次没轮到的照旧留着
    if fails >= 5 {
        for (k, it) in prev {
            if !seen.contains(k) && out.len() < MAX_ITEMS { out.push(it.clone()); }
        }
    }
    Ok(out)
}

fn gh_publish(root: &Path, s: &Source, title: &str, md: &str, prev: Option<&Value>, opts: &Value) -> Result<Value, String> {
    let c = conf(s);
    let w = c.get("write").cloned().unwrap_or(json!({}));
    let repo = cs(opts, "repo").or_else(|| cs(&w, "repo")).ok_or("还没设发到哪个仓库")?.to_string();
    let token = gh_token(root).ok_or("发回 GitHub 要令牌：在 /凭证 存 github_token（要 Contents 写权限）")?;
    let api = gh_api(&c);
    let client = http()?;
    let dir = cs(opts, "dir").or_else(|| cs(&w, "dir")).unwrap_or("docs").trim_matches('/').to_string();
    let name = safe_file(title);
    let path = prev.and_then(|p| p["path"].as_str()).filter(|_| prev.and_then(|p| p["repo"].as_str()) == Some(repo.as_str())).map(String::from).unwrap_or_else(|| if dir.is_empty() { format!("{name}.md") } else { format!("{dir}/{name}.md") });
    let branch = cs(opts, "branch").or_else(|| cs(&w, "branch")).map(String::from);
    let mut url = format!("{api}/repos/{repo}/contents/{}", path.split('/').map(urlenc).collect::<Vec<_>>().join("/"));
    if let Some(b) = &branch {
        url.push_str(&format!("?ref={}", urlenc(b)));
    }
    // 已有同名文件：带上 sha 才能更新
    let sha = client.get(&url).bearer_auth(&token).header("accept", "application/vnd.github+json").send().ok().filter(|r| r.status().is_success()).and_then(|r| r.json::<Value>().ok()).and_then(|v| v["sha"].as_str().map(String::from));
    let mut body = json!({ "message": format!("docs: {}（从鹿蕊 Litmus 发出）", title), "content": base64::engine::general_purpose::STANDARD.encode(md.as_bytes()) });
    if let Some(sh) = &sha {
        body["sha"] = json!(sh);
    }
    if let Some(b) = &branch {
        body["branch"] = json!(b);
    }
    let put_url = url.split('?').next().unwrap_or(&url).to_string();
    let resp = client.put(&put_url).bearer_auth(&token).header("accept", "application/vnd.github+json").json(&body).send().map_err(|e| format!("连不上 GitHub：{e}"))?;
    let st = resp.status();
    let v: Value = resp.json().unwrap_or(json!({}));
    if !st.is_success() {
        return Err(format!("GitHub 没收下（{st}）：{}", v["message"].as_str().unwrap_or("")));
    }
    Ok(json!({ "repo": repo, "path": path, "url": v.pointer("/content/html_url").cloned().unwrap_or(json!(format!("https://github.com/{repo}/blob/{}/{path}", branch.unwrap_or_else(|| "main".into())))), "updated": sha.is_some() }))
}

fn urlenc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
fn safe_file(t: &str) -> String {
    let s: String = t.chars().filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '\n' | '\r' | '\t')).collect::<String>().trim().chars().take(60).collect();
    if s.is_empty() { "untitled".into() } else { s }
}

// ═══════════ Sanity ═══════════

fn sanity_base(c: &Value) -> Result<(String, String, String), String> {
    let project = cs(c, "project").ok_or("还没填 Sanity 的 Project ID")?;
    let dataset = cs(c, "dataset").unwrap_or("production").to_string();
    let ver = cs(c, "api_version").unwrap_or("2024-01-01").trim_start_matches('v').to_string();
    let base = cs(c, "api_base").map(|b| b.trim_end_matches('/').to_string()).unwrap_or_else(|| format!("https://{project}.api.sanity.io"));
    Ok((base, dataset, ver))
}

/// Portable Text → Markdown（标题、引用、列表、粗体 / 斜体 / 行内代码、链接）。
pub fn pt_to_md(blocks: &[Value]) -> String {
    let mut out = String::new();
    let mut last_list = false;
    for b in blocks {
        if b["_type"] != "block" {
            if let Some(code) = b.get("code").and_then(|x| x.as_str()) {
                out.push_str(&format!("\n```\n{code}\n```\n\n"));
            }
            continue;
        }
        let defs: HashMap<String, String> = b["markDefs"].as_array().into_iter().flatten().filter_map(|d| Some((d["_key"].as_str()?.to_string(), d["href"].as_str()?.to_string()))).collect();
        let text: String = b["children"].as_array().into_iter().flatten().map(|sp| {
            let mut t = sp["text"].as_str().unwrap_or("").to_string();
            if t.trim().is_empty() {
                return t;
            }
            for m in sp["marks"].as_array().into_iter().flatten().filter_map(|m| m.as_str()) {
                t = match m {
                    "strong" => format!("**{t}**"),
                    "em" => format!("*{t}*"),
                    "code" => format!("`{t}`"),
                    k => defs.get(k).map(|h| format!("[{t}]({h})")).unwrap_or(t),
                };
            }
            t
        }).collect();
        let list = b.get("listItem").and_then(|x| x.as_str());
        if last_list && list.is_none() {
            out.push('\n');
        }
        let line = match (b["style"].as_str().unwrap_or("normal"), list) {
            (_, Some("number")) => format!("1. {text}\n"),
            (_, Some(_)) => format!("{}- {text}\n", "  ".repeat(b["level"].as_u64().unwrap_or(1).saturating_sub(1) as usize)),
            ("h1", _) => format!("# {text}\n\n"),
            ("h2", _) => format!("## {text}\n\n"),
            ("h3", _) => format!("### {text}\n\n"),
            ("h4", _) | ("h5", _) | ("h6", _) => format!("#### {text}\n\n"),
            ("blockquote", _) => format!("> {text}\n\n"),
            _ => format!("{text}\n\n"),
        };
        last_list = list.is_some();
        out.push_str(&line);
    }
    out.trim().to_string()
}

fn is_pt(v: &Value) -> bool {
    v.as_array().map(|a| !a.is_empty() && a.iter().any(|b| b["_type"] == "block" && b.get("children").is_some())).unwrap_or(false)
}

/// 一篇 Sanity 文档 → Markdown：Portable Text 字段转正文，长字符串成段，对象与数组按字段名分小节。
pub fn sanity_doc_md(v: &Value, depth: usize) -> String {
    let mut out = String::new();
    let Some(obj) = v.as_object() else { return String::new() };
    for (k, x) in obj {
        if k.starts_with('_') || matches!(k.as_str(), "title" | "name" | "slug" | "seo" | "metadata" | "asset" | "crop" | "hotspot") {
            continue;
        }
        if is_pt(x) {
            if depth > 0 || obj.len() > 3 {
                out.push_str(&format!("{} {k}\n\n", "#".repeat((depth + 2).min(4))));
            }
            out.push_str(&pt_to_md(x.as_array().unwrap()));
            out.push_str("\n\n");
        } else if let Some(sv) = x.as_str() {
            let t = sv.trim();
            if t.chars().count() >= 60 {
                out.push_str(&format!("{t}\n\n"));
            } else if !t.is_empty() && depth < 2 && !t.starts_with("http") && !k.ends_with("Id") {
                out.push_str(&format!("- **{k}**：{t}\n"));
            }
        } else if x.is_object() && depth < 3 {
            let inner = sanity_doc_md(x, depth + 1);
            if !inner.trim().is_empty() {
                out.push_str(&format!("\n{} {k}\n\n{inner}\n", "#".repeat((depth + 2).min(4))));
            }
        } else if let Some(a) = x.as_array() {
            if depth < 3 {
                let parts: Vec<String> = a.iter().filter(|e| e.is_object()).map(|e| sanity_doc_md(e, depth + 1)).filter(|t| !t.trim().is_empty()).collect();
                if !parts.is_empty() {
                    out.push_str(&format!("\n{} {k}\n\n{}\n", "#".repeat((depth + 2).min(4)), parts.join("\n\n")));
                }
            }
        }
    }
    out.trim().to_string()
}

fn sanity_title(v: &Value) -> String {
    ["title", "name", "heading", "seoTitle", "label"].iter().find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(String::from)).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| v["_id"].as_str().unwrap_or("Sanity 文档").to_string())
}

fn scan_sanity(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let c = conf(s);
    let (base, dataset, ver) = sanity_base(&c)?;
    let q = cs(&c, "query").map(String::from).unwrap_or_else(|| r#"*[!(_id in path("drafts.**")) && !(_type match "sanity.*") && !(_type match "system.*") && !(_type in ["sanity.imageAsset","sanity.fileAsset"])] | order(_updatedAt desc)[0...2000]"#.into());
    let client = http()?;
    let mut r = client.get(format!("{base}/v{ver}/data/query/{dataset}")).query(&[("query", q.as_str())]);
    if let Some(t) = secret(root, "sanity_token") {
        r = r.bearer_auth(t);
    }
    let resp = r.send().map_err(|e| format!("连不上 Sanity：{e}"))?;
    let st = resp.status();
    let v: Value = resp.json().map_err(|e| format!("Sanity 回的不是 JSON：{e}"))?;
    if !st.is_success() {
        return Err(format!("Sanity 返回 {st}：{}", v.pointer("/error/description").or_else(|| v.get("message")).and_then(|m| m.as_str()).unwrap_or("")));
    }
    let docs = v["result"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for d in docs.iter().take(MAX_ITEMS) {
        if !super::sources::scan_go() { break; }
        let Some(id) = d["_id"].as_str() else { continue };
        let key = format!("ext:{}/{id}", s.id);
        let rev = d["_rev"].as_str().map(String::from);
        if let Some(old) = prev.get(&key).filter(|o| rev.is_some() && o.etag == rev) {
            out.push(old.clone());
            continue;
        }
        let title = sanity_title(d);
        let body = sanity_doc_md(d, 0);
        let t = d["_updatedAt"].as_str().map(parse_time).unwrap_or(now_secs() as i64);
        let loc = format!("sanity://{}/{dataset}/{id}", cs(&c, "project").unwrap_or(""));
        let mut it = item(root, s, id, &title, &body, "sanity", &loc, t, rev);
        if let Some(tp) = d["_type"].as_str() {
            it.note = Some(format!("类型 {tp}"));
        }
        out.push(it);
    }
    Ok(out)
}

/// Markdown → Portable Text（段落、标题、引用、列表、代码块当作正文段；行内粗体 / 斜体 / 代码保留成 mark）。
pub fn md_to_pt(md: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut n = 0usize;
    let mut key = || {
        n += 1;
        format!("l{n:04}")
    };
    let mut in_code = false;
    let mut code = String::new();
    let mut para = String::new();
    let span = |t: &str, k: String| -> Vec<Value> {
        // 只拆 **粗体** 与 `代码`（够用、不出错）
        let mut v = Vec::new();
        let mut rest = t;
        let mut i = 0;
        while !rest.is_empty() {
            let (mark, open) = match (rest.find("**"), rest.find('`')) {
                (Some(a), Some(b)) if b < a => ("code", b),
                (Some(a), _) => ("strong", a),
                (None, Some(b)) => ("code", b),
                _ => {
                    v.push(json!({ "_type": "span", "_key": format!("{k}s{i}"), "text": rest, "marks": [] }));
                    break;
                }
            };
            let delim = if mark == "code" { "`" } else { "**" };
            let after = &rest[open + delim.len()..];
            let Some(close) = after.find(delim) else {
                v.push(json!({ "_type": "span", "_key": format!("{k}s{i}"), "text": rest, "marks": [] }));
                break;
            };
            if open > 0 {
                v.push(json!({ "_type": "span", "_key": format!("{k}s{i}"), "text": &rest[..open], "marks": [] }));
                i += 1;
            }
            v.push(json!({ "_type": "span", "_key": format!("{k}s{i}"), "text": &after[..close], "marks": [mark] }));
            i += 1;
            rest = &after[close + delim.len()..];
        }
        if v.is_empty() {
            v.push(json!({ "_type": "span", "_key": format!("{k}s0"), "text": "", "marks": [] }));
        }
        v
    };
    let mut push = |out: &mut Vec<Value>, style: &str, list: Option<&str>, text: &str, key: String| {
        let mut b = json!({ "_type": "block", "_key": key.clone(), "style": style, "markDefs": [], "children": span(text, key) });
        if let Some(l) = list {
            b["listItem"] = json!(l);
            b["level"] = json!(1);
        }
        out.push(b);
    };
    let body = md.strip_prefix("---\n").and_then(|r| r.find("\n---\n").map(|i| &r[i + 5..])).unwrap_or(md);
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                push(&mut out, "normal", None, &format!("`{}`", code.trim_end()), key());
                code.clear();
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        let t = line.trim();
        if t.is_empty() {
            if !para.is_empty() {
                push(&mut out, "normal", None, para.trim(), key());
                para.clear();
            }
            continue;
        }
        let block = |pre: &str| t.strip_prefix(pre).map(|x| x.trim().to_string());
        let item = block("# ").map(|x| ("h1", None, x)).or_else(|| block("## ").map(|x| ("h2", None, x))).or_else(|| block("### ").map(|x| ("h3", None, x))).or_else(|| block("#### ").map(|x| ("h4", None, x))).or_else(|| block("> ").map(|x| ("blockquote", None, x))).or_else(|| block("- ").or_else(|| block("* ")).map(|x| ("normal", Some("bullet"), x))).or_else(|| {
            let d = t.find(". ")?;
            t[..d].chars().all(|c| c.is_ascii_digit()).then(|| ("normal", Some("number"), t[d + 2..].trim().to_string()))
        });
        match item {
            Some((style, list, text)) => {
                if !para.is_empty() {
                    push(&mut out, "normal", None, para.trim(), key());
                    para.clear();
                }
                push(&mut out, style, list, &text, key());
            }
            None => {
                para.push_str(t);
                para.push(' ');
            }
        }
    }
    if !para.is_empty() {
        push(&mut out, "normal", None, para.trim(), key());
    }
    out
}

fn sanity_publish(root: &Path, s: &Source, title: &str, md: &str, doc_path: &str, prev: Option<&Value>, opts: &Value) -> Result<Value, String> {
    let c = conf(s);
    let w = c.get("write").cloned().unwrap_or(json!({}));
    let (base, dataset, ver) = sanity_base(&c)?;
    let token = secret(root, "sanity_token").ok_or("发回 Sanity 要令牌：在 /凭证 存 sanity_token（Editor 权限）")?;
    let tp = cs(opts, "type").or_else(|| cs(&w, "type")).unwrap_or("post").to_string();
    let body_field = cs(opts, "body_field").or_else(|| cs(&w, "body_field")).unwrap_or("body").to_string();
    // 草稿：drafts.<id>；同一篇再发，替换同一份草稿
    let id = prev.and_then(|p| p["id"].as_str()).map(String::from).unwrap_or_else(|| format!("drafts.litmus-{}", key_id(doc_path)));
    let slug: String = {
        let ascii: String = title.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
        let s2 = ascii.split('-').filter(|x| !x.is_empty()).collect::<Vec<_>>().join("-");
        if s2.len() >= 3 { s2 } else { format!("litmus-{}", &key_id(doc_path)[..8]) }
    };
    let mut doc = json!({ "_id": id, "_type": tp, "title": title, "slug": { "_type": "slug", "current": slug } });
    doc[&body_field] = json!(md_to_pt(&body_without_title(md)));
    let resp = http()?
        .post(format!("{base}/v{ver}/data/mutate/{dataset}?returnIds=true"))
        .bearer_auth(token)
        .json(&json!({ "mutations": [{ "createOrReplace": doc }] }))
        .send()
        .map_err(|e| format!("连不上 Sanity：{e}"))?;
    let st = resp.status();
    let v: Value = resp.json().unwrap_or(json!({}));
    if !st.is_success() {
        return Err(format!("Sanity 没收下（{st}）：{}", v.pointer("/error/description").or_else(|| v.get("message")).and_then(|m| m.as_str()).unwrap_or("")));
    }
    Ok(json!({ "id": id, "type": tp, "draft": true, "updated": prev.is_some(), "url": format!("sanity://{}/{dataset}/{id}", cs(&c, "project").unwrap_or("")) }))
}

// ═══════════ MFlow（机器接口，只读 + 进选题队列） ═══════════

fn mflow_base(c: &Value) -> String {
    cs(c, "url").unwrap_or("https://nownexts.com/mflow").trim_end_matches('/').to_string()
}

fn mflow_get(client: &reqwest::blocking::Client, url: &str, token: &str) -> Result<Value, String> {
    let resp = client.get(url).header("X-MFlow-Token", token).send().map_err(|e| format!("连不上 MFlow：{e}"))?;
    let st = resp.status();
    if st.as_u16() == 401 || st.as_u16() == 403 {
        return Err("MFlow 不认这个令牌（/凭证 里的 mflow_token，要和服务器 MFLOW_API_TOKEN 一致）".into());
    }
    if !st.is_success() {
        return Err(format!("MFlow 返回 {st}（{url}）"));
    }
    resp.json().map_err(|e| format!("MFlow 回的不是 JSON：{e}"))
}

fn scan_mflow(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let c = conf(s);
    let base = mflow_base(&c);
    let token = secret(root, "mflow_token").ok_or("先在 /凭证 存 mflow_token（MFlow 服务器的 MFLOW_API_TOKEN）")?;
    let only = list_of(&c, "only");
    let want = |label: &str| only.is_empty() || only.iter().any(|o| label.contains(o.as_str()));
    let client = http()?;
    // (标签, 路径, 日期)
    let mut files: Vec<(String, String, String)> = Vec::new();
    let tree = mflow_get(&client, &format!("{base}/api/kb/tree"), &token)?;
    for src in tree.as_array().into_iter().flatten() {
        let Some(name) = src["name"].as_str() else { continue };
        if !want(name) || src["files"].as_u64() == Some(0) {
            continue;
        }
        let list = mflow_get(&client, &format!("{base}/api/kb/list?src={}", urlenc(name)), &token)?;
        for f in list.as_array().into_iter().flatten() {
            if let Some(p) = f["path"].as_str() {
                files.push((name.to_string(), p.to_string(), f["date"].as_str().unwrap_or("").to_string()));
            }
        }
    }
    let reports = mflow_get(&client, &format!("{base}/api/reports"), &token)?;
    for (label, arr) in reports.as_object().into_iter().flatten() {
        if !want(label) || label.contains("会话") {
            continue;
        }
        for f in arr.as_array().into_iter().flatten() {
            if let Some(p) = f["path"].as_str() {
                files.push((label.clone(), p.to_string(), f["date"].as_str().unwrap_or("").to_string()));
            }
        }
    }
    files.sort_by(|a, b| b.2.cmp(&a.2));
    files.dedup_by(|a, b| a.1 == b.1);
    let mut out = Vec::new();
    for (label, path, date) in files.into_iter().take(MAX_ITEMS) {
        if !super::sources::scan_go() { break; }
        let key = format!("ext:{}/{path}", s.id);
        if let Some(old) = prev.get(&key).filter(|o| !date.is_empty() && o.etag.as_deref() == Some(date.as_str())) {
            out.push(old.clone());
            continue;
        }
        let v = match mflow_get(&client, &format!("{base}/api/content/raw?path={}", urlenc(&path)), &token) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let text = v["text"].as_str().unwrap_or("").to_string();
        let fname = path.rsplit('/').next().unwrap_or(&path).trim_end_matches(".md").to_string();
        let title = title_of_md(&text, &fname);
        let t = if date.len() >= 10 { parse_time(&date) } else { now_secs() as i64 };
        let mut it = item(root, s, &path, &title, &text, "md", &format!("{base}/ · {path}"), t, Some(date.clone()).filter(|d| !d.is_empty()));
        it.note = Some(label);
        out.push(it);
    }
    if out.is_empty() {
        return Err("MFlow 里没读到文档（知识中台和报告都是空的，或者被「只要这些」筛没了）".into());
    }
    Ok(out)
}

fn mflow_publish(root: &Path, s: &Source, title: &str, md: &str, doc_path: &str) -> Result<Value, String> {
    let c = conf(s);
    let base = mflow_base(&c);
    let token = secret(root, "mflow_token").ok_or("先在 /凭证 存 mflow_token")?;
    let ascii: String = title.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let mut id = ascii.split('-').filter(|x| !x.is_empty()).collect::<Vec<_>>().join("-");
    if id.len() < 3 {
        id = format!("litmus-{}", &key_id(doc_path)[..10]);
    }
    id.truncate(80);
    let body: String = body_without_title(md).chars().take(4000).collect();
    let resp = http()?
        .post(format!("{base}/api/item/upsert"))
        .header("X-MFlow-Token", token)
        .json(&json!({ "id": id, "topic": title, "brief": body, "source": "litmus", "category": "blog", "target_type": "blog" }))
        .send()
        .map_err(|e| format!("连不上 MFlow：{e}"))?;
    let st = resp.status();
    let v: Value = resp.json().unwrap_or(json!({}));
    if st.as_u16() == 401 || st.as_u16() == 403 || st.as_u16() == 405 {
        return Err("MFlow 没开机器写入：服务器 run/env.sh 里设 MFLOW_MACHINE_WRITE=1 后重启 mflow-console".into());
    }
    if !st.is_success() || v["ok"] == false {
        return Err(format!("MFlow 没收下（{st}）：{}", v["error"].as_str().or(v["out"].as_str()).unwrap_or("")));
    }
    Ok(json!({ "id": id, "queue": true, "url": format!("{base}/") }))
}

// ═══════════ MCP 系：Notion（本机 npx）与 OpenFlow（HTTP） ═══════════

fn mcp_cfg(root: &Path, s: &Source) -> Result<kernel_core::ConnectionConfig, String> {
    let c = conf(s);
    let mut cfg = kernel_core::ConnectionConfig { name: s.name.clone(), command: String::new(), args: vec![], env: Default::default(), url: None, headers: Default::default() };
    match s.kind.as_str() {
        "notion" => {
            let tok = secret(root, "notion_token").ok_or("先在 /凭证 存 notion_token（Notion 内部集成的令牌）")?;
            cfg.command = cs(&c, "command").unwrap_or("npx").to_string();
            cfg.args = if c.get("args").is_some() { list_of(&c, "args") } else { vec!["-y".into(), "@notionhq/notion-mcp-server".into()] };
            cfg.env.insert("NOTION_TOKEN".into(), tok.clone());
            cfg.env.insert("OPENAPI_MCP_HEADERS".into(), json!({ "Authorization": format!("Bearer {tok}"), "Notion-Version": "2022-06-28" }).to_string());
        }
        "openflow" => {
            let key = secret(root, "openflow_api_key").ok_or("先在 /凭证 存 openflow_api_key（OpenFlow 后台 → 开发者 → API Key）")?;
            cfg.url = Some(cs(&c, "url").unwrap_or("https://nownexts.com/mcp-server.php").to_string());
            cfg.headers.insert("X-Api-Key".into(), key);
        }
        k => return Err(format!("{k} 不是 MCP 对接")),
    }
    Ok(cfg)
}

fn tool_named(tools: &[Value], parts: &[&str]) -> Option<String> {
    tools.iter().filter_map(|t| t["name"].as_str()).find(|n| {
        let l = n.to_ascii_lowercase();
        parts.iter().any(|p| l == *p || l.ends_with(p))
    }).map(String::from)
}

fn call_json(c: &mut litmus_mcp::client::McpClient, tool: &str, args: Value) -> Result<Value, String> {
    let r = c.call_tool(tool, args).map_err(|e| e.to_string())?;
    if r["isError"] == true {
        return Err(litmus_mcp::client::extract_text(&r).unwrap_or_else(|| "工具报错".into()).chars().take(300).collect());
    }
    if let Some(sc) = r.get("structuredContent").filter(|v| !v.is_null()) {
        return Ok(sc.clone());
    }
    let t = litmus_mcp::client::extract_text(&r).unwrap_or_default();
    Ok(serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

fn notion_rich(rt: &Value) -> String {
    rt.as_array().into_iter().flatten().map(|r| {
        let t = r["plain_text"].as_str().or_else(|| r.pointer("/text/content").and_then(|x| x.as_str())).unwrap_or("").to_string();
        let a = &r["annotations"];
        let mut t = if a["code"] == true { format!("`{t}`") } else { t };
        if a["bold"] == true && !t.trim().is_empty() {
            t = format!("**{t}**");
        }
        match r["href"].as_str() {
            Some(h) if !t.trim().is_empty() => format!("[{t}]({h})"),
            _ => t,
        }
    }).collect()
}

/// Notion 块 → Markdown。
pub fn notion_blocks_md(blocks: &[Value]) -> String {
    let mut out = String::new();
    for b in blocks {
        let tp = b["type"].as_str().unwrap_or("");
        let d = &b[tp];
        let text = notion_rich(&d["rich_text"]);
        let line = match tp {
            "heading_1" => format!("# {text}\n\n"),
            "heading_2" => format!("## {text}\n\n"),
            "heading_3" => format!("### {text}\n\n"),
            "bulleted_list_item" => format!("- {text}\n"),
            "numbered_list_item" => format!("1. {text}\n"),
            "to_do" => format!("- [{}] {text}\n", if d["checked"] == true { "x" } else { " " }),
            "quote" | "callout" => format!("> {text}\n\n"),
            "code" => format!("```{}\n{text}\n```\n\n", d["language"].as_str().unwrap_or("")),
            "divider" => "---\n\n".into(),
            "toggle" => format!("**{text}**\n\n"),
            "child_page" => format!("- 子页面：{}\n", d["title"].as_str().unwrap_or("")),
            "bookmark" | "embed" | "link_preview" => d["url"].as_str().map(|u| format!("<{u}>\n\n")).unwrap_or_default(),
            _ if !text.is_empty() => format!("{text}\n\n"),
            _ => String::new(),
        };
        out.push_str(&line);
    }
    out.trim().to_string()
}

fn notion_title(page: &Value) -> String {
    page["properties"].as_object().into_iter().flatten().find(|(_, p)| p["type"] == "title").map(|(_, p)| notion_rich(&p["title"])).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "无标题".into())
}

fn scan_notion(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let cfg = mcp_cfg(root, s)?;
    let mut c = litmus_mcp::client::McpClient::connect(&cfg).map_err(|e| format!("Notion MCP 没起来（要本机有 Node 和 npx）：{e}"))?;
    let tools = c.list_tools().map_err(|e| e.to_string())?;
    let search = tool_named(&tools, &["post-search", "search"]).ok_or("这个 Notion MCP 没有搜索工具")?;
    let children = tool_named(&tools, &["get-block-children", "retrieve-block-children"]).ok_or("这个 Notion MCP 没有读块的工具")?;
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let mut a = json!({ "filter": { "property": "object", "value": "page" }, "page_size": 100 });
        if let Some(cu) = &cursor {
            a["start_cursor"] = json!(cu);
        }
        let v = call_json(&mut c, &search, a)?;
        pages.extend(v["results"].as_array().cloned().unwrap_or_default());
        cursor = v["next_cursor"].as_str().map(String::from).filter(|_| v["has_more"] == true);
        if cursor.is_none() || pages.len() >= 800 {
            break;
        }
    }
    let mut out = Vec::new();
    for p in pages.iter().take(800) {
        if !super::sources::scan_go() { break; }
        let Some(id) = p["id"].as_str() else { continue };
        if p["archived"] == true || p["in_trash"] == true {
            continue;
        }
        let edited = p["last_edited_time"].as_str().unwrap_or("").to_string();
        let key = format!("ext:{}/{id}", s.id);
        if let Some(old) = prev.get(&key).filter(|o| !edited.is_empty() && o.etag.as_deref() == Some(edited.as_str())) {
            out.push(old.clone());
            continue;
        }
        let v = call_json(&mut c, &children, json!({ "block_id": id, "page_size": 100 }))?;
        let body = notion_blocks_md(v["results"].as_array().map(|a| a.as_slice()).unwrap_or(&[]));
        let title = notion_title(p);
        let url = p["url"].as_str().unwrap_or("").to_string();
        out.push(item(root, s, id, &title, &body, "notion", &url, parse_time(&edited), Some(edited)));
    }
    Ok(out)
}

/// Markdown → Notion 块（标题、列表、待办、引用、代码、段落）。
pub fn md_to_notion(md: &str) -> Vec<Value> {
    let rt = |t: &str| json!([{ "type": "text", "text": { "content": t.chars().take(1900).collect::<String>() } }]);
    let blk = |tp: &str, t: &str| json!({ "object": "block", "type": tp, tp: { "rich_text": rt(t) } });
    let mut out = Vec::new();
    let mut code = String::new();
    let mut in_code = false;
    let body = md.strip_prefix("---\n").and_then(|r| r.find("\n---\n").map(|i| &r[i + 5..])).unwrap_or(md);
    let mut lines = body.lines().peekable();
    // 第一行一级标题是页面标题，不再重复
    while let Some(l) = lines.peek().copied() {
        if l.trim().is_empty() || l.trim_start().starts_with("# ") {
            lines.next();
            if l.trim_start().starts_with("# ") {
                break;
            }
        } else {
            break;
        }
    }
    for line in lines {
        if line.trim_start().starts_with("```") {
            if in_code {
                out.push(json!({ "object": "block", "type": "code", "code": { "rich_text": rt(code.trim_end()), "language": "plain text" } }));
                code.clear();
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let b = if let Some(x) = t.strip_prefix("### ") {
            blk("heading_3", x)
        } else if let Some(x) = t.strip_prefix("## ") {
            blk("heading_2", x)
        } else if let Some(x) = t.strip_prefix("# ") {
            blk("heading_1", x)
        } else if let Some(x) = t.strip_prefix("- [ ] ").or_else(|| t.strip_prefix("- [x] ")) {
            json!({ "object": "block", "type": "to_do", "to_do": { "rich_text": rt(x), "checked": t.starts_with("- [x]") } })
        } else if let Some(x) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            blk("bulleted_list_item", x)
        } else if let Some(x) = t.strip_prefix("> ") {
            blk("quote", x)
        } else if t == "---" {
            json!({ "object": "block", "type": "divider", "divider": {} })
        } else if t.find(". ").map(|d| t[..d].chars().all(|c| c.is_ascii_digit()) && d > 0).unwrap_or(false) {
            blk("numbered_list_item", &t[t.find(". ").unwrap() + 2..])
        } else {
            blk("paragraph", t)
        };
        out.push(b);
    }
    out
}

/// "https://www.notion.so/xxx/标题-1a2b3c…" 或 32 位 ID（带不带连字符都行）→ 带连字符的 UUID。
pub fn notion_id(s: &str) -> Option<String> {
    let base = s.split(['?', '#']).next().unwrap_or(s).trim().trim_end_matches('/');
    let flat: Vec<char> = base.chars().filter(|c| *c != '-').collect();
    if flat.len() < 32 {
        return None;
    }
    let h: String = flat[flat.len() - 32..].iter().collect();
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let h = h.to_ascii_lowercase();
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

fn notion_publish(root: &Path, s: &Source, title: &str, md: &str, opts: &Value) -> Result<Value, String> {
    let c = conf(s);
    let w = c.get("write").cloned().unwrap_or(json!({}));
    let parent = cs(opts, "parent").or_else(|| cs(&w, "parent")).and_then(notion_id).ok_or("还没设发到哪个 Notion 页面下（页面链接或 ID；那个页面要「连接」到你的集成）")?;
    let cfg = mcp_cfg(root, s)?;
    let mut cl = litmus_mcp::client::McpClient::connect(&cfg).map_err(|e| format!("Notion MCP 没起来：{e}"))?;
    let tools = cl.list_tools().map_err(|e| e.to_string())?;
    let create = tool_named(&tools, &["post-page", "create-a-page", "create-pages"]).ok_or("这个 Notion MCP 没有建页面的工具")?;
    let append = tool_named(&tools, &["patch-block-children", "append-block-children"]);
    let blocks = md_to_notion(md);
    let first: Vec<Value> = blocks.iter().take(100).cloned().collect();
    let v = call_json(&mut cl, &create, json!({ "parent": { "page_id": parent }, "properties": { "title": { "title": [{ "text": { "content": title } }] } }, "children": first }))?;
    let id = v["id"].as_str().ok_or_else(|| format!("Notion 没回页面 ID：{}", v.to_string().chars().take(200).collect::<String>()))?.to_string();
    if blocks.len() > 100 {
        let ap = append.ok_or("页面太长：这个 Notion MCP 不支持追加块")?;
        for chunk in blocks[100..].chunks(100) {
            call_json(&mut cl, &ap, json!({ "block_id": id, "children": chunk }))?;
        }
    }
    Ok(json!({ "id": id, "url": v["url"].clone() }))
}

fn openflow_site(c: &Value) -> String {
    cs(c, "site").map(String::from).unwrap_or_else(|| {
        let u = cs(c, "url").unwrap_or("https://nownexts.com/mcp-server.php");
        u.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_else(|| u.to_string())
    })
}

fn html_to_md(html: &str) -> String {
    if !html.contains('<') {
        return html.to_string();
    }
    kernel_convert::convert("html", html.as_bytes()).map(|c| c.markdown).unwrap_or_else(|_| html.to_string())
}

fn scan_openflow(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let c = conf(s);
    let site = openflow_site(&c);
    let cfg = mcp_cfg(root, s)?;
    let mut cl = litmus_mcp::client::McpClient::connect(&cfg).map_err(|e| format!("连不上 OpenFlow 的 MCP：{e}"))?;
    let tools = cl.list_tools().map_err(|e| e.to_string())?;
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    let mut out = Vec::new();
    if names.contains(&"articles_list") {
        // articles_list 一次最多给 20 篇：按状态、再按出现过的分类各问一遍，尽量多拿
        let mut seen: HashMap<String, Value> = HashMap::new();
        let mut cats: Vec<String> = Vec::new();
        let mut ask = |cl: &mut litmus_mcp::client::McpClient, a: Value, seen: &mut HashMap<String, Value>, cats: &mut Vec<String>| {
            if let Ok(Value::Array(arr)) = call_json(cl, "articles_list", a) {
                for x in arr {
                    if let Some(cat) = x["category"].as_str() {
                        if !cats.contains(&cat.to_string()) {
                            cats.push(cat.to_string());
                        }
                    }
                    if let Some(id) = x["id"].as_str() {
                        seen.entry(id.to_string()).or_insert(x.clone());
                    }
                }
            }
        };
        for a in [json!({ "limit": 500 }), json!({ "status": "published", "limit": 500 }), json!({ "status": "draft", "limit": 500 })] {
            ask(&mut cl, a, &mut seen, &mut cats);
        }
        for cat in cats.clone() {
            for st in ["published", "draft"] {
                ask(&mut cl, json!({ "category": cat, "status": st, "limit": 500 }), &mut seen, &mut cats);
            }
        }
        for (id, a) in seen.into_iter().take(MAX_ITEMS) {
            if !super::sources::scan_go() { break; }
            let key = format!("ext:{}/article/{id}", s.id);
            let upd = a["updated_at"].as_str().unwrap_or("").to_string();
            if let Some(old) = prev.get(&key).filter(|o| !upd.is_empty() && o.etag.as_deref() == Some(upd.as_str())) {
                out.push(old.clone());
                continue;
            }
            let full = if a.get("content").and_then(|x| x.as_str()).is_some() { a.clone() } else { call_json(&mut cl, "article_get", json!({ "id": id })).unwrap_or(a.clone()) };
            let title = full["title"].as_str().unwrap_or("OpenFlow 文章").to_string();
            let body = html_to_md(full["content"].as_str().unwrap_or(""));
            let slug = full["slug"].as_str().unwrap_or(&id).to_string();
            let t = full["updated_at"].as_str().or(full["created_at"].as_str()).map(|d| parse_time(&d.replace(' ', "T"))).unwrap_or(now_secs() as i64);
            let mut it = item(root, s, &format!("article/{id}"), &title, &body, "article", &format!("{site}/article/{slug}"), t, Some(upd).filter(|u| !u.is_empty()));
            it.note = Some(format!("{} · {}", full["category"].as_str().unwrap_or(""), full["status"].as_str().unwrap_or("")));
            out.push(it);
        }
    }
    if names.contains(&"skills_list") {
        if let Ok(Value::Array(arr)) = call_json(&mut cl, "skills_list", json!({})) {
            for sk in arr {
                let Some(id) = sk["id"].as_str() else { continue };
                let title = sk["title"].as_str().unwrap_or(id).to_string();
                let body = format!("{}\n\n- 类型：{}", sk["description"].as_str().unwrap_or(""), sk["type"].as_str().unwrap_or(""));
                out.push(item(root, s, &format!("skill/{id}"), &title, &body, "skill", &format!("{site}/marketplace"), now_secs() as i64, None));
            }
        }
    }
    if out.is_empty() {
        return Err("OpenFlow 没给出文章或技能：检查 API Key 有没有 articles 权限".into());
    }
    Ok(out)
}

fn openflow_publish(root: &Path, s: &Source, title: &str, md: &str, opts: &Value) -> Result<Value, String> {
    let c = conf(s);
    let w = c.get("write").cloned().unwrap_or(json!({}));
    let cfg = mcp_cfg(root, s)?;
    let mut cl = litmus_mcp::client::McpClient::connect(&cfg).map_err(|e| format!("连不上 OpenFlow 的 MCP：{e}"))?;
    let html = kernel_core::to_html(&kernel_core::from_markdown(&body_without_title(md)).unwrap_or_default());
    let body = html.split("<body>").nth(1).and_then(|b| b.split("</body>").next()).unwrap_or(&html).to_string();
    let cat = cs(opts, "category").or_else(|| cs(&w, "category")).unwrap_or("insight");
    let v = call_json(&mut cl, "article_create", json!({ "title": title, "content": body, "category": cat, "status": "draft" }))?;
    let id = v["id"].as_str().ok_or_else(|| format!("OpenFlow 没建成：{}", v.to_string().chars().take(200).collect::<String>()))?.to_string();
    let mut published = false;
    if opts["publish"] == true {
        call_json(&mut cl, "article_publish", json!({ "id": id }))?;
        published = true;
    }
    Ok(json!({ "id": id, "published": published, "url": format!("{}/xmp/content-hub?tab=articles", openflow_site(&c)) }))
}

// ═══════════ WebsFlow（开放工具 API：/api/tools/call） ═══════════

fn wf_base(c: &Value) -> String {
    cs(c, "url").unwrap_or("https://nownexts.com/webflow").trim_end_matches('/').to_string()
}
/// "https://a.com/webflow" → "https://a.com"（托管页地址是 /webflow/p/<token> 这种站内路径）
fn origin_of(base: &str) -> String {
    match base.find("://") {
        Some(i) => base[..i + 3 + base[i + 3..].find('/').unwrap_or(base.len() - i - 3)].to_string(),
        None => base.to_string(),
    }
}
fn wf_call(client: &reqwest::blocking::Client, base: &str, key: &str, name: &str, args: Value) -> Result<Value, String> {
    let resp = client.post(format!("{base}/api/tools/call")).bearer_auth(key).json(&json!({ "name": name, "args": args })).send().map_err(|e| format!("连不上 WebsFlow：{e}"))?;
    let st = resp.status();
    let v: Value = resp.json().unwrap_or(json!({}));
    if st.as_u16() == 401 {
        return Err("WebsFlow 不认这个密钥（/凭证 里的 websflow_api_key，控制台「设置 → API 密钥」里建的 wfk_ 开头那串）".into());
    }
    if st.as_u16() == 403 {
        return Err(format!("WebsFlow 拒绝了：{}（读要 read 权限，发回去要 write）", v["error"].as_str().unwrap_or("密钥权限不够")));
    }
    if !st.is_success() || v["ok"] == false {
        return Err(format!("WebsFlow {name} 没成（{st}）：{}", v["error"].as_str().unwrap_or("")));
    }
    Ok(v["result"].clone())
}
fn num_s(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::Null => "—".into(),
        Value::String(s) => s.clone(),
        x => x.to_string(),
    }
}

fn scan_websflow(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let c = conf(s);
    let base = wf_base(&c);
    let origin = origin_of(&base);
    let key = secret(root, "websflow_api_key").ok_or("先在 /凭证 存 websflow_api_key（WebsFlow 控制台「设置 → API 密钥」）")?;
    let client = http()?;
    let pages = wf_call(&client, &base, &key, "list_pages", json!({}))?;
    let today = ymd(now_secs() as i64);
    let mut out = Vec::new();
    for p in pages["pages"].as_array().into_iter().flatten().take(MAX_ITEMS) {
        if !super::sources::scan_go() { break; }
        let Some(id) = p["id"].as_str().map(String::from).or_else(|| p["id"].as_i64().map(|n| n.to_string())) else { continue };
        let upd = p["updated_at"].as_str().unwrap_or("").to_string();
        let published = p["published"] == true;
        // 数据每天变：同一天、页面没改就不重读
        let etag = format!("{upd}|{published}|{today}");
        let key_s = format!("ext:{}/page/{id}", s.id);
        if let Some(old) = prev.get(&key_s).filter(|o| o.etag.as_deref() == Some(etag.as_str())) {
            out.push(old.clone());
            continue;
        }
        let name = p["name"].as_str().unwrap_or("WebsFlow 页面").to_string();
        let url = p["url"].as_str().map(|u| if u.starts_with("http") { u.to_string() } else { format!("{origin}{u}") });
        let mode = match p["mode"].as_str().unwrap_or("site") { "h5" => "H5", "ppt" => "演示", "story" => "故事线", _ => "落地页" };
        let mut md = format!("- 形态：{mode}\n- {}\n", if published { format!("已发布：{}", url.clone().unwrap_or_default()) } else { "还没发布".to_string() });
        // 正文：发布了就读托管页（服务端直出，不会算进访问数据）；没发布只有各模块的摘要
        let mut body = String::new();
        if let (true, Some(u)) = (published, url.as_ref()) {
            if let Ok(r) = client.get(u).send() {
                if r.status().is_success() {
                    // 托管页的大标题是转换器拿去当 title 的：放回正文，它往往就是主卖点
                    if let Ok(cv) = kernel_convert::convert("html", r.text().unwrap_or_default().as_bytes()) {
                        body = match cv.title.filter(|t| !t.trim().is_empty() && !cv.markdown.contains(t.trim())) {
                            Some(t) => format!("**{}**\n\n{}", t.trim(), cv.markdown),
                            None => cv.markdown,
                        };
                    }
                }
            }
        }
        if body.trim().is_empty() {
            if let Ok(g) = wf_call(&client, &base, &key, "get_page", json!({ "project_id": id })) {
                let lines: Vec<String> = g["blocks"].as_array().into_iter().flatten().filter(|b| b["hidden"] != true).map(|b| {
                    let sum = b["summary"].as_str().unwrap_or("").trim();
                    format!("- {}{}", b["type"].as_str().unwrap_or("模块"), if sum.is_empty() { String::new() } else { format!("：{sum}") })
                }).collect();
                if !lines.is_empty() {
                    body = format!("页面由这些模块组成：\n\n{}", lines.join("\n"));
                }
            }
        }
        if !body.trim().is_empty() {
            md.push_str(&format!("\n## 页面内容\n\n{}\n", body.trim()));
        }
        if let Ok(st) = wf_call(&client, &base, &key, "page_stats", json!({ "project_id": id, "days": 14 })) {
            if st.is_object() {
                md.push_str(&format!("\n## 近 {} 天数据（{today}）\n\n| 曝光 | 点击 | 线索 | 点击率 |\n|---|---|---|---|\n| {} | {} | {} | {} |\n",
                    num_s(&st["days"]).replace('—', "14"), num_s(&st["views"]), num_s(&st["clicks"]), num_s(&st["leads"]),
                    if st["cvr"].is_null() { "—".to_string() } else { format!("{}%", num_s(&st["cvr"])) }));
            }
        }
        let t = if upd.len() >= 10 { parse_time(&upd.replace(' ', "T")) } else { now_secs() as i64 };
        let mut it = item(root, s, &format!("page/{id}"), &name, &md, "page", &url.unwrap_or_else(|| format!("{base}/")), t, Some(etag));
        it.note = Some(format!("{mode} · {}", if published { "已发布" } else { "草稿" }));
        out.push(it);
    }
    // 平台经验：哪些改法常赢、哪些常输
    if let Ok(l) = wf_call(&client, &base, &key, "lessons_summary", json!({})) {
        let rows: Vec<&Value> = l["summary"].as_array().into_iter().flatten().filter(|r| r["decided"].as_u64().unwrap_or(0) > 0).collect();
        if !rows.is_empty() {
            let mut md = "WebsFlow 记下的投放经验：同一个场景下，每种改法赢了几次。\n\n| 场景 | 改法 | 赢 / 可判定 | 胜率 |\n|---|---|---|---|\n".to_string();
            for r in rows.iter().take(200) {
                let rate = r["winRate"].as_f64().map(|x| format!("{:.0}%", x * 100.0)).unwrap_or("—".into());
                md.push_str(&format!("| {} | {} | {} / {} | {rate} |\n", r["scene_label"].as_str().or(r["scene"].as_str()).unwrap_or(""), r["action_label"].as_str().or(r["action"].as_str()).unwrap_or(""), num_s(&r["wins"]), num_s(&r["decided"])));
            }
            out.push(item(root, s, "lessons", "WebsFlow 投放经验", &md, "lessons", &format!("{base}/"), now_secs() as i64, None));
        }
    }
    if out.is_empty() {
        return Err("WebsFlow 里还没有页面".into());
    }
    Ok(out)
}

fn websflow_publish(root: &Path, s: &Source, title: &str, md: &str, opts: &Value) -> Result<Value, String> {
    let c = conf(s);
    let w = c.get("write").cloned().unwrap_or(json!({}));
    let base = wf_base(&c);
    let key = secret(root, "websflow_api_key").ok_or("先在 /凭证 存 websflow_api_key")?;
    let mode = cs(opts, "mode").or_else(|| cs(&w, "mode")).filter(|m| ["site", "h5", "ppt", "story"].contains(m)).unwrap_or("site");
    let body: String = body_without_title(md).chars().take(1800).collect();
    let prompt = format!("按这份资料做一个{}：{title}\n\n{body}", match mode { "h5" => "H5 页面", "ppt" => "演示页", "story" => "故事线页面", _ => "落地页" });
    let publish = opts["publish"] == true;
    let r = wf_call(&http()?, &base, &key, "create_page", json!({ "prompt": prompt, "name": title.chars().take(60).collect::<String>(), "mode": mode, "publish": publish }))?;
    let id = r["id"].as_str().map(String::from).or_else(|| r["id"].as_i64().map(|n| n.to_string())).ok_or_else(|| format!("WebsFlow 没建成：{}", r.to_string().chars().take(200).collect::<String>()))?;
    let url = r["url"].as_str().map(|u| if u.starts_with("http") { u.to_string() } else { format!("{}{u}", origin_of(&base)) }).unwrap_or_else(|| format!("{base}/"));
    Ok(json!({ "id": id, "published": publish && r["url"].is_string(), "url": url }))
}

// ═══════════ PayFlow（开放 API v1，只读：商品与经营汇总） ═══════════

fn pf_base(c: &Value) -> String {
    cs(c, "url").unwrap_or("https://nownexts.com/payflow").trim_end_matches('/').to_string()
}
fn pf_get(client: &reqwest::blocking::Client, url: &str, key: &str) -> Result<Value, String> {
    let resp = client.get(url).bearer_auth(key).send().map_err(|e| format!("连不上 PayFlow：{e}"))?;
    let st = resp.status();
    let v: Value = resp.json().unwrap_or(json!({}));
    if st.as_u16() == 401 || st.as_u16() == 403 {
        return Err("PayFlow 不认这个密钥（/凭证 里的 payflow_api_key，格式是 key_id.secret，在 PayFlow 后台「API」页新建）".into());
    }
    if st.as_u16() == 429 {
        return Err("PayFlow 说调得太勤了，过一会儿再扫".into());
    }
    if !st.is_success() || v["ok"] == false {
        return Err(format!("PayFlow 返回 {st}：{}", v["error"].as_str().unwrap_or("")));
    }
    Ok(v)
}
fn money(cents: &Value, cur: &str) -> String {
    let n = cents.as_f64().unwrap_or(0.0) / 100.0;
    let sym = match cur { "CNY" | "" => "¥", "USD" => "$", "EUR" => "€", x => x };
    format!("{sym}{n:.2}")
}

fn scan_payflow(root: &Path, s: &Source, _prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let c = conf(s);
    let base = pf_base(&c);
    let key = secret(root, "payflow_api_key").ok_or("先在 /凭证 存 payflow_api_key（PayFlow 后台「API」页新建，key_id.secret）")?;
    let days = c.get("days").and_then(|d| d.as_u64().or_else(|| d.as_str().and_then(|x| x.trim().parse().ok()))).unwrap_or(30).clamp(1, 365);
    let client = http()?;
    let now = now_secs() as i64;
    let today = ymd(now);
    let mut out = Vec::new();
    let prods = pf_get(&client, &format!("{base}/api/v1/products"), &key)?;
    let list: Vec<&Value> = prods["products"].as_array().into_iter().flatten().collect();
    if !list.is_empty() {
        let mut md = "在售的商品（PayFlow）。\n\n| 商品 | 类型 | 价格 | 周期 | 标识 |\n|---|---|---|---|---|\n".to_string();
        for p in &list {
            let ty = match p["type"].as_str().unwrap_or("") { "subscription" => "订阅", "one_time" | "onetime" | "once" => "一次性", "license" => "授权", x => x };
            let iv = match p["interval"].as_str().unwrap_or("") { "month" | "monthly" => "按月", "year" | "yearly" => "按年", "week" => "按周", "" => "—", x => x };
            md.push_str(&format!("| {} | {ty} | {} | {iv} | `{}` |\n", p["name"].as_str().unwrap_or("").replace('|', "/"), money(&p["amount_cents"], p["currency"].as_str().unwrap_or("CNY")), p["slug"].as_str().or(p["id"].as_str()).unwrap_or("")));
        }
        out.push(item(root, s, "products", "PayFlow 商品目录", &md, "catalog", &format!("{base}/"), now, Some(today.clone())));
    }
    let sm = pf_get(&client, &format!("{base}/api/v1/analytics/summary?days={days}"), &key)?;
    let m = &sm["summary"];
    if m.is_object() {
        let cur = "CNY";
        let mut md = format!("PayFlow 近 {days} 天的经营情况，截至 {today}（不含沙箱订单）。\n\n## 收入\n\n| 成交额 | 客单价 | 退款 | 优惠 |\n|---|---|---|---|\n| {} | {} | {} | {} |\n\n## 订单\n\n| 下单 | 付款 | 已发货 | 退款 | 转化率 | 新客户 |\n|---|---|---|---|---|---|\n| {} | {} | {} | {} | {}% | {} |\n",
            money(&m["gmv_cents"], cur), money(&m["aov_cents"], cur), money(&m["refund_cents"], cur), money(&m["discount_cents"], cur),
            num_s(&m["created"]), num_s(&m["paid"]), num_s(&m["delivered"]), num_s(&m["refunded"]), num_s(&m["conversion_rate"]), num_s(&m["new_customers"]));
        let sub = &m["subscriptions"];
        if sub.is_object() {
            md.push_str(&format!("\n## 订阅\n\n| 有效 | 欠费 | 已取消 | 月经常性收入 |\n|---|---|---|---|\n| {} | {} | {} | {} |\n", num_s(&sub["active"]), num_s(&sub["past_due"]), num_s(&sub["canceled"]), money(&sub["mrr_cents"], cur)));
        }
        if let Some(ch) = m["channels"].as_object().filter(|o| !o.is_empty()) {
            md.push_str("\n## 支付渠道\n\n| 渠道 | 笔数 | 成交额 |\n|---|---|---|\n");
            for (k, v) in ch {
                md.push_str(&format!("| {k} | {} | {} |\n", num_s(&v["count"]), money(&v["gmv"], cur)));
            }
        }
        let cm = &m["commission"];
        if cm.is_object() {
            md.push_str(&format!("\n## 推荐佣金\n\n| 冻结中 | 可提现 | 已付 | 已冲回 |\n|---|---|---|---|\n| {} | {} | {} | {} |\n", money(&cm["pending"], cur), money(&cm["available"], cur), money(&cm["paid"], cur), money(&cm["reversed"], cur)));
        }
        let mut it = item(root, s, "summary", &format!("PayFlow 经营概览（近 {days} 天）"), &md, "report", &format!("{base}/"), now, Some(today));
        it.note = Some(format!("截至 {}", ymd(now)));
        out.push(it);
    }
    if out.is_empty() {
        return Err("PayFlow 里还没有在售的商品，也没拿到经营汇总".into());
    }
    Ok(out)
}

// ═══════════ inFlow（REST v1：洞察 + 工作区记忆；发回去 = 写进工作区记忆） ═══════════

fn if_base(c: &Value) -> String {
    cs(c, "url").unwrap_or("https://nownexts.com/inflow").trim_end_matches('/').to_string()
}
fn if_req(rb: reqwest::blocking::RequestBuilder, key: Option<&str>) -> Result<Value, String> {
    let rb = match key { Some(k) => rb.header("X-API-Key", k), None => rb };
    let resp = rb.send().map_err(|e| format!("连不上 inFlow：{e}"))?;
    let st = resp.status();
    if st.as_u16() == 401 || st.as_u16() == 403 {
        return Err("inFlow 要 API Key：在 /凭证 存 inflow_api_key（inFlow 后台或 POST /api/v1/auth/keys 建，读要 read，发回去要 write）".into());
    }
    if st.as_u16() == 404 {
        return Err("inFlow 找不到这个工作区（「工作区」填的 id 对不上）".into());
    }
    if !st.is_success() {
        return Err(format!("inFlow 返回 {st}"));
    }
    resp.json().map_err(|e| format!("inFlow 回的不是 JSON：{e}"))
}
/// (id, 名字)
fn if_workspaces(client: &reqwest::blocking::Client, base: &str, c: &Value, key: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let want = list_of(c, "workspace");
    let v = if_req(client.get(format!("{base}/api/v1/workspaces")), key);
    let all: Vec<(String, String)> = v.as_ref().map(|v| v["workspaces"].as_array().into_iter().flatten().filter_map(|w| Some((w["id"].as_str()?.to_string(), w["name"].as_str().unwrap_or("").to_string()))).collect()).unwrap_or_default();
    if !want.is_empty() {
        return Ok(want.into_iter().map(|w| { let n = all.iter().find(|(i, _)| *i == w).map(|x| x.1.clone()).unwrap_or_default(); (w, n) }).collect());
    }
    let all = match v { Ok(_) => all, Err(e) => return Err(e) };
    if all.is_empty() {
        return Err("inFlow 里还没有工作区".into());
    }
    Ok(all)
}
fn tags_of(v: &Value, k: &str) -> Vec<String> {
    let x = &v[k];
    let arr = match x { Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(json!([])), x => x.clone() };
    arr.as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect()
}

fn scan_inflow(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Result<Vec<Item>, String> {
    let c = conf(s);
    let base = if_base(&c);
    let key = secret(root, "inflow_api_key");
    let client = http()?;
    let wss = if_workspaces(&client, &base, &c, key.as_deref())?;
    let multi = wss.len() > 1;
    let mut out = Vec::new();
    for (ws, wname) in &wss {
        if !super::sources::scan_go() { break; }
        let label = if wname.is_empty() { ws.clone() } else { wname.clone() };
        let ins = if_req(client.get(format!("{base}/api/v1/insights")).query(&[("workspace_id", ws.as_str()), ("limit", "200")]), key.as_deref())?;
        for i in ins["insights"].as_array().into_iter().flatten() {
            if out.len() >= MAX_ITEMS { break; }
            let Some(id) = i["id"].as_str().filter(|x| !x.is_empty()) else { continue };
            let status = i["status"].as_str().unwrap_or("new");
            if status == "dismissed" {
                continue;
            }
            let etag = format!("{status}|{}", i["verified_at"].as_str().unwrap_or(""));
            let k = format!("ext:{}/insight/{id}", s.id);
            if let Some(old) = prev.get(&k).filter(|o| o.etag.as_deref() == Some(etag.as_str())) {
                out.push(old.clone());
                continue;
            }
            let title = i["title"].as_str().unwrap_or("inFlow 洞察").to_string();
            let sev = match i["severity"].as_str().unwrap_or("") { "critical" => "严重", "high" => "高", "medium" => "中", "low" => "低", "info" => "提示", x => x };
            let st_cn = match status { "new" => "新", "acknowledged" => "已确认", "actioned" => "已行动", "verified" => "已验证", x => x };
            let mut md = format!("{}\n\n- 重要程度：{sev}\n- 置信度：{:.0}%\n- 状态：{st_cn}\n- 类型：{}\n- 工作区：{label}\n", i["summary"].as_str().unwrap_or("").trim(), i["confidence"].as_f64().unwrap_or(0.0) * 100.0, i["type"].as_str().unwrap_or(""));
            let ev: Vec<String> = i["evidence_json"].as_array().into_iter().flatten().take(20).map(|e| match e.as_object() {
                Some(o) => format!("- {}", o.iter().map(|(k, v)| format!("{k}：{}", match v { Value::String(s) => s.clone(), x => x.to_string() })).collect::<Vec<_>>().join("，").chars().take(400).collect::<String>()),
                None => format!("- {e}"),
            }).collect();
            if !ev.is_empty() {
                md.push_str(&format!("\n## 证据\n\n{}\n", ev.join("\n")));
            }
            let acts: Vec<String> = i["actions_json"].as_array().into_iter().flatten().map(|a| {
                let d = a["description"].as_str().unwrap_or("").trim();
                format!("- {}{}", if d.is_empty() { "（没写说明）" } else { d }, a["action_type"].as_str().map(|t| format!("（{t}）")).unwrap_or_default())
            }).collect();
            if !acts.is_empty() {
                md.push_str(&format!("\n## 推荐动作\n\n{}\n", acts.join("\n")));
            }
            let t = i["created_at"].as_str().map(parse_time).unwrap_or(now_secs() as i64);
            let mut it = item(root, s, &format!("insight/{id}"), &title, &md, "insight", &format!("{base}/console/insights"), t, Some(etag));
            it.note = Some(if multi { format!("{label} · {sev} · {st_cn}") } else { format!("{sev} · {st_cn}") });
            out.push(it);
        }
        let notes = if_req(client.get(format!("{base}/api/v1/agent/notes")).query(&[("workspace_id", ws.as_str()), ("limit", "200")]), key.as_deref()).unwrap_or(json!({}));
        for n in notes["notes"].as_array().into_iter().flatten() {
            if out.len() >= MAX_ITEMS { break; }
            // 从鹿蕊发过去的那些不再接回来，免得场上出现两份
            if n["author"] == "litmus" {
                continue;
            }
            let Some(id) = n["id"].as_str().filter(|x| !x.is_empty()) else { continue };
            let title = n["title"].as_str().unwrap_or("inFlow 记忆").to_string();
            let tags = tags_of(n, if n.get("tags").is_some() { "tags" } else { "tags_json" });
            let md = format!("{}{}", n["body"].as_str().unwrap_or("").trim(), if tags.is_empty() { String::new() } else { format!("\n\n标签：{}", tags.join("、")) });
            let upd = n["updated_at"].as_str().unwrap_or("").to_string();
            let t = if upd.len() >= 10 { parse_time(&upd) } else { now_secs() as i64 };
            let mut it = item(root, s, &format!("note/{id}"), &title, &md, "note", &format!("{base}/console"), t, Some(format!("{upd}|{}", num_s(&n["version"]))));
            it.note = Some(if multi { format!("{label} · 记忆") } else { "记忆".into() });
            out.push(it);
        }
    }
    if out.is_empty() {
        return Err("inFlow 里没有洞察，也没有工作区记忆".into());
    }
    Ok(out)
}

fn inflow_publish(root: &Path, s: &Source, title: &str, md: &str, doc_path: &str, opts: &Value) -> Result<Value, String> {
    let c = conf(s);
    let w = c.get("write").cloned().unwrap_or(json!({}));
    let base = if_base(&c);
    let key = secret(root, "inflow_api_key");
    let client = http()?;
    let ws = match cs(opts, "workspace").or_else(|| cs(&w, "workspace")) {
        Some(x) => x.to_string(),
        None => if_workspaces(&client, &base, &c, key.as_deref())?.into_iter().next().map(|x| x.0).ok_or("inFlow 里还没有工作区")?,
    };
    let body: String = body_without_title(md).chars().take(8000).collect();
    let note_key = format!("litmus-{}", key_id(doc_path));
    let v = if_req(client.post(format!("{base}/api/v1/agent/notes")).json(&json!({ "workspace_id": ws, "title": title, "body": body, "tags": ["litmus"], "note_key": note_key, "author": "litmus" })), key.as_deref())?;
    if v["ok"] == false {
        return Err(format!("inFlow 没收下：{}", v["detail"].as_str().or(v["error"].as_str()).unwrap_or("")));
    }
    Ok(json!({ "id": v["note"]["id"].as_str().unwrap_or(&note_key), "workspace": ws, "version": v["note"]["version"].clone(), "url": format!("{base}/console") }))
}

// ═══════════ 入口 ═══════════

/// sources::start_scan 调这里：返回 None 表示不是这几种。
pub fn scan(root: &Path, s: &Source, prev: &HashMap<String, Item>) -> Option<Result<Vec<Item>, String>> {
    Some(match s.kind.as_str() {
        "github" => scan_github(root, s, prev),
        "sanity" => scan_sanity(root, s, prev),
        "mflow" => scan_mflow(root, s, prev),
        "notion" => scan_notion(root, s, prev),
        "openflow" => scan_openflow(root, s, prev),
        "websflow" => scan_websflow(root, s, prev),
        "payflow" => scan_payflow(root, s, prev),
        "inflow" => scan_inflow(root, s, prev),
        _ => return None,
    })
}

pub const KINDS: [&str; 8] = ["github", "notion", "sanity", "openflow", "mflow", "websflow", "payflow", "inflow"];
/// PayFlow 只读：收钱的系统不往里写东西。
pub const WRITABLE: [&str; 7] = ["github", "notion", "sanity", "openflow", "mflow", "websflow", "inflow"];

/// 测连接：最便宜的一次调用。
fn probe(root: &Path, s: &Source) -> Result<String, String> {
    let c = conf(s);
    match s.kind.as_str() {
        "github" => {
            let client = http()?;
            let t = gh_token(root);
            match (&t, list_of(&c, "repos").first()) {
                (_, Some(r)) => gh_get(&client, &format!("{}/repos/{r}", gh_api(&c)), t.as_deref()).map(|v| format!("能读 {}（{}）", r, v["default_branch"].as_str().unwrap_or("main"))),
                (Some(tk), None) => gh_get(&client, &format!("{}/user", gh_api(&c)), Some(tk)).map(|v| format!("已登录为 {}", v["login"].as_str().unwrap_or("?"))),
                (None, None) => Err("没有令牌，也没填仓库".into()),
            }
        }
        "sanity" => {
            let (base, dataset, ver) = sanity_base(&c)?;
            let mut r = http()?.get(format!("{base}/v{ver}/data/query/{dataset}")).query(&[("query", "count(*[!(_id in path(\"drafts.**\"))])")]);
            if let Some(t) = secret(root, "sanity_token") {
                r = r.bearer_auth(t);
            }
            let v: Value = r.send().map_err(|e| e.to_string())?.json().map_err(|e| e.to_string())?;
            v["result"].as_u64().map(|n| format!("数据集里有 {n} 篇已发布文档")).ok_or_else(|| format!("Sanity：{}", v.pointer("/error/description").and_then(|m| m.as_str()).unwrap_or("查询失败")))
        }
        "mflow" => {
            let token = secret(root, "mflow_token").ok_or("先在 /凭证 存 mflow_token")?;
            let v = mflow_get(&http()?, &format!("{}/api/kb/tree", mflow_base(&c)), &token)?;
            let n: u64 = v.as_array().into_iter().flatten().filter_map(|x| x["files"].as_u64()).sum();
            Ok(format!("连上了：知识中台 {} 个库、{n} 篇", v.as_array().map(|a| a.len()).unwrap_or(0)))
        }
        "websflow" => {
            let key = secret(root, "websflow_api_key").ok_or("先在 /凭证 存 websflow_api_key")?;
            let v = wf_call(&http()?, &wf_base(&c), &key, "list_pages", json!({}))?;
            Ok(format!("连上了：{} 个页面", v["pages"].as_array().map(|a| a.len()).unwrap_or(0)))
        }
        "payflow" => {
            let key = secret(root, "payflow_api_key").ok_or("先在 /凭证 存 payflow_api_key")?;
            let v = pf_get(&http()?, &format!("{}/api/v1/products", pf_base(&c)), &key)?;
            Ok(format!("连上了：{} 个在售商品", v["products"].as_array().map(|a| a.len()).unwrap_or(0)))
        }
        "inflow" => {
            let w = if_workspaces(&http()?, &if_base(&c), &c, secret(root, "inflow_api_key").as_deref())?;
            Ok(format!("连上了：{} 个工作区（{}）", w.len(), w.iter().map(|x| if x.1.is_empty() { x.0.clone() } else { x.1.clone() }).take(4).collect::<Vec<_>>().join("、")))
        }
        "notion" | "openflow" => {
            let cfg = mcp_cfg(root, s)?;
            let mut cl = litmus_mcp::client::McpClient::connect(&cfg).map_err(|e| e.to_string())?;
            let tools = cl.list_tools().map_err(|e| e.to_string())?;
            Ok(format!("连上了 {}，有 {} 个工具", cl.server_name(), tools.len()))
        }
        k => Err(format!("{k} 不支持测试")),
    }
}

/// GET /connectors：各种对接的说明（壳里的添加表单）+ 每个已接来源能不能发回去、上次发到哪。
pub async fn list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let targets: Vec<Value> = super::sources::load_sources(&root).into_iter().filter(|s| WRITABLE.contains(&s.kind.as_str())).map(|s| json!({ "id": s.id, "name": s.name, "kind": s.kind, "write": conf(&s).get("write").cloned().unwrap_or(json!({})) })).collect();
    Json(json!({ "presets": presets(), "targets": targets, "outbound": load_outbound(&root) })).into_response()
}

/// POST /connectors/test {id}：测一下能不能连上。
pub async fn test(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let id = req["id"].as_str().unwrap_or("").to_string();
    let Some(s) = super::sources::load_sources(&root).into_iter().find(|s| s.id == id) else { return err(StatusCode::NOT_FOUND, "没有这个来源").into_response() };
    match tokio::task::spawn_blocking(move || probe(&root, &s)).await.unwrap_or_else(|e| Err(e.to_string())) {
        Ok(m) => Json(json!({ "ok": true, "message": m })).into_response(),
        Err(e) => Json(json!({ "ok": false, "message": e })).into_response(),
    }
}

fn outbound_path(root: &Path) -> std::path::PathBuf {
    root.join(".litmus/local").join("outbound.json")
}
fn load_outbound(root: &Path) -> Value {
    // 文件坏了或不是对象（比如被改成了 []）：当成空的，别在发完以后记账时 panic
    std::fs::read_to_string(outbound_path(root)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).filter(|v| v.is_object()).unwrap_or(json!({}))
}

/// POST /connectors/publish {id, path, confirm:true, opts?}：把库里的一篇发到对方。
pub async fn publish(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    if req["confirm"] != true {
        return err(StatusCode::BAD_REQUEST, "发到外面之前要确认（confirm: true）").into_response();
    }
    let path = req["path"].as_str().unwrap_or("").to_string();
    if !kernel_core::is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "只能发库里 Notes/ 下的文档").into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let id = req["id"].as_str().unwrap_or("").to_string();
    let Some(s) = super::sources::load_sources(&root).into_iter().find(|s| s.id == id) else { return err(StatusCode::NOT_FOUND, "没有这个对接").into_response() };
    let Ok(md) = std::fs::read_to_string(root.join(&path)) else { return err(StatusCode::NOT_FOUND, "这篇读不到").into_response() };
    let title = title_of_md(&md, path.rsplit('/').next().unwrap_or(&path).trim_end_matches(".md"));
    let opts = req.get("opts").cloned().unwrap_or(json!({}));
    let ob = load_outbound(&root);
    let prev = ob.get(&path).and_then(|m| m.get(&s.id)).cloned();
    let (root2, s2, md2, title2, path2) = (root.clone(), s.clone(), md.clone(), title.clone(), path.clone());
    let r = tokio::task::spawn_blocking(move || match s2.kind.as_str() {
        "github" => gh_publish(&root2, &s2, &title2, &md2, prev.as_ref(), &opts),
        "sanity" => sanity_publish(&root2, &s2, &title2, &md2, &path2, prev.as_ref(), &opts),
        "notion" => notion_publish(&root2, &s2, &title2, &md2, &opts),
        "openflow" => openflow_publish(&root2, &s2, &title2, &md2, &opts),
        "mflow" => mflow_publish(&root2, &s2, &title2, &md2, &path2),
        "websflow" => websflow_publish(&root2, &s2, &title2, &md2, &opts),
        "inflow" => inflow_publish(&root2, &s2, &title2, &md2, &path2, &opts),
        k => Err(format!("{k} 不能发回去")),
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match r {
        Ok(mut v) => {
            v["at"] = json!(now_secs());
            v["target"] = json!(s.name);
            let mut ob = load_outbound(&root);
            if !ob[&path].is_object() {
                ob[&path] = json!({});
            }
            ob[&path][&s.id] = v.clone();
            let _ = std::fs::create_dir_all(root.join(".litmus/local"));
            let _ = std::fs::write(outbound_path(&root), serde_json::to_vec_pretty(&ob).unwrap_or_default());
            audit_log(&root.join(".litmus/local"), "connector-publish", &json!({ "path": path, "to": s.kind, "target": s.name, "result": v }));
            Json(v).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_text_round_trip() {
        let pt = md_to_pt("# 标题\n\n第一段有 **粗体** 和 `代码`。\n\n- 一\n- 二\n\n1. 甲\n\n> 引用");
        assert_eq!(pt[0]["style"], "h1");
        assert_eq!(pt[1]["children"][1]["marks"][0], "strong");
        assert_eq!(pt[1]["children"][3]["marks"][0], "code");
        assert_eq!(pt[2]["listItem"], "bullet");
        assert_eq!(pt[4]["listItem"], "number");
        assert_eq!(pt[5]["style"], "blockquote");
        let md = pt_to_md(&pt);
        assert!(md.starts_with("# 标题") && md.contains("**粗体**") && md.contains("`代码`") && md.contains("- 一\n- 二") && md.contains("> 引用"), "{md}");
    }

    #[test]
    fn sanity_doc_flattens_sections() {
        let d = json!({ "_id": "a", "_type": "compositePage", "title": "落地页", "slug": {"current": "x"},
            "sections": [{ "_type": "hero", "heading": "大标题", "body": [{ "_type": "block", "style": "normal", "markDefs": [], "children": [{ "_type": "span", "text": "第一屏的说明文字", "marks": [] }] }] }],
            "summary": "这是一段足够长的摘要文字，超过六十个字符才会被当成正文段落放进去，所以要写得长一些长一些再长一些。" });
        let md = sanity_doc_md(&d, 0);
        assert!(md.contains("第一屏的说明文字") && md.contains("## sections") && md.contains("足够长的摘要"), "{md}");
        assert_eq!(sanity_title(&d), "落地页");
    }

    #[test]
    fn notion_blocks_and_ids() {
        let b = json!([{ "type": "heading_2", "heading_2": { "rich_text": [{ "plain_text": "小节" }] } },
            { "type": "to_do", "to_do": { "checked": true, "rich_text": [{ "plain_text": "做完" }] } },
            { "type": "paragraph", "paragraph": { "rich_text": [{ "plain_text": "加粗", "annotations": { "bold": true } }, { "plain_text": "链接", "href": "https://a.b" }] } }]);
        assert_eq!(notion_blocks_md(b.as_array().unwrap()), "## 小节\n\n- [x] 做完\n**加粗**[链接](https://a.b)");
        assert_eq!(notion_id("https://www.notion.so/team/页面标题-1a2b3c4d5e6f78901a2b3c4d5e6f7890?pvs=4").as_deref(), Some("1a2b3c4d-5e6f-7890-1a2b-3c4d5e6f7890"));
        assert_eq!(notion_id("1a2b3c4d5e6f78901a2b3c4d5e6f7890").as_deref(), Some("1a2b3c4d-5e6f-7890-1a2b-3c4d5e6f7890"));
        assert!(notion_id("没有 id").is_none());
        let blocks = md_to_notion("# 页面标题\n\n## 一\n\n正文\n\n- 列表\n- [ ] 待办\n\n```\ncode\n```");
        assert_eq!(blocks[0]["type"], "heading_2");
        assert_eq!(blocks[1]["type"], "paragraph");
        assert_eq!(blocks[2]["type"], "bulleted_list_item");
        assert_eq!(blocks[3]["type"], "to_do");
        assert_eq!(blocks[4]["type"], "code");
    }

    #[test]
    fn github_doc_paths() {
        assert!(gh_wanted("README.md", &[]));
        assert!(gh_wanted("docs/spec/field.md", &[]));
        assert!(gh_wanted("harness/Skills/x/SKILL.md", &[]));
        assert!(gh_wanted("packages/a/README.md", &[]));
        assert!(!gh_wanted("src/lib.rs", &[]));
        assert!(!gh_wanted("node_modules/x/README.md", &[]));
        assert!(!gh_wanted("src/deep/notes.md", &[]));
        assert!(gh_wanted("src/deep/notes.md", &["src/".into()]));
        assert!(!gh_wanted("README.md", &["docs/".into()]));
    }

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), 86_400);
        assert_eq!(ymd(parse_time("2026-10-27T08:00:00.000Z")), "2026-10-27");
    }
}
