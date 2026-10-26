//! litmus-mcp::client — MCP 入口侧：把任意 MCP server 变成数据源。
//!
//! 以子进程方式启动 MCP server（stdio 传输），完成 initialize 握手后，
//! 可列举/调用其 tools、列举/读取其 resources，抽成内容进入采集管道。

use anyhow::{Context, Result, bail};
use kernel_core::ConnectionConfig;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use crate::LATEST_PROTOCOL;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 一个已连接（握手完成）的 MCP server。
pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: u64,
    pub protocol_version: String,
    pub server_info: Value,
    pub capabilities: Value,
}

impl McpClient {
    /// 启动并完成 initialize 握手。
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
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
            child,
            stdin,
            rx,
            next_id: 0,
            protocol_version: LATEST_PROTOCOL.to_string(),
            server_info: Value::Null,
            capabilities: Value::Null,
        };
        client.initialize()?;
        Ok(client)
    }

    fn send_raw(&mut self, msg: &Value) -> Result<()> {
        writeln!(self.stdin, "{msg}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// 发起请求并等待对应 id 的响应（丢弃期间的通知与无关消息）。
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                bail!("MCP request '{method}' timed out after {:?}", REQUEST_TIMEOUT);
            }
            let line = match self.rx.recv_timeout(remain) {
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
            if v.get("id").and_then(|i| i.as_u64()) != Some(id) {
                continue; // 通知 / 服务端主动请求，M9 入口侧暂不处理
            }
            if let Some(err) = v.get("error") {
                bail!(
                    "MCP error on '{method}': {}",
                    err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")
                );
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
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
        let _ = self.child.kill();
        let _ = self.child.wait();
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
