//! 内嵌终端（0.27，桌面独占）：真 PTY，经 WebSocket 接到壳里的 xterm。
//!
//! - 只有桌面壳起的 daemon（`LITMUS_DESKTOP=1`，只听 127.0.0.1）才有；线上部署一律 404。
//! - 会话活在 daemon 里：关掉面板、刷新页面再打开，接着上次的会话和最近的输出（保留 256 KB）。
//! - 协议：服务端 → 壳，二进制帧是终端输出，文本帧是 `{"t":"hello"|"exit",...}`；
//!   壳 → 服务端，文本帧 `{"t":"in","d":"..."}` / `{"t":"resize","c":80,"r":24}`。

use super::{check_token, desktop_guard, err, AppState};
use axum::{
    body::Bytes,
    extract::{ws::WebSocketUpgrade, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const KEEP: usize = 256 * 1024;
const MAX_SESSIONS: usize = 8;

#[derive(Clone, Debug)]
enum Out {
    Data(Vec<u8>),
    Exit(i64),
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct Session {
    id: String,
    cwd: String,
    shell: String,
    started: u64,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    /// 最近的输出（重连时先回放）+ 推给在线的连接。两者在同一把锁下改，保证不丢不重。
    buf: Mutex<(Vec<u8>, Option<i64>)>,
    tx: tokio::sync::broadcast::Sender<Out>,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn sessions() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, Arc<Session>>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 用户的登录 shell：$SHELL → macOS 默认 zsh → bash → sh；Windows 用 PowerShell。
pub fn default_shell() -> String {
    if cfg!(windows) {
        return "powershell.exe".into();
    }
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() && std::path::Path::new(&s).is_file() {
            return s;
        }
    }
    for s in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
        if std::path::Path::new(s).is_file() {
            return s.into();
        }
    }
    "sh".into()
}

/// 网页来源检查：带了 Origin 的必须是本机（防别的网站借浏览器连过来）。
fn origin_ok(h: &HeaderMap) -> bool {
    match h.get("origin").and_then(|o| o.to_str().ok()) {
        None => true,
        Some(o) => {
            let o = o.trim_end_matches('/');
            ["http://127.0.0.1", "http://localhost", "tauri://localhost", "https://tauri.localhost", "http://tauri.localhost"].iter().any(|p| o == *p || o.starts_with(&format!("{p}:")))
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn spawn(root: &std::path::Path, cwd: Option<&str>, cols: u16, rows: u16) -> Result<Arc<Session>, String> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    if sessions().lock().unwrap().len() >= MAX_SESSIONS {
        return Err(format!("已经开了 {MAX_SESSIONS} 个终端，先关掉几个"));
    }
    // 绝对路径、~ 开头的照用；相对的是库里的文件夹（比如阅读页「在终端里打开所在文件夹」给的 Notes/项目）
    let dir = cwd
        .filter(|c| !c.trim().is_empty())
        .map(|c| if c.starts_with('/') || c.starts_with('~') || c.get(1..3) == Some(":\\") { super::sources::expand_pub(c) } else { root.join(c) })
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| root.to_path_buf());
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize { rows: rows.clamp(4, 400), cols: cols.clamp(10, 1000), pixel_width: 0, pixel_height: 0 }).map_err(|e| format!("开不了终端：{e}"))?;
    let shell = default_shell();
    let mut cmd = CommandBuilder::new(&shell);
    if !cfg!(windows) {
        cmd.arg("-l");
    }
    cmd.cwd(&dir);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("LITMUS_VAULT", root.to_string_lossy().to_string());
    // 桌面壳的环境变量不往终端里带（里面有 LITMUS_DESKTOP 之类，只对 daemon 有意义）
    cmd.env_remove("LITMUS_DESKTOP");
    let mut child = pair.slave.spawn_command(cmd).map_err(|e| format!("起不来 {shell}：{e}"))?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let killer = child.clone_killer();
    let (tx, _) = tokio::sync::broadcast::channel(512);
    let id = format!("t{}", &kernel_core::Cas::hash_hex(format!("{}{:?}", now_secs(), std::time::Instant::now()).as_bytes())[..10]);
    let s = Arc::new(Session { id: id.clone(), cwd: dir.to_string_lossy().to_string(), shell: shell.clone(), started: now_secs(), writer: Mutex::new(writer), master: Mutex::new(pair.master), killer: Mutex::new(killer), buf: Mutex::new((Vec::new(), None)), tx });
    sessions().lock().unwrap().insert(id.clone(), s.clone());
    let s2 = s.clone();
    std::thread::Builder::new()
        .name("litmus-term".into())
        .spawn(move || {
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match std::io::Read::read(&mut reader, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = s2.buf.lock().unwrap();
                        b.0.extend_from_slice(&chunk[..n]);
                        if b.0.len() > KEEP {
                            let cut = b.0.len() - KEEP;
                            b.0.drain(..cut);
                        }
                        let _ = s2.tx.send(Out::Data(chunk[..n].to_vec()));
                    }
                }
            }
            let code = child.wait().map(|st| st.exit_code() as i64).unwrap_or(-1);
            let mut b = s2.buf.lock().unwrap();
            b.1 = Some(code);
            let _ = s2.tx.send(Out::Exit(code));
            drop(b);
            // 退出后留一会儿让壳看到「已退出」，然后清掉
            std::thread::sleep(std::time::Duration::from_secs(30));
            sessions().lock().unwrap().remove(&s2.id);
        })
        .map_err(|e| e.to_string())?;
    Ok(s)
}

/// GET /desktop/term?token=&id=&cwd=&cols=&rows=：接上一个终端（没有 id 就新开）。
pub async fn ws(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    if q.get("token").map(|t| t != &st.token).unwrap_or(true) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    if let Err(e) = desktop_guard(&st) {
        return e.into_response();
    }
    if !origin_ok(&h) {
        return err(StatusCode::FORBIDDEN, "只有本机的鹿蕊能开终端").into_response();
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (q, ws);
        return err(StatusCode::NOT_FOUND, "手机上没有终端").into_response();
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let root = st.kernel.lock().unwrap().vault.root.clone();
        let num = |k: &str, d: u16| q.get(k).and_then(|v| v.parse::<u16>().ok()).unwrap_or(d);
        let (cols, rows) = (num("cols", 100), num("rows", 30));
        let existing = q.get("id").and_then(|id| sessions().lock().unwrap().get(id).cloned());
        let s = match existing {
            Some(s) => {
                let _ = s.master.lock().unwrap().resize(portable_pty::PtySize { rows: rows.clamp(4, 400), cols: cols.clamp(10, 1000), pixel_width: 0, pixel_height: 0 });
                s
            }
            None => match spawn(&root, q.get("cwd").map(|s| s.as_str()), cols, rows) {
                Ok(s) => s,
                Err(e) => return err(StatusCode::CONFLICT, e).into_response(),
            },
        };
        super::audit_log(&root.join(".litmus/local"), "terminal", &json!({ "id": s.id, "cwd": s.cwd, "attach": q.contains_key("id") }));
        ws.on_upgrade(move |socket| run(s, socket)).into_response()
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn run(s: Arc<Session>, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    let (replay, done, mut rx) = {
        let b = s.buf.lock().unwrap();
        (b.0.clone(), b.1, s.tx.subscribe())
    };
    let hello = json!({ "t": "hello", "id": s.id, "cwd": s.cwd, "shell": s.shell });
    if sink.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }
    if !replay.is_empty() && sink.send(Message::Binary(replay.into())).await.is_err() {
        return;
    }
    if let Some(code) = done {
        let _ = sink.send(Message::text(json!({ "t": "exit", "code": code }).to_string())).await;
        return;
    }
    let out = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Out::Data(d)) => {
                    if sink.send(Message::Binary(d.into())).await.is_err() {
                        break;
                    }
                }
                Ok(Out::Exit(code)) => {
                    let _ = sink.send(Message::text(json!({ "t": "exit", "code": code }).to_string())).await;
                    break;
                }
                // 壳跟不上（一下子输出太多）：丢掉中间那段，接着推
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    while let Some(Ok(m)) = stream.next().await {
        let Message::Text(t) = m else { continue };
        let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
        match v["t"].as_str() {
            Some("in") => {
                if let Some(d) = v["d"].as_str() {
                    let mut w = s.writer.lock().unwrap();
                    let _ = w.write_all(d.as_bytes());
                    let _ = w.flush();
                }
            }
            Some("resize") => {
                let c = v["c"].as_u64().unwrap_or(80).clamp(10, 1000) as u16;
                let r = v["r"].as_u64().unwrap_or(24).clamp(4, 400) as u16;
                let _ = s.master.lock().unwrap().resize(portable_pty::PtySize { rows: r, cols: c, pixel_width: 0, pixel_height: 0 });
            }
            _ => {}
        }
        if out.is_finished() {
            break;
        }
    }
    out.abort();
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::io::Write as _;

/// GET /desktop/terms：开着的终端。
pub async fn list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    if let Err(e) = desktop_guard(&st) {
        return e.into_response();
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let list: Vec<Value> = {
        let mut v: Vec<Value> = sessions().lock().unwrap().values().map(|s| json!({ "id": s.id, "cwd": s.cwd, "shell": s.shell, "started": s.started, "alive": s.buf.lock().unwrap().1.is_none() })).collect();
        v.sort_by_key(|x| x["started"].as_u64().unwrap_or(0));
        v
    };
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let list: Vec<Value> = Vec::new();
    Json(json!({ "terms": list, "shell": default_shell(), "available": cfg!(not(any(target_os = "android", target_os = "ios"))) })).into_response()
}

/// POST /desktop/terms/close {id}：关掉一个终端（结束里面的进程）。
pub async fn close(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    if let Err(e) = desktop_guard(&st) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let id = req["id"].as_str().unwrap_or("").to_string();
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let Some(s) = sessions().lock().unwrap().remove(&id) else { return err(StatusCode::NOT_FOUND, "这个终端已经关了").into_response() };
        let _ = s.killer.lock().unwrap().kill();
    }
    let _ = id;
    Json(json!({ "ok": true })).into_response()
}
