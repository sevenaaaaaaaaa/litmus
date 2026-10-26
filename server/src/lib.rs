//! thirdc-server: 本地 daemon 的 HTTP API（loopback + token 认证）。
//!
//! 所有客户端（桌面/移动/Web/扩展/agent）都通过这套 API 操作库，
//! 不允许任何客户端绕过内核直改 sidecar。
//!
//! 端点：
//!   GET  /health
//!   GET  /status
//!   GET  /search?q=...
//!   GET  /doc?path=Notes/x.md
//!   PUT  /doc?path=Notes/x.md        body = markdown
//!   DELETE /doc?path=Notes/x.md
//!   POST /asset?name=photo.png       body = 字节
//!   POST /sync

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use kernel_core::{Kernel, Vault};
use serde_json::{Value, json};
use std::collections::HashMap;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

mod field;

pub struct AppState {
    pub kernel: Arc<Mutex<Kernel>>,
    pub mcp: Arc<thirdc_mcp::McpServer>,
    pub hub: Arc<PresenceHub>,
    pub token: String,
    /// memo 模式的文档索引缓存：(构建时的数据代数, 文档列表)。
    /// memo 模式的文档索引缓存：(构建时的数据代数, 序列化好的 JSON 响应体)。
    /// 缓存字节而非结构体：1.2 万篇 3MB 的 JSON 不必每次请求重序列化。
    memo_cache: Arc<Mutex<Option<(u64, axum::body::Bytes)>>>,
    /// 是否由桌面壳启动（THIRDC_DESKTOP=1）：控制 /desktop/* 独占端点的可用性。
    pub desktop: bool,
    /// 远端拉取的后台任务状态：结构秒回，正文流式拉取，前端轮询进度。
    pub pull: Arc<Mutex<PullJob>>,
    /// 密向量重建任务状态（/rag/status 秒回，进度可轮询）。
    pub rag: Arc<RagJob>,
    /// 备份任务状态（/backup/status）。
    pub backup: Arc<BackupJob>,
}

/// RAG 建向量后台任务的可观察状态。
#[derive(Default)]
pub struct RagJob {
    pub running: std::sync::atomic::AtomicBool,
    pub done: std::sync::atomic::AtomicUsize,
    pub total: std::sync::atomic::AtomicUsize,
    pub last_error: std::sync::Mutex<String>,
}

/// 备份后台任务状态（/backup/status 秒回）。
#[derive(Default)]
pub struct BackupJob {
    pub running: std::sync::atomic::AtomicBool,
    pub last_error: std::sync::Mutex<String>,
}

/// 后台拉取任务的可观察状态。登录/建目录是同步阶段（快），正文在后台流式写入。
#[derive(Default)]
pub struct PullJob {
    active: bool,
    /// idle | content | done | error
    phase: String,
    server: String,
    user: String,
    total: usize,
    dirs: usize,
    done: usize,
    pulled: usize,
    skipped: usize,
    failed: usize,
    bytes: u64,
    error: String,
    started: u64,
    finished: u64,
}

fn pull_json(j: &PullJob) -> Value {
    json!({
        "active": j.active,
        "phase": j.phase,
        "server": j.server,
        "user": j.user,
        "total": j.total,
        "folders": j.dirs,
        "done": j.done,
        "pulled": j.pulled,
        "skipped": j.skipped,
        "failed": j.failed,
        "bytes": j.bytes,
        "error": j.error,
        "started": j.started,
        "finished": j.finished,
        "pct": if j.total == 0 { 100 } else { (j.done * 100 / j.total).min(100) },
    })
}

fn err(code: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg.to_string() })))
}

fn check_token(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-thirdc-token").and_then(|v| v.to_str().ok()))
        .unwrap_or("");
    if provided == state.token {
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "invalid or missing token"))
    }
}

/// 构建带认证的 API 路由。`/` 与静态资产不需要令牌，页面内用令牌调 API。
/// Service Worker 源码：__SHELL_VERSION__ 由路由注入 index.html 内容哈希。
const SW_SRC: &str = r#"
const SHELL = '__SHELL_VERSION__';
self.addEventListener('install', e => self.skipWaiting());
self.addEventListener('activate', e => e.waitUntil((async () => {
  const keys = await caches.keys();
  const stale = keys.filter(k => k !== SHELL);
  await Promise.all(stale.map(k => caches.delete(k)));
  await self.clients.claim();
  // 升级（存在旧版本缓存）时通知已打开的旧页面刷新——旧页面也认识 shell-updated，会自己 reload
  if (stale.length) {
    const cs = await self.clients.matchAll({ type: 'window' });
    cs.forEach(c => c.postMessage({ type: 'shell-updated' }));
  }
})()));

self.addEventListener('fetch', e => {
  const req = e.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== location.origin) return;

  if (req.mode === 'navigate') {
    // 外壳：网络优先（内核就在本机或同源服务器，取新壳只要几毫秒），断网才回落缓存。
    // 旧策略"缓存优先 + 后台比对"会让新版本至少晚一次启动才出现，且通知消息可能在页面监听前发出而丢失。
    e.respondWith((async () => {
      const cache = await caches.open(SHELL);
      try {
        const r = await fetch(req);
        // 每个壳页面各存一份（/ 与 /field），断网时回落到自己，不串壳
        if (r.ok) cache.put(url.pathname, r.clone());
        return r;
      } catch (_) {
        return (await cache.match(url.pathname)) || (await cache.match('/')) || new Response('offline', { status: 503 });
      }
    })());
    return;
  }
  // 白名单：只有静态资产走缓存。其余（/trash 等一切 API）一律网络，绝不缓存
  const p = url.pathname;
  if (!(p.startsWith('/assets/') || p === '/icon48.png' || p === '/manifest-pwa.json')) return;
  e.respondWith(caches.open(SHELL).then(async c => {
    const hit = await c.match(req);
    if (hit) return hit;
    const r = await fetch(req);
    if (r.ok) c.put(req, r.clone());
    return r;
  }));
});
"#;

/// 壳版本：index.html 内容哈希（构建期内嵌，改动自动出新缓存键）。
fn shell_version() -> &'static String {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        // 壳 = index.html + tokens.css：只改配色也必须换缓存键
        let html = [include_str!("../web/index.html"), include_str!("../web/field.html"), include_str!("../web/tokens.css")].concat();
        format!("thirdc-shell-{}", &kernel_core::Cas::hash_hex(html.as_bytes())[..12])
    })
}

/// 构建带认证的 API 路由。`/` 与静态资产不需要令牌，页面内用令牌调 API。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(app))
        .route("/field", get(field::page))
        .route("/field/data", get(field::data))
        .route("/field/react", get(field::react))
        .route("/field/place", post(field::place))
        .route("/field/crystal", post(field::crystal))
        .route("/field/note", post(field::note))
        .route("/field/ai", get(field::ai_get).post(field::ai_put))
        .route("/field/ai/test", post(field::ai_test))
        .route("/sticky.html", get(|| async {
            ([("content-type","text/html; charset=utf-8"),("cache-control","no-cache")], include_str!("../web/sticky.html"))
        }))
        .route("/assets/tokens.css", get(tokens_css))
        .route("/icon48.png", get(|| async { ([("content-type","image/png"),("cache-control","public, max-age=86400")], include_bytes!("../web/icon48.png").as_slice()) }))
        .route("/manifest-pwa.json", get(|| async { axum::response::Json(serde_json::json!({"name":"鹿蕊 Litmus","short_name":"鹿蕊","start_url":"/","display":"standalone","background_color":"#22252d","theme_color":"#22252d"})) }))
        .route("/sw.js", get(|| async {
    // 壳版本 = index.html 内容哈希（构建期自动）：改前端必出新缓存键，忘 bump 也不再吃旧壳。
    let body = SW_SRC.replace("__SHELL_VERSION__", shell_version());
    ([("content-type", "application/javascript; charset=utf-8"), ("cache-control", "no-cache")], body).into_response()
}))
        .route("/assets/fonts/{name}", get(font))
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/status", get(status))
        .route("/docs", get(list_docs_api))
        .route("/memo/docs", get(memo_docs))
        .route("/graph", get(graph))
        .route("/browse", get(browse))
        .route("/tree", get(tree))
        .route("/board", get(get_board).put(put_board))
        .route("/boards", get(boards_list).post(boards_create))
        .route("/boards/{name}", axum::routing::delete(boards_delete))
        .route("/doc/move", post(move_doc))
        .route("/doc/tidy", post(doc_tidy))
        .route("/doc/autoname", post(doc_autoname))
        .route("/doc/copy", post(copy_doc))
        .route("/dir", post(create_dir).delete(remove_dir))
        .route("/dir/move", post(move_dir))
        .route("/dir/copy", post(copy_dir))
        .route("/doc/raw", get(raw_doc))
        .route("/asset-file", get(asset_file))
        .route("/connections", get(connections))
        .route("/conn/probe", post(conn_probe))
        .route("/conn/pull", post(conn_pull))
        .route("/design", get(design_list))
        .route("/design/import", post(design_import))
        .route("/design/active", post(design_active))
        .route("/design/scope", post(design_scope))
        .route("/design/import-url", post(design_import_url))
        .route("/design/presets", get(design_presets))
        .route("/ingest/topic", post(ingest_topic))
        .route("/ingest/web", post(ingest_web))
        .route("/ingest/html", post(ingest_html))
        .route("/ingest/file", post(ingest_file))
        .route("/export/{fmt}", get(export_doc))
        .route("/desktop/info", get(desktop_info))
        .route("/desktop/import", post(desktop_import))
        .route("/desktop/terminal", post(desktop_terminal))
        .route("/desktop/scan", get(desktop_scan))
        .route("/desktop/quarantine", post(desktop_quarantine))
        .route("/desktop/pull", post(desktop_pull))
        .route("/desktop/pull/status", get(desktop_pull_status))
        .route("/organize/plan", post(organize_plan))
        .route("/organize/apply", post(organize_apply))
        .route("/git/status", get(git_status))
        .route("/git/log", get(git_log))
        .route("/git/commit", post(git_commit))
        .route("/git/branches", get(git_branches))
        .route("/git/checkout", post(git_checkout))
        .route("/git/sync", post(git_sync))
        .route("/presentation", get(presentation))
        .route("/share", post(share_create))
        .route("/metrics", get(metrics))
        .route("/design/{name}", get(design_get))
        .route("/a2ui/render", post(a2ui_render))
        .route("/a2ui/action", post(a2ui_action))
        .route("/chat", post(chat))
        .route("/chat/stream", post(chat_stream))
        .route("/agent/apply", post(agent_apply))
        .route("/publish", get(publish_list).post(publish))
        .route("/publish/targets", get(publish_targets))
        .route("/publish/site", post(publish_site))
        .route("/publish/deploy", post(publish_deploy))
        .route("/publish/{name}", get(publish_file))
        .route("/search", get(search))
        .route("/search/hybrid", get(search_hybrid))
        .route("/rag/reindex", post(rag_reindex))
        .route("/rag/status", get(rag_status))
        .route("/backup/run", post(backup_run))
        .route("/backup/status", get(backup_status))
        .route("/secrets", get(secrets_list).post(secrets_put).delete(secrets_delete))
        .route("/doc", get(get_doc).put(put_doc).delete(delete_doc))
        .route("/asset", post(post_asset))
        .route("/sync", post(sync))
        .route("/auth/login", post(auth_login))
        .route("/refresh", post(force_refresh))
        .route("/backup", post(backup_vault))
        .route("/agent/memory", get(agent_memory_list).post(agent_memory_add))
        .route("/view/resolve", post(view_resolve))
        .route("/trash", get(trash_list))
        .route("/trash/restore", post(trash_restore))
        .route("/trash/purge", post(trash_purge))
        .route("/agent/recall", get(agent_recall))
        .route("/ui-state", get(get_ui_state).put(put_ui_state))
        .route("/comments", get(comments_get).post(comments_update))
        .route("/plugins", get(plugins_list))
        .route("/plugins/install", post(plugins_install))
        .route("/plugins/toggle", post(plugins_toggle))
        .route("/plugins/remove", post(plugins_remove))
        .route("/plugins/view/{id}/{file}", get(plugins_view_file))
        .route("/plugin-index", get(|| async {
            ([("cache-control", "public, max-age=300")], include_str!("../web/plugin-index.json"))
        }))
        .route("/ws", get(ws_handler))
        .route("/conflow/conversations", get(conflow_conversations))
        .route("/conflow/digest", post(conflow_digest))
        .with_state(state)
}

/// 内嵌的 Web 客户端（单文件，无构建步骤）。开发期禁止缓存，避免看到旧版。
async fn app() -> impl IntoResponse {
    // 内联 tokens.css：省掉一次跨洋往返（无 CDN 时每次往返 ~0.8s）
    static PAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let html = PAGE.get_or_init(|| {
        include_str!("../web/index.html").replace(
            "<link rel=\"stylesheet\" href=\"assets/tokens.css\">",
            &format!(
                "<style>{}</style>",
                include_str!("../web/tokens.css").replace("url(\"fonts/", "url(\"/assets/fonts/")
            ),
        )
    });
    (
        [(header::CACHE_CONTROL, "no-cache, must-revalidate")],
        axum::response::Html(html.as_str()),
    )
}

async fn tokens_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../web/tokens.css"),
    )
}

/// 内嵌字体（Albert Sans / Space Grotesk / JetBrains Mono，与 OpenFlow 同一套声部）。
async fn font(AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    let bytes: &'static [u8] = match name.as_str() {
        "albert-sans.woff2" => include_bytes!("../web/fonts/albert-sans.woff2"),
        "space-grotesk-400.woff2" => include_bytes!("../web/fonts/space-grotesk-400.woff2"),
        "space-grotesk-600.woff2" => include_bytes!("../web/fonts/space-grotesk-600.woff2"),
        "jetbrains-mono-400.woff2" => include_bytes!("../web/fonts/jetbrains-mono-400.woff2"),
        _ => return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], b"".as_slice()).into_response(),
    };
    (
        [(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
        bytes,
    )
        .into_response()
}

/// 知识图谱：文档/采集/附件为节点，wiki 链接与附件引用为边。
async fn graph(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    if let Err(e) = k.sync_throttled() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    match build_graph(&mut k) {
        Ok(g) => Json(g).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 画布布局（sidecar，非真相，可重建）。多画布：?name=
async fn get_board(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let name = q.get("name").cloned().unwrap_or_else(|| "main".into());
    let k = st.kernel.lock().unwrap();
    match board_load(&k, &name) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn put_board(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let name = q.get("name").cloned().unwrap_or_else(|| "main".into());
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let k = st.kernel.lock().unwrap();
    match board_save(&k, &name, &v) {
        Ok(()) => Json(json!({ "saved": true, "name": name })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 多画布列表。
async fn boards_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    Json(json!({ "boards": board_names(&k) })).into_response()
}

async fn boards_create(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").trim().to_string();
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "invalid board name").into_response();
    }
    let k = st.kernel.lock().unwrap();
    if let Err(e) = board_save(&k, &name, &json!({ "nodes": {} })) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "created": name, "boards": board_names(&k) })).into_response()
}

async fn boards_delete(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    if name == "main" {
        return err(StatusCode::BAD_REQUEST, "main board cannot be deleted").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let p = board_path(&k, &name);
    let _ = std::fs::remove_file(p);
    Json(json!({ "deleted": name, "boards": board_names(&k) })).into_response()
}

/// 移动文档（看板拖拽 = 真实移动文件）。
async fn move_doc(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let from = req.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let to = req.get("to").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !kernel_core::is_safe_doc_path(&from) || !kernel_core::is_safe_doc_path(&to) {
        return err(StatusCode::BAD_REQUEST, "paths must be under Notes/").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    let src = k.vault.root.join(&from);
    let dst = k.vault.root.join(&to);
    if !src.is_file() {
        return err(StatusCode::NOT_FOUND, "source not found").into_response();
    }
    if dst.exists() {
        return err(StatusCode::CONFLICT, "target exists").into_response();
    }
    if let Some(dir) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Err(e) = std::fs::rename(&src, &dst) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let _ = k.sync_all();
    Json(json!({ "from": from, "to": to })).into_response()
}

/// 目录路径合法性：Notes/ 前缀、无 ..、无反斜杠、非根本身、段非空。
fn is_safe_dir_path(p: &str) -> bool {
    p.starts_with("Notes/")
        && p != "Notes"
        && !p.contains("..")
        && !p.contains('\\')
        && !p.ends_with('/')
        && !p.contains("//")
        && p.split('/').all(|s| !s.is_empty())
}

/// 新建文件夹（Notes/ 下，递归创建）。
async fn create_dir(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !is_safe_dir_path(&path) {
        return err(StatusCode::BAD_REQUEST, "dir path must be under Notes/").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let abs = k.vault.root.join(&path);
    // 重名 409（main 8a2fcdd 的约定，前端据此提示"已存在"）
    if abs.exists() {
        return err(StatusCode::CONFLICT, "目录或文件已存在").into_response();
    }
    if let Err(e) = std::fs::create_dir_all(&abs) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "ok": true, "path": path })).into_response()
}

/// 文件夹重命名 / 移动（整目录 rename；随后 sync_all 重建索引路径）。
async fn move_dir(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let from = req.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let to = req.get("to").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !is_safe_dir_path(&from) || !is_safe_dir_path(&to) {
        return err(StatusCode::BAD_REQUEST, "dir paths must be under Notes/").into_response();
    }
    if to.starts_with(&(from.clone() + "/")) {
        return err(StatusCode::BAD_REQUEST, "cannot move a folder into itself").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    let src = k.vault.root.join(&from);
    let dst = k.vault.root.join(&to);
    if !src.is_dir() {
        return err(StatusCode::NOT_FOUND, "source folder not found").into_response();
    }
    if dst.exists() {
        return err(StatusCode::CONFLICT, "target exists").into_response();
    }
    if let Some(dir) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Err(e) = std::fs::rename(&src, &dst) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let _ = k.sync_all();
    Json(json!({ "from": from, "to": to })).into_response()
}

/// 复制整个文件夹（含子目录）：文档走 put_doc（新 doc 身份，避免 CRDT 同源冲突），
/// 其余文件字节级复制。文件夹也是真相，复制粘贴一样不少。
async fn copy_dir(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let from = req.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let to = req.get("to").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !is_safe_dir_path(&from) || !is_safe_dir_path(&to) {
        return err(StatusCode::BAD_REQUEST, "dir paths must be under Notes/").into_response();
    }
    if from == to || to.starts_with(&(from.clone() + "/")) {
        return err(StatusCode::BAD_REQUEST, "不能复制到自身或其子目录").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    let root = k.vault.root.clone();
    if root.join(&to).exists() {
        return err(StatusCode::CONFLICT, "目标文件夹已存在").into_response();
    }
    if let Err(e) = k.sync_throttled() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let mut copied = 0usize;
    fn rec(
        src: &std::path::Path,
        dst: &std::path::Path,
        root: &std::path::Path,
        k: &mut kernel_core::Kernel,
        copied: &mut usize,
    ) -> Result<(), String> {
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for e in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let name = e.file_name().to_string_lossy().into_owned();
            if is_junk_name(&name) {
                continue;
            }
            let sp = e.path();
            let dp = dst.join(&name);
            if sp.is_dir() {
                rec(&sp, &dp, root, k, copied)?;
            } else {
                let rel = dp
                    .strip_prefix(root)
                    .unwrap_or(&dp)
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_doc_file(&sp) {
                    let text = std::fs::read_to_string(&sp).map_err(|e| e.to_string())?;
                    k.put_doc(&rel, &text).map_err(|e| e.to_string())?;
                } else {
                    if let Some(p) = dp.parent() {
                        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                    }
                    std::fs::copy(&sp, &dp).map_err(|e| e.to_string())?;
                }
                *copied += 1;
            }
        }
        Ok(())
    }
    match rec(&root.join(&from), &root.join(&to), &root, &mut k, &mut copied) {
        Ok(()) => Json(json!({ "from": from, "to": to, "copied": copied })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 删除空文件夹（非空返回 409，提示先移走内容）。
async fn remove_dir(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    if !is_safe_dir_path(&path) {
        return err(StatusCode::BAD_REQUEST, "dir path must be under Notes/").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let abs = k.vault.root.join(&path);
    if !abs.is_dir() {
        return err(StatusCode::NOT_FOUND, "folder not found").into_response();
    }
    if let Err(e) = std::fs::remove_dir(&abs) {
        return err(StatusCode::CONFLICT, format!("文件夹非空或删除失败：{e}")).into_response();
    }
    Json(json!({ "ok": true, "path": path })).into_response()
}

/// 直接读取真相区文件（附件预览用），仅允许 Assets/ 下。
async fn asset_file(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // <img src> 带不了 header：query token 兜底
    if check_token(&st, &h).is_err() && q.get("token").map(|t| t != &st.token).unwrap_or(true) {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing token").into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    if !path.starts_with("Assets/") || path.contains("..") || path.contains('\\') {
        return err(StatusCode::BAD_REQUEST, "path must be under Assets/").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let abs = k.vault.root.join(&path);
    match std::fs::read(&abs) {
        Ok(bytes) => {
            let mime = kernel_core::refs::mime_for_ext(
                abs.extension().and_then(|e| e.to_str()).unwrap_or(""),
            );
            ([(header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(_) => err(StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}

/// A2UI（Agent-to-User Interface）渲染：JSONL 流 → 自包含 HTML。
/// 这是 Google A2UI 协议的**原生渲染入口**：agent 推 surfaceUpdate/dataModelUpdate/beginRendering，
/// 客户端按目录渲染；我们同时提供 HTML 产物，便于发布与沙箱预览。
async fn a2ui_render(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let title = req.get("title").and_then(|t| t.as_str()).unwrap_or("A2UI");
    // 三种输入：jsonl 字符串 / messages 数组 / 单条消息对象
    let jsonl = if let Some(s) = req.get("jsonl").and_then(|v| v.as_str()) {
        s.to_string()
    } else if let Some(arr) = req.get("messages").and_then(|v| v.as_array()) {
        arr.iter().map(|m| m.to_string()).collect::<Vec<_>>().join("\n")
    } else if req.get("surfaceUpdate").is_some() || req.get("beginRendering").is_some() {
        req.to_string()
    } else {
        return err(StatusCode::BAD_REQUEST, "需要 jsonl / messages / 单条消息").into_response();
    };
    match req.get("fragment").and_then(|v| v.as_bool()).unwrap_or(false) {
        // 片段模式：无文档外壳，宿主页面 Shadow DOM 原生承载（A2UI-3 去 iframe）
        true => match kernel_a2ui::render_jsonl_fragment(&jsonl) {
            Ok(html) => Json(json!({
                "html": html,
                "css": kernel_a2ui::A2UI_CSS,
                "fragment": true,
                "bytes": html.len()
            }))
            .into_response(),
            Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
        },
        false => match kernel_a2ui::render_jsonl(&jsonl, title) {
            Ok(html) => Json(json!({ "html": html, "bytes": html.len() })).into_response(),
            Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
        },
    }
}

/// A2UI 用户交互回传（客户端 → agent）。当前记录并回显，接入 agent 会话后转发。
async fn a2ui_action(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let k = st.kernel.lock().unwrap();
    let log = kernel_core::events_dir(&k.vault).join("a2ui-actions.jsonl");
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        let _ = writeln!(f, "{}", v);
    }
    Json(json!({ "ok": true, "action": name, "note": "已记录；接入 agent 会话后转发给模型" })).into_response()
}

/// 对话：有 `[ai]` 配置走模型工具循环，否则命令模式。
async fn chat(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let message = req.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
    let server = st.mcp.clone();

    let outcome = match ai {
        Some(cfg) if !cfg.base_url.is_empty() => {
            match thirdc_mcp::agent::run_ai(&server, &cfg, &message).await {
                Ok(o) => o,
                Err(e) => thirdc_mcp::agent::Outcome {
                    mode: "ai-error",
                    reply: format!("模型调用失败：{e}"),
                    steps: Vec::new(),
                    tokens: 0,
                },
            }
        }
        _ => thirdc_mcp::agent::command_mode(&server, &message),
    };

    let steps: Vec<Value> = outcome
        .steps
        .iter()
        .map(|s| json!({ "tool": s.tool, "ok": s.ok, "summary": s.summary, "args": s.args }))
        .collect();
    Json(json!({ "mode": outcome.mode, "reply": outcome.reply, "steps": steps })).into_response()
}

/// 划选即指令：对当前选区执行一个动词（规则优先，AI 可选）。
/// body: { op, text, path? } → { op, mode: rule|ai, placement, markdown, path? }
async fn agent_apply(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let text = req.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let target = req.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let paths: Vec<String> = req
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
    let ai_ready = ai.as_ref().map(|c| !c.base_url.is_empty()).unwrap_or(false);

    // ── 节点 / 批量动词（规则，离线可用）──
    match op.as_str() {
        "summarize-node" => {
            if !is_safe_doc_path(&path) {
                return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
            }
            return match ab_summarize_node(&st, &path) {
                Ok(v) => Json(v).into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            };
        }
        "connect" => {
            if !is_safe_doc_path(&path) || target.trim().is_empty() {
                return err(StatusCode::BAD_REQUEST, "需要 path 与 target").into_response();
            }
            return match ab_connect(&st, &path, &target) {
                Ok(v) => Json(v).into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            };
        }
        "tag" => {
            let tag = target.trim().trim_start_matches('#').to_string();
            if tag.is_empty() || paths.is_empty() {
                return err(StatusCode::BAD_REQUEST, "需要 paths 与 target(标签)").into_response();
            }
            let n = ab_tag_paths(&st, &paths, &tag);
            return Json(json!({"op":op,"mode":"rule","tag":tag,"tagged":n})).into_response();
        }
        "merge" => {
            if paths.len() < 2 {
                return err(StatusCode::BAD_REQUEST, "至少选 2 篇再合并").into_response();
            }
            return match ab_merge(&st, &paths) {
                Ok(v) => Json(v).into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            };
        }
        "outline" => {
            if paths.is_empty() {
                return err(StatusCode::BAD_REQUEST, "先选中若干节点").into_response();
            }
            return match ab_outline(&st, &paths) {
                Ok(v) => Json(v).into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            };
        }
        _ => {}
    }

    // ── 文本动词 ──
    if text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "选区为空").into_response();
    }
    match op.as_str() {
        "condense" => {
            return Json(json!({"op":op,"mode":"rule","placement":"replace","markdown":ab_condense(&text)})).into_response();
        }
        "summarize" => {
            return Json(json!({"op":op,"mode":"rule","placement":"replace","markdown":ab_summarize(&text)})).into_response();
        }
        "extract-node" => {
            let title = ab_first_line_title(&text);
            let slug = ab_slug(&title);
            let root = { st.kernel.lock().unwrap().vault.root.clone() };
            let new_path = unique_path(&root, &slug);
            let content = format!("# {title}\n\n{}\n", text.trim());
            if let Err(e) = write_md(&st, &new_path, &content) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
            }
            return Json(json!({"op":op,"mode":"rule","placement":"replace","markdown":format!("[[{title}]]"),"path":new_path})).into_response();
        }
        "rewrite" | "expand" | "translate" | "expand-node" => {
            if !ai_ready {
                return err(StatusCode::NOT_IMPLEMENTED, "该动作需要先在「设置 → 连接」配置模型；离线可用：总结 / 精简 / 提取为节点 / 合并 / 打标签 / 生成结构").into_response();
            }
        }
        _ => return err(StatusCode::BAD_REQUEST, format!("未知动作：{op}")).into_response(),
    }

    // ── AI 动词 ──
    let cfg = match ai { Some(c) => c, None => return err(StatusCode::NOT_IMPLEMENTED, "未配置模型").into_response() };
    let server = st.mcp.clone();
    let (verb, subject) = match op.as_str() {
        "rewrite" => ("改写（保持原意，表达更清晰）", text.clone()),
        "expand" => ("扩写（补充细节与例子，但不改变事实）", text.clone()),
        "translate" => ("翻译成英文", text.clone()),
        "expand-node" => {
            let md = match read_md(&st, &path) {
                Ok(m) => m,
                Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
            };
            ("扩写（补充细节与例子，但不改变事实）", md)
        }
        _ => unreachable!(),
    };
    let prompt = format!("你是文本编辑。请对下面的文字执行：{verb}。只输出结果本身，不要解释、不要 Markdown 代码围栏。\n\n{subject}");
    let outcome = match thirdc_mcp::agent::run_ai(&server, &cfg, &prompt).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("模型调用失败：{e}")).into_response(),
    };
    let out = strip_fence(&outcome.reply);
    if op == "expand-node" {
        if let Err(e) = write_md(&st, &path, &out) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        return Json(json!({"op":op,"mode":"ai","placement":"writeback","path":path})).into_response();
    }
    Json(json!({"op":op,"mode":"ai","placement":"replace","markdown":out})).into_response()
}

/* ───────── 魔法排版 / 自动命名 ───────── */

/// POST /doc/tidy {markdown, mode: diagnose|rule|ai, pangu?} → {markdown, issues, total, changed, mode, warning?}
/// 规则版离线可用；AI 版先跑规则再交给模型（无工具），逐块校验「内容没被改写」，不过关的块退回规则结果。
async fn doc_tidy(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let md = req.get("markdown").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mode = req.get("mode").and_then(|v| v.as_str()).unwrap_or("rule").to_string();
    let opts = kernel_core::tidy::TidyOptions {
        promote_title: req.get("title").and_then(|v| v.as_bool()).unwrap_or(true),
        pangu: req.get("pangu").and_then(|v| v.as_bool()).unwrap_or(false),
    };
    let rule = kernel_core::tidy::tidy(&md, &opts);
    if mode == "diagnose" {
        return Json(json!({ "mode": "diagnose", "issues": rule.issues, "total": rule.total, "changed": rule.changed })).into_response();
    }
    if mode != "ai" {
        return Json(json!({ "mode": "rule", "markdown": rule.markdown, "issues": rule.issues, "total": rule.total, "changed": rule.changed })).into_response();
    }
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
    let Some(cfg) = ai.filter(|c| !c.base_url.is_empty()) else {
        return err(StatusCode::NOT_IMPLEMENTED, "AI 排版需要先在「设置 → 连接」配置模型；规则排版离线可用").into_response();
    };
    let chunks = tidy_chunks(&rule.markdown, 6000);
    let mut set = tokio::task::JoinSet::new();
    for (idx, c) in chunks.iter().cloned().enumerate() {
        let cfg = cfg.clone();
        set.spawn(async move {
            let r = thirdc_mcp::agent::complete(&cfg, TIDY_SYSTEM, &c).await;
            (idx, c, r)
        });
    }
    let mut parts: Vec<(usize, String, bool)> = Vec::new();
    let mut errors = Vec::new();
    while let Some(j) = set.join_next().await {
        let Ok((idx, orig, r)) = j else { continue };
        match r {
            Ok(out) => {
                let out = strip_fence_md(&out);
                let ok = content_preserved(&orig, &out);
                parts.push((idx, if ok { out } else { orig }, ok));
            }
            Err(e) => {
                errors.push(e.to_string());
                parts.push((idx, orig, false));
            }
        }
    }
    parts.sort_by_key(|p| p.0);
    let rejected = parts.iter().filter(|p| !p.2).count();
    let joined = parts.into_iter().map(|p| p.1.trim().to_string()).collect::<Vec<_>>().join("\n\n");
    // AI 产物再过一遍规则做收口（统一空行 / 表格 / 序号），保证幂等
    let fin = kernel_core::tidy::tidy(&joined, &opts);
    let warning = if !errors.is_empty() {
        Some(format!("模型调用失败，{} 段保留规则排版：{}", rejected, errors[0]))
    } else if rejected > 0 {
        Some(format!("{rejected} 段 AI 结果改动了文字内容，已退回规则排版（只动格式不动字）"))
    } else {
        None
    };
    Json(json!({
        "mode": "ai", "markdown": fin.markdown, "issues": rule.issues, "total": rule.total,
        "changed": fin.markdown != md, "chunks": chunks.len(), "rejected": rejected, "warning": warning,
    }))
    .into_response()
}

const TIDY_SYSTEM: &str = "你是 Markdown 排版修复器。输入是从网页、Word、飞书、PDF 等处复制来的文本，格式可能已经损坏。\
只修复格式，绝不改写、删减、增加任何文字内容（标点、数字、链接、专有名词都保持原样）。可以做的事：\
恢复标题层级（# / ## / ###）；把挤在一段里的条目拆成列表；把本该独立成行的内容分行；把被硬换行打断的句子接上；\
把表格数据还原为 Markdown 表格；把 JSON、代码、命令行放进带语言标记的代码块并保持原文；把流程图描述放进 ```mermaid 代码块；\
把公式写成 $…$ 或 $$…$$；修正列表序号；去掉多余空格和空行。\
直接输出修复后的完整 Markdown，不要解释，不要用代码围栏包裹全文。";

/// 按标题 / 空行切块，每块不超过 limit 字符（代码块不切开）。
fn tidy_chunks(md: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut in_fence = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let boundary = !in_fence && (line.starts_with("# ") || line.starts_with("## ") || line.trim().is_empty());
        if boundary && cur.chars().count() >= limit {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        chunks.push(cur);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn strip_fence_md(s: &str) -> String {
    let t = s.trim();
    for open in ["```markdown\n", "```md\n", "```\n"] {
        if let Some(rest) = t.strip_prefix(open) {
            if let Some(inner) = rest.strip_suffix("```") {
                return inner.trim().to_string();
            }
        }
    }
    t.to_string()
}

/// 内容保真：去掉空白与 Markdown 记号后，两边字符多重集的重合率 ≥ 96%。
fn content_preserved(a: &str, b: &str) -> bool {
    fn bag(s: &str) -> HashMap<char, usize> {
        let mut m = HashMap::new();
        for c in s.chars() {
            if c.is_whitespace() || "#*-_>|`[]()$~:：•●▪◦·+=".contains(c) {
                continue;
            }
            *m.entry(c).or_insert(0) += 1;
        }
        m
    }
    let (ba, bb) = (bag(a), bag(b));
    let la: usize = ba.values().sum();
    let lb: usize = bb.values().sum();
    if la == 0 {
        return true;
    }
    let common: usize = ba.iter().map(|(c, n)| (*n).min(*bb.get(c).unwrap_or(&0))).sum();
    common * 100 >= la.max(lb) * 96
}

/// 占位文件名：新建默认名、时间戳、粘贴导入名……这些允许被自动改名。
fn is_placeholder_name(stem: &str) -> bool {
    let s = stem.trim().to_lowercase();
    let base = s.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '_' || c == ' ');
    ["未命名", "无标题", "新文档", "新建文档", "untitled", "note", "doc", "memo", "粘贴", "paste", "导入", "新笔记"]
        .iter()
        .any(|p| base == *p || base.is_empty())
        || s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '_' || c == ' ' || c == ':')
        || (s.starts_with("doc-") || s.starts_with("note-")) && s.len() <= 20
}

/// 标题 → 安全文件名：保留中文、大小写与空格，只替换文件系统非法字符。
fn safe_file_stem(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| if "/\\:*?\"<>|#^[]".contains(c) || c.is_control() { '-' } else { c })
        .collect();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_matches(|c: char| c == '-' || c == '.' || c == ' ');
    s.chars().take(80).collect::<String>().trim().to_string()
}

fn doc_h1(md: &str) -> Option<String> {
    let mut lines = md.lines();
    let mut first = lines.next()?;
    if first.trim() == "---" {
        // frontmatter：title: 优先
        let mut fm_title = None;
        for l in lines.by_ref() {
            if l.trim() == "---" {
                break;
            }
            if let Some(v) = l.strip_prefix("title:") {
                let v = v.trim().trim_matches('"').trim_matches('\'').trim();
                if !v.is_empty() {
                    fm_title = Some(v.to_string());
                }
            }
        }
        if fm_title.is_some() {
            return fm_title;
        }
        first = lines.find(|l| !l.trim().is_empty())?;
    }
    let t = first.trim_start().strip_prefix("# ")?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// POST /doc/autoname {path, ai?} → {path, title, source: h1|ai|rule|keep, renamed}
/// 一级标题 = 文件名；没有标题（或标题是占位）且文件名是占位时，AI 起名（未配模型时取首句）。
async fn doc_autoname(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let allow_ai = req.get("ai").and_then(|v| v.as_bool()).unwrap_or(true);
    if !is_safe_doc_path(&path) || !path.ends_with(".md") {
        return err(StatusCode::BAD_REQUEST, "path must be a .md under Notes/").into_response();
    }
    let (root, ai) = {
        let k = st.kernel.lock().unwrap();
        (k.vault.root.clone(), k.vault.config.ai.clone())
    };
    let abs = root.join(&path);
    let md = match std::fs::read_to_string(&abs) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::NOT_FOUND, e).into_response(),
    };
    let stem = stem_of(&path);
    let h1 = doc_h1(&md).filter(|t| !is_placeholder_name(t));
    let (title, source) = match h1 {
        Some(t) => (t, "h1"),
        None => {
            if !is_placeholder_name(&stem) {
                return Json(json!({ "path": path, "title": stem, "source": "keep", "renamed": false })).into_response();
            }
            let body: String = md.lines().filter(|l| !l.trim_start().starts_with("# ")).collect::<Vec<_>>().join("\n");
            if body.trim().chars().count() < 6 {
                return Json(json!({ "path": path, "title": stem, "source": "keep", "renamed": false })).into_response();
            }
            let mut got = None;
            if allow_ai {
                if let Some(cfg) = ai.filter(|c| !c.base_url.is_empty()) {
                    let sample: String = body.chars().take(3000).collect();
                    let sys = "为用户的笔记起一个简洁准确的中文标题：不超过 20 个字，概括主题，不要书名号、引号、句末标点，只输出标题本身。";
                    if let Ok(t) = thirdc_mcp::agent::complete(&cfg, sys, &sample).await {
                        let t = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                        let t = t.trim_start_matches('#').trim().trim_matches(|c| "《》\"“”'「」：:".contains(c)).trim();
                        let t: String = t.chars().take(30).collect();
                        if !t.is_empty() {
                            got = Some((t, "ai"));
                        }
                    }
                }
            }
            got.unwrap_or_else(|| {
                let t = ab_first_line_title(&body);
                (t.chars().take(24).collect(), "rule")
            })
        }
    };
    let new_stem = safe_file_stem(&title);
    if new_stem.is_empty() || new_stem == stem {
        return Json(json!({ "path": path, "title": title, "source": source, "renamed": false })).into_response();
    }
    let dir = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_else(|| "Notes".into());
    let mut to = format!("{dir}/{new_stem}.md");
    let mut n = 2;
    while root.join(&to).exists() && n < 100 {
        to = format!("{dir}/{new_stem} {n}.md");
        n += 1;
    }
    if !is_safe_doc_path(&to) {
        return err(StatusCode::BAD_REQUEST, "bad target").into_response();
    }
    // 标题是占位（「# 未命名」）而我们起了新名：顺手把 H1 也换掉
    let placeholder_h1 = doc_h1(&md).map_or(false, |t| is_placeholder_name(&t));
    if placeholder_h1 && source != "h1" {
        if let Some(first) = md.lines().position(|l| l.trim_start().starts_with("# ")) {
            let mut ls: Vec<String> = md.lines().map(String::from).collect();
            ls[first] = format!("# {title}");
            let _ = std::fs::write(&abs, ls.join("\n") + "\n");
        }
    }
    if let Err(e) = std::fs::rename(&abs, root.join(&to)) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let final_stem = stem_of(&to);
    let relinked = rewrite_wikilinks(&root, &stem, &final_stem);
    let _ = st.kernel.lock().unwrap().sync_all();
    Json(json!({ "path": to, "from": path, "title": title, "source": source, "renamed": true, "relinked": relinked })).into_response()
}

/// 改名后修双链：[[旧名]] / [[旧名|别名]] / [[旧名#小节]] → 新名。
fn rewrite_wikilinks(root: &std::path::Path, old: &str, new: &str) -> usize {
    let mut changed = 0;
    let mut stack = vec![root.join("Notes")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Ok(s) = std::fs::read_to_string(&p) else { continue };
            if !s.contains(&format!("[[{old}")) {
                continue;
            }
            let mut out = s.clone();
            for tail in ["]]", "|", "#"] {
                out = out.replace(&format!("[[{old}{tail}"), &format!("[[{new}{tail}"));
            }
            if out != s && std::fs::write(&p, out).is_ok() {
                changed += 1;
            }
        }
    }
    changed
}

fn strip_fence(s: &str) -> String {
    let t = s.trim();
    if !t.starts_with("```") {
        return t.to_string();
    }
    let t = t.trim_start_matches("```");
    let t = t.trim_end_matches("```").trim();
    match t.find('\n') {
        Some(nl) if t[..nl].chars().all(|c| c.is_ascii_alphanumeric()) => t[nl + 1..].trim().to_string(),
        _ => t.to_string(),
    }
}

fn read_md(st: &Arc<AppState>, path: &str) -> Result<String, String> {
    let mut k = st.kernel.lock().unwrap();
    let _ = k.sync_throttled();
    let model = k.get_doc(path).map_err(|e| e.to_string())?;
    Ok(kernel_core::to_markdown(&model))
}

fn write_md(st: &Arc<AppState>, path: &str, md: &str) -> Result<(), String> {
    let mut k = st.kernel.lock().unwrap();
    k.put_doc(path, md).map_err(|e| e.to_string())?;
    let _ = k.sync_all();
    Ok(())
}

fn stem_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md").to_string()
}

fn unique_path(root: &std::path::Path, base: &str) -> String {
    let mut p = format!("Notes/{base}.md");
    let mut n = 2;
    while root.join(&p).exists() && n < 1000 {
        p = format!("Notes/{base}-{n}.md");
        n += 1;
    }
    p
}

/// 总结节点 → 新节点（摘要），并在原文档追加反向链接。
fn ab_summarize_node(st: &Arc<AppState>, path: &str) -> Result<Value, String> {
    let md = read_md(st, path)?;
    let stem = stem_of(path);
    let summary = ab_summarize(&md);
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let new_path = unique_path(&root, &format!("{stem}-摘要"));
    let new_stem = stem_of(&new_path);
    let content = format!("# {stem} 摘要\n\n{summary}\n\n> 来源：[[{stem}]]\n");
    write_md(st, &new_path, &content)?;
    let link_line = format!("[[{new_stem}]]");
    if !md.contains(&link_line) {
        let mut src = md.trim_end().to_string();
        src.push_str(&format!("\n\n> 摘要：{link_line}\n"));
        write_md(st, path, &src)?;
    }
    Ok(json!({"op":"summarize-node","mode":"rule","placement":"new-node","path":new_path,"markdown":summary}))
}

/// 连到：把 `[[target]]` 追加到来源文档。
fn ab_connect(st: &Arc<AppState>, from: &str, target: &str) -> Result<Value, String> {
    let md = read_md(st, from)?;
    let link = if target.starts_with("Notes/") {
        if !kernel_core::is_safe_doc_path(target) {
            return Err("invalid target path".into());
        }
        // 画布点选传精确路径，保留目录，避免同名卡片连接到错误的文档。
        format!("[[{target}]]")
    } else {
        format!("[[{}]]", target.trim().trim_start_matches("[[").trim_end_matches("]]"))
    };
    if md.contains(&link) {
        return Ok(json!({"op":"connect","mode":"rule","ok":true,"already":true,"link":link}));
    }
    let mut out = md.trim_end().to_string();
    out.push_str(&format!("\n\n{link}\n"));
    write_md(st, from, &out)?;
    Ok(json!({"op":"connect","mode":"rule","ok":true,"link":link}))
}

/// 批量打标签（正文 #标签）。
fn ab_tag_paths(st: &Arc<AppState>, paths: &[String], tag: &str) -> usize {
    let mut n = 0;
    for p in paths {
        if !is_safe_doc_path(p) {
            continue;
        }
        let Ok(md) = read_md(st, p) else { continue };
        if md.contains(&format!("#{tag}")) {
            continue;
        }
        let updated = match md.find("\n\n") {
            Some(i) => format!("{}\n#{tag}\n{}", &md[..i], &md[i..]),
            None => format!("{md}\n\n#{tag}\n"),
        };
        if write_md(st, p, &updated).is_ok() {
            n += 1;
        }
    }
    n
}

/// 合并多篇 → 新文档（保留来源链接）。
fn ab_merge(st: &Arc<AppState>, paths: &[String]) -> Result<Value, String> {
    let mut body = format!("# 合并：{} 等 {} 篇\n\n", stem_of(&paths[0]), paths.len());
    body.push_str("> 来源：");
    body.push_str(&paths.iter().map(|p| format!("[[{}]]", stem_of(p))).collect::<Vec<_>>().join(" · "));
    body.push('\n');
    for p in paths {
        let Ok(md) = read_md(st, p) else { continue };
        body.push_str(&format!("\n## {}\n\n{}\n", stem_of(p), md.trim()));
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let new_path = unique_path(&root, "合并");
    write_md(st, &new_path, &body)?;
    Ok(json!({"op":"merge","mode":"rule","placement":"new-node","path":new_path,"files":paths.len()}))
}

/// 生成结构：按一级文件夹分组，产出一页带链接的目录。
fn ab_outline(st: &Arc<AppState>, paths: &[String]) -> Result<Value, String> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in paths {
        let rel = p.trim_start_matches("Notes/");
        let group = if rel.contains('/') {
            rel.split('/').next().unwrap_or("").to_string()
        } else {
            "（根）".to_string()
        };
        groups.entry(group).or_default().push(stem_of(p));
    }
    let mut body = String::from("# 结构\n\n");
    for (g, items) in &groups {
        body.push_str(&format!("- **{g}**\n"));
        for it in items {
            body.push_str(&format!("  - [[{it}]]\n"));
        }
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let new_path = unique_path(&root, "结构");
    write_md(st, &new_path, &body)?;
    Ok(json!({"op":"outline","mode":"rule","placement":"new-node","path":new_path,"groups":groups.len()}))
}

/// 规则动词：精简（去空行/多余空白/连续重复行）。
fn ab_condense(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut prev_blank = false;
    for raw in text.lines() {
        let t = raw.trim();
        if t.is_empty() {
            if !prev_blank && !out.is_empty() {
                out.push(String::new());
                prev_blank = true;
            }
            continue;
        }
        let mut s = String::new();
        let mut sp = 0usize;
        for ch in t.chars() {
            if ch == ' ' || ch == '\t' {
                sp += 1;
                if sp <= 1 {
                    s.push(' ');
                }
            } else {
                sp = 0;
                s.push(ch);
            }
        }
        if out.last().map(|l| l == &s).unwrap_or(false) {
            continue;
        }
        out.push(s);
        prev_blank = false;
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

fn ab_first_sentence(s: &str) -> String {
    let mut out = String::new();
    for ch in s.trim().chars() {
        out.push(ch);
        if "。！？!?".contains(ch) {
            break;
        }
        if out.chars().count() >= 90 {
            break;
        }
    }
    out.trim().to_string()
}

/// 规则动词：总结（标题 + 每段首句，最多 12 条）。
fn ab_summarize(text: &str) -> String {
    let mut bullets: Vec<String> = Vec::new();
    for para in text.split("\n\n") {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(rest) = p.strip_prefix('#') {
            let h = rest.trim_start_matches('#').trim();
            if !h.is_empty() {
                bullets.push(format!("- **{h}**"));
            }
            continue;
        }
        let fs = ab_first_sentence(p);
        if !fs.is_empty() {
            bullets.push(format!("- {fs}"));
        }
        if bullets.len() >= 12 {
            break;
        }
    }
    if bullets.is_empty() {
        "-（无内容）".to_string()
    } else {
        bullets.join("\n")
    }
}

fn ab_first_line_title(text: &str) -> String {
    for l in text.lines() {
        let t = l.trim().trim_start_matches('#').trim();
        let t = t
            .trim_end_matches(|c: char| "。！？!?，,.;；:：、".contains(c))
            .trim();
        if !t.is_empty() {
            return t.chars().take(40).collect();
        }
    }
    "提取片段".to_string()
}

fn ab_slug(title: &str) -> String {
    let mut s = String::new();
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                s.push(lc);
            }
        } else {
            s.push('-');
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        format!("note-{}", now_secs())
    } else {
        s
    }
}

async fn list_docs_api(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    // 首屏/侧栏补全：只列路径。标题用文件名，不读正文、不合入 CRDT。
    // 展开文件夹时 /browse 会 peek 标题；打开文档才读全文。
    let k = st.kernel.lock().unwrap();
    let docs = match kernel_core::list_docs(&k.vault) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let root = k.vault.root.clone();
    drop(k);
    let arr: Vec<Value> = docs
        .iter()
        .filter_map(|p| p.to_str())
        .map(|p| {
            let name = p.rsplit('/').next().unwrap_or(p);
            // 标签：frontmatter + 正文 #行内（读头 4KB，与 memo 同模式；视图/检索共用）
            use std::io::Read;
            let mut buf = [0u8; 4096];
            let n = std::fs::File::open(root.join(p))
                .and_then(|mut f| f.read(&mut buf))
                .unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let mtime = std::fs::metadata(root.join(p))
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            json!({ "path": p, "title": title_from_filename(name), "tags": extract_memo_tags(&head), "mtime": mtime })
        })
        .collect();
    Json(json!({ "docs": arr })).into_response()
}

/// memo 模式的文档索引行。
#[derive(Clone, serde::Serialize)]
struct MemoDoc {
    path: String,
    title: String,
    tags: Vec<String>,
    mtime: i64,
    /// 来源：memo（Notes/Memos/）| inbox（Notes/Sources/ 剪藏与转码代理）
    #[serde(default)]
    src: String,
    /// 正文预览（首段截断，卡片直显）。
    #[serde(default)]
    excerpt: String,
}

fn memo_push_tag(out: &mut Vec<String>, t: &str) {
    let t = t.trim().trim_matches('"').trim_matches('\'')
        .trim_start_matches(['#', '[', '('])
        .trim_end_matches([',', '，', '、', ';', '；', ']', ')'])
        .trim();
    if t.is_empty() || t.chars().count() > 40 || t.contains(['[', ']', '(', ')']) {
        return; // 括号残留说明是链接/引用文本，不是标签
    }
    if !out.iter().any(|x| x == t) {
        out.push(t.to_string());
    }
}

/// 从文件头部提取标签：frontmatter 的 keywords/tags（行内或列表式）+ 正文行内 #标签。
/// 只读前 4KB —— 1.2 万篇全量读正文是秒级开销，头部覆盖绝大多数标签场景。
fn extract_memo_tags(head: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_fm = false;
    let mut fm_seen = 0usize;
    let mut list_key: Option<String> = None;
    for raw in head.lines() {
        let line = raw.trim_end();
        let t = line.trim();
        if t == "---" {
            fm_seen += 1;
            if fm_seen >= 2 {
                // frontmatter 结束，剩余部分只做行内 #标签 扫描
                in_fm = false;
                list_key = None;
                continue;
            }
            in_fm = true;
            continue;
        }
        if in_fm {
            if let Some((k, v)) = t.split_once(':') {
                let k = k.trim();
                if k == "keywords" || k == "tags" || k == "keyword" || k == "tag" {
                    list_key = if v.trim().is_empty() { Some(k.to_string()) } else { None };
                    if !v.trim().is_empty() {
                        // 行内写法：keywords: a, b 或 YAML 流式 [a, b] —— 按逗号切，不按空格
                        let flow = v.trim().trim_start_matches('[').trim_end_matches(']');
                        for part in flow.split([',', '，', '、', ';', '；']) {
                            memo_push_tag(&mut out, part);
                        }
                    }
                    continue;
                }
                if list_key.is_some() {
                    list_key = None; // 下一个键，列表结束
                }
                continue;
            }
            if let Some(key) = &list_key {
                if let Some(item) = t.strip_prefix("- ") {
                    let _ = key;
                    memo_push_tag(&mut out, item);
                }
            }
            continue;
        }
        // 正文：行内 #标签。跳过 markdown 标题行（# 后带空格）；#后紧跟字母/数字才算。
        let heading = t.starts_with('#')
            && t[1..].starts_with(|c: char| c.is_whitespace() || t[1..].trim_start_matches('#').starts_with(' '));
        if !heading {
            for tok in t.split_whitespace() {
                let mut cs = tok.chars();
                if cs.next() != Some('#') {
                    continue;
                }
                let rest = cs.as_str();
                // 中段标点一并截断：#工作,节奏不错 → 工作（全角/半角都算边界）
                let core = rest
                    .split(|c: char| ".,;!?，。；！？、:：)）」\"'".contains(c))
                    .next()
                    .unwrap_or(rest)
                    .trim_end_matches(|c: char| ".,;!?，。；！？)）」\"'".contains(c));
                let mut rc = core.chars();
                let valid = match rc.next() {
                    Some(c) if c.is_alphanumeric() => true,
                    _ => false,
                };
                if !valid {
                    continue; // `#`、`##`、`#!` 之类都不是标签
                }
                if !core.is_empty() {
                    memo_push_tag(&mut out, core);
                }
            }
        }
        if out.len() >= 12 {
            break; // 头部足够了
        }
    }
    out
}

/// 构建 memo 文档索引：路径/标题来自索引表，标签读每篇头部 4KB。
/// 只统计 memo 笔记本（Thino 式每日随手记目录 `Notes/Memos/`），不再扫全 Notes。
fn memo_build_sync(root: &std::path::Path, stats: &HashMap<String, (i64, i64)>) -> Vec<MemoDoc> {
    use std::io::Read;
    let mut docs: Vec<MemoDoc> = Vec::with_capacity(stats.len());
    for (p, (mt, sz)) in stats {
        let is_memo = p.starts_with("Notes/Memos/");
        // inbox：剪藏与转码代理（Sources/ 下任何 md/html——记录性内容，进 memo 流可预览）
        let is_inbox = p.starts_with("Notes/Sources/");
        if !(is_memo || is_inbox) || !(p.ends_with(".md") || p.ends_with(".html")) {
            continue;
        }
        let mut buf = [0u8; 4096];
        let n = std::fs::File::open(root.join(p))
            .and_then(|mut f| f.read(&mut buf))
            .unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let name = p.rsplit('/').next().unwrap_or(p);
        // 预览：跳过 frontmatter 与标题行，首段截 140 字符
        let excerpt: String = head
            .lines()
            .skip_while(|l| l.trim() == "---" || l.starts_with("tags:") || l.starts_with("created:") || l.trim_start_matches('-').trim().is_empty())
            .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#') && l.trim() != "---")
            .map(|l| l.trim().chars().take(140).collect())
            .unwrap_or_default();
        docs.push(MemoDoc {
            path: p.clone(),
            title: title_from_filename(name),
            tags: extract_memo_tags(&head),
            mtime: *mt,
            src: if is_memo { "memo".into() } else { "inbox".into() },
            excerpt: if is_memo || *sz < 200_000 { excerpt } else { String::new() },
        });
    }
    docs.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    docs
}

/// GET /memo/docs —— memo 模式数据源（按数据代数缓存；日期/标签筛选由前端做）。
async fn memo_docs(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let epoch = {
        let mut k = st.kernel.lock().unwrap();
        let _ = k.sync_throttled();
        k.data_epoch()
    };
    let cached = st.memo_cache.lock().unwrap().clone();
    let body = match cached {
        Some((e, b)) if e == epoch => b,
        _ => {
            let stats = st.kernel.lock().unwrap().doc_stats().unwrap_or_default();
            let root = st.kernel.lock().unwrap().vault.root.clone();
            let docs = memo_build_sync(&root, &stats);
            let payload = json!({ "docs": docs, "total": docs.len() });
            let built = axum::body::Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());
            *st.memo_cache.lock().unwrap() = Some((epoch, built.clone()));
            built
        }
    };
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn health(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    Json(json!({ "ok": true, "name": "thirdc", "version": env!("CARGO_PKG_VERSION"), "api": HOST_API_LEVEL })).into_response()
}

/// 公开版本端点（无 token）：浏览器版「关于」页显示当前版本，桌面端走 Tauri updater。
async fn version() -> impl IntoResponse {
    Json(json!({
        "name": "litmus",
        "version": env!("CARGO_PKG_VERSION"),
        "shell": shell_version(),
        "api": HOST_API_LEVEL,
    }))
}

async fn status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    // 索引优先；空索引（刚 init / 尚未 sync）回退 WalkDir，避免状态栏显示 0。
    let indexed = k.indexed_count().unwrap_or(0);
    let docs = if indexed > 0 {
        indexed
    } else {
        kernel_core::list_docs(&k.vault).map(|d| d.len()).unwrap_or(0)
    };
    let assets = k.assets_count().unwrap_or(0);
    Json(json!({
        "vault": k.vault.config.name,
        "root": k.vault.root.display().to_string(),
        "docs": docs,
        "indexed": indexed,
        "assets": assets,
    }))
    .into_response()
}

async fn search_hybrid(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let query = q.get("q").cloned().unwrap_or_default();
    // 第一程（锁内）：FTS + TF-IDF + 取 embeddings 配置
    let (fts_len, hybrid, embed_cfg) = {
        let mut k = st.kernel.lock().unwrap();
        if let Err(e) = k.sync_throttled() {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        let fts = k.search(&query).unwrap_or_default();
        let hybrid = match k.search_hybrid(&query, 20) {
            Ok(m) => m,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
        let model = k.embedding_model();
        let endpoint = (!model.is_empty()).then(|| k.embedding_endpoint()).flatten();
        (fts.len(), hybrid, endpoint.map(|(b, key)| (b, key, model)))
    };
    // 第二程（锁外网络）：查询向量。失败静默降级为两路结果。
    let mut dense_count = 0usize;
    let mut dense_model = String::new();
    if let Some((base_url, api_key, model)) = embed_cfg {
        dense_model = model.clone();
        let input = kernel_core::rag::embed_text(&query);
        let embedded = tokio::task::spawn_blocking(move || {
            kernel_core::rag::embed_batch(&base_url, &api_key, &model, &[input])
        }).await.ok().and_then(|r| r.ok()).and_then(|mut v| if v.len() == 1 { Some(v.remove(0)) } else { None });
        if let Some(qv) = embedded {
            let mut k = st.kernel.lock().unwrap();
            if let Ok(dense) = k.search_dense(&qv, 20) {
                dense_count = dense.len();
                let merged = kernel_core::rag::rrf_merge_n(&[&hybrid, &dense], 20);
                return hybrid_response(query, fts_len, merged, dense_count, dense_model);
            }
        } else {
            eprintln!("search/hybrid: 查询向量失败，降级为 FTS+TF-IDF");
        }
    }
    hybrid_response(query, fts_len, hybrid, dense_count, dense_model)
}

fn hybrid_response(
    query: String,
    fts_len: usize,
    merged: Vec<(String, f64)>,
    dense_count: usize,
    dense_model: String,
) -> axum::response::Response {
    let hits: Vec<Value> = merged.iter()
        .map(|(path, score)| json!({ "path": path, "score": score, "source": "rrf" }))
        .collect();
    Json(json!({
        "query": query,
        "hits": hits,
        "fts_count": fts_len,
        "vec_count": merged.len(),
        "dense_count": dense_count,
        "dense_model": dense_model,
    })).into_response()
}

// ── RAG 密向量重建（AI embeddings 旁路索引；向量只进 doc_vectors，可整表重建）──────

/// 建向量同步任务：跑在 spawn_blocking 或 preheat 线程上。
/// 增量：vector_stale 只补缺失/过期项；文本取自索引语料（与检索同 scope：Notes/ md/html）。
pub fn rag_job_blocking(state: Arc<AppState>) {
    use std::sync::atomic::Ordering;
    if state.rag.running.swap(true, Ordering::SeqCst) {
        return; // 已有一轮在跑
    }
    *state.rag.last_error.lock().unwrap() = String::new();
    let t0 = std::time::Instant::now();
    (|| {
        let (model, endpoint, stale, corpus) = {
            let mut k = state.kernel.lock().unwrap();
            let model = k.embedding_model();
            if model.is_empty() { return; } // 未配置：静默跳过
            let endpoint = match k.embedding_endpoint() {
                Some(v) => v,
                None => { *state.rag.last_error.lock().unwrap() = "[ai].base_url 未配置".into(); return; }
            };
            let stale = k.vector_stale().unwrap_or_default();
            let corpus: std::collections::HashMap<String, String> = k.hybrid_snapshot()
                .map(|(_, c)| c.into_iter().collect())
                .unwrap_or_default();
            (model, endpoint, stale, corpus)
        };
        let todo: Vec<(String, i64)> = stale.into_iter().filter(|(p, _)| corpus.contains_key(p)).collect();
        state.rag.total.store(todo.len(), Ordering::SeqCst);
        state.rag.done.store(0, Ordering::SeqCst);
        if todo.is_empty() { return; }
        for batch in todo.chunks(16) {
            let inputs: Vec<String> = batch.iter()
                .map(|(p, _)| kernel_core::rag::embed_text(corpus.get(p).map(String::as_str).unwrap_or("")))
                .collect();
            let (base_url, api_key) = endpoint.clone();
            let out = kernel_core::rag::embed_batch(&base_url, &api_key, &model, &inputs);
            match out {
                Ok(vecs) => {
                    let rows: Vec<(String, i64, Vec<f32>)> = batch.iter().cloned()
                        .zip(vecs).map(|((p, m), v)| (p, m, v)).collect();
                    if let Err(e) = state.kernel.lock().unwrap().vectors_store(&model, &rows) {
                        *state.rag.last_error.lock().unwrap() = e.to_string();
                        return;
                    }
                    state.rag.done.fetch_add(batch.len(), Ordering::SeqCst);
                }
                Err(e) => {
                    *state.rag.last_error.lock().unwrap() = e;
                    return;
                }
            }
        }
    })();
    state.rag.running.store(false, Ordering::SeqCst);
    eprintln!(
        "rag: 密向量就绪 {}/{}（+{}ms，模型 {}）",
        state.rag.done.load(Ordering::SeqCst),
        state.rag.total.load(Ordering::SeqCst),
        t0.elapsed().as_millis(),
        { state.kernel.lock().unwrap().embedding_model() }
    );
}

/// 触发密向量重建（后台跑，进度看 /rag/status）。
async fn rag_reindex(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let model = { st.kernel.lock().unwrap().embedding_model() };
    if model.is_empty() {
        return Json(json!({ "ok": false, "note": "未配置 [ai].embedding_model，密向量未启用" })).into_response();
    }
    let already = st.rag.running.load(std::sync::atomic::Ordering::SeqCst);
    if !already {
        let st2 = st.clone();
        tokio::task::spawn_blocking(move || rag_job_blocking(st2));
    }
    Json(json!({
        "ok": true,
        "queued": !already,
        "note": if already { "已有重建任务在跑，进度看 /rag/status" } else { "密向量重建已在后台开始" },
    })).into_response()
}

// ── E2EE 备份（kernel-backup：age 快照 → webdav/local/rclone；3-2-1 合规）──────

/// 备份执行体（spawn_blocking / 调度线程调用）。
pub fn backup_job_blocking(state: Arc<AppState>, filter: Option<String>) {
    use std::sync::atomic::Ordering;
    if state.backup.running.swap(true, Ordering::SeqCst) {
        return;
    }
    let outcome = (|| -> Result<(), String> {
        let (cfg, root) = {
            let k = state.kernel.lock().unwrap();
            let cfg = k.vault.config.backup.clone().ok_or("未配置 [backup]")?;
            (cfg, k.vault.root.clone())
        };
        let (reports, st) = kernel_backup::run_backup(&root, &cfg, filter.as_deref())
            .map_err(|e| e.to_string())?;
        for r in &reports {
            if !r.ok {
                return Err(format!("{}：{}", r.target, r.error));
            }
        }
        kernel_backup::save_state(&root, &st);
        let (ok, msg) = kernel_backup::compliance(&st);
        eprintln!("backup: {}（{}）", msg, if ok { "合规" } else { "待补目标" });
        Ok(())
    })();
    match outcome {
        Ok(()) => *state.backup.last_error.lock().unwrap() = String::new(),
        Err(e) => *state.backup.last_error.lock().unwrap() = e,
    }
    state.backup.running.store(false, Ordering::SeqCst);
}

/// 触发备份（后台跑）。可选 { "target": "名字" } 只跑一个目标。
async fn backup_run(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let filter: Option<String> = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v.get("target").and_then(|t| t.as_str()).map(String::from),
            Err(_) => None,
        }
    };
    let cfg_ready = { let k = st.kernel.lock().unwrap(); k.vault.config.backup.is_some() };
    if !cfg_ready {
        return Json(json!({ "ok": false, "note": "未配置 [backup]（thirdc.toml）" })).into_response();
    }
    let already = st.backup.running.load(std::sync::atomic::Ordering::SeqCst);
    if !already {
        let st2 = st.clone();
        tokio::task::spawn_blocking(move || backup_job_blocking(st2, filter));
    }
    Json(json!({
        "ok": true,
        "queued": !already,
        "note": if already { "已有备份在跑，看 /backup/status" } else { "备份已在后台开始" },
    })).into_response()
}

/// 备份状态：任务 + 各目标上次结果 + 3-2-1 合规。
async fn backup_status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let (root, configured) = {
        let k = st.kernel.lock().unwrap();
        (k.vault.root.clone(), k.vault.config.backup.is_some())
    };
    let state = kernel_backup::load_state(&root);
    let (ok, msg) = kernel_backup::compliance(&state);
    Json(json!({
        "configured": configured,
        "running": st.backup.running.load(std::sync::atomic::Ordering::SeqCst),
        "last_error": st.backup.last_error.lock().unwrap().clone(),
        "compliance": { "ok": ok, "message": msg },
        "targets": state.targets,
    })).into_response()
}

/// 密向量状态：模型、覆盖率、任务进度。
async fn rag_status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    use std::sync::atomic::Ordering;
    let (model, covered, docs) = {
        let k = st.kernel.lock().unwrap();
        let m = k.embedding_model();
        let (c, d) = k.vector_stats().unwrap_or((0, 0));
        (m, c, d)
    };
    Json(json!({
        "model": model,
        "covered": covered,
        "docs": docs,
        "running": st.rag.running.load(Ordering::SeqCst),
        "done": st.rag.done.load(Ordering::SeqCst),
        "total": st.rag.total.load(Ordering::SeqCst),
        "last_error": st.rag.last_error.lock().unwrap().clone(),
    })).into_response()
}

// ── 凭证库：本地登录凭证/密码的简单保管入口 ─────────────────────────────
// 文件真相：.thirdc/secrets.json（unix 0600，.thirdc 天然不入 git）。
// 与 machine.toml 的 token 同一威胁模型——拿到磁盘/TOKEN 的人本就能读到一切。

fn secrets_path(vault_root: &std::path::Path) -> std::path::PathBuf {
    vault_root.join(".thirdc").join("secrets.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn secrets_load(vault_root: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(secrets_path(vault_root))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("secrets").and_then(|s| s.as_array()).cloned())
        .unwrap_or_default()
}

fn secrets_save(vault_root: &std::path::Path, rows: &[Value]) -> Result<(), String> {
    let p = secrets_path(vault_root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = json!({ "secrets": rows, "updatedAt": now_ms() });
    std::fs::write(&p, serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

async fn secrets_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let rows = { let k = st.kernel.lock().unwrap(); secrets_load(&k.vault.root) };
    Json(json!({ "secrets": rows })).into_response()
}

async fn secrets_put(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 name").into_response();
    }
    let entry = json!({
        "name": name,
        "value": req.get("value").and_then(|v| v.as_str()).unwrap_or(""),
        "note": req.get("note").and_then(|n| n.as_str()).unwrap_or(""),
        "updated": now_ms(),
    });
    let out = {
        let k = st.kernel.lock().unwrap();
        let mut rows: Vec<Value> = secrets_load(&k.vault.root)
            .into_iter().filter(|r| r.get("name").and_then(|n| n.as_str()) != Some(&name)).collect();
        rows.insert(0, entry);
        match secrets_save(&k.vault.root, &rows) {
            Ok(()) => rows.len(),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    };
    Json(json!({ "ok": true, "count": out })).into_response()
}

async fn secrets_delete(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let name = q.get("name").cloned().unwrap_or_default();
    let removed = {
        let k = st.kernel.lock().unwrap();
        let before = secrets_load(&k.vault.root);
        let after: Vec<Value> = before
            .iter().filter(|r| r.get("name").and_then(|n| n.as_str()) != Some(name.as_str())).cloned().collect();
        let removed = before.len() - after.len();
        match secrets_save(&k.vault.root, &after) {
            Ok(()) => removed,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    };
    Json(json!({ "ok": true, "removed": removed })).into_response()
}

async fn search(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let query = q.get("q").cloned().unwrap_or_default();
    let mut k = st.kernel.lock().unwrap();
    if let Err(e) = k.sync_throttled() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    match k.search(&query) {
        Ok(hits) => {
            /* 文件即真相：命中非文档文件时标注 kind=file，前端走下载/系统打开而非编辑器 */
            let fileset: std::collections::HashSet<String> =
                k.index_files().unwrap_or_default().into_iter().collect();
            let arr: Vec<Value> = hits
                .into_iter()
                .map(|(path, rank)| {
                    let kind = if fileset.contains(&path) { "file" } else { "doc" };
                    json!({ "path": path, "rank": rank, "kind": kind })
                })
                .collect();
            Json(json!({ "query": query, "hits": arr })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_doc(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    if !is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    if let Err(e) = k.sync_throttled() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let html = k.render_doc_html_with_views(&path).ok();
    let source = std::fs::read_to_string(k.vault.root.join(&path)).unwrap_or_default();
    let meta = std::fs::metadata(k.vault.root.join(&path)).ok();
    let format = if kernel_core::is_html_rel(&path) {
        "html"
    } else if kernel_core::is_verbatim_rel(&path) {
        "text"
    } else {
        "markdown"
    };
    match k.get_doc(&path) {
        Ok(model) => {
            let doc_tags = extract_tags(&kernel_core::to_markdown(&model));
            Json(json!({
            "path": path,
            "title": model.title,
            "format": format,
            "tags": doc_tags,
            "source": source,
            "blocks": model.blocks,
            "markdown": kernel_core::to_markdown(&model),
            "html": html,
            "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            "mtime": meta.as_ref().and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64).unwrap_or(0),
        }))
        .into_response()
        }
        Err(e) => err(StatusCode::NOT_FOUND, e).into_response(),
    }
}

/// 复制文档（文件真相的平凡操作：读源文件字节 → put_doc 到新路径）。
async fn copy_doc(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let from = req.get("from").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let to = req.get("to").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !kernel_core::is_safe_doc_path(&from) || !kernel_core::is_safe_doc_path(&to) {
        return err(StatusCode::BAD_REQUEST, "paths must be under Notes/").into_response();
    }
    if from == to {
        return err(StatusCode::BAD_REQUEST, "源与目标相同").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    if let Err(e) = k.sync_throttled() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let text = match std::fs::read_to_string(k.vault.root.join(&from)) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::NOT_FOUND, format!("源不可读：{e}")).into_response(),
    };
    match k.put_doc(&to, &text) {
        Ok(()) => Json(json!({ "written": to })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 原文件下载（Notes/ 下任意文档字节级原样，Content-Disposition 附件）。
async fn raw_doc(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    if !kernel_core::is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let abs = k.vault.root.join(&path);
    match std::fs::read(&abs) {
        Ok(bytes) => {
            let name = path.rsplit('/').next().unwrap_or("file");
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename*=UTF-8''{}", urlencoding_encode(name)),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => err(StatusCode::NOT_FOUND, "file not found").into_response(),
    }
}

/// RFC 5987 filename* 的最小百分号编码。
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn put_doc(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    if !is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let md = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return err(StatusCode::BAD_REQUEST, "body must be utf-8 markdown").into_response(),
    };
    let mut k = st.kernel.lock().unwrap();
    match k.put_doc(&path, md) {
        Ok(()) => {
            let html = k.render_doc_html_with_views(&path).ok();
            Json(json!({ "written": path, "html": html })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_doc(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    if !is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    let abs = k.vault.root.join(&path);
    if !abs.exists() {
        return err(StatusCode::NOT_FOUND, "not found").into_response();
    }
    let days = k.vault.config.trash_days.unwrap_or(30);
    let _ = trash_purge_expired(&k.vault.root, days);
    let mut entries = trash_load(&k.vault.root);
    let trash_rel = trash_unique_rel(&k.vault.root, &entries, &path);
    let dest = trash_dir(&k.vault.root).join(&trash_rel);
    if let Some(p) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(p) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    // rename 对文件与文件夹都成立（软删除 = 移入回收站，可恢复）
    match std::fs::rename(&abs, &dest) {
        Ok(()) => {
            entries.push(TrashEntry {
                path: path.clone(),
                trash: trash_rel,
                deleted_at: trash_now(),
            });
            if let Err(e) = trash_save(&k.vault.root, &entries) {
                // 清单写失败则回滚，避免“移走了却查不到”
                let _ = std::fs::rename(&dest, &abs);
                return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
            }
            let _ = k.sync_all();
            Json(json!({ "deleted": path, "trash": true, "retention_days": days })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/* ───────── 回收站（软删除 / 恢复 / 保留期） ─────────
 * 删除 = 移入 `.thirdc/trash/<相对路径>`，清单 `.thirdc/trash/index.json`。
 * 保留期取 thirdc.toml `trash_days`：缺省 30 天，0 = 永久保留。 */

fn trash_dir(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".thirdc").join("trash")
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct TrashEntry {
    /// 原始库内路径（Notes/…）
    path: String,
    /// 回收站内相对路径（同 path；冲突时带 .1/.2 后缀）
    trash: String,
    /// 删除时刻（unix 秒）
    deleted_at: u64,
}

fn trash_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn trash_load(root: &std::path::Path) -> Vec<TrashEntry> {
    std::fs::read_to_string(trash_dir(root).join("index.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn trash_save(root: &std::path::Path, entries: &[TrashEntry]) -> std::io::Result<()> {
    let dir = trash_dir(root);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(entries)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(dir.join("index.json"), json)
}

fn trash_unique_rel(root: &std::path::Path, entries: &[TrashEntry], path: &str) -> String {
    let mut cand = path.to_string();
    let mut i = 1usize;
    // 查重不能只看清单：回收站里可能有清单之外的物理残留。最典型场景——
    // 子项先逐个删除（回收站里它们都躺在 <父目录>/ 容器下，清单只有子项
    // 记录），再删父目录时 rename 目标撞上这个非空容器 → ENOTEMPTY。
    // 这里必须清单与文件系统一起查。
    while entries.iter().any(|e| e.trash == cand) || trash_dir(root).join(&cand).exists() {
        cand = format!("{path}.{i}");
        i += 1;
    }
    cand
}

/// 清除超过保留期的条目；返回清除数。days=0 → 永久保留。
fn trash_purge_expired(root: &std::path::Path, days: u32) -> usize {
    if days == 0 {
        return 0;
    }
    let mut entries = trash_load(root);
    let now = trash_now();
    let before = entries.len();
    let limit = days as u64 * 86_400;
    entries.retain(|e| {
        if now.saturating_sub(e.deleted_at) >= limit {
            let p = trash_dir(root).join(&e.trash);
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            false
        } else {
            true
        }
    });
    let removed = before - entries.len();
    if removed > 0 {
        let _ = trash_save(root, &entries);
    }
    removed
}

/// GET /trash — 回收站列表（顺带清除过期）
async fn trash_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let days = k.vault.config.trash_days.unwrap_or(30);
    let _ = trash_purge_expired(&k.vault.root, days);
    let entries = trash_load(&k.vault.root);
    let now = trash_now();
    let items: Vec<Value> = entries
        .iter()
        .rev()
        .map(|e| {
            let elapsed_d = now.saturating_sub(e.deleted_at) / 86_400;
            json!({
                "path": e.path,
                "deleted_at": e.deleted_at,
                "days_left": if days == 0 { Value::Null } else { json!(days.saturating_sub(elapsed_d as u32)) },
            })
        })
        .collect();
    Json(json!({ "retention_days": days, "entries": items })).into_response()
}

/// POST /trash/restore {path} — 恢复到原路径
async fn trash_restore(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
    if !is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    let mut entries = trash_load(&k.vault.root);
    let Some(idx) = entries.iter().position(|e| e.path == path) else {
        return err(StatusCode::NOT_FOUND, "not in trash").into_response();
    };
    let src = trash_dir(&k.vault.root).join(&entries[idx].trash);
    let dst = k.vault.root.join(&path);
    if dst.exists() {
        return err(StatusCode::CONFLICT, "target path already exists").into_response();
    }
    if let Some(p) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(p) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    if let Err(e) = std::fs::rename(&src, &dst) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    entries.remove(idx);
    if let Err(e) = trash_save(&k.vault.root, &entries) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let _ = k.sync_all();
    Json(json!({ "restored": path })).into_response()
}

/// POST /trash/purge {path} 或 {all:true} — 彻底删除（不可恢复）
async fn trash_purge(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let all = req.get("all").and_then(|a| a.as_bool()).unwrap_or(false);
    let path = req.get("path").and_then(|p| p.as_str()).map(|s| s.to_string());
    if !all && path.is_none() {
        return err(StatusCode::BAD_REQUEST, "需要 path 或 all:true").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let mut entries = trash_load(&k.vault.root);
    let mut purged = 0usize;
    if all {
        for e in &entries {
            let p = trash_dir(&k.vault.root).join(&e.trash);
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            purged += 1;
        }
        entries.clear();
    } else {
        let path = path.unwrap();
        if let Some(idx) = entries.iter().position(|e| e.path == path) {
            let p = trash_dir(&k.vault.root).join(&entries[idx].trash);
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            entries.remove(idx);
            purged += 1;
        }
    }
    if let Err(e) = trash_save(&k.vault.root, &entries) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(json!({ "purged": purged })).into_response()
}

async fn post_asset(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let name = q.get("name").cloned().unwrap_or_else(|| "asset".into());
    let mut k = st.kernel.lock().unwrap();
    match k.put_asset(&name, &body) {
        Ok(meta) => {
            let md = Kernel::asset_markdown(&meta, &name);
            Json(json!({
                "hash": meta.hash, "path": meta.rel, "mime": meta.mime,
                "size": meta.size, "markdown": md,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn sync(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    // 分批：单次最多 1500 篇，避免大库首次索引时 daemon 无响应
    match k.sync_limited(1500) {
        Ok((changed, more)) => Json(json!({ "changed": changed, "more": more })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 只允许 Notes/ 下的相对路径（防目录穿越）。
fn is_safe_doc_path(p: &str) -> bool {
    !p.is_empty()
        && p.starts_with("Notes/")
        && !p.contains("..")
        && !p.starts_with('/')
        && !p.contains('\\')
}

/// 启动后台 watcher：外部改动实时进入内核（共享同一 Kernel 锁）。
pub fn spawn_watcher(state: Arc<AppState>) -> anyhow::Result<()> {
    let sidecar = state.kernel.lock().unwrap().vault.sidecar();
    let notes_dir = state.kernel.lock().unwrap().vault.notes_dir();
    let (tx, rx) = std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    use notify::Watcher;
    watcher.watch(&notes_dir, notify::RecursiveMode::Recursive)?;

    std::thread::spawn(move || {
        let _keep = watcher;
        while rx.recv().is_ok() {
            // 防抖
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
            while rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).is_ok() {}
            let mut k = state.kernel.lock().unwrap();
            match k.sync_all() {
                Ok(n) if n > 0 => {
                    println!("[thirdc] watcher merged {n} external change(s)");
                    drop(k);
                    maybe_auto_publish(&state, &sidecar);
                }
                Ok(_) => {}
                Err(e) => eprintln!("[thirdc] watcher sync error: {e}"),
            }
        }
    });
    Ok(())
}

/// 打开库并构建状态。
pub fn build_state(vault: Vault) -> anyhow::Result<Arc<AppState>> {
    let machine = vault.ensure_machine()?;
    let kernel = Arc::new(Mutex::new(Kernel::open(vault)?));
    let mcp = Arc::new(thirdc_mcp::McpServer::new(kernel.clone()));
    Ok(Arc::new(AppState {
        kernel,
        mcp,
        hub: Arc::new(PresenceHub::new()),
        token: machine.token,
        memo_cache: Arc::new(Mutex::new(None)),
        desktop: std::env::var("THIRDC_DESKTOP").map(|v| v == "1").unwrap_or(false),
        pull: Arc::new(Mutex::new(PullJob::default())),
        rag: Arc::new(RagJob::default()),
        backup: Arc::new(BackupJob::default()),
    }))
}

/// 在指定地址启动 daemon（阻塞）。
pub async fn serve(addr: &str, state: Arc<AppState>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(listener, state).await
}

/// 用已绑定的 listener 启动 daemon（桌面端用来拿实际端口）。
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
) -> anyhow::Result<()> {
    // 启动预热：后台分批全量同步（每批 2000 篇，批间释放内核锁），
    // 把首次点开文档的冷同步挪到 daemon 起步时，用户登录后即是热路径。
    let preheat = state.kernel.clone();
    let preheat_state = state.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        loop {
            let Ok(mut k) = preheat.lock() else { return };
            let Ok((n, more)) = k.sync_limited(2000) else { return };
            if !more {
                k.mark_synced();
                eprintln!("preheat: 索引就绪（{}ms，本批 {} 篇变更）", t0.elapsed().as_millis(), n);
                // 语料/memo 预热在锁外构建：锁内只取快照（秒级），构建期间站点照常服务
                let hw = t0.elapsed();
                let snap = match k.hybrid_snapshot() {
                    Ok(v) => v,
                    Err(e) => { eprintln!("preheat: 语料快照失败：{e}"); return; }
                };
                let memo_stats = k.doc_stats().unwrap_or_default();
                let memo_root = k.vault.root.clone();
                drop(k); // 长构建全程不持内核锁

                let idx = kernel_core::rag::TfidfIndex::build(&snap.1);
                if let Ok(mut k) = preheat.lock() {
                    k.hybrid_install(snap.0, idx);
                    eprintln!("preheat: 混合检索语料就绪（+{}ms）", (t0.elapsed() - hw).as_millis());
                }
                let hm = t0.elapsed();
                let docs = memo_build_sync(&memo_root, &memo_stats);
                let payload = json!({ "docs": docs, "total": docs.len() });
                let built = axum::body::Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());
                *preheat_state.memo_cache.lock().unwrap() = Some((snap.0.0 as u64, built));
                eprintln!("preheat: memo 索引就绪（+{}ms）", (t0.elapsed() - hm).as_millis());
                // 密向量增量补齐：未配置 [ai].embedding_model 时任务内部直接返回
                let rag_state = preheat_state.clone();
                std::thread::spawn(move || rag_job_blocking(rag_state));
                // E2EE 备份自动调度：auto_hours>0 且距上次成功到期则自跑（每 30 分钟巡检一次）
                {
                    let bs = preheat_state.clone();
                    std::thread::spawn(move || loop {
                        std::thread::sleep(std::time::Duration::from_secs(1800));
                        let Ok(k) = bs.kernel.lock() else { continue };
                        let Some(cfg) = k.vault.config.backup.clone() else { continue };
                        let root = k.vault.root.clone();
                        drop(k);
                        if cfg.auto_hours == 0 || cfg.passphrase.is_empty() { continue; }
                        let st = kernel_backup::load_state(&root);
                        if bs.backup.running.load(std::sync::atomic::Ordering::SeqCst) { continue; }
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                        let due = st.targets.values().all(|t| t.last_ok == 0)
                            || st.targets.values().filter(|t| t.last_ok > 0).count() == 0
                            || st.targets.values().all(|t| now.saturating_sub(t.last_ok) > cfg.auto_hours as u64 * 3600);
                        if due {
                            let st2 = bs.clone();
                            std::thread::spawn(move || backup_job_blocking(st2, None));
                        }
                    });
                }
                return;
            }
            drop(k); // 批间放锁，UI 请求可插入
        }
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_router() -> (Router, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path(), "api-test").unwrap();
        let state = build_state(vault).unwrap();
        let token = state.token.clone();
        (router(state), token, dir)
    }

    #[tokio::test]
    async fn comments_crud_and_path_guard() {
        let (app, token, _dir) = test_router();
        let auth = format!("Bearer {token}");
        let app = app.clone();
        // 先建一篇文档
        let _ = app.clone()
            .oneshot(
                Request::builder()
                    .uri("/doc?path=Notes/c.md")
                    .header("authorization", &auth)
                    .method("PUT")
                    .header("content-type", "text/markdown")
                    .body(Body::from("# C"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let add = |body: String| {
            let app = app.clone();
            let auth = auth.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri("/comments")
                        .header("authorization", &auth)
                        .method("POST")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        // add
        let resp = add(r#"{"path":"Notes/c.md","op":"add","start":5,"end":9,"quote":"正文","author":"小明","text":"这里要扩写"}"#.to_string()).await;
        assert_eq!(resp.status(), 200);
        let v = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&v).unwrap();
        let id = v["comments"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(v["comments"][0]["author"], "小明");
        // reply
        let resp = add(format!(r#"{{"path":"Notes/c.md","op":"reply","id":"{id}","author":"小红","text":"同意"}}"#)).await;
        assert_eq!(resp.status(), 200);
        // resolve
        let resp = add(format!(r#"{{"path":"Notes/c.md","op":"resolve","id":"{id}","resolved":true}}"#)).await;
        assert_eq!(resp.status(), 200);
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(v["comments"][0]["resolved"], true);
        assert_eq!(v["comments"][0]["replies"].as_array().unwrap().len(), 1);
        // delete
        let resp = add(format!(r#"{{"path":"Notes/c.md","op":"delete","id":"{id}"}}"#)).await;
        assert_eq!(resp.status(), 200);
        // 越权路径拒绝
        let resp = add(r#"{"path":"../etc/passwd","op":"add"}"#.to_string()).await;
        assert_eq!(resp.status(), 400);
        // GET
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/comments?path=Notes/c.md")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(v["comments"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn inline_html_project_folds_local_refs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("style.css"), "body{background:url(bg.png)}h1{color:red}").unwrap();
        std::fs::write(root.join("bg.png"), b"\x89PNG fake").unwrap();
        std::fs::write(root.join("app.js"), "console.log('hi')").unwrap();
        std::fs::write(
            root.join("index.html"),
            "<!doctype html><head><link rel=\"stylesheet\" href=\"style.css\">\
             <script src=\"app.js\"></script></head>\
             <body><img src=\"assets/../bg.png\"><img src=\"https://x.com/a.png\">\
             <a href=\"other.html\">next</a></body>",
        )
        .unwrap();

        let out = inline_html_project(
            &std::fs::read_to_string(root.join("index.html")).unwrap(),
            root,
            std::path::Path::new(""),
        );

        assert!(out.contains("<style>"), "css 应内联为 style：{out}");
        assert!(out.contains("h1{color:red}"));
        assert!(out.contains("data:image/png;base64"), "bg.png 应转 data URI：{out}");
        assert!(out.contains("console.log('hi')"), "js 应内联：{out}");
        assert!(!out.contains("<script src="), "script src 引用应消失");
        assert!(out.contains("https://x.com/a.png"), "外链图片原样保留");
        assert!(out.contains("href=\"other.html\""), "页面链接不动");
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn peek_doc_reads_excerpt_tags_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.md");
        std::fs::write(&f, "---\ntags: [化学, \"设计\"]\nstatus: x\n---\n# 石蕊\n\n石蕊遇酸**变红**，见 [[CRDT 与文件真相|CRDT]] 与 [[路线#b2]]。\n\n```rust\nfn x(){}\n```\n").unwrap();
        let pk = peek_doc(&f, "a");
        assert_eq!(pk.title, "石蕊");
        assert_eq!(pk.tags, vec!["化学", "设计"]);
        assert_eq!(pk.links, vec!["CRDT 与文件真相", "路线"]);
        assert!(pk.excerpt.starts_with("石蕊遇酸变红"), "{}", pk.excerpt);
        assert!(!pk.excerpt.contains("fn x"), "代码块不进摘要：{}", pk.excerpt);
        // 无 frontmatter 时取正文 #标签；标题与编号不算
        let g = dir.path().join("b.md");
        std::fs::write(&g, "# 标题\n- 10:20 读完一本书 #阅读 #设计 见 #1\n").unwrap();
        assert_eq!(peek_doc(&g, "b").tags, vec!["阅读", "设计"]);
    }

    #[tokio::test]
    async fn browse_peeks_title_without_full_body() {
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        std::fs::create_dir_all(dir.path().join("Notes/Topic")).unwrap();
        let mut body = String::from("# 你好世界\n\n");
        body.push_str(&"x".repeat(80_000));
        std::fs::write(dir.path().join("Notes/hello.md"), body).unwrap();
        std::fs::write(dir.path().join("Notes/Topic/a.md"), "no heading\n").unwrap();

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/browse?path=Notes")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let docs = v["docs"].as_array().unwrap();
        assert!(
            docs.iter().any(|d| d["title"] == "你好世界"),
            "expected peeked heading, got {v}"
        );
        let folders = v["folders"].as_array().unwrap();
        assert!(folders.iter().any(|f| f["name"] == "Topic" && f["count"] == 1));
    }

    #[tokio::test]
    async fn browse_pagination_reaches_documents_beyond_first_page() {
        let (app, token, dir) = test_router();
        std::fs::create_dir_all(dir.path().join("Notes/Paged/Child")).unwrap();
        for i in 0..35 {
            std::fs::write(dir.path().join(format!("Notes/Paged/{i:02}.md")), "# Entry\n").unwrap();
        }
        let mut paths = std::collections::HashSet::new();
        for (offset, expected) in [(0, 30), (30, 5), (35, 0)] {
            let resp = app.clone().oneshot(Request::builder()
                .uri(format!("/browse?path=Notes/Paged&offset={offset}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let v = body_json(resp).await;
            assert_eq!(v["total_docs"], 35);
            let docs = v["docs"].as_array().unwrap();
            assert_eq!(docs.len(), expected);
            for d in docs { assert!(paths.insert(d["path"].as_str().unwrap().to_owned())); }
        }
        assert_eq!(paths.len(), 35);
    }

    #[tokio::test]
    async fn canvas_connect_preserves_exact_target_and_is_idempotent() {
        let (app, token, dir) = test_router();
        std::fs::create_dir_all(dir.path().join("Notes/Topic")).unwrap();
        std::fs::write(dir.path().join("Notes/source.md"), "# Source\n\nAn idea.\n").unwrap();
        std::fs::write(dir.path().join("Notes/Topic/same.md"), "# Target\n").unwrap();
        std::fs::write(dir.path().join("Notes/same.md"), "# Different target\n").unwrap();
        for already in [false, true] {
            let resp = app.clone().oneshot(Request::builder().method("POST").uri("/agent/apply")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"op":"connect","path":"Notes/source.md","target":"Notes/Topic/same.md"}"#)).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let v = body_json(resp).await;
            assert_eq!(v["link"], "[[Notes/Topic/same.md]]");
            assert_eq!(v["already"].as_bool().unwrap_or(false), already);
        }
        let md = std::fs::read_to_string(dir.path().join("Notes/source.md")).unwrap();
        assert_eq!(md.matches("[[Notes/Topic/same.md]]").count(), 1);
        assert!(!md.contains("[[same]]"));
        let resp = app.oneshot(Request::builder().uri("/graph")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let graph = body_json(resp).await;
        assert!(graph["edges"].as_array().unwrap().iter().any(|e|
            e["from"] == "doc:Notes/source.md" && e["to"] == "doc:Notes/Topic/same.md"));
    }

    #[tokio::test]
    async fn dir_create_move_remove_roundtrip() {
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        let post = |uri: &str, body: &str| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", &auth)
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        // 新建 Notes/A/B
        let resp = app.clone().oneshot(post("/dir", r#"{"path":"Notes/A/B"}"#)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(dir.path().join("Notes/A/B").is_dir());
        // 非法路径拒绝
        let resp = app.clone().oneshot(post("/dir", r#"{"path":"Notes/../evil"}"#)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 放一篇文档进去，再整目录改名 A → C
        std::fs::write(dir.path().join("Notes/A/B/x.md"), "# x\n").unwrap();
        let resp = app
            .clone()
            .oneshot(post("/dir/move", r#"{"from":"Notes/A","to":"Notes/C"}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(dir.path().join("Notes/C/B/x.md").is_file());
        assert!(!dir.path().join("Notes/A").exists());
        // 移进自身子目录拒绝
        let resp = app
            .clone()
            .oneshot(post("/dir/move", r#"{"from":"Notes/C","to":"Notes/C/B/D"}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 删空文件夹成功；删非空失败（409）
        let del = |uri: &str| {
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("authorization", &auth)
                .body(Body::empty())
                .unwrap()
        };
        let resp = app.clone().oneshot(del("/dir?path=Notes/C")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT); // 还有内容，拒绝
        let resp = app
            .clone()
            .oneshot(del("/dir?path=Notes/C/B"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT); // 同上（内含 x.md）
        // 建一个空目录再删 → 成功
        let resp = app.clone().oneshot(post("/dir", r#"{"path":"Notes/D"}"#)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app.clone().oneshot(del("/dir?path=Notes/D")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn memo_only_counts_memos_notebook() {
        let (app, token, _dir) = test_router();
        let auth = format!("Bearer {token}");
        // 直接写盘（API 也可，但更稳的是先落盘再 sync）
        std::fs::create_dir_all(_dir.path().join("Notes/Memos")).unwrap();
        std::fs::create_dir_all(_dir.path().join("Notes/Topic")).unwrap();
        std::fs::write(_dir.path().join("Notes/Topic/other.md"), "# other\n").unwrap();
        std::fs::write(_dir.path().join("Notes/Memos/2026-09-24.md"), "# memo day\n").unwrap();

        // 通过 API 触发 sync + memo 构建
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/memo/docs")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let docs = v["docs"].as_array().unwrap();
        assert!(
            docs.iter().any(|d| d["path"] == "Notes/Memos/2026-09-24.md"),
            "expected memo notebook entry, got {v}"
        );
        assert!(
            !docs.iter().any(|d| d["path"] == "Notes/Topic/other.md"),
            "non-memo notebook must not be counted, got {v}"
        );
    }

    #[tokio::test]
    async fn delete_goes_to_trash_then_restore_and_purge() {
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        // 建文档
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/doc?path=Notes/x.md")
                    .header("authorization", &auth)
                    .header("content-type", "text/markdown")
                    .body(Body::from("# X\n\n内容\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 删除 → 软删除进回收站，原路径消失
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/doc?path=Notes/x.md")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["trash"], true, "delete 应进回收站: {v}");
        assert!(!dir.path().join("Notes/x.md").exists(), "原路径应消失");
        assert!(
            dir.path().join(".thirdc/trash/Notes/x.md").is_file(),
            "文件应在回收站"
        );

        // 回收站列表
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/trash")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["retention_days"], 30, "缺省保留 30 天");
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["path"], "Notes/x.md");

        // 恢复 → 回到原路径
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/trash/restore")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"Notes/x.md"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(dir.path().join("Notes/x.md").is_file(), "恢复后回到原路径");
        assert!(!dir.path().join(".thirdc/trash/Notes/x.md").exists());

        // 再删一次 + 彻底删除
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/doc?path=Notes/x.md")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/trash/purge")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"Notes/x.md"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["purged"], 1);
        assert!(!dir.path().join(".thirdc/trash/Notes/x.md").exists(), "彻底删除后回收站内文件消失");

        // 列表已空
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/trash")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["entries"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn delete_folder_moves_directory_to_trash() {
        // 复现原 bug：文件夹节点点删除曾报 Operation not permitted（对目录 remove_file）
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/doc?path=Notes/研究/b.md")
                    .header("authorization", &auth)
                    .header("content-type", "text/markdown")
                    .body(Body::from("# B\n\n内容\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/doc?path=Notes/研究")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "文件夹删除应成功");
        assert!(!dir.path().join("Notes/研究").exists(), "原文件夹应消失");
        assert!(
            dir.path().join(".thirdc/trash/Notes/研究/b.md").is_file(),
            "文件夹整体应在回收站"
        );

        // 恢复整个文件夹
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/trash/restore")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"Notes/研究"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(dir.path().join("Notes/研究/b.md").is_file(), "文件夹恢复后内容完整");
    }

    #[tokio::test]
    async fn delete_parent_folder_after_children_lands_in_trash() {
        // 复现线上场景：先逐个删除子文件夹（它们都以 <父目录>/ 为容器躺在回收站，
        // 清单里只有子项记录），再删除父目录本身——rename 目标撞上那个非空容器
        // 报 ENOTEMPTY (os error 66)，用户表现为"文件夹怎么都删不掉"。
        // 修复后：trash_unique_rel 连文件系统一起查重，父目录落到 Projects.1。
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        for rel in ["Notes/Projects/a.md", "Notes/Projects/子/b.md"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(format!("/doc?path={rel}"))
                        .header("authorization", &auth)
                        .header("content-type", "text/markdown")
                        .body(Body::from("# x\n"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        // 先删子文件夹「子」→ 回收站容器 Notes/Projects/ 出现且非空
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/doc?path=Notes/Projects/子")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(dir.path().join(".thirdc/trash/Notes/Projects/子/b.md").is_file());

        // 再删父目录 Projects：目标名 trash/Notes/Projects 已被非空容器占用
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/doc?path=Notes/Projects")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "父目录删除应绕开非空容器落去 Projects.1");
        assert!(!dir.path().join("Notes/Projects").exists());
        assert!(dir.path().join(".thirdc/trash/Notes/Projects.1/a.md").is_file());
        // 旧子项的清单记录不受影响，仍可恢复
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/trash/restore")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"Notes/Projects/子"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "旧子项记录应仍可恢复");
        assert!(dir.path().join("Notes/Projects/子/b.md").is_file());
    }

    #[test]
    fn rule_verbs_condense_and_summarize() {
        // 精简：去空行、折叠空白、去连续重复行
        assert_eq!(ab_condense("A   B\n\nC\nC\n\n\nD   "), "A B\n\nC\n\nD");
        // 总结：标题 + 每段首句
        let s = ab_summarize("# 标题\n\n第一句。第二句。\n\n## 小节\n\n第三句。");
        assert!(s.contains("- **标题**"), "{s}");
        assert!(s.contains("- 第一句。"), "{s}");
        assert!(s.contains("- **小节**"), "{s}");
        assert!(s.contains("- 第三句。"), "{s}");
        // slug：中文保留，符号转连字符
        assert_eq!(ab_slug("Hello 世界!"), "hello-世界");
        assert_eq!(ab_first_line_title("# 我的片段\n\n正文"), "我的片段");
    }

    #[tokio::test]
    async fn agent_apply_rules_offline() {
        let (app, token, _dir) = test_router();
        let auth = format!("Bearer {token}");
        // 规则动词：精简（无模型也能用）
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/apply")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"condense","text":"a   b\n\n\nb"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["mode"], "rule");
        assert_eq!(v["placement"], "replace");
        assert_eq!(v["markdown"], "a b\n\nb");

        // 需要模型的动作：离线时明确拒绝（501）
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/apply")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"rewrite","text":"hello"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn view_resolve_rule_mode_and_ai_prompt_without_config() {
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        std::fs::write(dir.path().join("Notes/a.md"), "---\nhours: 3\n---\n# A\n\nx\n").unwrap();

        // 规则模式：spec 字符串解析正常
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/view/resolve")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"spec":"type: table\nsource: folder:Notes\nfields: title, hours\nlimit: 10"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["mode"], "rule");
        assert_eq!(v["rows"].as_array().map(|a| a.len()), Some(1));
        assert!(v["spec_text"].as_str().unwrap_or("").contains("fields"));

        // AI prompt：未配置模型 → 501
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/view/resolve")
                    .header("authorization", &auth)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"prompt":"按工时倒序的任务表"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn agent_apply_node_ops() {
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        let put = |path: &str, md: &str| {
            let app = app.clone();
            let auth = auth.clone();
            let uri = format!("/doc?path={path}");
            let md = md.to_string();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(uri)
                        .header("authorization", &auth)
                        .header("content-type", "text/markdown")
                        .body(Body::from(md))
                        .unwrap(),
                )
                .await
                .unwrap();
            }
        };
        put("Notes/a.md", "# A\n\n第一句。第二句。\n\n## 子\n\n内容。").await;
        put("Notes/Dir/b.md", "# B\n\n正文。").await;

        let post = |body: &str| {
            let app = app.clone();
            let auth = auth.clone();
            let body = body.to_string();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/agent/apply")
                        .header("authorization", &auth)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        // 总结为新节点 + 原文档追加链接
        let v = body_json(post(r#"{"op":"summarize-node","path":"Notes/a.md"}"#).await).await;
        assert_eq!(v["placement"], "new-node");
        let newp = v["path"].as_str().unwrap().to_string();
        assert!(std::fs::read_to_string(dir.path().join(&newp)).unwrap().contains("摘要"));
        assert!(std::fs::read_to_string(dir.path().join("Notes/a.md")).unwrap().contains("[["));

        // 批量打标签
        let v = body_json(post(r#"{"op":"tag","paths":["Notes/a.md","Notes/Dir/b.md"],"target":"idea"}"#).await).await;
        assert_eq!(v["tagged"], 2);
        assert!(std::fs::read_to_string(dir.path().join("Notes/Dir/b.md")).unwrap().contains("#idea"));

        // 生成结构（按文件夹分组）
        let v = body_json(post(r#"{"op":"outline","paths":["Notes/a.md","Notes/Dir/b.md"]}"#).await).await;
        let body = std::fs::read_to_string(dir.path().join(v["path"].as_str().unwrap())).unwrap();
        assert!(body.contains("- **Dir**"), "{body}");
        assert!(body.contains("[[b]]"), "{body}");

        // 合并
        let v = body_json(post(r#"{"op":"merge","paths":["Notes/a.md","Notes/Dir/b.md"]}"#).await).await;
        let body = std::fs::read_to_string(dir.path().join(v["path"].as_str().unwrap())).unwrap();
        assert!(body.contains("来源") && body.contains("[[a]]") && body.contains("## b"), "{body}");
    }

    #[test]
    fn citation_surface_protocol() {
        let cands = vec![("Notes/a.md".into(), 0.83), ("Notes/b.md".into(), 0.41)];
        // 守协议:有标注 → 无警告
        let cited = append_citation_surface("结论甲〔来源:Notes/a.md〕", &cands);
        assert!(cited.contains("```a2ui"), "来源必须是 A2UI surface:{}", cited);
        assert!(!cited.contains("未按协议标注"));
        assert!(cited.contains("\"Link\"") && cited.contains("Notes/a.md"), "来源卡必须带 Link 回链：{}", cited);
        // 必须能被渲染器真正出图（含 beginRendering），否则来源卡在对话里只剩"渲染失败"
        let block = cited.split("```a2ui\n").nth(1).unwrap().split("\n```").next().unwrap();
        let html = kernel_a2ui::render_jsonl_fragment(block).expect("来源 surface 必须可渲染");
        assert!(html.contains("data-doc=\"Notes/a.md\""), "{html}");
        assert_eq!(html.matches(">a.md<").count(), 1, "来源标题只应出现在 Link 内一次：{html}");
        // 不守协议:零标注 → 警告
        let uncited = append_citation_surface("结论甲", &cands);
        assert!(uncited.contains("未按协议标注"));
        assert!(uncited.contains("\"Link\""));
        // 无候选 → 明说
        let none = append_citation_surface("结论", &[]);
        assert!(none.contains("未检索到库内相关来源"));
        assert!(!none.contains("\"Link\""));
    }

    #[tokio::test]
    async fn docs_lists_filenames_without_sync() {
        let (app, token, dir) = test_router();
        let auth = format!("Bearer {token}");
        std::fs::write(dir.path().join("Notes/plain.md"), "# 不应被读取的标题\n").unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/docs")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let docs = v["docs"].as_array().unwrap();
        assert!(docs.iter().any(|d| d["path"] == "Notes/plain.md" && d["title"] == "plain"));
    }

    #[tokio::test]
    async fn auth_is_enforced() {
        let (app, _token, _d) = test_router();
        let resp = app
            .oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn put_get_search_roundtrip() {
        let (app, token, _d) = test_router();
        let auth = format!("Bearer {token}");

        let put = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/doc?path=Notes/hello.md")
                    .header("authorization", &auth)
                    .body(Body::from("# 你好\n\n内容一段\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::OK);

        let got = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/doc?path=Notes/hello.md")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(got.status(), StatusCode::OK);
        let v = body_json(got).await;
        assert_eq!(v["title"], "你好");
        assert!(v["markdown"].as_str().unwrap().contains("内容一段"));

        let s = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/search?q=%E5%86%85%E5%AE%B9%E4%B8%80%E6%AE%B5")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(s).await;
        assert_eq!(v["hits"][0]["path"], "Notes/hello.md");
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let (app, token, _d) = test_router();
        let auth = format!("Bearer {token}");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/doc?path=Notes/../secret.md")
                    .header("authorization", &auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

// ---------- 图谱 / 画布布局 辅助 ----------

/// 行内 #标签 抽取（跳过标题行；去重保序）。
fn extract_tags(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in md.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            let prev_ok = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '#');
            if chars[i] == '#' && prev_ok {
                let mut j = i + 1;
                let mut s = String::new();
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '-' || chars[j] == '_') {
                    s.push(chars[j]);
                    j += 1;
                }
                let n = s.chars().count();
                if n >= 2 && n <= 24 && !out.contains(&s) {
                    out.push(s);
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    out
}

/// 看板分栏依据：Notes/ 下第一层目录；根目录文档归入「(根)」。
fn collection_of(path: &str) -> String {
    let rest = path.strip_prefix("Notes/").unwrap_or(path);
    match rest.split_once('/') {
        Some((seg, _)) => {
            if seg == "Sources" {
                "采集".to_string()
            } else {
                seg.to_string()
            }
        }
        None => "(根)".to_string(),
    }
}

/// 抽取 `[[wiki 链接]]` 目标。
fn extract_wikilinks(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = md;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find("]]") {
            let target = after[..end].trim();
            if !target.is_empty() {
                out.push(target.to_string());
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }
    out
}

fn build_graph(k: &mut Kernel) -> anyhow::Result<Value> {
    k.sync_all()?;
    let docs = kernel_core::list_docs(&k.vault)?;
    let mut nodes: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut by_key: HashMap<String, String> = HashMap::new();
    let mut bodies: Vec<(String, String)> = Vec::new();

    for p in &docs {
        let path = p.to_string_lossy().into_owned();
        by_key.insert(path.to_lowercase(), path.clone());
        by_key.insert(p.with_extension("").to_string_lossy().to_lowercase(), path.clone());
        // 轻量：直接读文件（不加载 CRDT），大库也能秒开
        let text = std::fs::read_to_string(k.vault.root.join(&path)).unwrap_or_default();
        let (title_opt, tags_fast) = kernel_core::refs::extract_title_and_tags(&text);
        let title = title_opt.unwrap_or_default();
        if !title.is_empty() {
            by_key.insert(title.to_lowercase(), path.clone());
        }
        if let Some(stem) = p.file_stem() {
            by_key.insert(stem.to_string_lossy().to_lowercase(), path.clone());
        }
        let md = text.clone();
        let excerpt: String = md
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#') && !t.starts_with('>') && !t.starts_with('-')
            })
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(160)
            .collect();
        let kind = if path.contains("/Sources/") { "capture" } else { "doc" };
        let mtime = std::fs::metadata(k.vault.root.join(&path))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let collection = collection_of(&path);
        let tags = tags_fast;
        nodes.push(json!({
            "id": format!("doc:{path}"), "kind": kind, "path": path, "title": title,
            "excerpt": excerpt, "mtime": mtime, "collection": collection, "tags": tags
        }));
        bodies.push((path, md));
    }
    for (path, md) in &bodies {
        for target in extract_wikilinks(md) {
            if let Some(to) = by_key.get(&target.to_lowercase()) {
                if to != path {
                    edges.push(json!({
                        "from": format!("doc:{path}"), "to": format!("doc:{to}"), "kind": "link"
                    }));
                }
            }
        }
    }

    for (hash, path, mime) in k.asset_list()? {
        let mtime = std::fs::metadata(k.vault.root.join(&path))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        nodes.push(json!({
            "id": format!("asset:{hash}"), "kind": "asset", "path": path, "mime": mime,
            "mtime": mtime, "collection": "附件"
        }));
    }
    for (doc, hash) in k.doc_asset_pairs()? {
        edges.push(json!({
            "from": format!("doc:{doc}"), "to": format!("asset:{hash}"), "kind": "asset"
        }));
    }
    Ok(json!({ "nodes": nodes, "edges": edges }))
}

fn board_path(k: &Kernel, name: &str) -> std::path::PathBuf {
    k.vault.sidecar().join("boards").join(format!("{name}.json"))
}

fn board_names(k: &Kernel) -> Vec<String> {
    let dir = k.vault.sidecar().join("boards");
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(|s| s.trim_end_matches(".json").to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !out.iter().any(|n| n == "main") {
        out.push("main".to_string());
    }
    out.sort();
    out
}

fn board_load(k: &Kernel, name: &str) -> anyhow::Result<Value> {
    let p = board_path(k, name);
    if !p.is_file() {
        return Ok(json!({ "nodes": {} }));
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

fn board_save(k: &Kernel, name: &str, v: &Value) -> anyhow::Result<()> {
    let p = board_path(k, name);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

// ---------- 流式对话（SSE） ----------

type SseEvent = Result<axum::response::sse::Event, std::convert::Infallible>;

fn ev(name: &str, data: Value) -> SseEvent {
    Ok(axum::response::sse::Event::default().event(name).data(data.to_string()))
}

/// 对话流：agent 每步工具调用即时上屏，最后给回复。
async fn chat_stream(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    use axum::response::sse::Sse;
    use futures_util::stream;

    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let message = req.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
    let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
    let server = st.mcp.clone();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<SseEvent>();

    let chat_t0 = std::time::Instant::now();
    tokio::spawn(async move {
        // Agent 记忆 · 回忆：回答前先找相关记忆与历史
        let recalled = {
            match server.kernel.lock() {
                Ok(mut k) => k.recall(&message, 3).unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        };
        if !recalled.is_empty() {
            let _ = tx.send(ev("recall", json!({
                "count": recalled.len(),
                "items": recalled.iter().map(|(p, sc, snip)| json!({"path": p, "score": sc, "snippet": snip})).collect::<Vec<_>>()
            })));
        }
        let enriched = if recalled.is_empty() {
            message.clone()
        } else {
            let mem: Vec<String> = recalled.iter().map(|(p, _, sn)| format!("- 〔{p}〕{sn}")).collect();
            format!("{message}\n\n[相关记忆]\n{}", mem.join("\n"))
        };

        // 引用回链候选：混合检索 top5（机制⑤：来源必然出现在答案旁）
        let cite_candidates: Vec<(String, f64)> = {
            match server.kernel.lock() {
                Ok(mut k) => {
                    let _ = k.sync_throttled();
                    k.search_hybrid(&message, 5).unwrap_or_default()
                }
                Err(_) => Vec::new(),
            }
        };
        let mut enriched = enriched;
        if !cite_candidates.is_empty() {
            let list: Vec<String> = cite_candidates
                .iter()
                .map(|(p, _)| format!("- {p}"))
                .collect();
            enriched.push_str(&format!(
                "\n\n[引用协议] 库内检索到以下可能相关文档：\n{}\n凡参考了库内内容的结论，行尾标注〔来源:路径〕；若均不相关则不要引用。\n",
                list.join("\n")
            ));
        }

        let result = match ai {
            Some(cfg) if !cfg.base_url.is_empty() => {
                let tx_step = tx.clone();
                thirdc_mcp::agent::run_ai_with(&server, &cfg, &enriched, move |s| {
                    let _ = tx_step.send(ev(
                        "step",
                        json!({ "tool": s.tool, "ok": s.ok, "summary": s.summary }),
                    ));
                })
                .await
            }
            _ => {
                let out = thirdc_mcp::agent::command_mode(&server, &enriched);
                for s in &out.steps {
                    let _ = tx.send(ev("step", json!({ "tool": s.tool, "ok": s.ok, "summary": s.summary, "args": s.args })));
                }
                Ok(out)
            }
        };
        // 对话落库（Agent 记忆的第二半：记录）
        {
            let today = kernel_core::today_string();
            if let Ok(mut k) = server.kernel.lock() {
                let _ = k.append_conversation(&today, "我", &message);
                if let Ok(out) = &result {
                    let _ = k.append_conversation(&today, "studio", &out.reply);
                }
            }
        }
        match result {
            Ok(out) => {
                let reply = append_citation_surface(&out.reply, &cite_candidates);
                let _ = tx.send(ev(
                    "reply",
                    json!({ "mode": out.mode, "reply": reply }),
                ));
                let _ = tx.send(ev("done", json!({
                    "ok": true,
                    "tokens": out.tokens,
                    "elapsed_ms": chat_t0.elapsed().as_millis() as u64,
                })));
            }
            Err(e) => {
                let _ = tx.send(ev("error", json!({ "message": e.to_string() })));
                let _ = tx.send(ev("done", json!({ "ok": false, "elapsed_ms": chat_t0.elapsed().as_millis() as u64 })));
            }
        }
    });

    let out = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Sse::new(out).into_response()
}

// ---------- AI 引用回链（协议级反幻觉，product-principles 机制⑤） ----------

/// 回复后校验并附加 A2UI 来源块：
/// - 有候选且回复含〔来源:…〕标注 → 来源卡（Link 组件，data-doc 回链）
/// - 有候选但零标注 → 附加 ⚠ 提示（模型未按协议标注，来源仅作对照）
/// - 无候选 → 附加"本次回答无库内来源"卡
/// 无论模型守不守协议，**来源必然出现在答案旁边**——反幻觉是结构保证，不是提示词祈祷。
pub fn append_citation_surface(reply: &str, candidates: &[(String, f64)]) -> String {
    use serde_json::json;
    let cited = reply.contains("〔来源") || reply.contains("[来源") || reply.contains("(来源");
    let mut lines: Vec<String> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut comp = |lines: &mut Vec<String>, ids: &mut Vec<String>, id: &str, component: Value| {
        lines.push(json!({"surfaceUpdate": {"components": [{"id": id, "component": component}]}}).to_string());
        ids.push(id.to_string());
    };
    comp(&mut lines, &mut ids, "h", json!({"Text": {"usageHint": "h4", "text": {"literalString": "库内来源"}}}));
    if candidates.is_empty() {
        comp(&mut lines, &mut ids, "none", json!({"Text": {"text": {"literalString": "本次回答未检索到库内相关来源——结论请自行核实"}}}));
    } else {
        if !cited {
            comp(&mut lines, &mut ids, "warn", json!({"Text": {"usageHint": "caption", "text": {"literalString": "模型未按协议标注〔来源〕——以下为自动检索候选，仅供参考"}}}));
        }
        for (i, (path, score)) in candidates.iter().take(5).enumerate() {
            let lid = format!("lnk{i}");
            let lbl = format!("lbl{i}");
            let meta = format!("meta{i}");
            let label = path.rsplit('/').next().unwrap_or(path).to_string();
            comp(&mut lines, &mut ids, &lid, json!({"Link": {"doc": {"literalString": path}, "child": &lbl}}));
            comp(&mut lines, &mut ids, &lbl, json!({"Text": {"text": {"literalString": label}}}));
            comp(&mut lines, &mut ids, &meta, json!({"Text": {"usageHint": "caption", "text": {"literalString": format!("{path} · 相关度 {score:.2}")}}}));
        }
    }
    // root 的 explicitList 指向全部顶层组件 id（后写覆盖前写，root 放最后）。
    // Link 的文字子组件（lbl*）只属于 Link，若也挂到 root 会在卡片外再渲染一遍
    ids.retain(|id| !id.starts_with("lbl"));
    let root_line = json!({"surfaceUpdate": {"components": [{"id": "root", "component": {"Column": {"children": {"explicitList": ids}}}}]}});
    lines.push(root_line.to_string());
    // 没有 beginRendering，渲染器会按协议拒绝出图（"no surface"）——来源卡曾因此在对话里显示为渲染失败
    lines.push(json!({"beginRendering": {"root": "root"}}).to_string());
    let jsonl = lines.join("\n");
    format!("{reply}\n\n```a2ui\n{jsonl}\n```\n")
}

// ---------- 发布（本地预览产物） ----------

fn publish_dir(k: &Kernel) -> std::path::PathBuf {
    k.vault.sidecar().join("publish")
}

async fn publish(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let path = req.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
    if !kernel_core::is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let prep = (|| -> Result<Result<_, axum::response::Response>, axum::response::Response> {
        let mut k = st.kernel.lock().unwrap();
        if let Err(e) = k.sync_all() {
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response());
        }
        let model = match k.get_doc(&path) {
            Ok(m) => m,
            Err(e) => return Err(err(StatusCode::NOT_FOUND, e).into_response()),
        };
        let title = model.title.clone().unwrap_or_else(|| path.clone());
        let html = match k.render_doc_html(&path) {
            Ok(h) => h,
            Err(_) => kernel_core::to_html(&model),
        };
        let slug = {
            let s = kernel_core::slugify(&title);
            if s.is_empty() {
                format!("doc-{}", &kernel_core::Cas::hash_hex(path.as_bytes())[..8])
            } else {
                s
            }
        };
        let dir = publish_dir(&k);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response());
        }
        let file = dir.join(format!("{slug}.html"));
        // 分享出去的是单个文件：库内图片转成 data URI，离开本机也能完整显示
        let html = inline_vault_assets(&html, &k.vault.root);
        // 站点级 llms.txt：让 agent 能发现已发布内容
        let mut manifest_path = dir.join("llms.txt");
        if let Err(e) = std::fs::write(&file, html.as_bytes()) {
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response());
        }
        let mut lines = vec![format!("# {} 发布内容", k.vault.config.name), String::new()];
        if let Ok(read) = std::fs::read_dir(&dir) {
            let mut names: Vec<String> = read
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .filter(|n| n.ends_with(".html"))
                .collect();
            names.sort();
            for n in names {
                lines.push(format!("- /publish/{n}"));
            }
        }
        let _ = std::fs::write(&mut manifest_path, lines.join("\n"));

        // ── 公开分享：配置了公网目标（GitHub Pages / S3·OSS·R2 / Cloudflare Pages）就把这一页推上去 ──
        let share = share_target(&k.vault.config.publish);
        let local_url = format!("/publish/{slug}.html");
        let Some((target, base)) = share else {
            return Ok(Err(Json(json!({
                "path": path, "title": title, "url": local_url, "bytes": html.len(), "public": false,
                "hint": "还没有配置公开发布目标：这个链接只在本机打开。可以下载 HTML 文件直接发给别人，或在「发布」面板里配置 GitHub Pages / 对象存储，之后一键得到公网链接。",
            }))
            .into_response()));
        };
        let site = k.vault.root.join(kernel_deploy::target_dir(&target, &k.vault.config.publish.site_dir));
        let share_file = site.join("share").join(format!("{slug}.html"));
        if let Err(e) = std::fs::create_dir_all(share_file.parent().unwrap()).and_then(|_| std::fs::write(&share_file, html.as_bytes())) {
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response());
        }
        let issues = k.compliance_check(&site, &target).unwrap_or_default();
        let blocking: Vec<_> = issues.iter().filter(|i| i.level == "block").cloned().collect();
        let sidecar = k.vault.sidecar();
        Ok(Ok((title, local_url, html.len(), target, base, site, share_file, sidecar, blocking, slug)))
    })();
    let (title, local_url, html_len, target, base, site, share_file, sidecar, blocking, slug) = match prep {
        Ok(Ok(v)) => v,
        Ok(Err(done)) => return done,
        Err(e) => return e,
    };
    if !blocking.is_empty() {
        let _ = std::fs::remove_file(&share_file);
        return err(StatusCode::BAD_REQUEST, format!("发布前检查未通过：{}", serde_json::to_string(&blocking).unwrap_or_default())).into_response();
    }
    let t2 = target.clone();
    let (site2, sidecar2) = (site.clone(), sidecar.clone());
    let r = tokio::task::spawn_blocking(move || kernel_deploy::deploy(&site2, &sidecar2, &t2))
        .await
        .unwrap_or_else(|j| Err(kernel_deploy::DeployError::Io(std::io::Error::other(format!("join: {j}")))));
    match r {
        Ok(report) => {
            let root = report.url.clone().filter(|u| u.starts_with("http") && base.is_none()).or(base.clone()).unwrap_or_default();
            let public_url = if root.is_empty() { None } else { Some(format!("{}/share/{slug}.html", root.trim_end_matches('/'))) };
            audit_log(&sidecar, "share", &json!({ "path": path, "target": target.name, "url": public_url }));
            Json(json!({
                "path": path, "title": title, "url": local_url, "bytes": html_len,
                "public": public_url.is_some(), "public_url": public_url, "target": target.name,
                "hint": if public_url.is_none() { Some("已推送到发布目标，但没配置公开地址（publish.base_url 或目标的 public_base_url），无法拼出链接。") } else { None },
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "path": path, "title": title, "url": local_url, "bytes": html_len, "public": false,
            "target": target.name, "hint": format!("推送到「{}」失败：{e}。这个链接只在本机打开，可以先下载 HTML 发给别人。", target.name),
        }))
        .into_response(),
    }
}

/// 可产生公网链接的目标：默认目标优先，否则唯一的 git / s3 / cf-pages 目标。返回 (目标, 公开根地址)。
fn share_target(cfg: &kernel_core::PublishConfig) -> Option<(kernel_core::PublishTarget, Option<String>)> {
    let public = |t: &&kernel_core::PublishTarget| matches!(t.kind.as_str(), "git" | "s3" | "cf-pages");
    let t = cfg
        .default_target
        .as_deref()
        .and_then(|n| cfg.targets.iter().find(|t| t.name == n))
        .filter(public)
        .or_else(|| {
            let v: Vec<_> = cfg.targets.iter().filter(public).collect();
            (v.len() == 1).then(|| v[0])
        })?;
    let base = t.public_base_url.clone().or_else(|| cfg.base_url.clone()).filter(|u| !u.trim().is_empty());
    Some((t.clone(), base))
}

/// 把 HTML 里指向库内附件的 <img src> 换成 data URI（单图 ≤ 8MB、总量 ≤ 24MB）。
fn inline_vault_assets(html: &str, root: &std::path::Path) -> String {
    use base64::Engine;
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    let mut budget: usize = 24 << 20;
    while let Some(i) = rest.find("src=\"") {
        let (head, tail) = rest.split_at(i + 5);
        out.push_str(head);
        let Some(end) = tail.find('"') else {
            rest = tail;
            break;
        };
        let raw = &tail[..end];
        let rel = raw
            .strip_prefix("/asset-file?path=")
            .or_else(|| raw.strip_prefix("asset-file?path="))
            .unwrap_or(raw)
            .trim_start_matches("./")
            .trim_start_matches('/');
        let rel = rel.split('&').next().unwrap_or(rel).replace("%2F", "/");
        let mut replaced = false;
        if (rel.starts_with("Assets/") || rel.starts_with("Notes/")) && !rel.contains("..") {
            let abs = root.join(&rel);
            if let Ok(bytes) = std::fs::read(&abs) {
                if bytes.len() <= 8 << 20 && bytes.len() <= budget {
                    budget -= bytes.len();
                    let mime = kernel_core::refs::mime_for_ext(abs.extension().and_then(|e| e.to_str()).unwrap_or(""));
                    out.push_str(&format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)));
                    replaced = true;
                }
            }
        }
        if !replaced {
            out.push_str(raw);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

async fn publish_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let dir = publish_dir(&k);
    let mut items = Vec::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for e in read.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".html") {
                continue;
            }
            let meta = e.metadata().ok();
            items.push(json!({
                "name": name,
                "url": format!("/publish/{name}"),
                "bytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            }));
        }
    }
    Json(json!({ "items": items, "llms_txt": "/publish/llms.txt" })).into_response()
}

/// 服务发布产物（sidecar/publish，仅 .html/.txt，防穿越）。
async fn publish_file(
    State(st): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    }
    if !(name.ends_with(".html") || name.ends_with(".txt")) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let dir = {
        let k = st.kernel.lock().unwrap();
        publish_dir(&k)
    };
    let abs = dir.join(&name);
    match std::fs::read(&abs) {
        Ok(bytes) => {
            let mime = if name.ends_with(".html") {
                "text/html; charset=utf-8"
            } else {
                "text/plain; charset=utf-8"
            };
            ([(header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// 已配置的外部数据源连接（供 ⌘K / 采集面板展示）。
async fn connections(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let list: Vec<Value> = k
        .vault
        .config
        .connections
        .iter()
        .map(|c| json!({ "name": c.name, "command": c.command, "args": c.args }))
        .collect();
    let ai = k
        .vault
        .config
        .ai
        .as_ref()
        .map(|a| json!({ "model": a.model, "base_url": a.base_url, "configured": !a.base_url.is_empty() }))
        .unwrap_or(Value::Null);
    Json(json!({ "connections": list, "ai": ai })).into_response()
}

// ---------- 设计规范 ----------

async fn design_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let profiles = match k.designs() {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let active = k.active_design().map(|p| p.name).unwrap_or_default();
    let arr: Vec<Value> = profiles
        .iter()
        .map(|p| {
            json!({
                "name": p.name, "kind": p.kind, "source": p.source,
                "tokens": p.tokens, "fonts": p.fonts, "rules": p.rules,
                "archetypes": p.archetypes, "active": p.name == active,
            })
        })
        .collect();
    let scopes = k.design_scopes();
    Json(json!({ "profiles": arr, "active": active, "scopes": scopes })).into_response()
}

async fn design_import(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let name = req
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "design".into());
    let k = st.kernel.lock().unwrap();

    // 路径导入（文件或目录 / skill 包）
    if let Some(path) = req.get("path").and_then(|p| p.as_str()) {
        return match k.import_design_path(std::path::Path::new(path), Some(&name)) {
            Ok(p) => Json(json!({
                "name": p.name, "kind": p.kind, "tokens": p.tokens.len(),
                "fonts": p.fonts, "rules": p.rules.len(), "archetypes": p.archetypes.len()
            }))
            .into_response(),
            Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
        };
    }

    let content = req.get("content").and_then(|c| c.as_str()).unwrap_or("");
    if content.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 content 或 path").into_response();
    }
    match k.import_design_text(&name, content) {
        Ok(p) => Json(json!({
            "name": p.name, "kind": p.kind, "tokens": p.tokens, "fonts": p.fonts,
            "rules": p.rules, "archetypes": p.archetypes
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn design_active(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let k = st.kernel.lock().unwrap();
    let r = if name.is_empty() {
        k.clear_active_design()
    } else {
        k.set_active_design(name)
    };
    match r {
        Ok(()) => Json(json!({ "active": name })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn design_get(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    match k.designs().ok().and_then(|list| list.into_iter().find(|p| p.name == name)) {
        Some(p) => Json(json!({
            "name": p.name, "kind": p.kind, "source": p.source, "tokens": p.tokens,
            "fonts": p.fonts, "rules": p.rules, "archetypes": p.archetypes,
            "summary": p.summary(),
        }))
        .into_response(),
        None => err(StatusCode::NOT_FOUND, "design not found").into_response(),
    }
}

/// 从 URL 消化一个页面的排版（ego-lite → headless Chromium → 直接抓取）。
async fn design_import_url(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") && !url.starts_with("file://") {
        return err(StatusCode::BAD_REQUEST, "url must be http(s):// or file://").into_response();
    }
    let name = req
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            url.trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("page")
                .to_string()
        });
    let via = req.get("via").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
    let (cfg_ego, cfg_chrome) = {
        let k = st.kernel.lock().unwrap();
        (
            k.vault.config.browser.ego.clone(),
            k.vault.config.browser.chrome.clone(),
        )
    };

    let mut tried: Vec<String> = Vec::new();
    // 依次尝试：ego → render → http
    let ego_try = via == "auto" || via.eq_ignore_ascii_case("ego");
    if ego_try {
        let u = url.clone();
        let e = cfg_ego.clone();
        let r = tokio::task::spawn_blocking(move || {
            kernel_core::ego_digest(&u, e.as_deref(), std::time::Duration::from_secs(60))
        })
        .await
        .unwrap_or_else(|j| Err(kernel_core::BrowserError::Failed(format!("join: {j}"))));
        match r {
            Ok(digest) => return finish_import(&st, &name, &url, "ego", &digest),
            Err(e) => tried.push(format!("ego: {e}")),
        }
        if via != "auto" {
            return err(StatusCode::BAD_REQUEST, format!("ego 后端失败：{}", tried.join("; "))).into_response();
        }
    }
    let render_try = via == "auto" || via.eq_ignore_ascii_case("render") || via.eq_ignore_ascii_case("headless");
    if render_try {
        let u = url.clone();
        let c = cfg_chrome.clone();
        let r = tokio::task::spawn_blocking(move || {
            kernel_core::dump_dom(&u, c.as_deref(), std::time::Duration::from_secs(45))
        })
        .await
        .unwrap_or_else(|j| Err(kernel_core::BrowserError::Failed(format!("join: {j}"))));
        match r {
            Ok(html) => return finish_import_html(&st, &name, &url, "render", &html),
            Err(e) => tried.push(format!("render: {e}")),
        }
        if via != "auto" && via != "http" {
            return err(StatusCode::BAD_REQUEST, format!("headless 后端失败：{}", tried.join("; "))).into_response();
        }
    }
    let u = url.clone();
    let r = tokio::task::spawn_blocking(move || {
        kernel_core::fetch_html(&u, std::time::Duration::from_secs(30))
    })
    .await
    .unwrap_or_else(|j| Err(kernel_core::BrowserError::Failed(format!("join: {j}"))));
    match r {
        Ok(html) => finish_import_html(&st, &name, &url, "http", &html),
        Err(e) => {
            tried.push(format!("http: {e}"));
            err(StatusCode::BAD_REQUEST, format!("全部后端失败：{}", tried.join("; "))).into_response()
        }
    }
}

fn finish_import(
    st: &Arc<AppState>,
    name: &str,
    url: &str,
    backend: &str,
    digest: &Value,
) -> axum::response::Response {
    let k = st.kernel.lock().unwrap();
    match k.import_design_digest(name, url, digest) {
        Ok(p) => Json(json!({
            "name": p.name, "kind": p.kind, "backend": backend, "source": p.source,
            "tokens": p.tokens.len(), "fonts": p.fonts, "rules": p.rules,
            "archetypes": p.archetypes.len(),
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

fn finish_import_html(
    st: &Arc<AppState>,
    name: &str,
    url: &str,
    backend: &str,
    html: &str,
) -> axum::response::Response {
    let k = st.kernel.lock().unwrap();
    match k.import_design_html(name, url, html) {
        Ok(p) => Json(json!({
            "name": p.name, "kind": p.kind, "backend": backend, "source": p.source,
            "tokens": p.tokens.len(), "fonts": p.fonts, "rules": p.rules,
            "archetypes": p.archetypes.len(),
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn design_scope(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let scope = req.get("scope").and_then(|s| s.as_str()).unwrap_or("vault");
    let name = req.get("name").and_then(|s| s.as_str()).unwrap_or("");
    let k = st.kernel.lock().unwrap();
    match k.set_design_scope(scope, name) {
        Ok(()) => Json(json!({ "scope": scope, "name": name, "scopes": k.design_scopes() })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

// ---------- 发布目标（站点构建 + 推送） ----------

fn publish_targets_of(k: &kernel_core::Kernel) -> Vec<Value> {
    let cfg = &k.vault.config.publish;
    let mut out: Vec<Value> = Vec::new();
    if cfg.targets.is_empty() {
        out.push(json!({ "name": "local", "kind": "local", "dir": cfg.site_dir, "is_default": true }));
    }
    for t in &cfg.targets {
        let dir = kernel_deploy::target_dir(t, &cfg.site_dir);
        out.push(json!({
            "name": t.name, "kind": t.kind, "dir": dir.display().to_string(),
            "remote": t.remote, "branch": t.branch, "bucket": t.bucket,
            "endpoint": t.endpoint, "project": t.project,
            "is_default": cfg.default_target.as_deref() == Some(t.name.as_str()),
        }));
    }
    out
}

async fn publish_targets(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let cfg = &k.vault.config.publish;
    Json(json!({
        "targets": publish_targets_of(&k),
        "site_dir": cfg.site_dir,
        "base_url": cfg.base_url,
        "default": cfg.default_target,
    }))
    .into_response()
}

/// 构建站点（不推送）。
async fn publish_site(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let target_name = req.get("target").and_then(|t| t.as_str()).unwrap_or("");
    let mut k = st.kernel.lock().unwrap();
    let cfg = k.vault.config.publish.clone();
    let dir = match cfg.targets.iter().find(|t| t.name == target_name) {
        Some(t) => kernel_deploy::target_dir(t, &cfg.site_dir),
        None => std::path::PathBuf::from(&cfg.site_dir),
    };
    let out = k.vault.root.join(&dir);
    let base = cfg.base_url.clone();
    let icp = cfg.targets.iter().find(|t| t.name == target_name).and_then(|t| t.icp.clone());
    match k.build_site(&out, base.as_deref(), icp.as_deref()) {
        Ok(m) => Json(json!({
            "site_dir": dir.display().to_string(),
            "files": m.files.len(),
            "bytes": m.files.iter().map(|f| f.bytes).sum::<u64>(),
            "base_url": m.base_url,
            "files_preview": m.files.iter().take(20).map(|f| f.path.clone()).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 构建并推送到目标。
async fn publish_deploy(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = req
        .get("target")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    let (dir, target, sidecar, base) = {
        let mut k = st.kernel.lock().unwrap();
        let cfg = k.vault.config.publish.clone();
        let target = cfg
            .targets
            .iter()
            .find(|t| t.name == name)
            .cloned()
            .or_else(|| {
                if name.is_empty() || name == "local" {
                    Some(kernel_core::PublishTarget {
                        name: "local".into(),
                        kind: "local".into(),
                        dir: Some(cfg.site_dir.clone()),
                        ..Default::default()
                    })
                } else {
                    None
                }
            });
        let Some(target) = target else {
            return err(StatusCode::BAD_REQUEST, format!("未知目标：{name}")).into_response();
        };
        let dir = k.vault.root.join(kernel_deploy::target_dir(&target, &cfg.site_dir));
        let sidecar = k.vault.sidecar();
        let base = cfg.base_url.clone();
        let icp = target.icp.clone();
        if let Err(e) = k.build_site(&dir, base.as_deref(), icp.as_deref()) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        (dir, target, sidecar, base)
    };

    // PUB-4：发布前检查（阻断级问题不推送）
    let issues = match {
        let mut k = st.kernel.lock().unwrap();
        k.compliance_check(&dir, &target)
    } {
        Ok(i) => i,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let blocking: Vec<_> = issues.iter().filter(|i| i.level == "block").collect();
    audit_log(&sidecar, "publish", &json!({
        "target": target.name, "kind": target.kind,
        "checks": issues, "passed": blocking.is_empty(),
    }));
    if !blocking.is_empty() {
        return err(StatusCode::BAD_REQUEST, format!("发布前检查未通过：{}", serde_json::to_string(&blocking).unwrap_or_default())).into_response();
    }

    let t2 = target.clone();
    let sidecar2 = sidecar.clone();
    let r = tokio::task::spawn_blocking(move || kernel_deploy::deploy(&dir, &sidecar2, &t2))
        .await
        .unwrap_or_else(|j| Err(kernel_deploy::DeployError::Io(std::io::Error::other(format!("join: {j}")))));
    match r {
        Ok(mut report) => {
            if report.url.is_none() {
                report.url = base.clone();
            }
            audit_log(&sidecar, "publish", &json!({ "target": report.target, "uploaded": report.uploaded, "skipped": report.skipped, "detail": report.detail }));
            Json(serde_json::to_value(report).unwrap_or(json!({}))).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// 审计日志：append-only events（PUB-4 留痕要求）。
fn audit_log(sidecar: &std::path::Path, kind: &str, v: &Value) {
    let dir = sidecar.join("events");
    let _ = std::fs::create_dir_all(&dir);
    let line = json!({ "ts": now_secs(), "kind": kind, "data": v });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("publish.jsonl")) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ── PUB-5：变更自动发布 ──
/// watcher 同步到外部改动后调用；有变更且配置 auto → 构建并推到默认目标（节流 60s）。
pub fn maybe_auto_publish(state: &Arc<AppState>, sidecar: &std::path::Path) {
    let (auto, default, target) = {
        let k = state.kernel.lock().unwrap();
        let cfg = k.vault.config.publish.clone();
        let t = cfg
            .targets
            .iter()
            .find(|t| Some(&t.name) == cfg.default_target.as_ref())
            .cloned();
        (cfg.auto, cfg.default_target.clone(), t)
    };
    if !auto {
        return;
    }
    let Some(target) = target.or_else(|| default.map(|n| kernel_core::PublishTarget {
        name: n, kind: "local".into(), dir: None, ..Default::default()
    })) else { return; };

    let dir = state.kernel.lock().unwrap().vault.root.join(kernel_deploy::target_dir(&target, "site"));
    let base = {
        let k = state.kernel.lock().unwrap();
        k.vault.config.publish.base_url.clone()
    };
    let icp = target.icp.clone();
    if let Err(e) = {
        let mut k = state.kernel.lock().unwrap();
        k.build_site(&dir, base.as_deref(), icp.as_deref())
    } {
        audit_log(sidecar, "auto-publish", &json!({ "error": e.to_string() }));
        return;
    }
    let st2 = state.clone();
    let t2 = target.clone();
    let sc = sidecar.to_path_buf();
    std::thread::spawn(move || {
        let r = kernel_deploy::deploy(&dir, &sc, &t2);
        audit_log(&sc, "auto-publish", &match r {
            Ok(rep) => json!({ "target": rep.target, "uploaded": rep.uploaded, "skipped": rep.skipped, "detail": rep.detail }),
            Err(e) => json!({ "error": e.to_string() }),
        });
        let _ = st2;
    });
}

// ── 连接器管理端点（采集入口可视化） ──
async fn conn_probe(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    let cfg = {
        let k = st.kernel.lock().unwrap();
        k.vault.config.connections.iter().find(|c| c.name == name).cloned()
    };
    let Some(cfg) = cfg else {
        return err(StatusCode::NOT_FOUND, format!("连接 {name} 不存在")).into_response();
    };
    let r = tokio::task::spawn_blocking(move || thirdc_mcp::pull::probe(&cfg)).await;
    match r {
        Ok(Ok((server, tools, resources))) => Json(json!({ "server": server, "tools": tools, "resources": resources })).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn conn_pull(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    let cfg = {
        let k = st.kernel.lock().unwrap();
        k.vault.config.connections.iter().find(|c| c.name == name).cloned()
    };
    let Some(cfg) = cfg else {
        return err(StatusCode::NOT_FOUND, format!("连接 {name} 不存在")).into_response();
    };
    let kernel = st.kernel.clone();
    let r = tokio::task::spawn_blocking(move || thirdc_mcp::pull::pull_resources(&cfg, &kernel)).await;
    match r {
        Ok(Ok(report)) => Json(json!({
            "server": report.server,
            "tools": report.tools,
            "imported": report.imported.iter().map(|(u, rel)| json!({"uri": u, "rel": rel})).collect::<Vec<_>>(),
            "skipped": report.skipped,
        }))
        .into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── UX-4：协作光标（WebSocket presence） ──

use std::sync::atomic::{AtomicU64, Ordering};

/// 在线协作者注册表：进房/广播/退房。
pub struct PresenceHub {
    clients: Mutex<std::collections::HashMap<u64, tokio::sync::mpsc::UnboundedSender<Value>>>,
    next: AtomicU64,
}

impl Default for PresenceHub {
    fn default() -> Self {
        Self::new()
    }
}

impl PresenceHub {
    pub fn new() -> Self {
        PresenceHub {
            clients: Mutex::new(std::collections::HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    /// 订阅：返回 (本端 id, 收件队列)。本端会立刻收到 hello（拿到自己的 id）。
    pub fn subscribe(&self) -> (u64, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut guard = self.clients.lock().unwrap();
        let peers = guard.len() + 1;
        guard.insert(id, tx.clone());
        let _ = tx.send(json!({ "type": "hello", "id": id, "peers": peers }));
        (id, rx)
    }

    pub fn unsubscribe(&self, id: u64) {
        self.clients.lock().unwrap().remove(&id);
    }

    /// 广播给所有人（含发送者；客户端自行过滤光标去重）。
    pub fn broadcast(&self, msg: &Value) {
        for tx in self.clients.lock().unwrap().values() {
            let _ = tx.send(msg.clone());
        }
    }

    pub fn count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }
}

use axum::extract::ws::WebSocketUpgrade;

/* ───────── ConFlow 对话流（矩阵整合）：对话列表 + 知识卡生成 ───────── */
async fn conflow_conversations(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    match thirdc_mcp::conflow::list_conversations(&st.mcp) {
        Ok(list) => Json(json!({ "conversations": list })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn conflow_digest(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string();
    match thirdc_mcp::conflow::digest(&st.mcp, &path) {
        Ok(r) => {
            audit_log(&st.kernel.lock().unwrap().vault.sidecar(), "conflow-digest", &r);
            Json(json!({ "ok": true, "result": r })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn ws_handler(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let token = q.get("token").cloned().unwrap_or_default();
    if token != st.token {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    ws.on_upgrade(move |socket| ws_loop(st, socket))
}

async fn ws_loop(st: Arc<AppState>, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};

    let hub = st.hub.clone();
    let (id, mut rx) = hub.subscribe();
    hub.broadcast(&json!({ "type": "join", "from": id, "peers": hub.count() }));

    let (mut sink, mut stream) = socket.split();
    let hub2 = hub.clone();
    let out = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(Message::text(msg.to_string())).await.is_err() {
                break;
            }
        }
        let _ = hub2;
    });

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) => {
                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                // 只广播白名单类型，防止乱灌
                if matches!(ty, "cursor" | "select" | "note" | "state" | "caret") {
                    hub.broadcast(&json!({ "type": ty, "from": id, "data": v }));
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    hub.unsubscribe(id);
    hub.broadcast(&json!({ "type": "leave", "from": id, "peers": hub.count() }));
    out.abort();
}

#[cfg(test)]
pub mod presence_tests;

/// 主题一键采集：快速建立知识库（wikipedia / hackernews / arxiv + MCP 连接器扩展）。
async fn ingest_topic(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let topic = req.get("topic").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    if topic.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 topic").into_response();
    }
    let sources: Vec<String> = req
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_else(|| vec!["wikipedia".into(), "hackernews".into(), "arxiv".into()]);
    let limit = req.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    let t2 = topic.clone();
    let src2 = sources.clone();
    let r = tokio::task::spawn_blocking(move || kernel_core::ingest::ingest_topic(&t2, &src2, limit))
        .await
        .unwrap_or_else(|j| Err(format!("join: {j}")));
    let docs = match r {
        Ok(d) => d,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut k = st.kernel.lock().unwrap();
    let mut imported = Vec::new();
    for d in docs {
        let uri = format!("topic://{}?t={}&src={}", urlenc_query(&topic),
            &kernel_core::Cas::hash_hex(d.title.as_bytes())[..12], urlenc_query(&d.source));
        match k.import_capture("topic", &uri, Some(&d.title), &format!("# {}\n\n> 来源：{}（{}）\n\n{}", d.title, d.url, d.source, d.text), &d.mime) {
            Ok(rel) => imported.push(json!({ "title": d.title, "rel": rel, "source": d.source })),
            Err(e) => {
                return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
            }
        }
    }
    let _ = urlenc_query;
    Json(json!({ "topic": topic, "imported": imported })).into_response()
}

fn urlenc_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}


async fn metrics(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let docs = kernel_core::list_docs(&k.vault).map(|d| d.len()).unwrap_or(0);
    let assets = k.assets_count().unwrap_or(0);
    let items = k.items_count().unwrap_or(0);
    let publishes = std::fs::read_to_string(k.vault.sidecar().join("events/publish.jsonl"))
        .map(|t| t.lines().count())
        .unwrap_or(0);
    let body = format!(
        "# HELP thirdc_docs 知识库文档数\n# TYPE thirdc_docs gauge\nthirdc_docs {docs}\n\
# HELP thirdc_assets 附件数\n# TYPE thirdc_assets gauge\nthirdc_assets {assets}\n\
# HELP thirdc_captured_items 采集条目数\n# TYPE thirdc_captured_items gauge\nthirdc_captured_items {items}\n\
# HELP thirdc_publishes_total 发布次数\n# TYPE thirdc_publishes_total counter\nthirdc_publishes_total {publishes}\n"
    );
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

/// 一键整理：按首标签聚类根目录文档（规则版）；AI 可用时让模型给方案。
async fn organize_plan(State(st): State<Arc<AppState>>, h: HeaderMap, _body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    if let Err(e) = k.sync_all() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let docs = match kernel_core::list_docs(&k.vault) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let mut items: Vec<(String, String, Vec<String>)> = Vec::new(); // (path, title, tags)
    for p in &docs {
        let rel = p.to_string_lossy().into_owned();
        // 只整理根目录的散文档（目录结构保留）
        if rel.matches('/').count() > 1 {
            continue;
        }
        let (title, tags) = match k.get_doc(&rel) {
            Ok(m) => (m.title.clone().unwrap_or_default(), extract_tags(&kernel_core::to_markdown(&m))),
            Err(_) => continue,
        };
        items.push((rel, title, tags));
    }
    // 规则：按第一个标签聚类；无标签的不动
    let mut moves: Vec<Value> = Vec::new();
    for (rel, _title, tags) in &items {
        if let Some(tag) = tags.first() {
            let fname = rel.rsplit('/').next().unwrap_or("");
            let to = format!("Notes/{tag}/{fname}");
            if to != *rel {
                moves.push(json!({ "from": rel, "to": to, "reason": format!("共标签 #{tag}") }));
            }
        }
    }
    Ok::<_, ()>(()).unwrap();
    let report = json!({ "moves": moves, "considered": items.len(), "mode": "rule" });
    Json(report).into_response()
}

/// 应用整理方案（逐个移动文件）。
async fn organize_apply(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut applied = 0usize;
    let mut failed: Vec<Value> = Vec::new();
    let (root, sidecar) = {
        let k = st.kernel.lock().unwrap();
        (k.vault.root.clone(), k.vault.sidecar())
    };
    for m in req.get("moves").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let (Some(from), Some(to)) = (m.get("from").and_then(|v| v.as_str()), m.get("to").and_then(|v| v.as_str()))
        else {
            continue;
        };
        if !kernel_core::is_safe_doc_path(from) || !kernel_core::is_safe_doc_path(to) {
            failed.push(json!({ "from": from, "error": "非法路径" }));
            continue;
        }
        let src = root.join(from);
        let dst = root.join(to);
        if !src.is_file() || dst.exists() {
            failed.push(json!({ "from": from, "error": "源缺失或目标已存在" }));
            continue;
        }
        if let Some(p) = dst.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => applied += 1,
            Err(e) => failed.push(json!({ "from": from, "error": e.to_string() })),
        }
    }
    if applied > 0 {
        let mut k = st.kernel.lock().unwrap();
        let _ = k.sync_all();
    }
    audit_log(&sidecar, "organize", &json!({ "applied": applied, "failed": failed.len() }));
    Json(json!({ "applied": applied, "failed": failed })).into_response()
}

// ── git 工作区端点 ──
async fn git_status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    match kernel_deploy::gitops::status(&k.vault.root) {
        Ok((branch, changes)) => Json(json!({ "branch": branch, "changes": changes })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn git_log(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    match kernel_deploy::gitops::log(&k.vault.root, 30) {
        Ok(l) => Json(json!({ "log": l.iter().map(|(h, s)| json!({"hash": h, "subject": s})).collect::<Vec<_>>() })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn git_commit(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let msg = req.get("message").and_then(|m| m.as_str()).unwrap_or("wip").to_string();
    let k = st.kernel.lock().unwrap();
    match kernel_deploy::gitops::commit_all(&k.vault.root, &msg) {
        Ok(n) => {
            audit_log(&k.vault.sidecar(), "git-commit", &json!({ "message": msg, "files": n }));
            Json(json!({ "committed": n })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn git_branches(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    match kernel_deploy::gitops::branches(&k.vault.root) {
        Ok(b) => Json(json!({ "branches": b })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn git_checkout(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    if name.is_empty() || name.contains("..") || name.contains(' ') {
        return err(StatusCode::BAD_REQUEST, "invalid branch name").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let r = if req.get("create").and_then(|c| c.as_bool()).unwrap_or(false) {
        kernel_deploy::gitops::create_branch(&k.vault.root, &name)
    } else {
        kernel_deploy::gitops::checkout(&k.vault.root, &name)
    };
    match r {
        Ok(()) => Json(json!({ "branch": name })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn git_sync(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let remote = req.get("remote").and_then(|r| r.as_str()).unwrap_or("").to_string();
    if remote.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 remote").into_response();
    }
    let k = st.kernel.lock().unwrap();
    match kernel_deploy::gitops::sync(&k.vault.root, &remote) {
        Ok((committed, detail)) => Json(json!({ "committed": committed, "detail": detail })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// 整库 PPT：封面 + 目录 + 每篇文档一页（AI-HTML 按比例嵌入）。
async fn presentation(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    if let Err(e) = k.sync_all() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let docs = kernel_core::list_docs(&k.vault).unwrap_or_default();
    let profile_css = k.active_design().map(|p| p.to_css()).unwrap_or_default();
    let vault_name = k.vault.config.name.clone();
    let mut slides = String::new();

    for p in &docs {
        let rel = p.to_string_lossy().into_owned();
        let Ok(model) = k.get_doc(&rel) else { continue };
        let title = model.title.clone().unwrap_or_else(|| rel.clone());
        let html = k.render_doc_html(&rel).unwrap_or_default();
        let t = html.replace("</body>", "");
        let t = t.replace("<body", "<body style=\"transform:scale(var(--s));transform-origin:0 0;width:1280px\"");
        slides.push_str(&format!(
            "<section class=\"slide\" data-title=\"{}\"><iframe loading=\"lazy\" sandbox=\"\" srcdoc=\"{}\"></iframe></section>\n",
            html_escape_attr(&title),
            html_escape_attr(&t)
        ));
        let _ = t;
        let _ = title;
    }
    let count = docs.len();
    let today = {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let (y, m, d, _, _, _) = kernel_deploy::s3::civil_from_unix(secs as i64);
        format!("{y:04}-{m:02}-{d:02}")
    };
    let page = format!(
        r#"<!doctype html><html lang="zh"><head><meta charset="utf-8"><title>{vault_name} · 演示</title>
<style>
{profile_css}
:root{{--deck-accent:var(--accent,oklch(54% .17 258))}}
html,body{{height:100%;margin:0;background:oklch(19% .014 70);color:oklch(93% .008 85);font-family:system-ui,sans-serif;overflow:hidden}}
.stage{{position:fixed;inset:0;display:grid;place-items:center}}
.slide{{position:absolute;width:100vw;height:100vh;display:none;place-items:center}}
.slide.on{{display:grid;animation:fade .45s var(--ease-spring,cubic-bezier(.32,.72,0,1)) both}}
@keyframes fade{{from{{opacity:0;transform:scale(.97)}}to{{opacity:1;transform:none}}}}
.slide iframe{{width:1280px;height:820px;border:none;border-radius:18px;box-shadow:0 40px 120px -30px oklch(0% 0 0/.7);background:#fff;
transform:scale(min(calc(100vw/1320),calc(100vh/880)));transform-origin:center}}
.cover{{text-align:center}}
.cover h1{{font-size:56px;margin:0 0 18px;letter-spacing:.02em}}
.cover .sub{{color:oklch(70% .01 80);font-size:18px}}
.cover .mark{{width:64px;height:64px;border-radius:20px;background:var(--deck-accent);color:oklch(100% 0 0);display:inline-grid;place-items:center;font-size:26px;font-weight:800;margin-bottom:26px}}
.toc{{max-width:720px;text-align:left}}
.toc li{{padding:10px 0;border-bottom:1px solid oklch(100% 0 0/.08);font-size:19px;list-style:none}}
.bar{{position:fixed;left:0;bottom:0;height:4px;background:var(--deck-accent);transition:width .4s var(--ease-spring,cubic-bezier(.32,.72,0,1))}}
.hud{{position:fixed;right:18px;bottom:16px;font-family:ui-monospace,monospace;font-size:12px;color:oklch(70% .01 80)}}
.hint{{position:fixed;left:18px;bottom:14px;font-size:12px;color:oklch(60% .01 80)}}
</style></head><body>
<div class="stage">
  <section class="slide on cover"><div><div class="mark">3C</div><h1>{vault_name}</h1><div class="sub">{count} 篇知识 · {today} · 鹿蕊 Litmus</div></div></section>
  <section class="slide"><div class="toc"><h2 style="margin:0 0 18px">目录</h2><ul style="padding:0;margin:0">{toc_items}</ul></div></section>
  {slides}
</div>
<div class="bar" id="bar" style="width:0"></div>
<div class="hud" id="hud">1 / {total}</div>
<div class="hint">← → 翻页 · End 直达 · 刷新重排</div>
<script>
const slides=[...document.querySelectorAll('.slide')];let i=0;
function go(n){{i=Math.max(0,Math.min(slides.length-1,n));
  slides.forEach((s,x)=>s.classList.toggle('on',x===i));
  document.getElementById('bar').style.width=((i+1)/slides.length*100)+'%';
  document.getElementById('hud').textContent=(i+1)+' / '+slides.length;}}
addEventListener('keydown',e=>{{
  if(e.key==='ArrowRight'||e.key==='PageDown'||e.key===' ')go(i+1);
  if(e.key==='ArrowLeft'||e.key==='PageUp')go(i-1);
  if(e.key==='Home')go(0); if(e.key==='End')go(slides.length-1);
}});
addEventListener('click',e=>{{if(e.clientX>innerWidth/2)go(i+1);else go(i-1)}});
go(0);
</script></body></html>"#,
        toc_items = docs.iter().filter_map(|p| p.file_stem().map(|s| format!("<li>{}</li>", s.to_string_lossy()))).collect::<Vec<_>>().join(""),
        total = count + 2,
    );
    axum::response::Html(page).into_response()
}

fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// OpenFlow 主题预设（提取自 ThemeSystem::presets()）。
async fn design_presets() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")],
        include_str!("../web/presets.json"),
    )
}

/// 网页本地化：抓 URL → HTML→MD → 导入为文档。
async fn ingest_web(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return err(StatusCode::BAD_REQUEST, "url must be http(s)").into_response();
    }
    let u2 = url.clone();
    let r = tokio::task::spawn_blocking(move || {
        kernel_core::fetch_html(&u2, std::time::Duration::from_secs(30))
    }).await;
    let html = match r {
        Ok(Ok(h)) => h,
        _ => return err(StatusCode::BAD_REQUEST, "抓取失败").into_response(),
    };
    // HTML→MD（简化：提取标题 + 正文纯文本 + 原始 HTML 存为 AI-HTML 块）
    let title = req.get("title").and_then(|t| t.as_str()).map(|s| s.to_string())
        .unwrap_or_else(|| {
        let lower = html.to_lowercase();
        let i = lower.find("<title").and_then(|i| html[i..].find('>').map(|g| i + g + 1));
        i.and_then(|i| html[i..].find("</title>").map(|e| html[i..i + e].trim().to_string()))
         .unwrap_or_else(|| url.clone())
    });
    // 抽取正文（去 script/style 标签）
    let mut text = html.clone();
    for tag in ["script", "style", "noscript"] {
        let open = format!("<{}", &tag[..1]);
        let close = format!("</{}>", tag);
        while let Some(i) = text.to_lowercase().find(&open) {
            let rest = &text[i..];
            if let Some(j) = rest.to_lowercase().find(&close) {
                let e2 = j + close.len();
                text = format!("{}{}", &text[..i], &text[i + e2..]);
            } else { break; }
        }
    }
    let stripped = strip_all_tags(&text);
    let words: Vec<&str> = stripped.split_whitespace().collect();
    let excerpt = words.iter().take(500).cloned().collect::<Vec<_>>().join(" ");
    let md = format!("# {}\\n\\n> 来源：{}\\n\\n{}\\n\\n---\\n\\n<!-- 原始 HTML 已归档，见附件 -->\\n", title, url, excerpt);
    let mut k = st.kernel.lock().unwrap();
    let uri = format!("web://{}", kernel_core::Cas::hash_hex(url.as_bytes())[..16].to_string());
    match k.import_capture("web", &uri, Some(&title), &md, "text/markdown") {
        Ok(rel) => {
            audit_log(&k.vault.sidecar(), "ingest-web", &json!({ "url": url, "rel": rel }));
            Json(json!({ "title": title, "rel": rel, "bytes": excerpt.len() })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// POST /ingest/html：外部（如桌面端内置 ego-lite 渲染器）送来的已渲染 HTML → 本地化为文档。
async fn ingest_html(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
    let html = req.get("html").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if html.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "html 为空").into_response();
    }
    let lower = html.to_lowercase();
    let mut text = html.clone();
    for tag in ["script", "style", "noscript", "svg"] {
        let open = format!("<{}", &tag[..1]);
        let close = format!("</{}>", tag);
        while let Some(i) = text.to_lowercase().find(&open) {
            let rest = &text[i..];
            if let Some(j) = rest.to_lowercase().find(&close) {
                let e2 = j + close.len();
                text = format!("{}{}", &text[..i], &text[i + e2..]);
            } else { break; }
        }
    }
    let title = req.get("title").and_then(|t| t.as_str()).map(|s| s.to_string()).unwrap_or_else(|| {
        let i = lower.find("<title").and_then(|i| html[i..].find('>').map(|g| i + g + 1));
        i.and_then(|i| html[i..].find("</title>").map(|e| html[i..i + e].trim().to_string()))
         .unwrap_or_else(|| if url.is_empty() { "渲染页面".into() } else { url.clone() })
    });
    let stripped = strip_all_tags(&text);
    let words: Vec<&str> = stripped.split_whitespace().collect();
    let excerpt = words.iter().take(800).cloned().collect::<Vec<_>>().join(" ");
    let src_line = if url.is_empty() { String::new() } else { format!("\n> 来源：{url}\n") };
    let md = format!("# {title}\n{src_line}\n{excerpt}\n\n---\n\n<!-- 由内置 ego-lite 渲染器抓取（JS 渲染后 DOM） -->\n");
    let mut k = st.kernel.lock().unwrap();
    let uri = format!("ego://{}", kernel_core::Cas::hash_hex(if url.is_empty() { title.as_bytes() } else { url.as_bytes() })[..16].to_string());
    match k.import_capture("ego", &uri, Some(&title), &md, "text/markdown") {
        Ok(rel) => {
            audit_log(&k.vault.sidecar(), "ingest-ego", &json!({ "url": url, "rel": rel }));
            Json(json!({ "title": title, "rel": rel, "bytes": excerpt.len() })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 去掉全部 HTML 标签，留纯文本。
fn strip_all_tags(html: &str) -> String {    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            match html[i..].find('>') {
                Some(g) => { i += g + 1; continue; }
                None => break,
            }
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    html_unescape(&out)
}

fn html_unescape(s: &str) -> String {
    s.replace("&nbsp;", " ").replace("&lt;", "<").replace("&gt;", ">")
     .replace("&quot;", "\u{0022}").replace("&amp;", "&")
}

/// 加密分享：存密文，服务器永远没有密钥（在 URL fragment 里）。
async fn share_create(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let content = req.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
    let iv = req.get("iv").and_then(|v| v.as_array()).map(|a| serde_json::to_string(a).unwrap_or_default()).unwrap_or_default();
    if content.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 content").into_response();
    }
    let id = format!("{}", kernel_core::Cas::hash_hex(content.as_bytes())[..16].to_string());
    let dir = st.kernel.lock().unwrap().vault.sidecar().join("shares");
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_string(&json!({"content":content,"iv":iv})).unwrap_or_default()).unwrap_or(());
    Json(json!({ "id": id })).into_response()
}

#[allow(dead_code)]
async fn share_get(AxumPath(id): AxumPath<String>) -> impl IntoResponse {
    if id.contains("..") || id.contains('/') {
        return err(StatusCode::BAD_REQUEST, "bad id").into_response();
    }
    // 共享不需要 token（接收人没有）——密钥在 URL fragment 里保护隐私
    let dir = {
        // 无 state（公共端点），从环境/配置找第一个库？简化：不验 token，只读 sidecar
        // 实际上需要 state，但我们让它公开（密钥加密保护内容）
        std::path::PathBuf::from("/data/kb/.thirdc/shares")
    };
    let _ = dir;
    err(StatusCode::NOT_FOUND, "share not found (需要完整路径)").into_response()
}

/* ───────── 文档格式互转：导入（结构保真）/ 导出（docx · odt · 打印版 PDF） ───────── */

/// 任意文档文件 → 结构化 Markdown（图片尚未落库，占位 kc-img://n）。返回 (结果, 抽取器名)。
/// 原生：docx/odt/pptx/odp/xlsx/ods/epub/rtf/html；PDF：pdftotext → 版式清理；
/// .doc：macOS textutil(HTML) → LibreOffice 转 docx → 失败提示另存。
fn import_document(ext: &str, bytes: &[u8]) -> Result<(kernel_convert::Converted, String), String> {
    let ext = ext.to_ascii_lowercase();
    if kernel_convert::supports(&ext) {
        return kernel_convert::convert(&ext, bytes).map(|c| (c, format!("native-{ext}")));
    }
    let tmp_dir = std::env::temp_dir().join(format!("thirdc-imp-{}-{}", std::process::id(), now_secs()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let tmp = tmp_dir.join(format!("in.{ext}"));
    let _ = std::fs::write(&tmp, bytes);
    let run = |prog: &str, args: &[&str]| -> Option<Vec<u8>> {
        std::process::Command::new(prog).args(args).output().ok().filter(|o| o.status.success()).map(|o| o.stdout)
    };
    let p = tmp.to_string_lossy().to_string();
    let out = match ext.as_str() {
        "pdf" => {
            let text = run("pdftotext", &["-enc", "UTF-8", &p, "-"]).map(|b| String::from_utf8_lossy(&b).into_owned()).filter(|t| !t.trim().is_empty());
            let text = text.or_else(|| python_extract(&tmp));
            match text {
                Some(t) => Ok((kernel_convert::Converted { markdown: pdf_text_to_md(&t), ..Default::default() }, "pdftotext".to_string())),
                None => Err("本机没有 PDF 解析器：请在客户端导入（浏览器内解析），或安装 poppler（pdftotext）".to_string()),
            }
        }
        "doc" | "wps" | "wpd" | "pages" | "wordml" | "webarchive" => {
            // macOS：textutil 转 HTML 保留标题 / 列表 / 表格 / 粗斜体
            if let Some(html) = run("textutil", &["-convert", "html", "-stdout", &p]) {
                kernel_convert::html(&String::from_utf8_lossy(&html)).map(|c| (c, "textutil".to_string()))
            } else if run("soffice", &["--headless", "--convert-to", "docx", "--outdir", &tmp_dir.to_string_lossy(), &p]).is_some() {
                let docx = std::fs::read(tmp_dir.join("in.docx")).map_err(|e| e.to_string())?;
                kernel_convert::convert("docx", &docx).map(|c| (c, "libreoffice".to_string()))
            } else {
                Err(format!(".{ext} 是旧版二进制格式：请在 Word / WPS 里另存为 .docx 后再导入"))
            }
        }
        _ => match python_extract(&tmp) {
            Some(t) => Ok((kernel_convert::Converted { markdown: t, ..Default::default() }, "python".to_string())),
            None => Err(format!("暂不支持 .{ext}")),
        },
    };
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let (mut c, x) = out?;
    if x == "pdftotext" || x == "python" {
        let r = kernel_core::tidy::tidy(&c.markdown, &kernel_core::tidy::TidyOptions { promote_title: true, pangu: false });
        c.markdown = r.markdown;
    }
    Ok((c, x))
}

/// 旧路径：scripts/extract_doc.py（服务器部署形态常备 PyMuPDF / python-docx）。
fn python_extract(path: &std::path::Path) -> Option<String> {
    let script = std::env::var("THIRDC_EXTRACT_SCRIPT").ok().map(std::path::PathBuf::from).filter(|p| p.is_file())
        .or_else(|| Some(std::path::PathBuf::from("scripts/extract_doc.py")).filter(|p| p.is_file()))
        .or_else(|| std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("../src/scripts/extract_doc.py"))).filter(|p| p.is_file()))?;
    let python = std::env::var("THIRDC_PYTHON").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "python3".to_string());
    let o = std::process::Command::new(python).arg(&script).arg(path).output().ok()?;
    let v: Value = serde_json::from_slice(&o.stdout).ok()?;
    v["ok"].as_bool().filter(|x| *x)?;
    v["text"].as_str().map(String::from).filter(|t| !t.trim().is_empty())
}

/// PDF 抽出的纯文本 → 版式清理：去掉每页重复的页眉页脚与页码，换页处接回被截断的句子，
/// 其余（硬换行、列表、标题）交给魔法排版规则。
fn pdf_text_to_md(text: &str) -> String {
    let pages: Vec<Vec<&str>> = text.split('\u{c}').map(|p| p.lines().collect()).filter(|p: &Vec<&str>| p.iter().any(|l| !l.trim().is_empty())).collect();
    let n = pages.len();
    // 页眉页脚：每页首/末 2 行里，去掉数字后在 ≥ 一半页面重复出现的行
    let norm = |l: &str| l.chars().filter(|c| !c.is_ascii_digit() && !c.is_whitespace()).collect::<String>();
    let mut freq: HashMap<String, usize> = HashMap::new();
    if n >= 3 {
        for p in &pages {
            let ls: Vec<&&str> = p.iter().filter(|l| !l.trim().is_empty()).collect();
            let mut edge: Vec<String> = ls.iter().take(2).chain(ls.iter().rev().take(2)).map(|l| norm(l)).collect();
            edge.sort();
            edge.dedup();
            for e in edge {
                *freq.entry(e).or_default() += 1;
            }
        }
    }
    let repeated: std::collections::HashSet<String> = freq.into_iter().filter(|(k, c)| *c * 2 >= n && k.chars().count() <= 40).map(|(k, _)| k).collect();
    let mut out: Vec<String> = Vec::new();
    for p in pages {
        let ls: Vec<&str> = p.into_iter().collect();
        let nonblank: Vec<usize> = ls.iter().enumerate().filter(|(_, l)| !l.trim().is_empty()).map(|(i, _)| i).collect();
        let edges: std::collections::HashSet<usize> = nonblank.iter().take(2).chain(nonblank.iter().rev().take(2)).copied().collect();
        for (i, l) in ls.iter().enumerate() {
            let t = l.trim();
            if edges.contains(&i) && (repeated.contains(&norm(t)) || t.chars().all(|c| c.is_ascii_digit() || "-–— /第页共Page of".contains(c))) && !t.is_empty() {
                continue;
            }
            out.push(t.to_string());
        }
        // 换页：上一页末行没收句就直接接下一页
        out.push("\u{0}PAGE".into());
    }
    let mut md = String::new();
    let mut prev = String::new();
    for l in out {
        if l == "\u{0}PAGE" {
            if !prev.is_empty() && "。！？.!?：:；;".contains(prev.chars().last().unwrap_or('。')) {
                md.push('\n');
            }
            continue;
        }
        md.push_str(&l);
        md.push('\n');
        if !l.is_empty() {
            prev = l;
        }
    }
    md
}

/// 存图片并换占位符，返回完整 Markdown（# 标题 + 正文）。
fn materialize(k: &mut Kernel, mut c: kernel_convert::Converted, fallback_title: &str) -> String {
    c.resolve_images(|img| k.put_asset(&img.name, &img.bytes).ok().map(|m| m.rel));
    c.document(fallback_title)
}

/// GET /export/{fmt}?path=…&token=…  fmt = docx | odt | md | html | print
async fn export_doc(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    AxumPath(fmt): AxumPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // 下载走 <a href>：带不了 header，query token 兜底
    if check_token(&st, &h).is_err() && q.get("token").map(|t| t != &st.token).unwrap_or(true) {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing token").into_response();
    }
    let path = q.get("path").cloned().unwrap_or_default();
    if !is_safe_doc_path(&path) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let (md, title, root, html) = {
        let mut k = st.kernel.lock().unwrap();
        let _ = k.sync_throttled();
        let model = match k.get_doc(&path) {
            Ok(m) => m,
            Err(e) => return err(StatusCode::NOT_FOUND, e).into_response(),
        };
        let html = if fmt == "print" || fmt == "html" { k.render_doc_html(&path).unwrap_or_else(|_| kernel_core::to_html(&model)) } else { String::new() };
        let raw = std::fs::read_to_string(k.vault.root.join(&path)).unwrap_or_else(|_| kernel_core::to_markdown(&model));
        (raw, model.title.clone().unwrap_or_else(|| stem_of(&path)), k.vault.root.clone(), html)
    };
    let stem = safe_file_stem(&title);
    let stem = if stem.is_empty() { stem_of(&path) } else { stem };
    let root2 = root.clone();
    let load = move |src: &str| -> Option<Vec<u8>> {
        let rel = src.strip_prefix("/asset-file?path=").unwrap_or(src).trim_start_matches("./").replace("%2F", "/");
        if rel.contains("..") || !(rel.starts_with("Assets/") || rel.starts_with("Notes/")) {
            return None;
        }
        std::fs::read(root2.join(rel)).ok()
    };
    let opts = kernel_convert::export::ExportOpts { title: None, load_image: &load };
    let (bytes, mime, ext): (Vec<u8>, &str, &str) = match fmt.as_str() {
        "docx" => match kernel_convert::export::to_docx(&strip_frontmatter(&md), &opts) {
            Ok(b) => (b, "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "docx"),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        "odt" => match kernel_convert::export::to_odt(&strip_frontmatter(&md), &opts) {
            Ok(b) => (b, "application/vnd.oasis.opendocument.text", "odt"),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        "md" => (md.into_bytes(), "text/markdown; charset=utf-8", "md"),
        "html" => (inline_vault_assets(&html, &root).into_bytes(), "text/html; charset=utf-8", "html"),
        "print" => {
            let auto = q.get("auto").map(|v| v == "1").unwrap_or(false);
            let page = print_html(&inline_vault_assets(&html, &root), auto);
            return ([(header::CONTENT_TYPE, "text/html; charset=utf-8".to_string())], page).into_response();
        }
        _ => return err(StatusCode::BAD_REQUEST, "fmt 只支持 docx / odt / md / html / print").into_response(),
    };
    let fname = format!("{stem}.{ext}");
    let ascii: String = fname.chars().map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '_' }).collect();
    let disp = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", urlenc_query(&fname));
    ([(header::CONTENT_TYPE, mime.to_string()), (header::CONTENT_DISPOSITION, disp)], bytes).into_response()
}

fn strip_frontmatter(md: &str) -> String {
    if let Some(rest) = md.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            return rest[end + 4..].trim_start_matches('\n').to_string();
        }
    }
    md.to_string()
}

/// 打印版：A4 页面、页边距、标题不落页尾、表格/代码/图片不被拦腰截断、链接打印出地址。
fn print_html(html: &str, auto: bool) -> String {
    let css = "<style data-print>\
@page{size:A4;margin:20mm 18mm 22mm}\
@media print{html,body{background:#fff!important}body{max-width:none!important;padding:0!important;font-size:11pt!important;line-height:1.7!important}\
h1,h2,h3,h4{break-after:avoid;page-break-after:avoid}h1{break-before:avoid}\
table,pre,blockquote,img,figure,.kb-embed,.math-display,pre.mermaid{break-inside:avoid;page-break-inside:avoid}\
p,li{orphans:3;widows:3}thead{display:table-header-group}tr{break-inside:avoid}\
pre{white-space:pre-wrap;word-break:break-word;box-shadow:none!important;border:1px solid #ddd}\
img{box-shadow:none!important;max-height:240mm}\
a[href^=\"http\"]::after{content:\" (\" attr(href) \")\";font-size:.8em;color:#777;word-break:break-all}\
.kb-embed{display:none}}</style>";
    let script = if auto {
        "<script>addEventListener('load',()=>setTimeout(()=>{try{window.print()}catch(e){}},400))</script>"
    } else {
        ""
    };
    let extra = format!("{css}{script}");
    if html.contains("</head>") { html.replacen("</head>", &format!("{extra}</head>"), 1) } else { format!("{extra}{html}") }
}

/// PDF / DOCX 内容解析：上传文件 → shell python3 extract_doc.py → 导入为文档。
async fn ingest_file(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("file.pdf").to_string();
    let content_b64 = req.get("content_base64").and_then(|c| c.as_str()).unwrap_or("").to_string();
    // 客户端已解析好的 Markdown（PDF 走浏览器内 pdf.js 版式分析）
    let client_md = req.get("markdown").and_then(|c| c.as_str()).map(String::from).filter(|s| !s.trim().is_empty());
    use base64::Engine;
    let bytes = if content_b64.is_empty() {
        Vec::new()
    } else {
        match base64::engine::general_purpose::STANDARD.decode(&content_b64) {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("base64: {e}")).into_response(),
        }
    };
    if bytes.is_empty() && client_md.is_none() {
        return err(StatusCode::BAD_REQUEST, "需要 content_base64 或 markdown").into_response();
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    let stem = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(&name).to_string();

    let (conv, extractor) = match client_md {
        Some(md) => {
            let r = kernel_core::tidy::tidy(&md, &kernel_core::tidy::TidyOptions { promote_title: true, pangu: false });
            let mut c = kernel_convert::Converted { markdown: r.markdown, ..Default::default() };
            // 客户端带来的图片（PDF 页内图）：[{name, content_base64}]，正文里用 kc-img://n 占位
            if let Some(arr) = req.get("images").and_then(|v| v.as_array()) {
                for (i, im) in arr.iter().enumerate() {
                    let b = im.get("content_base64").and_then(|x| x.as_str()).and_then(|x| base64::engine::general_purpose::STANDARD.decode(x).ok());
                    if let Some(b) = b {
                        c.images.push(kernel_convert::Image { token: format!("kc-img://{i}"), name: im.get("name").and_then(|x| x.as_str()).unwrap_or("image.png").to_string(), bytes: b });
                    }
                }
            }
            (c, "client".to_string())
        }
        None => match tokio::task::spawn_blocking({
            let ext = ext.clone();
            let bytes = bytes.clone();
            move || import_document(&ext, &bytes)
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return err(StatusCode::BAD_REQUEST, e).into_response(),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
    };
    if conv.markdown.trim().is_empty() && conv.images.is_empty() {
        return err(StatusCode::BAD_REQUEST, "提取出的内容为空（扫描件需要 OCR）").into_response();
    }
    let title = conv.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| stem.clone());
    let title: String = title.chars().take(80).collect();

    let mut k = st.kernel.lock().unwrap();
    let full = materialize(&mut k, conv, &title);
    // 正文去掉首行标题（import_capture 会写 # 标题 + 来源行）
    let body_md = full.strip_prefix(&format!("# {}\n", full.lines().next().unwrap_or("").trim_start_matches("# "))).unwrap_or(&full).trim_start().to_string();
    let doc_title = full.lines().next().and_then(|l| l.strip_prefix("# ")).unwrap_or(&title).to_string();
    let uri = format!("upload://{}-{}", &kernel_core::Cas::hash_hex(name.as_bytes())[..12], &kernel_core::Cas::hash_hex(if bytes.is_empty() { body_md.as_bytes() } else { &bytes })[..8]);
    match k.import_capture("upload", &uri, Some(&doc_title), &body_md, "text/markdown") {
        Ok(rel) => {
            // 附件本体也入库（原文可下载）
            if !bytes.is_empty() {
                let _ = k.put_asset(&name, &bytes);
            }
            audit_log(&k.vault.sidecar(), "ingest-file", &json!({"name": name, "rel": rel, "bytes": bytes.len(), "extractor": extractor}));
            Json(json!({ "title": doc_title, "rel": rel, "text_len": body_md.len(), "file_size": bytes.len(), "extractor": extractor })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 用户名密码登录：验证后返回 token。
/// 凭据：thirdc.toml [auth] 设了用户名密码则用它；否则默认 admin / machine token。
async fn auth_login(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> impl IntoResponse {
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let username = req.get("username").and_then(|u| u.as_str()).unwrap_or("");
    let password = req.get("password").and_then(|p| p.as_str()).unwrap_or("");
    if username.is_empty() || password.is_empty() {
        return err(StatusCode::BAD_REQUEST, "需要 username 和 password").into_response();
    }
    let (machine, auth_cfg) = {
        let k = st.kernel.lock().unwrap();
        (k.vault.ensure_machine().unwrap_or_default(), k.vault.config.auth.clone())
    };
    let (expected_user, expected_password) = match auth_cfg {
        Some(a) if !a.username.is_empty() && !a.password.is_empty() => (a.username, a.password),
        _ => ("admin".to_string(), machine.token.clone()),
    };
    // 会话一律发 machine token：API 鉴权（check_token）只认它，密码只守登录表单
    if username == expected_user && password == expected_password {
        Json(json!({ "token": machine.token, "ok": true })).into_response()
    } else {
        err(StatusCode::UNAUTHORIZED, "用户名或密码错误").into_response()
    }
}

/// 强制刷新：清缓存 + 全量同步 + 返回最新状态。
async fn force_refresh(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let mut k = st.kernel.lock().unwrap();
    let changed = k.sync_all().unwrap_or(0);
    let docs = kernel_core::list_docs(&k.vault).map(|d| d.len()).unwrap_or(0);
    let indexed = k.indexed_count().unwrap_or(0);
    Json(json!({
        "ok": true,
        "changed": changed,
        "docs": docs,
        "indexed": indexed,
        "message": format!("已刷新：{changed} 篇变更，{docs} 篇文档，{indexed} 篇已索引")
    })).into_response()
}

/// 内联视图：直接解析一个视图声明（调试/自定义面板用）。
async fn view_resolve(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let current = req.get("current").and_then(|c| c.as_str()).map(|s| s.to_string());

    // AI 一句话建视图：{ prompt } → 模型生成 yamlish → 解析；未配置模型 → 501
    let (spec_text, mode) = if let Some(p) = req.get("prompt").and_then(|p| p.as_str()).filter(|s| !s.trim().is_empty()) {
        let ai = st.kernel.lock().unwrap().vault.config.ai.clone();
        let cfg = match ai {
            Some(c) if !c.base_url.is_empty() => c,
            _ => return err(StatusCode::NOT_IMPLEMENTED, "AI建视图需要先在「设置 → 连接」配置模型").into_response(),
        };
        let server = st.mcp.clone();
        let full = format!(
            "你是 ThirdC 视图规格生成器。把用户的一句话转成 ```view 代码块内的 yamlish 规格。\
             只输出规格文本本身，不要解释、不要 Markdown 代码围栏。\n\n\
             可用键：type(table|kanban|calendar|list)、source(all|here|folder:路径|query:关键词)、\
             fields(列，逗号分隔)、filter(tag:x|text:x|kind:x|key=value)、\
             sort(属性:asc|desc 或 updated_desc)、group_by(tag|collection|kind)、limit、\
             formula(名称 = 表达式，变量为frontmatter属性)、total(属性名)、\
             relation(关联字段)、rollup(关联字段.属性:sum|avg|count|min|max，可逗号分隔，可用「别名 = 表达式」)。\n\n\
             示例：\n用户：按工时倒序的任务表，带合计和客户关联汇总\n\
             type: table\nsource: folder:Notes\nfields: title, status, hours, related, 关联工时\n\
             relation: related\nrollup: 关联工时 = related.hours:sum\ntotal: hours\nsort: hours:desc\nlimit: 50\n\n\
             用户：{p}"
        );
        let outcome = match thirdc_mcp::agent::run_ai(&server, &cfg, &full).await {
            Ok(o) => o,
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("模型调用失败：{e}")).into_response(),
        };
        (extract_view_spec(&outcome.reply), "ai")
    } else {
        (
            req.get("spec").and_then(|s| s.as_str()).unwrap_or("type: table\nsource: all").to_string(),
            "rule",
        )
    };

    let spec = kernel_core::views::ViewSpec::from_yamlish(&spec_text);
    let mut k = st.kernel.lock().unwrap();
    match k.resolve_view(&spec, current.as_deref()) {
        Ok(rows) => Json(json!({ "spec": spec, "spec_text": spec_text, "mode": mode, "rows": rows, "html": kernel_core::views::render_html(&spec, &rows) })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 从模型回复中提取视图规格：优先 ```view 块，否则剥围栏取全文。
fn extract_view_spec(s: &str) -> String {
    let t = s.trim();
    if let Some(start) = t.find("```view") {
        let rest = &t[start + 7..];
        let end = rest.find("```").unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    strip_fence(t)
}

/// Agent 记忆：列出 / 追加 / 回忆。
async fn agent_memory_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let k = st.kernel.lock().unwrap();
    let content = std::fs::read_to_string(k.vault.root.join(kernel_core::memory::MEMORY_FILE)).unwrap_or_default();
    Json(json!({ "memory": content })).into_response()
}

async fn agent_memory_add(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let kind = req.get("kind").and_then(|k| k.as_str()).unwrap_or("事实");
    let text = req.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let mut k = st.kernel.lock().unwrap();
    match k.save_memory(kind, text) {
        Ok(rel) => Json(json!({ "saved": rel, "kind": kind })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn agent_recall(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let query = q.get("q").cloned().unwrap_or_default();
    let mut k = st.kernel.lock().unwrap();
    match k.recall(&query, 5) {
        Ok(hits) => Json(json!({ "query": query, "hits": hits.iter().map(|(p, s, sn)| json!({"path": p, "score": s, "snippet": sn})).collect::<Vec<_>>() })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 全量备份：打包整个 vault（加密可选），返回文件或推送到 WebDAV。
async fn backup_vault(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    use kernel_deploy::encrypted_backup;
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let password = req.get("password").and_then(|p| p.as_str()).unwrap_or("");
    if password.len() < 8 {
        return err(StatusCode::BAD_REQUEST, "备份密码至少 8 位（丢失密码=丢失备份）").into_response();
    }
    let k = st.kernel.lock().unwrap();
    let encrypted = match encrypted_backup::encrypt_vault(&k.vault.root, password) {
        Ok(e) => e,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let hash = kernel_core::Cas::hash_hex(&encrypted);
    let fname = format!("thirdc-backup-{}-{}.tcenc", chrono_today(), &hash[..8]);
    let dir = k.vault.sidecar().join("backups");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(&fname);
    std::fs::write(&path, &encrypted).unwrap_or(());
    // 可选推 WebDAV
    if let Some(dav_url) = req.get("webdav_url").and_then(|u| u.as_str()) {
        let user = std::env::var("WEBDAV_USER").unwrap_or_default();
        let pass = std::env::var("WEBDAV_PASS").unwrap_or_default();
        if !user.is_empty() {
            let client = reqwest::blocking::Client::new();
            let url = format!("{}/{}", dav_url.trim_end_matches('/'), fname);
            match client.put(&url).basic_auth(&user, Some(&pass)).body(encrypted.clone()).send() {
                Ok(r) if r.status().is_success() => { /* 上传成功 */ }
                Ok(r) => eprintln!("WebDAV backup failed: {}", r.status()),
                Err(e) => eprintln!("WebDAV error: {e}"),
            }
        }
    }
    let size = encrypted.len();
    audit_log(&k.vault.sidecar(), "backup", &json!({ "file": fname, "bytes": size, "encrypted": true }));
    Json(json!({
        "file": fname, "bytes": size, "encrypted": true,
        "cipher": "AES-256-GCM", "kdf": "PBKDF2-HMAC-SHA256(600k)", "signed": "Ed25519",
        "message": "备份已加密（AES-256-GCM）+ 设备签名；网盘方看不到原始数据，改动可被发现（丢失密码=丢失备份）"
    })).into_response()
}

fn chrono_today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d, _, _, _) = kernel_deploy::s3::civil_from_unix(secs as i64);
    format!("{y:04}{m:02}{d:02}")
}

fn is_junk_name(name: &str) -> bool {
    name.starts_with('.') || name.starts_with("._") || name == "node_modules"
}

fn is_doc_file(path: &std::path::Path) -> bool {
    path.extension().map_or(false, |x| {
        matches!(x.to_string_lossy().as_ref(), "md" | "html" | "htm" | "view")
    })
}

fn title_from_filename(name: &str) -> String {
    name.trim_end_matches(".md")
        .trim_end_matches(".html")
        .trim_end_matches(".htm")
        .to_string()
}

/// 只读文件头 2KB 抽标题，避免首屏把整篇正文读进内存。
/// 画布卡片的"一瞥"：同一次 2KB 读取里拿到标题、摘要、frontmatter 标签与 [[双链]] 目标。
/// 不读全文（首屏性能纪律），摘要只给卡片预览用，不是检索依据。
struct DocPeek {
    title: String,
    excerpt: String,
    tags: Vec<String>,
    links: Vec<String>,
}

fn peek_doc(path: &std::path::Path, fallback: &str) -> DocPeek {
    use std::io::Read;
    let mut buf = [0u8; 2048];
    let n = match std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return DocPeek { title: fallback.to_string(), excerpt: String::new(), tags: vec![], links: vec![] },
    };
    // 2KB 截断可能切在 UTF-8 中间：lossy 会在尾部留一个替换符，摘要阶段丢掉
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    let title = kernel_core::refs::extract_title(&text)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| fallback.to_string());
    let is_html = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));
    let (mut tags, body) = if is_html { (vec![], String::new()) } else { split_front_tags(&text) };
    // 没有 frontmatter 标签时，退而取正文里的 #标签（memo / 随手记的主流写法）
    if tags.is_empty() {
        tags = inline_hashtags(&body);
    }
    DocPeek { title: title.clone(), excerpt: md_excerpt(&body, &title, 140), tags, links: wiki_links(&body) }
}

/// frontmatter 里的 tags（`tags: [a, b]` 或逐行 `- a`）+ 去掉 frontmatter 后的正文。
fn split_front_tags(text: &str) -> (Vec<String>, String) {
    let mut tags = Vec::new();
    let t = text.trim_start_matches('\u{feff}');
    if let Some(rest) = t.strip_prefix("---\n").or_else(|| t.strip_prefix("---\r\n")) {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let mut in_tags = false;
            for line in fm.lines() {
                let l = line.trim();
                if let Some(v) = l.strip_prefix("tags:").or_else(|| l.strip_prefix("keywords:")) {
                    let v = v.trim();
                    in_tags = v.is_empty();
                    for x in v.trim_matches(|c| c == '[' || c == ']').split(',') {
                        let x = x.trim().trim_matches(|c| c == '"' || c == '\'' || c == '#');
                        if !x.is_empty() { tags.push(x.to_string()); }
                    }
                } else if in_tags && l.starts_with("- ") {
                    tags.push(l[2..].trim().trim_matches(|c| c == '"' || c == '\'' || c == '#').to_string());
                } else if !l.is_empty() {
                    in_tags = false;
                }
            }
            let body = rest[end + 4..].trim_start_matches(|c| c == '-' || c == '\r' || c == '\n').to_string();
            tags.truncate(6);
            return (tags, body);
        }
    }
    (tags, t.to_string())
}

/// 正文 #标签：行首或空白之后的 `#词`，跳过标题行与代码块，纯数字（#1 之类编号）不算，最多 6 个。
fn inline_hashtags(body: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut in_code = false;
    for line in body.lines() {
        let l = line.trim_start();
        if l.starts_with("```") { in_code = !in_code; continue; }
        if in_code || l.starts_with("# ") || l.starts_with("## ") || l.starts_with("### ") || l.starts_with("####") { continue; }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '#' && (i == 0 || chars[i - 1].is_whitespace()) {
                let mut j = i + 1;
                while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '#' && !"，。；！？、,.;!?()（）[]【】".contains(chars[j]) { j += 1; }
                let t: String = chars[i + 1..j].iter().collect();
                if !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit()) && t.chars().count() <= 20 && !v.contains(&t) {
                    v.push(t);
                    if v.len() >= 6 { return v; }
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }
    v
}

/// 纯文本摘要：跳过标题行/代码块/分隔线，剥掉常见 Markdown 记号，取前 max 个字符。
fn md_excerpt(body: &str, title: &str, max: usize) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for line in body.lines() {
        let l = line.trim();
        if l.starts_with("```") { in_code = !in_code; continue; }
        if in_code || l.is_empty() || l.starts_with('#') || l == "---" || l.starts_with("<!--") || l.starts_with('|') { continue; }
        let mut s = l.trim_start_matches(|c: char| c == '>' || c == '-' || c == '*' || c == '+' || c.is_ascii_digit() || c == '.' || c == ' ').to_string();
        for pat in ["**", "__", "`", "[[", "]]", "*", "~~"] { s = s.replace(pat, ""); }
        if s == title || s.is_empty() { continue; }
        if !out.is_empty() { out.push(' '); }
        out.push_str(&s);
        if out.chars().count() >= max { break; }
    }
    let out: String = out.chars().filter(|&c| c != '\u{fffd}').collect();
    if out.chars().count() > max { format!("{}…", out.chars().take(max).collect::<String>()) } else { out }
}

/// `[[目标]]` / `[[目标|别名]]` / `[[目标#块]]` → 目标名（去重，最多 8 个）。
fn wiki_links(body: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("[[") {
        let after = &rest[i + 2..];
        let Some(j) = after.find("]]") else { break };
        let raw = &after[..j];
        let name = raw.split(['|', '#', '^']).next().unwrap_or("").trim();
        if !name.is_empty() && !name.contains('\n') && !v.iter().any(|x| x == name) { v.push(name.to_string()); }
        if v.len() >= 8 { break; }
        rest = &after[j + 2..];
    }
    v
}

fn count_docs_under(dir: &std::path::Path) -> usize {
    let mut n = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.filter_map(|x| x.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_junk_name(&name) {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                n += 1;
            }
        }
    }
    n
}

/// 画布浏览接口：只扫「当前文件夹」，返回子文件夹 + 该层文档。
/// 首屏热路径：不读全文、不算 wiki 入链；标题 peek 2KB，分数用 mtime+size。
async fn browse(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let dir = q.get("path").cloned().unwrap_or_else(|| "Notes".into());
    if !dir.starts_with("Notes") || dir.contains("..") || dir.contains('\\') {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let abs = root.join(&dir);
    if !abs.is_dir() {
        return err(StatusCode::NOT_FOUND, "folder not found").into_response();
    }

    let mut folders: Vec<Value> = Vec::new();
    let mut docs: Vec<(String, DocPeek, u64, i64, i64)> = Vec::new();
    let mut files: Vec<Value> = Vec::new();

    let rd = match std::fs::read_dir(&abs) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if is_junk_name(&name) {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            folders.push(json!({
                "name": name,
                "path": format!("{dir}/{name}"),
                "count": count_docs_under(&p)
            }));
        } else if is_doc_file(&p) {
            let fallback = title_from_filename(&name);
            let peek = peek_doc(&p, &fallback);
            let md = std::fs::metadata(&p).ok();
            let mtime = md
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = md.map(|m| m.len()).unwrap_or(0) as i64;
            let recent = if now.saturating_sub(mtime) < 7 * 86400 { 2.0 } else { 0.0 };
            let score = ((recent + (size as f64 + 1.0).log10()) * 1000.0) as i64;
            docs.push((format!("{dir}/{name}"), peek, mtime, size, score));
        } else {
            /* 文件即真相：非文档文件（.txt/.pdf/.zip/图片…）一样列出来，
               供下载、系统打开、访达显示——不许因为内核不解析就隐身。 */
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            files.push(json!({ "path": format!("{dir}/{name}"), "name": name, "size": size }));
        }
    }

    docs.sort_by(|a, b| b.4.cmp(&a.4).then(a.0.cmp(&b.0)));

    let has_sub = !folders.is_empty();
    let cap = if has_sub { 30 } else { 200 };
    let offset = q.get("offset").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let arr: Vec<Value> = docs
        .iter()
        .skip(offset)
        .take(cap)
        .map(|(p, pk, mt, sz, sc)| {
            // 只加字段（ABI 只增不破）：excerpt / tags / links 供画布卡片预览与双链连线
            json!({ "path": p, "title": pk.title, "mtime": mt, "size": sz, "score": sc,
                    "excerpt": pk.excerpt, "tags": pk.tags, "links": pk.links })
        })
        .collect();

    let rel_to_root = dir.trim_start_matches("Notes").trim_start_matches('/');
    let mut crumbs = vec![json!({ "name": "库根", "path": "Notes" })];
    let mut acc = String::from("Notes");
    if !rel_to_root.is_empty() {
        for seg in rel_to_root.split('/') {
            acc = format!("{acc}/{seg}");
            crumbs.push(json!({ "name": seg, "path": acc }));
        }
    }
    Json(json!({
        "path": dir,
        "breadcrumb": crumbs,
        "folders": folders,
        "docs": arr,
        "files": files,
        "total_docs": docs.len(),
        "total_files": files.len(),
        "rule": if has_sub { "子文件夹 + 最近文档" } else { "全部文档（按最近）" },
    }))
    .into_response()
}

/// 轻量目录树：只统计文件夹与文件数，不读文件内容（首屏用）。
async fn tree(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    #[derive(Default)]
    struct N {
        name: String,
        path: String,
        count: usize,
        dirs: Vec<N>,
    }
    fn walk(dir: &std::path::Path, root: &std::path::Path) -> Vec<N> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return out;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_junk_name(&name) {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                let kids = walk(&p, root);
                let files = count_docs_immediate(&p);
                let count = kids.iter().map(|k| k.count).sum::<usize>() + files;
                // 空目录也保留——文件真相：Finder 显示空文件夹，树同样显示
                out.push(N {
                    name,
                    path: p
                        .strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    count,
                    dirs: kids,
                });
            }
        }
        out
    }
    fn count_docs_immediate(dir: &std::path::Path) -> usize {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return 0;
        };
        rd.filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                /* 文件即真相：计数含全部非垃圾文件（不只 md/html），
                   只放了附件/图片的文件夹也要在树里出现 */
                !is_junk_name(&n) && e.path().is_file()
            })
            .count()
    }
    let notes = root.join("Notes");
    let dirs = walk(&notes, &root);
    let root_files = count_docs_immediate(&notes);
    let total = dirs.iter().map(|d| d.count).sum::<usize>() + root_files;
    Json(json!({
        "path": "Notes",
        "count": total,
        "files": root_files,
        "dirs": dirs.iter().map(|d| json!({
            "name": d.name,
            "path": d.path,
            "count": d.count,
            "dirs": d.dirs.iter().map(|x| json!({"name": x.name, "path": x.path, "count": x.count})).collect::<Vec<_>>()
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}


// ───────────────────────── 桌面端独占能力（/desktop/*） ─────────────────────────
// 仅当 THIRDC_DESKTOP=1（桌面壳启动）时可用；线上部署返回 404。

fn desktop_guard(st: &AppState) -> Result<(), (StatusCode, Json<Value>)> {
    if st.desktop {
        Ok(())
    } else {
        Err(err(StatusCode::NOT_FOUND, "not found"))
    }
}

/// 探测本机是否有可用的本地模型服务（Ollama / LM Studio）。
fn probe_local_model() -> Value {
    let probe = |host: &str, port: u16| {
        std::net::TcpStream::connect_timeout(
            &format!("{host}:{port}").parse().unwrap(),
            std::time::Duration::from_millis(120),
        )
        .is_ok()
    };
    if probe("127.0.0.1", 11434) {
        json!({"available": true, "kind": "ollama", "endpoint": "http://127.0.0.1:11434"})
    } else if probe("127.0.0.1", 1234) {
        json!({"available": true, "kind": "lmstudio", "endpoint": "http://127.0.0.1:1234"})
    } else {
        json!({"available": false})
    }
}

/// 本机硬件与库概况：让前端知道能开多重的后台任务。
async fn desktop_info(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mem_gb = total_memory_gb();
    let counts = count_vault_files(&root);
    Json(json!({
        "desktop": true,
        "machine": { "cores": cores, "memory_gb": mem_gb },
        "vault": { "root": root.display().to_string(), "files": counts.0, "bytes": counts.1 },
        "capabilities": {
            "local_import": true,
            "terminal": true,
            "transcode": false,
            "local_model": probe_local_model(),
            "local_embedding": false,
        }
    })).into_response()
}

fn total_memory_gb() -> f64 {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl").arg("-n").arg("hw.memsize").output() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                if let Ok(bytes) = s.trim().parse::<u64>() {
                    return (bytes as f64 / 1e9 * 10.0).round() / 10.0;
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    if let Some(kb) = rest.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()) {
                        return (kb as f64 / 1e6 * 10.0).round() / 10.0;
                    }
                }
            }
        }
    }
    0.0
}

/// 统计库内文件数与总字节（跳过 .git/.thirdc 等）。
fn count_vault_files(root: &std::path::Path) -> (usize, u64) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') { continue; }
            if p.is_dir() { stack.push(p); continue; }
            n += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (n, bytes)
}

// ── 转码工具（抽取层可插拔 + 溯源 + 去重）─────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn now_iso() -> String {
    // 仅用于记录，不追求时区精确；用 UNIX 秒即可。
    now_secs().to_string()
}

/// 读取导入溯源索引（sha256 → 记录）。
fn read_import_index(root: &std::path::Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(root.join(".thirdc").join("import-index.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/* ───────── HTML 项目内联：把目录内相对引用折叠为自包含单文件 ───────── */

/// 单资源内联上限：超过则保留原引用（防止误吞巨型文件）。
const INLINE_MAX_BYTES: u64 = 4 * 1024 * 1024;

fn tag_attr_value<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    for q in ['"', '\''] {
        let pat = format!("{attr}={q}");
        if let Some(i) = tag.find(&pat) {
            let rest = &tag[i + pat.len()..];
            if let Some(j) = rest.find(q) {
                return Some(&rest[..j]);
            }
        }
    }
    None
}

/// 相对引用 → 源码树内的真实文件（拒绝绝对路径 / 协议引用 / 逃出源码树）。
fn resolve_local_ref(src_root: &std::path::Path, html_dir: &std::path::Path, reference: &str) -> Option<std::path::PathBuf> {
    let reference = reference.split(['?', '#']).next().unwrap_or(reference);
    if reference.is_empty()
        || reference.starts_with('/')
        || reference.contains(':')
        || reference.starts_with("..")
    {
        return None;
    }
    let base = src_root.join(html_dir);
    let path = base.join(reference);
    let canonical = path.canonicalize().ok()?;
    let root_canonical = src_root.canonicalize().ok()?;
    if !canonical.starts_with(&root_canonical) {
        return None;
    }
    Some(canonical)
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

fn data_uri_for(path: &std::path::Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > INLINE_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
    use base64::Engine;
    Some(format!(
        "data:{};base64,{}",
        mime_for_ext(&ext),
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// 内联 CSS 内的 url(...) 本地引用（图片 / 字体 → data URI）。
fn inline_css_urls(css: &str, src_root: &std::path::Path, html_dir: &std::path::Path) -> String {
    let mut out = String::with_capacity(css.len());
    let bytes = css.as_bytes();
    let mut i = 0;
    while let Some(rel) = css[i..].find("url(") {
        let start = i + rel + 4;
        out.push_str(&css[i..start]);
        let rest = &css[start..];
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
        let q_len = quote.map_or(0, |_| 1);
        let body_end = rest[q_len..]
            .find(quote.map(|c| c as char).unwrap_or(')'))
            .map(|j| q_len + j)
            .unwrap_or(rest.len());
        let reference = rest[q_len..body_end].trim();
        let after = &rest[body_end..];
        if let Some(path) = resolve_local_ref(src_root, html_dir, reference) {
            if let Some(uri) = data_uri_for(&path) {
                out.push_str(&uri);
                out.push_str(after.get(..1).unwrap_or(""));
                i = start + body_end + 1;
                let _ = bytes;
                continue;
            }
        }
        out.push_str(&rest[..body_end]);
        i = start + body_end;
    }
    out.push_str(&css[i..]);
    out
}

/// 把 HTML 项目（源码目录内的相对引用）内联为自包含单文件：
/// `<link rel=stylesheet href=*.css>` → `<style>`；`<script src=*.js>` → 内联脚本；
/// `<img src>` 与内联 CSS 的 `url()` → data URI。仅处理源码树内存在的相对路径，
/// http(s) / data: / 绝对路径 / `..` 逃逸一律原样保留。
fn inline_html_project(html: &str, src_root: &std::path::Path, html_dir: &std::path::Path) -> String {
    let lower = html.to_lowercase();
    let mut out = String::with_capacity(html.len() * 2);
    let mut i = 0;
    while i < html.len() {
        // 找下一个候选标签（取三种里最靠前的）
        let next = ["<link", "<script", "<img"]
            .iter()
            .filter_map(|t| lower[i..].find(t).map(|p| (i + p, *t)))
            .min_by_key(|(p, _)| *p);
        let (pos, kind) = match next {
            Some(v) => v,
            None => {
                out.push_str(&html[i..]);
                break;
            }
        };
        out.push_str(&html[i..pos]);
        if kind == "<script" {
            // 整块 <script …>…</script>
            let tag_end = lower[pos..].find('>').map(|j| pos + j + 1).unwrap_or(html.len());
            let tag_text = &html[pos..tag_end.min(html.len())];
            let close = lower[tag_end..].find("</script>").map(|j| tag_end + j);
            match tag_attr_value(tag_text, "src")
                .and_then(|src| resolve_local_ref(src_root, html_dir, src))
                .filter(|p| p.extension().map_or(false, |e| e == "js" || e == "mjs"))
            {
                Some(path) if std::fs::metadata(&path).map(|m| m.len() <= INLINE_MAX_BYTES).unwrap_or(false) => {
                    let js = std::fs::read_to_string(&path).unwrap_or_default();
                    let attrs = tag_text
                        .strip_prefix("<script")
                        .unwrap_or("")
                        .strip_suffix('>')
                        .unwrap_or("")
                        .replace(&format!("src=\"{}\"", tag_attr_value(tag_text, "src").unwrap_or("")), "");
                    out.push_str("<script");
                    out.push_str(&attrs);
                    out.push('>');
                    out.push_str(&js);
                    out.push_str("</script>");
                    i = close.map(|c| c + "</script>".len()).unwrap_or(html.len());
                }
                _ => {
                    // 非 JS 或无法内联：原样抄整块
                    let end = close.map(|c| c + 9).unwrap_or(html.len());
                    out.push_str(&html[pos..end]);
                    i = end;
                }
            }
        } else {
            // 自闭合风格标签 <link …> / <img …>
            let tag_end = lower[pos..].find('>').map(|j| pos + j + 1).unwrap_or(html.len());
            let tag_text = &html[pos..tag_end];
            let attr = if kind == "<link" { "href" } else { "src" };
            let is_asset = kind == "<img"
                || (tag_text.to_lowercase().contains("stylesheet")
                    && tag_attr_value(tag_text, "href").map_or(false, |h| h.ends_with(".css")));
            let replaced = if is_asset {
                tag_attr_value(tag_text, attr).and_then(|reference| resolve_local_ref(src_root, html_dir, reference))
            } else {
                None
            };
            match replaced {
                Some(path) if kind == "<img" => {
                    if let Some(uri) = data_uri_for(&path) {
                        out.push_str(&tag_text.replace(
                            &format!("{attr}=\"{}\"", tag_attr_value(tag_text, attr).unwrap_or("")),
                            &format!("{attr}=\"{uri}\""),
                        ));
                    } else {
                        out.push_str(tag_text);
                    }
                }
                Some(path) => {
                    // stylesheet link → <style>（并内联 CSS 内的 url()）
                    match std::fs::read_to_string(&path) {
                        Ok(css) => {
                            out.push_str("<style>\n");
                            out.push_str(&inline_css_urls(&css, src_root, html_dir));
                            out.push_str("\n</style>");
                        }
                        Err(_) => out.push_str(tag_text),
                    }
                }
                None => out.push_str(tag_text),
            }
            i = tag_end;
        }
    }
    out
}

fn write_import_index(root: &std::path::Path, map: &serde_json::Map<String, Value>) {
    let dir = root.join(".thirdc");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(s) = serde_json::to_string_pretty(&Value::Object(map.clone())) {
        let _ = std::fs::write(dir.join("import-index.json"), s);
    }
}

/// 外部抽取器（可插拔）：返回 (文本, 抽取器名)。没有可用工具时 None。
/// macOS 自带 textutil；pdf 需 poppler 的 pdftotext；图片 OCR 需 tesseract。
fn extract_text(path: &std::path::Path) -> Option<(String, String)> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    let run = |prog: &str, args: &[&str]| -> Option<String> {
        std::process::Command::new(prog)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .filter(|t| !t.trim().is_empty())
    };
    let p = path.to_str()?;
    match ext.as_str() {
        "docx" | "doc" | "rtf" | "odt" | "wordml" | "html" | "htm" | "webarchive" => {
            run("textutil", &["-convert", "txt", "-stdout", p]).map(|t| (t, "textutil".into()))
        }
        "pdf" => run("pdftotext", &["-layout", p, "-"]).map(|t| (t, "pdftotext".into())),
        "png" | "jpg" | "jpeg" | "tiff" | "webp" => {
            run("tesseract", &[p, "stdout", "-l", "chi_sim+eng"]).map(|t| (t, "tesseract".into()))
        }
        _ => None,
    }
}

/// 纯文本 → 结构化 md：空行分段，短行且无结尾标点视为标题。
fn plain_to_md(text: &str) -> String {
    let mut out = String::new();
    let paras: Vec<&str> = if text.contains("\n\n") {
        text.split("\n\n").collect()
    } else {
        text.lines().collect()
    };
    for para in paras {
        let t = para.trim();
        if t.is_empty() {
            continue;
        }
        let nlines = t.lines().count();
        let chars = t.chars().count();
        let heading = nlines == 1
            && chars <= 40
            && !t.ends_with(['。', '．', '.', '！', '!', '？', '?', '：', ':', '；', ';', '，', ',']);
        if heading {
            out.push_str("## ");
        }
        out.push_str(&t.split_whitespace().collect::<Vec<_>>().join(" "));
        out.push_str("\n\n");
    }
    out
}

/// 带 OKF 溯源 frontmatter 的文档。
fn with_provenance(body_md: &str, src: &str, sha: &str, mtime: u64, extractor: &str) -> String {
    format!(
        "---\nsource: {src}\nsource_sha256: {sha}\nsource_mtime: {mtime}\nextractor: {extractor}\nextracted_at: {at}\n---\n\n{body_md}",
        at = now_iso()
    )
}

/// 导入本机目录到库：md/txt/html 直接入库；office/pdf/图片经抽取器转 md 入库；
/// 其余格式计入待转码。用 sha256 去重并记录溯源。
/// body: { path: "/abs/dir" }
/* ───────── UI 状态同步（侧栏树展开 / 最近文档：存 .thirdc/ui-state.json，随库跨设备） ───────── */
async fn get_ui_state(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let body = std::fs::read_to_string(root.join(".thirdc").join("ui-state.json")).unwrap_or_default();
    let v: Value = serde_json::from_str(&body).unwrap_or(json!({ "updatedAt": 0 }));
    Json(v).into_response()
}

async fn put_ui_state(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    if !v.is_object() {
        return err(StatusCode::BAD_REQUEST, "body must be a json object").into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let dir = root.join(".thirdc");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    match serde_json::to_string_pretty(&v) {
        Ok(s) => match std::fs::write(dir.join("ui-state.json"), s) {
            Ok(()) => Json(json!({ "ok": true })).into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/* ───────── 插件（Obsidian 式清单 + GitHub 链接安装；v0 仅声明式命令，不执行插件代码） ───────── */
const PLUGIN_TARBALL_MAX: u64 = 50 * 1024 * 1024;
/// 宿主插件 API 级别（docs/spec/os.md §3）。当前为 1：manifest+commands+views+tools。
/// 破坏性演进时递增：宿主 N 同时支持 ≤N 的插件；manifest 声明高于宿主级别则拒绝安装。
const HOST_API_LEVEL: u64 = 1;
const PLUGIN_TREE_MAX: u64 = 50 * 1024 * 1024;

fn plugins_root(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".thirdc").join("plugins")
}

fn read_plugin_manifest(dir: &std::path::Path) -> Option<Value> {
    let s = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    serde_json::from_str(&s).ok()
}

/// installed.json：name → { enabled, installedAt }（与上游 manifest 分离，避免污染更新）。
fn read_installed(root: &std::path::Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(plugins_root(root).join("installed.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_installed(root: &std::path::Path, map: &serde_json::Map<String, Value>) {
    let dir = plugins_root(root);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(s) = serde_json::to_string_pretty(&Value::Object(map.clone())) {
        let _ = std::fs::write(dir.join("installed.json"), s);
    }
}

async fn plugins_list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let installed = read_installed(&root);
    let mut plugins = Vec::new();
    if let Ok(rd) = std::fs::read_dir(plugins_root(&root)) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if !p.is_dir() { continue; }
            if let Some(m) = read_plugin_manifest(&p) {
                let name = m.get("id").and_then(|x| x.as_str()).unwrap_or_else(|| p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown")).to_string();
                let state = installed.get(&name).cloned().unwrap_or(json!({"enabled": false}));
                plugins.push(json!({
                    "id": name,
                    "name": m.get("name").cloned().unwrap_or(json!(name)),
                    "version": m.get("version").cloned().unwrap_or(json!("0.0.0")),
                    "description": m.get("description").cloned().unwrap_or(json!("")),
                    "author": m.get("author").cloned().unwrap_or(json!("")),
                    "homepage": m.get("homepage").cloned().unwrap_or(json!("")),
                    "commands": m.get("contributions").and_then(|c| c.get("commands")).cloned().unwrap_or(json!([])),
                    "views": m.get("contributions").and_then(|c| c.get("views")).cloned().unwrap_or(json!([])),
                    "tools": m.get("contributions").and_then(|c| c.get("tools")).cloned().unwrap_or(json!([])),
                    "toolsDisabled": state.get("tools").cloned().unwrap_or(json!({})),
                    "permissions": m.get("permissions").cloned().unwrap_or(json!([])),
                    "enabled": state.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
                    "installedAt": state.get("installedAt").cloned().unwrap_or(json!(null)),
                }));
            }
        }
    }
    plugins.sort_by(|a, b| a.get("id").and_then(|x| x.as_str()).cmp(&b.get("id").and_then(|x| x.as_str())));
    Json(json!({ "plugins": plugins })).into_response()
}

/// GitHub 仓库 → (api tarball url, 可选子目录)。支持 …/tree/<branch>[/sub/dir]。
fn parse_github_repo(url: &str) -> Option<(String, String, Option<String>, Option<String>)> {
    let u = url.trim().trim_end_matches('/');
    let rest = u.strip_prefix("https://github.com/").or_else(|| u.strip_prefix("http://github.com/"))?;
    let mut it = rest.split('/');
    let owner = it.next()?.to_string();
    let repo = it.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() { return None; }
    let mut branch = None;
    let mut subdir = None;
    if it.next() == Some("tree") {
        branch = it.next().map(|s| s.to_string());
        let rest_path: Vec<&str> = it.collect();
        if !rest_path.is_empty() {
            subdir = Some(rest_path.join("/"));
        }
    }
    Some((owner, repo, branch, subdir))
}

async fn plugins_install(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let url = req.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
    let req_path = req.get("path").and_then(|u| u.as_str()).unwrap_or("").trim_matches('/').to_string();
    let (owner, repo, branch, url_subdir) = match parse_github_repo(&url) {
        Some(v) => v,
        None => return err(StatusCode::BAD_REQUEST, "需要 GitHub 仓库链接（https://github.com/owner/repo）").into_response(),
    };
    let subdir = if req_path.is_empty() { url_subdir } else { Some(req_path) };
    let api_url = match &branch {
        Some(b) => format!("https://api.github.com/repos/{owner}/{repo}/tarball/refs/heads/{b}"),
        None => format!("https://api.github.com/repos/{owner}/{repo}/tarball"),
    };
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let out = match tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("thirdc-plugin-installer")
            .timeout(std::time::Duration::from_secs(60))
            .build().map_err(|e| e.to_string())?;
        let resp = client.get(&api_url).send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("GitHub 返回 {}", resp.status()));
        }
        let bytes = resp.bytes().map_err(|e| e.to_string())?;
        if bytes.len() as u64 > PLUGIN_TARBALL_MAX {
            return Err(format!("仓库压缩包超过 {}MB 上限", PLUGIN_TARBALL_MAX / 1024 / 1024));
        }
        let tmp = std::env::temp_dir().join(format!("thirdc-plug-{}-{}", std::process::id(), kernel_core::Cas::hash_hex(url.as_bytes())[..8].to_string()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let tgz = tmp.join("repo.tar.gz");
        std::fs::write(&tgz, &bytes).map_err(|e| e.to_string())?;
        let ex = tmp.join("extract");
        std::fs::create_dir_all(&ex).map_err(|e| e.to_string())?;
        let st2 = std::process::Command::new("tar").args(["-xzf", tgz.to_str().unwrap(), "-C", ex.to_str().unwrap()]).output().map_err(|e| e.to_string())?;
        if !st2.status.success() {
            return Err(format!("解压失败：{}", String::from_utf8_lossy(&st2.stderr).chars().take(200).collect::<String>()));
        }
        // GitHub tarball 有一层顶层目录 repo-<sha>/；定位 manifest（根或一层子目录，支持多插件仓库）
        let mut top = None;
        for e in std::fs::read_dir(&ex).map_err(|e| e.to_string())?.filter_map(|e| e.ok()) {
            if e.path().is_dir() { top = Some(e.path()); break; }
        }
        let top = top.ok_or("压缩包为空")?;
        // 定位插件目录：显式 path > 根 > 一层子目录（兼容多插件仓库）
        let base = match &subdir {
            Some(sp) => {
                let bp = top.join(sp);
                if !bp.is_dir() {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(format!("仓库内未找到目录：{sp}"));
                }
                bp
            }
            None => top.clone(),
        };
        let mut plugin_dirs: Vec<std::path::PathBuf> = Vec::new();
        if base.join("manifest.json").is_file() {
            plugin_dirs.push(base.clone());
        } else if subdir.is_none() {
            for e in std::fs::read_dir(&top).map_err(|e| e.to_string())?.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() && p.join("manifest.json").is_file() { plugin_dirs.push(p); }
            }
        }
        if plugin_dirs.is_empty() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err("未找到 manifest.json（仓库根或一层子目录）".into());
        }
        let dest_root = plugins_root(&root);
        std::fs::create_dir_all(&dest_root).map_err(|e| e.to_string())?;
        let mut installed_names = Vec::new();
        let mut installed_map = read_installed(&root);
        for src in &plugin_dirs {
            let m: Value = serde_json::from_str(&std::fs::read_to_string(src.join("manifest.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let id = m.get("id").and_then(|x| x.as_str()).unwrap_or_else(|| src.file_name().and_then(|n| n.to_str()).unwrap_or("")).to_string();
            if id.is_empty() || id.contains('/') || id.contains("..") {
                continue;
            }
            // API 门禁（os.md §3）：manifest 可声明 "api": <n>，缺省视为 1——
            // 声明高于宿主支持级别时拒绝该插件并整体回滚，不静默装载。
            let papi = m.get("api").and_then(|x| x.as_u64()).unwrap_or(1);
            if papi > HOST_API_LEVEL {
                let _ = std::fs::remove_dir_all(&tmp);
                let pid = m.get("id").and_then(|x| x.as_str()).unwrap_or("?");
                return Err(format!("插件 {pid} 需要宿主 API 级别 {papi}，当前宿主支持 {HOST_API_LEVEL}（请升级 ThirdC）"));
            }
            let dest = dest_root.join(&id);
            if dest.exists() {
                let _ = std::fs::remove_dir_all(&dest);
            }
            std::fs::rename(src, &dest).map_err(|e| e.to_string())?;
            installed_map.insert(id.clone(), json!({ "enabled": true, "installedAt": now_iso() }));
            installed_names.push(id);
        }
        write_installed(&root, &installed_map);
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(installed_names)
    }).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    match out {
        Ok(names) => Json(json!({ "ok": true, "installed": names })).into_response(),
        Err(msg) => err(StatusCode::BAD_REQUEST, msg).into_response(),
    }
}

async fn plugins_toggle(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    let enabled = req.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
    let tool = req.get("tool").and_then(|n| n.as_str()).unwrap_or("").to_string();
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "无效插件名").into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    if !plugins_root(&root).join(&name).is_dir() {
        return err(StatusCode::NOT_FOUND, "插件不存在").into_response();
    }
    let mut map = read_installed(&root);
    if !tool.is_empty() {
        // 工具级开关：installed.json 的 tools 覆盖表
        let entry = map.entry(name).or_insert(json!({ "enabled": true, "installedAt": now_iso() }));
        let tm = entry.as_object_mut().unwrap().entry("tools").or_insert(json!({}));
        tm.as_object_mut().unwrap().insert(tool, json!(enabled));
    } else {
        let mut entry = map.get(&name).cloned().unwrap_or(json!({ "installedAt": now_iso() }));
        entry["enabled"] = json!(enabled);
        map.insert(name, entry);
    }
    write_installed(&root, &map);
    Json(json!({ "ok": true })).into_response()
}

async fn plugins_remove(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "无效插件名").into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let dir = plugins_root(&root).join(&name);
    if !dir.is_dir() {
        return err(StatusCode::NOT_FOUND, "插件不存在").into_response();
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    let mut map = read_installed(&root);
    map.remove(&name);
    write_installed(&root, &map);
    Json(json!({ "ok": true })).into_response()
}

/* ── 插件视图（v1）：manifest.contributions.views 声明的 HTML 经沙箱 iframe 渲染，
      宿主注入 RPC 垫片，按 manifest.permissions 逐项授权（doc:read / doc:write / editor:insert） ── */

const PLUGIN_VIEW_SHIM: &str = r#"<script>
(function(){
  var seq=0,pending={};
  function rpc(call,args){
    return new Promise(function(res,rej){
      var id=++seq;pending[id]={res:res,rej:rej};
      parent.postMessage({source:'thirdc-plugin',call:call,id:id,args:args||{}},'*');
      setTimeout(function(){if(pending[id]){delete pending[id];rej(new Error('host timeout'))}},8000);
    });
  }
  window.thirdc={
    getDocument:function(){return rpc('getDocument')},
    insertText:function(t){return rpc('insertText',{text:String(t==null?'':t)})},
    saveDocument:function(md){return rpc('saveDocument',{markdown:String(md==null?'':md)})},
    toast:function(m){return rpc('toast',{message:String(m==null?'':m)})},
    setViewTitle:function(t){return rpc('setViewTitle',{title:String(t==null?'':t)})}
  };
  addEventListener('message',function(e){
    var d=e.data||{};
    if(d.source==='thirdc-host'&&pending[d.id]){pending[d.id][d.ok?'res':'rej'](d.ok?d.result:new Error(d.error||'host error'));delete pending[d.id];}
  });
})();
</script>"#;

fn plugin_content_type(file: &str) -> &'static str {
    let ext = file.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "md" | "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn plugins_view_file(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    axum::extract::Path((id, file)): axum::extract::Path<(String, String)>,
) -> impl IntoResponse {
    // iframe src 带不了 header：query token 兜底（与前端 asset-file 的 ?token= 用法一致）
    if check_token(&st, &h).is_err() && q.get("token").map(|t| t != &st.token).unwrap_or(true) {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing token").into_response();
    }
    if id.is_empty() || id.contains('/') || id.contains("..") || file.is_empty() || file.contains("..") || file.contains('\\') {
        return err(StatusCode::BAD_REQUEST, "无效路径").into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let base = plugins_root(&root).join(&id);
    if !base.is_dir() {
        return err(StatusCode::NOT_FOUND, "插件不存在").into_response();
    }
    let path = base.join(&file);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let ct = plugin_content_type(&file);
            if file.ends_with(".html") || file.ends_with(".htm") {
                // HTML 入口：注入 RPC 垫片（<head> 后，或文档最前）
                let html = String::from_utf8_lossy(&bytes).into_owned();
                let shimmed = if let Some(i) = html.to_lowercase().find("<head>") {
                    let split = i + "<head>".len();
                    format!("{}{}{}", &html[..split], PLUGIN_VIEW_SHIM, &html[split..])
                } else {
                    format!("{}{}", PLUGIN_VIEW_SHIM, html)
                };
                ([(axum::http::header::CONTENT_TYPE, ct.to_string())], shimmed).into_response()
            } else {
                ([(axum::http::header::CONTENT_TYPE, ct.to_string())], bytes).into_response()
            }
        }
        Err(_) => err(StatusCode::NOT_FOUND, "文件不存在").into_response(),
    }
}

/* ───────── 选区锚定评论（UX-6 v2）：存 .thirdc/comments/<path>.json，随库同步 ───────── */

fn comments_path(root: &std::path::Path, doc: &str) -> Option<std::path::PathBuf> {
    if !is_safe_doc_path(doc) {
        return None;
    }
    Some(root.join(".thirdc").join("comments").join(format!("{doc}.json")))
}

fn comments_read(root: &std::path::Path, doc: &str) -> Vec<Value> {
    let Some(p) = comments_path(root, doc) else { return Vec::new() };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("comments").cloned())
        .and_then(|c| c.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|c| c)
        .collect()
}

fn comments_write(root: &std::path::Path, doc: &str, list: &[Value]) -> Result<(), std::io::Error> {
    let Some(p) = comments_path(root, doc) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad path"));
    };
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(&json!({ "comments": list })).unwrap_or_default())
}

async fn comments_get(State(st): State<Arc<AppState>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let doc = q.get("path").cloned().unwrap_or_default();
    if !is_safe_doc_path(&doc) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    Json(json!({ "path": doc, "comments": comments_read(&root, &doc) })).into_response()
}

/// POST /comments/update：op = add | reply | resolve | delete
async fn comments_update(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let doc = req.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let op = req.get("op").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if !is_safe_doc_path(&doc) {
        return err(StatusCode::BAD_REQUEST, "path must be under Notes/").into_response();
    }
    let mut list = {
        let root = { st.kernel.lock().unwrap().vault.root.clone() };
        comments_read(&root, &doc)
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    match op.as_str() {
        "add" => {
            let id = format!("c{}{}", now, &kernel_core::Cas::hash_hex(serde_json::to_string(&req).unwrap_or_default().as_bytes())[..6]);
            list.push(json!({
                "id": id,
                "start": req.get("start").and_then(|x| x.as_u64()).unwrap_or(0),
                "end": req.get("end").and_then(|x| x.as_u64()).unwrap_or(0),
                "quote": req.get("quote").cloned().unwrap_or(json!("")),
                "author": req.get("author").cloned().unwrap_or(json!("协作者")),
                "ts": now,
                "resolved": false,
                "replies": [],
            }));
        }
        "reply" => {
            let id = req.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let text = req.get("text").cloned().unwrap_or(json!(""));
            let author = req.get("author").cloned().unwrap_or(json!("协作者"));
            let Some(c) = list.iter_mut().find(|c| c.get("id").and_then(|x| x.as_str()) == Some(id)) else {
                return err(StatusCode::NOT_FOUND, "评论不存在").into_response();
            };
            if let Some(arr) = c.get_mut("replies").and_then(|x| x.as_array_mut()) {
                arr.push(json!({ "author": author, "text": text, "ts": now }));
            }
        }
        "resolve" => {
            let id = req.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let resolved = req.get("resolved").and_then(|x| x.as_bool()).unwrap_or(true);
            let Some(c) = list.iter_mut().find(|c| c.get("id").and_then(|x| x.as_str()) == Some(id)) else {
                return err(StatusCode::NOT_FOUND, "评论不存在").into_response();
            };
            c["resolved"] = json!(resolved);
        }
        "delete" => {
            let id = req.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let before = list.len();
            list.retain(|c| c.get("id").and_then(|x| x.as_str()) != Some(id));
            if list.len() == before {
                return err(StatusCode::NOT_FOUND, "评论不存在").into_response();
            }
        }
        _ => return err(StatusCode::BAD_REQUEST, "op 必须是 add/reply/resolve/delete").into_response(),
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    match comments_write(&root, &doc, &list) {
        Ok(()) => Json(json!({ "ok": true, "comments": list })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn desktop_import(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let path = req.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
    if !path.starts_with('/') || !std::path::Path::new(&path).is_dir() {
        return err(StatusCode::BAD_REQUEST, format!("路径无效或不存在：{path}")).into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let dest_root = root.join("Notes").join("Imported");
    let src = std::path::Path::new(&path);
    let mut index = read_import_index(&root);

    let (mut copied, mut dup, mut extracted, mut pending, mut skipped) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut extraction = std::collections::BTreeMap::<String, usize>::new();
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" || name == "target" { continue; }
            if p.is_dir() { stack.push(p); continue; }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            let rel = p.parent().unwrap().strip_prefix(src).unwrap_or(std::path::Path::new(""));
            let Ok(bytes) = std::fs::read(&p) else { skipped += 1; continue };
            let sha = sha256_hex(&bytes);
            if index.contains_key(&sha) {
                dup += 1;
                continue; // 同内容已导入过（去重）
            }
            let mtime = std::fs::metadata(&p).ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()).unwrap_or(0);

            // 得到 (正文, 抽取器, 目标扩展名)。HTML 一等文档与 verbatim 代码/数据文本原样保留
            // （扩展名即契约：.html 走内核原文保留路径，代码文件字节级往返）；其余提取文本转 md。
            let got: Option<(String, String, &str)> = match ext.as_str() {
                "md" | "markdown" => Some((String::from_utf8_lossy(&bytes).into_owned(), "native".into(), "md")),
                "html" | "htm" => {
                    // HTML 一等文档 + 项目内联：目录内相对引用（css/js/图片）折叠为自包含单文件
                    let raw = String::from_utf8_lossy(&bytes).into_owned();
                    let html_dir = p.parent().unwrap().strip_prefix(src).unwrap_or(std::path::Path::new("")).to_path_buf();
                    let inlined = inline_html_project(&raw, src, &html_dir);
                    Some((inlined, "native+inline".into(), "html"))
                }
                "txt" => Some((plain_to_md(&String::from_utf8_lossy(&bytes)), "native".into(), "md")),
                _ if kernel_core::VERBATIM_EXTS.contains(&ext.as_str()) => {
                    Some((String::from_utf8_lossy(&bytes).into_owned(), "native".into(), &ext))
                }
                // 办公 / 开源文档 / PDF：结构保真转换（图片落 Assets）
                _ if kernel_convert::supports(&ext) || matches!(ext.as_str(), "pdf" | "doc" | "wps" | "pages" | "wordml" | "webarchive") => {
                    match import_document(&ext, &bytes) {
                        Ok((c, x)) => {
                            let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("doc").to_string();
                            let mut k = st.kernel.lock().unwrap();
                            let full = materialize(&mut k, c, &stem);
                            Some((full, x, "md"))
                        }
                        Err(_) => match extract_text(&p) {
                            Some((t, x)) => Some((plain_to_md(&t), x, "md")),
                            None => None,
                        },
                    }
                }
                _ => match extract_text(&p) {
                    Some((t, x)) => Some((plain_to_md(&t), x, "md")),
                    None => Some((String::from_utf8_lossy(&bytes).into_owned(), "native".into(), "html")),
                },
            };
            match got {
                Some((body, extractor, outext)) => {
                    let dest_dir = dest_root.join(rel);
                    let _ = std::fs::create_dir_all(&dest_dir);
                    let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("doc");
                    let target = dest_dir.join(format!("{stem}.{outext}"));
                    if target.exists() {
                        dup += 1;
                        continue;
                    }
                    let content = if outext == "md" {
                        with_provenance(&body, &p.display().to_string(), &sha, mtime, &extractor)
                    } else {
                        body
                    };
                    if std::fs::write(&target, content).is_ok() {
                        copied += 1;
                        if extractor != "native" {
                            extracted += 1;
                            *extraction.entry(extractor.clone()).or_insert(0) += 1;
                        }
                        let rel_out = target.strip_prefix(&root).unwrap_or(&target).display().to_string();
                        index.insert(sha.clone(), json!({
                            "source": p.display().to_string(),
                            "dest": rel_out,
                            "mtime": mtime,
                            "extractor": extractor,
                            "imported_at": now_iso(),
                        }));
                    }
                }
                None => {
                    if matches!(ext.as_str(), "docx"|"pptx"|"xlsx"|"pdf"|"epub"|"rtf"|"odt"|"doc") {
                        pending += 1;
                    } else {
                        skipped += 1;
                    }
                }
            }
        }
    }
    write_import_index(&root, &index);
    if copied > 0 {
        let mut k = st.kernel.lock().unwrap();
        let _ = k.sync_all();
    }
    audit_log(&root.join(".thirdc"), "desktop-import", &json!({
        "path": path, "copied": copied, "dup": dup, "extracted": extracted, "pending": pending
    }));
    Json(json!({
        "copied": copied, "dup": dup, "extracted": extracted,
        "pending_transcode": pending, "skipped": skipped,
        "by_extractor": extraction,
    })).into_response()
}

// ── 去噪扫描（严苛标准，只报告/隔离，不删除）──────────────────────────

fn normalize_for_hash(text: &str) -> String {
    let mut s = text;
    if let Some(rest) = s.strip_prefix("---") {
        if let Some(i) = rest.find("\n---") {
            s = &rest[i + 4..];
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn simhash64(tokens: &[&str]) -> u64 {
    let mut v = [0i32; 64];
    for t in tokens {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(t, &mut hasher);
        let hv = std::hash::Hasher::finish(&hasher);
        for (i, slot) in v.iter_mut().enumerate() {
            if (hv >> i) & 1 == 1 { *slot += 1 } else { *slot -= 1 }
        }
    }
    let mut out = 0u64;
    for (i, val) in v.iter().enumerate() {
        if *val > 0 { out |= 1u64 << i }
    }
    out
}

/// 扫描库内文档，给出「可能已无用」的候选清单（含命中的信号与理由）。
async fn desktop_scan(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let notes = root.join("Notes");
    let cutoff = now_secs().saturating_sub(180 * 86400);

    #[derive(Clone)]
    struct Rec { path: String, sha: String, sim: u64, ntok: usize, nchar: usize, uniq: usize, mtime: u64, stem: String }
    let mut recs: Vec<Rec> = Vec::new();
    let mut stack = vec![notes.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() { if p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false) { continue; } stack.push(p); continue; }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            if ext != "md" && ext != "txt" { continue; }
            let Ok(raw) = std::fs::read_to_string(&p) else { continue };
            let norm = normalize_for_hash(&raw);
            let toks: Vec<&str> = norm.split_whitespace().collect();
            let mut uniq = std::collections::HashSet::new();
            for t in &toks { uniq.insert(*t); }
            let mtime = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            recs.push(Rec {
                path: p.strip_prefix(&root).unwrap_or(&p).display().to_string(),
                sha: sha256_hex(norm.as_bytes()),
                sim: simhash64(&toks),
                ntok: toks.len(),
                nchar: norm.chars().count(),
                uniq: uniq.len(),
                mtime,
                stem: p.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_lowercase(),
            });
        }
    }

    // 引用图：谁被 [[stem]] 引用
    let mut referenced = std::collections::HashSet::<String>::new();
    for r in &recs {
        // 从归一化文本找 [[...]]
        let Ok(raw) = std::fs::read_to_string(root.join(&r.path)) else { continue };
        let mut rest = raw.as_str();
        while let Some(i) = rest.find("[[") {
            let after = &rest[i + 2..];
            let Some(j) = after.find("]]") else { break };
            referenced.insert(after[..j].split('|').next().unwrap_or("").trim().to_lowercase());
            rest = &after[j + 2..];
        }
    }

    let mut items: Vec<Value> = Vec::new();
    let mut seen_sha = std::collections::HashMap::<String, String>::new();
    for (i, r) in recs.iter().enumerate() {
        let mut sig: Vec<&str> = Vec::new();
        if r.nchar < 40 { sig.push("empty"); }
        else if r.ntok >= 20 && (r.uniq as f64 / r.ntok as f64) < 0.35 { sig.push("low-info"); }
        if let Some(prev) = seen_sha.get(&r.sha) {
            if prev.as_str() != r.path { sig.push("exact-dup"); }
        } else {
            seen_sha.insert(r.sha.clone(), r.path.clone());
        }
        // 近重复（限规模，避免 O(n²) 过大）
        if recs.len() <= 3000 && r.ntok >= 20 {
            for o in recs.iter().skip(i + 1) {
                if o.ntok < 20 { continue; }
                if (r.sim ^ o.sim).count_ones() <= 3 {
                    sig.push("near-dup");
                    break;
                }
            }
        }
        if r.mtime > 0 && r.mtime < cutoff && !referenced.contains(&r.stem) {
            sig.push("orphan-stale");
        }
        // 被同族更新的版本取代
        for o in &recs {
            if o.path != r.path && o.stem == r.stem && o.mtime > r.mtime {
                sig.push("superseded");
                break;
            }
        }
        if !sig.is_empty() {
            items.push(json!({
                "path": r.path, "signals": sig, "chars": r.nchar, "tokens": r.ntok,
                "mtime": r.mtime, "size": "—"
            }));
        }
    }
    // 严重度：信号越多越严重；exact-dup/empty 加权
    let sev = |sigs: &[Value]| -> i32 {
        sigs.iter().filter_map(|s| s.as_str()).map(|s| match s {
            "exact-dup" | "empty" => 3,
            "near-dup" | "superseded" => 2,
            "low-info" | "orphan-stale" => 1,
            _ => 0,
        }).sum()
    };
    items.sort_by_key(|it| -sev(it.get("signals").and_then(|s| s.as_array()).map(|a| a.as_slice()).unwrap_or(&[])));
    let candidates = items.len();
    Json(json!({
        "scanned": recs.len(),
        "candidates": candidates,
        "items": items,
    })).into_response()
}

/// 把文档移入隔离区（可逆，不删除）。body: { paths: ["Notes/x.md", ...] }
async fn desktop_quarantine(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let paths: Vec<String> = req.get("paths").and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let qdir = root.join(".thirdc").join("quarantine");
    let mut moved: Vec<String> = Vec::new();
    for rel in &paths {
        if !rel.starts_with("Notes/") || rel.contains("..") {
            continue;
        }
        let from = root.join(rel);
        if !from.is_file() { continue; }
        let to = qdir.join(rel);
        if let Some(parent) = to.parent() { let _ = std::fs::create_dir_all(parent); }
        if std::fs::rename(&from, &to).is_ok() || std::fs::copy(&from, &to).is_ok() {
            let _ = std::fs::remove_file(&from);
            moved.push(rel.clone());
        }
    }
    if !moved.is_empty() {
        let mut k = st.kernel.lock().unwrap();
        let _ = k.sync_all();
    }
    audit_log(&root.join(".thirdc"), "desktop-quarantine", &json!({"moved": moved}));
    Json(json!({"moved": moved, "quarantine": ".thirdc/quarantine/"})).into_response()
}

/// 客户端内部命令行：白名单只读命令 + thirdc 子命令，cwd 固定为库根。
/// body: { cmd: "thirdc status" }
async fn desktop_terminal(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let cmd = req.get("cmd").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
    if cmd.is_empty() {
        return err(StatusCode::BAD_REQUEST, "命令为空").into_response();
    }
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let prog = parts[0];
    let allowed = if prog == "thirdc" {
        true
    } else if matches!(prog, "ls" | "cat" | "grep" | "rg" | "find" | "head" | "tail" | "wc" | "tree" | "du" | "file" | "stat"
        | "mkdir" | "touch" | "echo" | "cp" | "mv" | "python3" | "node" | "sed" | "sort" | "uniq") {
        true
    } else if prog == "git" {
        matches!(parts.get(1).copied(), Some("status") | Some("log") | Some("diff") | Some("branch"))
    } else {
        false
    };
    if !allowed {
        return err(StatusCode::FORBIDDEN, format!("命令不在白名单：{prog}")).into_response();
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    // 解析可执行文件：`thirdc` 子命令用 THIRDC_CLI > 自身(若就是 CLI) > 同级 thirdc > PATH。
    let bin: std::path::PathBuf = if prog == "thirdc" {
        match std::env::var("THIRDC_CLI") {
            Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
            _ => {
                let exe = std::env::current_exe().ok();
                let is_cli = exe
                    .as_ref()
                    .and_then(|e| e.file_stem())
                    .and_then(|s| s.to_str())
                    .map(|s| s.starts_with("thirdc") && s != "thirdc-desktop")
                    .unwrap_or(false);
                if is_cli {
                    exe.unwrap()
                } else {
                    exe.and_then(|e| e.parent().map(|d| d.join("thirdc")))
                        .filter(|p| p.is_file())
                        .unwrap_or_else(|| std::path::PathBuf::from("thirdc"))
                }
            }
        }
    } else {
        std::path::PathBuf::from(prog)
    };
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(&bin).args(&parts[1..]).current_dir(&root).output(),
    )
    .await;
    match out {
        Ok(Ok(o)) => {
            let cap = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(64_000)]).into_owned();
            Json(json!({
                "code": o.status.code().unwrap_or(-1),
                "stdout": cap(&o.stdout),
                "stderr": cap(&o.stderr),
            })).into_response()
        }
        Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, format!("执行失败：{e}")).into_response(),
        Err(_) => err(StatusCode::GATEWAY_TIMEOUT, "执行超时（20s）").into_response(),
    }
}

/// 读取并解析 JSON 响应体，失败时给出可诊断的上下文（状态/类型/编码/长度/开头片段）。
/// reqwest 的 `.json()` 只会抛「error decoding response body」，把真实原因藏起来。
async fn read_json(resp: reqwest::Response, what: &str) -> Result<Value, String> {
    let status = resp.status();
    let hdr = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let ct = hdr("content-type");
    let enc = hdr("content-encoding");
    let cl = hdr("content-length");
    let bytes = resp.bytes().await.map_err(|e| format!("读取{what}响应失败（HTTP {status}）：{e}"))?;
    if bytes.is_empty() {
        return Err(format!("{what}响应为空（HTTP {status}，content-type: {ct}）"));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => Ok(v),
        Err(e) => {
            let n = bytes.len().min(200);
            let snip = String::from_utf8_lossy(&bytes[..n]).replace('\n', " ");
            Err(format!(
                "{what}响应不是合法 JSON（HTTP {status}，content-type: {ct}，encoding: {enc}，content-length: {cl}，实收 {} 字节）：{e}；开头：{snip}",
                bytes.len()
            ))
        }
    }
}

/// 从任意形状的 JSON 里提取文档路径列表：`{docs:[...]}` / `{paths:[...]}` / `[...]`，
/// 元素可以是字符串或 `{path:...}`。
fn docs_from_value(v: &Value) -> Vec<String> {
    let arr = v.get("docs").and_then(|d| d.as_array())
        .or_else(|| v.get("paths").and_then(|d| d.as_array()))
        .or_else(|| v.get("documents").and_then(|d| d.as_array()))
        .or_else(|| v.as_array());
    match arr {
        Some(a) => a.iter().filter_map(|d| {
            d.get("path").and_then(|p| p.as_str())
                .or_else(|| d.get("rel").and_then(|p| p.as_str()))
                .map(String::from)
                .or_else(|| d.as_str().map(String::from))
        }).collect(),
        None => Vec::new(),
    }
}

/// 拉取文档列表，兼容不同版本/代理的响应形态：
/// JSON（对象或数组）、NDJSON、纯文本路径行。全部失败才报错并附可诊断信息。
async fn read_docs_list(resp: reqwest::Response) -> Result<Vec<String>, String> {
    let status = resp.status();
    let hdr = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let ct = hdr("content-type");
    let enc = hdr("content-encoding");
    let cl = hdr("content-length");
    let bytes = resp.bytes().await.map_err(|e| format!("读取列表响应失败（HTTP {status}）：{e}"))?;
    if bytes.is_empty() {
        return Err(format!("列表响应为空（HTTP {status}，content-type: {ct}）"));
    }
    // ① 标准 JSON
    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
        let docs = docs_from_value(&v);
        if !docs.is_empty() { return Ok(docs); }
    }
    // ② NDJSON / 纯路径行（兼容旧版或代理改造过的响应）
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&bytes).lines() {
        let t = line.trim().trim_end_matches(',');
        if t.is_empty() { continue; }
        if let Ok(v) = serde_json::from_str::<Value>(t) {
            if let Some(p) = v.get("path").and_then(|p| p.as_str()).or_else(|| v.as_str()) {
                out.push(p.to_string());
                continue;
            }
            let nested = docs_from_value(&v);
            if !nested.is_empty() { out.extend(nested); }
        } else if t.starts_with("Notes/") {
            out.push(t.trim_matches('"').to_string());
        }
    }
    if !out.is_empty() { return Ok(out); }
    let n = bytes.len().min(200);
    let snip = String::from_utf8_lossy(&bytes[..n]).replace('\n', " ");
    Err(format!(
        "列表响应无法识别（HTTP {status}，content-type: {ct}，encoding: {enc}，content-length: {cl}，实收 {} 字节）；开头：{snip}",
        bytes.len()
    ))
}

/// 登录远端并拉全量（幂等：内容相同跳过）：**结构优先、正文后台**。
///
/// body: `{ server, token }` 或 `{ server, user, pass }`（无 token 时本机侧登录）。
/// 同步阶段只做三件快事：本机侧登录、拉 `/docs` 列表、建好目录骨架。
/// 拿到这些立刻返回，前端可以马上进编辑区；正文在后台并发流式写入，
/// 通过 `/desktop/pull/status` 轮询进度。
async fn desktop_pull(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let server = req.get("server").and_then(|v| v.as_str()).unwrap_or("").trim_end_matches('/').to_string();
    let mut rtoken = req.get("token").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut user = req.get("user").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !server.starts_with("http") {
        return err(StatusCode::BAD_REQUEST, "服务器地址需以 http(s):// 开头").into_response();
    }
    // 已有任务在跑：回报当前进度，避免并发重复写文件
    {
        let j = st.pull.lock().unwrap();
        if j.active {
            let snap = pull_json(&j);
            let total = j.total;
            return Json(json!({"ok": true, "already": true, "server": j.server, "user": j.user, "total": total, "job": snap})).into_response();
        }
    }
    let root = { st.kernel.lock().unwrap().vault.root.clone() };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        // 带 UA/Accept：部分 WAF（宝塔等）会拦截无 UA 的请求并返回 HTML 挑战页
        .user_agent("ThirdC/0.1 (+https://nownexts.com/thirdc)")
        .build()
    {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    // ⓪ 没有 token 时用账号密码在本机侧登录（避免浏览器跨域）
    if rtoken.is_empty() {
        let pass = req.get("pass").and_then(|v| v.as_str()).unwrap_or("");
        if user.is_empty() || pass.is_empty() {
            return err(StatusCode::BAD_REQUEST, "需要账号密码或 token").into_response();
        }
        let resp = client.post(format!("{server}/auth/login"))
            .json(&json!({"username": user, "password": pass})).send().await;
        let j: Value = match resp {
            Ok(r) => match read_json(r, "登录").await {
                Ok(v) => v,
                Err(e) => return err(StatusCode::BAD_GATEWAY, e).into_response(),
            },
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("连接登录接口失败：{e}")).into_response(),
        };
        match j.get("token").and_then(|t| t.as_str()) {
            Some(t) if !t.is_empty() => rtoken = t.to_string(),
            _ => {
                let msg = j.get("error").and_then(|e| e.as_str()).unwrap_or("账号或密码错误");
                return err(StatusCode::UNAUTHORIZED, format!("登录失败：{msg}")).into_response();
            }
        }
    }
    // ① 拉文档列表（结构）
    // 用 x-thirdc-token 而非 Authorization: Bearer——部分宝塔/WAF 会拦截带 Bearer 的请求
    // 并返回 200 HTML 挑战页（表现为「解析列表失败」）。服务端两种头都认。
    let list = match client.get(format!("{server}/docs"))
        .header(reqwest::header::ACCEPT, "application/json")
        .header("x-thirdc-token", &rtoken).send().await
    {
        Ok(r) => match r.error_for_status() { Ok(r) => r, Err(e) => return err(StatusCode::BAD_GATEWAY, format!("远端拒绝：{e}")).into_response() },
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("连接失败：{e}")).into_response(),
    };
    let docs: Vec<String> = match read_docs_list(list).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e).into_response(),
    };
    // 只有 Notes/ 下的文档会被拉取，进度分母与之一致
    let total = docs.iter().filter(|p| p.starts_with("Notes/")).count();
    // ② 结构落地：先把目录骨架建好，前端拿到就能渲染整棵树
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in &docs {
        if let Some(parent) = std::path::Path::new(p).parent() {
            let key = parent.to_string_lossy().into_owned();
            if !parent.as_os_str().is_empty() && seen.insert(key) {
                let _ = std::fs::create_dir_all(root.join(parent));
            }
        }
    }
    let dirs = seen.len();
    // 记住账号/服务器，重开可续、设置面板可显示
    let _ = std::fs::create_dir_all(root.join(".thirdc"));
    if user.is_empty() { user = "token".into(); }
    let _ = std::fs::write(
        root.join(".thirdc/remote.json"),
        serde_json::to_vec_pretty(&json!({"server": server, "user": user})).unwrap_or_default(),
    );
    // ③ 初始化任务并后台流式拉正文
    {
        let mut j = st.pull.lock().unwrap();
        *j = PullJob {
            active: true,
            phase: "content".into(),
            server: server.clone(),
            user: user.clone(),
            total,
            dirs,
            started: now_secs(),
            ..Default::default()
        };
    }
    let st2 = st.clone();
    let server2 = server.clone();
    let rtoken2 = rtoken.clone();
    let root2 = root.clone();
    tokio::spawn(async move { run_pull(st2, root2, client, server2, rtoken2, docs).await; });

    let snap = pull_json(&st.pull.lock().unwrap());
    Json(json!({
        "ok": true, "started": true,
        "server": server, "user": user, "total": total, "folders": dirs,
        "job": snap,
    })).into_response()
}

/// 查询后台拉取进度。
async fn desktop_pull_status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let j = st.pull.lock().unwrap();
    Json(pull_json(&j)).into_response()
}

/// 后台拉取正文：浅层优先、并发 8、单个失败重试一次，每 300 篇合入一次索引。
async fn run_pull(
    st: Arc<AppState>,
    root: std::path::PathBuf,
    client: reqwest::Client,
    server: String,
    rtoken: String,
    docs: Vec<String>,
) {
    // 只处理 Notes/ 下的文档；浅层（根目录）优先，让编辑区先有内容
    let mut ordered: Vec<String> = docs.into_iter().filter(|p| p.starts_with("Notes/")).collect();
    ordered.sort_by(|a, b| {
        a.matches('/').count().cmp(&b.matches('/').count()).then_with(|| a.cmp(b))
    });

    let mut stream = futures_util::stream::iter(ordered.into_iter().map(|path| {
        let client = client.clone();
        let server = server.clone();
        let rtoken = rtoken.clone();
        let root = root.clone();
        async move { pull_one(&client, &server, &rtoken, &root, &path).await }
    }))
    .buffer_unordered(8);

    let mut since_sync = 0usize;
    while let Some((code, bytes)) = stream.next().await {
        {
            let mut j = st.pull.lock().unwrap();
            j.done += 1;
            match code {
                1 => { j.pulled += 1; j.bytes += bytes; }
                2 => j.failed += 1,
                _ => j.skipped += 1,
            }
        }
        since_sync += 1;
        if since_sync >= 300 {
            since_sync = 0;
            sync_in_background(&st).await;
        }
    }
    sync_in_background(&st).await;

    let mut j = st.pull.lock().unwrap();
    j.active = false;
    j.phase = if j.pulled == 0 && j.failed > 0 { "error".into() } else { "done".into() };
    j.finished = now_secs();
    if j.error.is_empty() && j.failed > 0 {
        j.error = format!("{} 篇未能拉取（可点同步重试）", j.failed);
    }
    let snap = pull_json(&j);
    drop(j);
    audit_log(&root.join(".thirdc"), "desktop-pull", &snap);
}

/// 单篇拉取：0=跳过(相同/非 Notes)，1=已写入，2=失败。失败重试一次。
async fn pull_one(
    client: &reqwest::Client,
    server: &str,
    rtoken: &str,
    root: &std::path::Path,
    path: &str,
) -> (u8, u64) {
    for attempt in 0..2u8 {
        let resp = client.get(format!("{server}/doc"))
            .query(&[("path", path)])
            .header("x-thirdc-token", rtoken).send().await;
        if let Ok(resp) = resp {
            if let Ok(j) = read_json(resp, path).await {
                if let Some(src) = j.get("source").and_then(|s| s.as_str()) {
                    let dest = root.join(path);
                    if let Ok(existing) = std::fs::read_to_string(&dest) {
                        if existing == src { return (0, 0); }
                    }
                    if let Some(parent) = dest.parent() { let _ = std::fs::create_dir_all(parent); }
                    if std::fs::write(&dest, src).is_ok() {
                        return (1, src.len() as u64);
                    }
                }
            }
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    (2, 0)
}

/// 后台把磁盘上的外部改动合入内核索引（不阻塞 async runtime）。
async fn sync_in_background(st: &Arc<AppState>) {
    let k = st.kernel.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut g) = k.lock() {
            let _ = g.sync_all();
        }
    }).await;
}

#[cfg(test)]
mod tidy_share_tests {
    use super::*;

    #[test]
    fn placeholder_names_and_safe_stems() {
        for p in ["未命名", "未命名-2", "Untitled 3", "note-1728000000", "2026-10-14", "新文档", "doc-1a2b3c4d"] {
            assert!(is_placeholder_name(p), "{p}");
        }
        for p in ["GEO 渠道稿件", "周报", "geo-渠道稿件-调试稿件"] {
            assert!(!is_placeholder_name(p), "{p}");
        }
        assert_eq!(safe_file_stem("A/B: 方案?  v2 "), "A-B- 方案- v2");
        assert_eq!(safe_file_stem("GEO 渠道稿件 调试稿件"), "GEO 渠道稿件 调试稿件");
    }

    #[test]
    fn h1_and_frontmatter_title() {
        assert_eq!(doc_h1("# 标题\n\n正文").as_deref(), Some("标题"));
        assert_eq!(doc_h1("---\ntitle: 前言标题\n---\n# 别的\n").as_deref(), Some("前言标题"));
        assert_eq!(doc_h1("---\ntags: a\n---\n\n# 正文标题\n").as_deref(), Some("正文标题"));
        assert_eq!(doc_h1("没有标题\n# 第二行不算"), None);
    }

    #[test]
    fn ai_output_must_keep_the_words() {
        let a = "一、背景\n随着生成式搜索的普及 1. 验证 2. 测试 3. 观察";
        let ok = "## 一、背景\n\n随着生成式搜索的普及\n\n1. 验证\n2. 测试\n3. 观察\n";
        assert!(content_preserved(a, ok));
        let rewritten = "## 背景\n\n生成式搜索越来越流行。\n\n- 验证\n- 测试\n";
        assert!(!content_preserved(a, rewritten));
    }

    #[test]
    fn share_target_prefers_default_public_target() {
        let mut cfg = kernel_core::PublishConfig::default();
        assert!(share_target(&cfg).is_none(), "无目标 → 只能本机");
        cfg.targets.push(kernel_core::PublishTarget { name: "local".into(), kind: "local".into(), ..Default::default() });
        assert!(share_target(&cfg).is_none(), "local 目标不产生公网链接");
        cfg.targets.push(kernel_core::PublishTarget {
            name: "oss".into(),
            kind: "s3".into(),
            public_base_url: Some("https://cdn.example.com".into()),
            ..Default::default()
        });
        let (t, base) = share_target(&cfg).unwrap();
        assert_eq!(t.name, "oss");
        assert_eq!(base.as_deref(), Some("https://cdn.example.com"));
    }

    #[test]
    fn vault_images_are_inlined() {
        let dir = std::env::temp_dir().join(format!("thirdc-inline-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Assets")).unwrap();
        std::fs::write(dir.join("Assets/a.png"), [137u8, 80, 78, 71]).unwrap();
        let h = inline_vault_assets(r#"<img src="Assets/a.png"><img src="https://x.com/b.png"><img src="/asset-file?path=Assets/a.png">"#, &dir);
        assert_eq!(h.matches("data:image/png;base64,iVBORw==").count(), 2, "{h}");
        assert!(h.contains("https://x.com/b.png"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
