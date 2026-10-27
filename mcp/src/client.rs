//! litmus-mcp::client — MCP 入口侧：把任意 MCP server 变成数据源。
//!
//! 两种传输：子进程（stdio）；或远程地址（Streamable HTTP：POST JSON-RPC，回 JSON 或 SSE 的 `data:` 行，
//! 带 `Mcp-Session-Id`）。完成 initialize 握手后，可列举/调用其 tools、列举/读取其 resources。

use anyhow::{Context, Result, bail};
use kernel_core::ConnectionConfig;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use crate::LATEST_PROTOCOL;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

enum Transport {
    Stdio { child: Child, stdin: ChildStdin, rx: Receiver<String> },
    Http { client: reqwest::blocking::Client, url: String, headers: Vec<(String, String)>, session: Option<String> },
}

/// 一个已连接（握手完成）的 MCP server。
pub struct McpClient {
    t: Transport,
    next_id: u64,
    pub protocol_version: String,
    pub server_info: Value,
    pub capabilities: Value,
}

/// SSE 正文里的一条条消息（每个事件的 data 行拼起来）。不是 SSE 就当整段 JSON。
pub fn parse_http_body(body: &str) -> Vec<Value> {
    let t = body.trim_start();
    if !(t.starts_with("data:") || t.starts_with("event:") || t.starts_with("id:") || t.starts_with(':')) {
        return serde_json::from_str::<Value>(t).ok().map(|v| match v {
            Value::Array(a) => a,
            v => vec![v],
        }).unwrap_or_default();
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Value>| {
        if !buf.trim().is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(buf.trim()) {
                out.push(v);
            }
        }
        buf.clear();
    };
    for line in body.lines() {
        if line.trim().is_empty() {
            flush(&mut buf, &mut out);
        } else if let Some(d) = line.strip_prefix("data:") {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    flush(&mut buf, &mut out);
    out
}

impl McpClient {
    /// 连接并完成 initialize 握手：配了 url 走 HTTP，否则起子进程。
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        if let Some(url) = cfg.url.as_ref().filter(|u| !u.trim().is_empty()) {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("远程 MCP 地址要以 http(s):// 开头：{url}");
            }
            let client = reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .user_agent(concat!("Litmus/", env!("CARGO_PKG_VERSION")))
                .build()?;
            let mut c = McpClient {
                t: Transport::Http { client, url: url.clone(), headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(), session: None },
                next_id: 0,
                protocol_version: LATEST_PROTOCOL.to_string(),
                server_info: Value::Null,
                capabilities: Value::Null,
            };
            c.initialize()?;
            return Ok(c);
        }
        if cfg.command.trim().is_empty() {
            bail!("连接 {} 既没有启动命令也没有地址", cfg.name);
        }
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn MCP server '{}' ({})", cfg.name, cfg.command))?;
        let stdin = child.stdin.take().context("child stdin")?;
        let stdout = child.stdout.take().context("child stdout")?;

        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut client = McpClient {
            t: Transport::Stdio { child, stdin, rx },
            next_id: 0,
            protocol_version: LATEST_PROTOCOL.to_string(),
            server_info: Value::Null,
            capabilities: Value::Null,
        };
        client.initialize()?;
        Ok(client)
    }

    /// HTTP：POST 一条消息，返回服务端回的消息（JSON 或 SSE）。
    fn http_post(&mut self, msg: &Value) -> Result<Vec<Value>> {
        let Transport::Http { client, url, headers, session } = &mut self.t else { bail!("not http") };
        let mut req = client.post(url.as_str()).header("content-type", "application/json").header("accept", "application/json, text/event-stream");
        for (k, v) in headers.iter() {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(sid) = session.as_ref() {
            req = req.header("mcp-session-id", sid.as_str()).header("mcp-protocol-version", self.protocol_version.as_str());
        }
        let resp = req.body(msg.to_string()).send().map_err(|e| anyhow::anyhow!("连不上 {url}：{e}"))?;
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session = Some(sid.to_string());
        }
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            let why = parse_http_body(&text).first().and_then(|v| v.pointer("/error/message").or_else(|| v.get("error")).and_then(|m| m.as_str()).map(String::from)).unwrap_or_default();
            bail!("鉴权没通过（{status}）：检查密钥{}", if why.is_empty() { String::new() } else { format!("。服务端说：{why}") });
        }
        if !status.is_success() && status.as_u16() != 202 {
            bail!("{url} 返回 {status}：{}", text.chars().take(200).collect::<String>());
        }
        Ok(parse_http_body(&text))
    }

    fn send_raw(&mut self, msg: &Value) -> Result<()> {
        match &mut self.t {
            Transport::Stdio { stdin, .. } => {
                writeln!(stdin, "{msg}")?;
                stdin.flush()?;
                Ok(())
            }
            Transport::Http { .. } => self.http_post(msg).map(|_| ()),
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// 发起请求并等待对应 id 的响应（丢弃期间的通知与无关消息）。
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let pick = |v: &Value| -> Option<Result<Value>> {
            if v.get("id").and_then(|i| i.as_u64()) != Some(id) {
                return None;
            }
            if let Some(err) = v.get("error") {
                return Some(Err(anyhow::anyhow!("MCP error on '{method}': {}", err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown"))));
            }
            Some(Ok(v.get("result").cloned().unwrap_or(Value::Null)))
        };
        if matches!(self.t, Transport::Http { .. }) {
            let msgs = self.http_post(&msg)?;
            return msgs.iter().find_map(pick).unwrap_or_else(|| bail!("'{method}' 没有收到回答"));
        }
        self.send_raw(&msg)?;
        let Transport::Stdio { rx, .. } = &self.t else { unreachable!() };
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                bail!("MCP request '{method}' timed out after {:?}", REQUEST_TIMEOUT);
            }
            let line = match rx.recv_timeout(remain) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    bail!("MCP request '{method}' timed out after {:?}", REQUEST_TIMEOUT)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("MCP server exited while waiting for '{method}'")
                }
            };
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue, // 非 JSON 行（server 的杂项输出）忽略
            };
            if let Some(r) = pick(&v) {
                return r;
            }
        }
    }

    fn initialize(&mut self) -> Result<()> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": LATEST_PROTOCOL,
                "capabilities": {},
                "clientInfo": { "name": "litmus", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        self.protocol_version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(LATEST_PROTOCOL)
            .to_string();
        self.server_info = result.get("serverInfo").cloned().unwrap_or(Value::Null);
        self.capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    pub fn list_tools(&mut self) -> Result<Vec<Value>> {
        let r = self.request("tools/list", json!({}))?;
        Ok(r.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default())
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({ "name": name, "arguments": arguments }))
    }

    pub fn list_resources(&mut self) -> Result<Vec<Value>> {
        let r = self.request("resources/list", json!({}))?;
        Ok(r.get("resources")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default())
    }

    pub fn read_resource(&mut self, uri: &str) -> Result<Value> {
        self.request("resources/read", json!({ "uri": uri }))
    }

    pub fn server_name(&self) -> String {
        self.server_info
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string()
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Transport::Stdio { child, .. } = &mut self.t {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 从 tools/call 或 resources/read 的结果中提取可读文本。
pub fn extract_text(result: &Value) -> Option<String> {
    // tool result: { content: [{type:"text", text}] }
    if let Some(arr) = result.get("content").and_then(|c| c.as_array()) {
        let joined: Vec<String> = arr
            .iter()
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
            .collect();
        if !joined.is_empty() {
            return Some(joined.join("\n"));
        }
        // 无文本时回退到结构化内容
        if let Some(sc) = result.get("structuredContent") {
            return Some(sc.to_string());
        }
    }
    // resources/read: { contents: [{text, mimeType, uri}] }
    if let Some(arr) = result.get("contents").and_then(|c| c.as_array()) {
        if let Some(first) = arr.first() {
            if let Some(t) = first.get("text").and_then(|t| t.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 提取 MIME（resources/read），缺省 text/markdown。
pub fn extract_mime(result: &Value) -> String {
    result
        .get("contents")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|f| f.get("mimeType"))
        .and_then(|m| m.as_str())
        .unwrap_or("text/markdown")
        .to_string()
}
