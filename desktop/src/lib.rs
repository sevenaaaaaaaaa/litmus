#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! ThirdC 桌面壳：内嵌内核 + 本地 daemon + webview。
//!
//! 启动顺序：后台线程开内核并在 127.0.0.1 随机端口起 daemon → 把 (端口, token)
//! 回传主线程 → 主线程建窗口指向该端口。内核起不来时不静默崩溃，改为在窗口里
//! 显示一页可读的错误（库路径 + 原因 + 下一步怎么办）。

use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// 进程内内核状态句柄：IPC 桥（desktop_kernel_api）与全局快捷键快速捕捉共用。
/// serve 只提供 HTTP 通道；这里让 webview 在 daemon 不可达时仍能直达内核（脱机可用）。
static KERNEL_STATE: std::sync::OnceLock<std::sync::Arc<thirdc_server::AppState>> = std::sync::OnceLock::new();

/// ego-lite 内置渲染槽：隐藏 webview 抓取 JS 渲染后的页面 DOM。
#[derive(Default)]
struct RenderSlot(StdMutex<Option<String>>);

/// 多库注册表：独立开发、统一管理。库本身永远是普通目录（文件真相不受注册表影响）。
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct VaultEntry {
    name: String,
    path: String,
    origin: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
struct VaultRegistry {
    #[serde(default)]
    vaults: Vec<VaultEntry>,
    #[serde(default)]
    current: Option<VaultEntry>,
}

fn registry_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("配置目录不可用：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    Ok(dir.join("vaults.json"))
}

fn prefs_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("配置目录不可用：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    Ok(dir.join("prefs.json"))
}

/// 用户偏好（localStorage 的 thirdc_* 键）：跨库（跨 origin）持久化。
/// 每个 origin 的内核 token 不同，thirdc_token 排除——页面首帧从 URL 写入正确值。
#[tauri::command]
fn desktop_prefs_save(app: AppHandle, prefs: serde_json::Value) -> Result<(), String> {
    if !prefs.is_object() {
        return Err("prefs 必须是对象".into());
    }
    let p = prefs_path(&app)?;
    std::fs::write(&p, serde_json::to_string_pretty(&prefs).map_err(|e| e.to_string())?)
        .map_err(|e| format!("写入偏好失败：{e}"))
}

fn load_registry(app: &AppHandle) -> VaultRegistry {
    registry_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_registry(app: &AppHandle, reg: &VaultRegistry) -> Result<(), String> {
    let p = registry_path(app)?;
    std::fs::write(&p, serde_json::to_string_pretty(reg).map_err(|e| e.to_string())?)
        .map_err(|e| format!("写入注册表失败：{e}"))
}

fn vault_name_for(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Litmus".into())
}

fn sanitize_vault_name(name: &str) -> Result<String, String> {
    let t: String = name
        .trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || (*c as u32) > 0x2E7F)
        .collect();
    if t.is_empty() {
        return Err("库名不能为空（仅保留文字/数字/连字符）".into());
    }
    Ok(t.chars().take(40).collect())
}

/// 在新线程里启动新库的内核，注册表登记为当前库，并把主窗口导航过去。
/// 旧库的内核继续在后台监听（无句柄可停，资源占用小、互不干扰）——
/// 新实例优先绑 7717 失败会自动换端口，不会冲突。
fn switch_and_navigate(app: &AppHandle, path: PathBuf, name: String) -> Result<String, String> {
    let (port, token) = boot_kernel(path.clone())?;
    wait_ready(port);
    let origin = format!("http://127.0.0.1:{port}");
    let url = format!("{origin}/?token={token}");
    let mut reg = load_registry(app);
    let entry = VaultEntry { name, path: path.display().to_string(), origin: origin.clone() };
    reg.vaults.retain(|v| v.path != entry.path);
    reg.vaults.insert(0, entry.clone());
    reg.current = Some(entry);
    save_registry(app, &reg)?;
    if let Some(w) = app.get_webview_window("main") {
        w.navigate(url.parse().map_err(|e| format!("地址解析失败：{e}"))?)
            .map_err(|e| format!("窗口导航失败：{e}"))?;
    }
    Ok(url)
}

#[tauri::command]
fn ego_render_result(state: tauri::State<RenderSlot>, html: String) -> Result<(), String> {
    let mut slot = state.0.lock().map_err(|_| "渲染槽锁异常".to_string())?;
    if slot.is_none() {
        *slot = Some(html);
    }
    Ok(())
}

/// 内置 ego-lite：隐藏 webview 打开页面（跑完 JS），抓取渲染后的完整 DOM。
/// 页面 load 完成 + 1.8s 缓冲后自动回传；总超时 40s。
#[tauri::command]
async fn desktop_render_page(
    app: AppHandle,
    state: tauri::State<'_, RenderSlot>,
    url: String,
) -> Result<String, String> {
    {
        let mut slot = state.0.lock().map_err(|_| "渲染槽锁异常".to_string())?;
        *slot = None;
    }
    let (tx, _rx) = std::sync::mpsc::channel::<tauri::WebviewWindow>();
    let _ = tx;
    let label = format!("ego-render-{}", std::process::id());
    let label_for_close = label.clone();
    let label_inner = label.clone();
    let parsed: tauri::Url = url.parse().map_err(|e| format!("URL 解析失败：{e}"))?;
    let app2 = app.clone();
    app.run_on_main_thread(move || {
        let builder = WebviewWindowBuilder::new(
            &app2,
            &label_inner,
            WebviewUrl::External(parsed.clone()),
        )
        .title("ego render")
        .visible(false)
        .inner_size(1280.0, 900.0);
        let _ = builder
            .on_page_load(move |w, _payload| {
                if matches!(_payload.event(), tauri::webview::PageLoadEvent::Finished) {
                    let _ = w.eval(
                        "setTimeout(function(){try{window.__TAURI_INTERNALS__.invoke('ego_render_result',{html:document.documentElement.outerHTML})}catch(e){}},1800)",
                    );
                }
            })
            .build();
    })
    .map_err(|e| format!("主线程调度失败：{e}"))?;

    // 轮询渲染槽；拿到结果即关闭渲染窗口
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let got = {
            let mut slot = state.0.lock().map_err(|_| "渲染槽锁异常".to_string())?;
            slot.take()
        };
        if let Some(html) = got {
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.close();
            }
            if html.len() < 200 {
                return Err("页面渲染结果过短（可能是空页或被拦截）".into());
            }
            return Ok(html);
        }
    }
    if let Some(w) = app.get_webview_window(&label_for_close) {
        let _ = w.close();
    }
    Err("渲染超时（40s）".into())
}

#[tauri::command]
fn desktop_vaults(app: AppHandle) -> Result<serde_json::Value, String> {
    let reg = load_registry(&app);
    Ok(serde_json::json!({ "vaults": reg.vaults, "current": reg.current }))
}

#[tauri::command]
fn desktop_create_vault(app: AppHandle, name: String) -> Result<String, String> {
    let name = sanitize_vault_name(&name)?;
    let dir = default_vault()
        .parent()
        .unwrap_or(Path::new("."))
        .join(&name);
    if dir.exists() && !dir.join("thirdc.toml").is_file() {
        return Err(format!("目录已存在且不是 ThirdC 库：{}", dir.display()));
    }
    switch_and_navigate(&app, dir, name)
}

#[tauri::command]
fn desktop_switch_vault(app: AppHandle, path: String) -> Result<String, String> {
    let p = PathBuf::from(path.trim());
    if !p.is_dir() {
        return Err(format!("目录不存在：{}", p.display()));
    }
    let name = vault_name_for(&p);
    switch_and_navigate(&app, p, name)
}

/// 默认库位置：~/Documents/Litmus（旧安装沿用已注册的库，不受影响），THIRDC_VAULT 可覆盖。
fn default_vault() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Documents").join("Litmus")
}

fn open_or_init(path: &Path) -> anyhow::Result<kernel_core::Vault> {
    if path.join("thirdc.toml").is_file() {
        return Ok(kernel_core::Vault::open(path)?);
    }
    std::fs::create_dir_all(path)?;
    match kernel_core::Vault::init(path, "Litmus") {
        Ok(v) => Ok(v),
        // 目录已被别的实例初始化过（竞态）：回退到 open。
        Err(_) => Ok(kernel_core::Vault::open(path)?),
    }
}

/// 后台线程里跑内核；成功回传 (端口, token)，失败回传可读原因。
fn boot_kernel(vault_path: PathBuf) -> Result<(u16, String), String> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<(u16, String), String>>();
    std::thread::Builder::new()
        .name("thirdc-kernel".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("创建 tokio 运行时失败：{e}")));
                    return;
                }
            };
            rt.block_on(async move {
                let started = async {
                    let vault = open_or_init(&vault_path).map_err(|e| format!("打开库失败：{e}"))?;
                    let state = thirdc_server::build_state(vault)
                        .map_err(|e| format!("构建内核状态失败：{e}"))?;
                    // 优先用固定端口：origin 稳定，localStorage（账号/主题/引导）才能跨启动保留。
                    // 端口被占（例如已开另一个实例）再退回随机端口。
                    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 7717)).await {
                        Ok(l) => l,
                        Err(_) => tokio::net::TcpListener::bind("127.0.0.1:0")
                            .await
                            .map_err(|e| format!("绑定本地端口失败：{e}"))?,
                    };
                    let port = listener
                        .local_addr()
                        .map_err(|e| format!("读取端口失败：{e}"))?
                        .port();
                    Ok::<_, String>((listener, state, port))
                }
                .await;

                match started {
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                    Ok((listener, state, port)) => {
                        let _ = KERNEL_STATE.set(state.clone());
                        let _ = thirdc_server::spawn_watcher(state.clone());
                        let _ = tx.send(Ok((port, state.token.clone())));
                        let _ = thirdc_server::serve_listener(listener, state).await;
                    }
                }
            });
        })
        .map_err(|e| format!("启动内核线程失败：{e}"))?;

    // 线程 panic 时 sender 被丢弃，recv 返回 Err——不会永久挂起。
    rx.recv().map_err(|_| "内核线程意外退出".to_string())?
}

/// 端口就绪探测：daemon 通常毫秒级起来，最多等 2s。
fn wait_ready(port: u16) {
    for _ in 0..80 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 内核起不来时的兜底页面：写到临时文件，用 file:// 打开。
fn error_page(vault: &Path, reason: &str) -> Option<String> {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let html = format!(
        r#"<!doctype html><meta charset="utf-8"><title>鹿蕊 Litmus</title>
<style>
:root{{color-scheme:light dark;--bg:oklch(97.4% .012 85);--fg:oklch(24% .022 70);--muted:oklch(47% .018 72);
  --card:oklch(100% 0 0/.8);--border:oklch(88% .012 82);--danger:oklch(55% .2 25)}}
@media (prefers-color-scheme:dark){{:root{{--bg:oklch(26% .012 264);--fg:oklch(94% .008 85);--muted:oklch(72% .012 85);
  --card:oklch(31% .014 264);--border:oklch(38% .014 264);--danger:oklch(68% .18 25)}}}}
body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);
  font:15px/1.7 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif}}
.card{{max-width:540px;padding:32px 34px;background:var(--card);border:1px solid var(--border);border-radius:18px}}
h1{{margin:0 0 6px;font-size:19px}} p{{margin:0 0 14px;color:var(--muted)}}
code{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px;background:oklch(50% 0 0/.1);
  padding:2px 6px;border-radius:6px;word-break:break-all}}
pre{{margin:0 0 18px;padding:12px 14px;border-radius:12px;border:1px solid var(--border);
  color:var(--danger);white-space:pre-wrap;font-size:13px}}
ol{{margin:0;padding-left:20px;color:var(--muted)}} li{{margin:6px 0}}
</style>
<div class="card">
<h1>内核没能启动</h1>
<p>库位置：<code>{vault}</code></p>
<pre>{reason}</pre>
<ol>
<li>确认该目录存在且当前用户可读写。</li>
<li>换一个库：设 <code>THIRDC_VAULT=/你的/库路径</code> 后重开。</li>
<li>端口被占时会自动换端口——通常无需处理。</li>
<li>仍失败：命令行跑 <code>thirdc serve 该路径</code>，终端里能看到完整错误。</li>
</ol>
</div>"#,
        vault = esc(&vault.display().to_string()),
        reason = esc(reason),
    );
    let path = std::env::temp_dir().join("thirdc-desktop-error.html");
    std::fs::write(&path, html).ok()?;
    tauri::Url::from_file_path(&path).ok().map(|u| u.to_string())
}

// ── 菜单栏图标（tray）：呼出列表 / 新建 / 常用（最近文档）/ 便利贴 / 凭证库 ──

/// tray 与便利贴窗口需要的内核上下文。
#[cfg(desktop)]
struct TrayCtx {
    origin: String,
    token: String,
    vault: PathBuf,
    /// 菜单「最近文档」idx → 路径（菜单事件里反查）。
    recents: StdMutex<Vec<String>>,
}

#[cfg(desktop)]
fn ui_state_recents(vault: &Path) -> Vec<String> {
    std::fs::read_to_string(vault.join(".thirdc").join("ui-state.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("recentDocs")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        })
        .unwrap_or_default()
}

#[cfg(desktop)]
fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 向主窗口页面派发事件（shell 监听 CustomEvent 完成动作）。
#[cfg(desktop)]
fn dispatch_main(app: &AppHandle, name: &str, detail: &str) {
    if let Some(w) = app.get_webview_window("main") {
        let js = format!(
            "window.dispatchEvent(new CustomEvent('{name}',{detail}))"
        );
        let _ = w.eval(&js);
    }
}

#[cfg(desktop)]
fn open_sticky(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("sticky") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let ctx = app.state::<TrayCtx>();
    let url = format!("{}/sticky.html?token={}", ctx.origin, ctx.token);
    let parsed: tauri::Url = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[tray] 便利贴地址解析失败：{e}");
            return;
        }
    };
    if let Err(e) = WebviewWindowBuilder::new(app, "sticky", WebviewUrl::External(parsed))
        .title("便利贴 · 鹿蕊 Litmus")
        .inner_size(300.0, 380.0)
        .min_inner_size(220.0, 240.0)
        .always_on_top(true)
        .skip_taskbar(true)
        /* 关闭 Tauri 的系统文件拖放拦截：不拦截才能让页内 HTML5 拖拽
           （侧栏拖文档进文件夹、看板拖卡片）在 WKWebView 里正常触发 */
        .disable_drag_drop_handler()
        .build()
    {
        eprintln!("[tray] 便利贴窗口创建失败：{e}");
    }
}

/// 重建 tray 菜单（最近文档随 ui-state.json 实时变化，点击图标时刷新一次）。
#[cfg(desktop)]
fn build_tray_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{MenuBuilder, MenuItem, SubmenuBuilder};
    let recents = {
        let ctx = app.state::<TrayCtx>();
        let mut r = ctx
            .recents
            .lock()
            .map_err(|_| tauri::Error::WindowNotFound)?;
        let fresh = ui_state_recents(&ctx.vault);
        *r = fresh.clone();
        fresh
    };
    let show = MenuItem::with_id(app, "t-show", "显示主窗口", true, None::<&str>)?;
    let capture = MenuItem::with_id(app, "t-capture", "⚡ 捕捉剪贴板到库（Alt+Space）", true, None::<&str>)?;
    let new_doc = MenuItem::with_id(app, "t-new", "新建文档", true, None::<&str>)?;
    let sticky = MenuItem::with_id(app, "t-sticky", "新建便利贴", true, None::<&str>)?;
    let secrets = MenuItem::with_id(app, "t-secrets", "凭证库", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "t-quit", "退出 Litmus", true, None::<&str>)?;
    let mut sub = SubmenuBuilder::new(app, "常用 · 最近文档");
    if recents.is_empty() {
        sub = sub.item(&MenuItem::with_id(
            app,
            "t-none",
            "暂无最近文档",
            false,
            None::<&str>,
        )?);
    } else {
        for (i, p) in recents.iter().take(8).enumerate() {
            let label = p.rsplit('/').next().unwrap_or(p).to_string();
            sub = sub.item(&MenuItem::with_id(
                app,
                &format!("t-doc:{i}"),
                &label,
                true,
                Some(p.as_str()),
            )?);
        }
    }
    let recent_sub = sub.build()?;
    MenuBuilder::new(app)
        .item(&show)
        .item(&capture)
        .separator()
        .item(&new_doc)
        .item(&sticky)
        .item(&recent_sub)
        .item(&secrets)
        .separator()
        .item(&quit)
        .build()
}

/// 建菜单栏图标（仅桌面）。菜单事件全在这里分发。
#[cfg(desktop)]
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::tray::{TrayIconBuilder, TrayIconEvent};
    let menu = build_tray_menu(app.handle())?;
    let mut tray = TrayIconBuilder::with_id("litmus-tray")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("鹿蕊 Litmus")
        .on_menu_event(|app, event| {
            let id = event.id().0.clone();
            match id.as_str() {
                "t-show" => show_main(app),
                "t-capture" => quick_capture(app),
                "t-new" => {
                    show_main(app);
                    dispatch_main(app, "tray-new-doc", "{}");
                }
                "t-sticky" => open_sticky(app),
                "t-secrets" => {
                    show_main(app);
                    dispatch_main(app, "tray-secrets", "{}");
                }
                "t-quit" => app.exit(0),
                other => {
                    if let Some(idx) = other.strip_prefix("t-doc:") {
                        let path = {
                            let ctx = app.state::<TrayCtx>();
                            ctx.recents
                                .lock()
                                .ok()
                                .and_then(|r| r.get(idx.parse::<usize>().unwrap_or(99)).cloned())
                        };
                        if let Some(p) = path {
                            let detail = serde_json::to_string(&serde_json::json!({ "path": p }))
                                .unwrap_or_else(|_| "{}".into());
                            show_main(app);
                            dispatch_main(app, "tray-open-doc", &detail);
                        }
                    }
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            // 左键抬起时按 ui-state 现值重建菜单（macOS 点开即所见；其余平台同样受益）
            if let TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                if let Ok(m) = build_tray_menu(tray.app_handle()) {
                    let _ = tray.set_menu(Some(m));
                }
            }
        })
        .icon(app.default_window_icon().expect("bundle icon").clone())
        .build(app)?;
    let _ = &mut tray; // 句柄由 app 托管
    Ok(())
}

#[cfg(desktop)]
#[tauri::command]
fn desktop_focus_main(app: AppHandle) {
    show_main(&app);
}

/// 当前 App 版本（来自 tauri.conf.json / package_info），供「关于」页显示。
#[tauri::command]
fn desktop_app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// 检查在线更新：命中则回传新版本号与说明，否则 available=false。
/// 更新源（R2 / GitHub Releases 的 latest.json）与公钥见 tauri.conf.json 的 plugins.updater。
#[cfg(desktop)]
#[tauri::command]
async fn desktop_check_update(app: AppHandle) -> Result<serde_json::Value, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app
        .updater()
        .map_err(|e| format!("更新器初始化失败：{e}"))?;
    match updater.check().await {
        Ok(Some(u)) => Ok(serde_json::json!({
            "available": true,
            "version": u.version,
            "current": u.current_version,
            "notes": u.body,
        })),
        Ok(None) => Ok(serde_json::json!({ "available": false, "current": app.package_info().version.to_string() })),
        Err(e) => Err(format!("检查更新失败：{e}")),
    }
}

/// 下载并安装更新，装完自动重启。进度经事件 `update-progress` 回传前端。
#[cfg(desktop)]
#[tauri::command]
async fn desktop_install_update(app: AppHandle) -> Result<String, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app
        .updater()
        .map_err(|e| format!("更新器初始化失败：{e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("检查更新失败：{e}"))?
        .ok_or_else(|| "已是最新版本".to_string())?;
    let handle = app.clone();
    update
        .download_and_install(
            move |downloaded, total| {
                let _ = handle.emit(
                    "update-progress",
                    serde_json::json!({ "downloaded": downloaded, "total": total }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| format!("下载或安装失败：{e}"))?;
    // 安装完成——重启进程加载新版本（此调用不返回）。
    app.restart();
}

/// 系统原生文件夹选择对话框（桌面端「原生开发占比」第一步）。
/// 返回绝对路径或 None（用户取消）；由前端换算成库内相对路径。
#[cfg(desktop)]
#[tauri::command]
async fn desktop_pick_folder(app: AppHandle, title: String) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<String>>();
    app.dialog()
        .file()
        .set_title(&title)
        .pick_folder(move |f| {
            let _ = tx.send(f.map(|p| p.to_string()));
        });
    rx.await.map_err(|e| e.to_string())
}

/// 内核 IPC 桥：webview 经 Tauri invoke 直达进程内 axum router（不经 TCP socket）。
/// 前端 fetch 失败时自动回退到这里——daemon 挂掉/端口被占，桌面端照样能读写文件。
#[cfg(desktop)]
#[tauri::command]
async fn desktop_kernel_api(
    method: String,
    uri: String,
    body: Option<String>,
    token: String,
) -> Result<serde_json::Value, String> {
    use tower::util::ServiceExt;
    let state = KERNEL_STATE
        .get()
        .ok_or_else(|| "内核未就绪".to_string())?
        .clone();
    let app = thirdc_server::router(state);
    let m: axum::http::Method = method.parse().map_err(|e| format!("method 非法：{e}"))?;
    let u: axum::http::Uri = uri.parse().map_err(|e| format!("uri 非法：{e}"))?;
    let mut builder = axum::http::Request::builder()
        .method(m)
        .uri(u)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder
        .body(axum::body::Body::from(body.unwrap_or_default()))
        .map_err(|e| e.to_string())?;
    let res = app.oneshot(req).await.map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 32 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "status": status,
        "text": String::from_utf8_lossy(&bytes),
    }))
}

/// 全局快捷键 3 秒落库：剪贴板文本 → Notes/Inbox/捕捉-<时间戳>.md。
/// 复制 → Alt+Space → 进库，全程不碰窗口（PRODUCT.md「三秒落库」的热键入口）。
#[cfg(desktop)]
fn quick_capture(app: &AppHandle) {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let text = match app.clipboard().read_text() {
        Ok(t) => t,
        Err(_) => return, // 剪贴板是图片/文件等非文本，忽略
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some(state) = KERNEL_STATE.get() else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let path = format!("Notes/Inbox/捕捉-{}.md", now);
    let secs = now / 1000;
    let tod = secs % 86400;
    let content = format!(
        "# 捕捉 {:02}:{:02} UTC\n\n{}\n\n---\n\n> 来源：全局快捷键（Alt+Space）· 复制即落库\n",
        tod / 3600,
        (tod % 3600) / 60,
        text
    );
    let mut k = state.kernel.lock().unwrap();
    let inbox = k.vault.root.join("Notes/Inbox");
    let _ = std::fs::create_dir_all(&inbox);
    match k.put_doc(&path, &content) {
        Ok(()) => {
            let _ = app.emit("quick-captured", serde_json::json!({ "path": path }));
        }
        Err(e) => eprintln!("[capture] 快速捕捉失败：{e}"),
    }
}

/// 在系统文件管理器中显示（macOS 访达 / Windows 资源管理器 / Linux 文件管理器）。
/// 「文件即真相」的原生出口：库里任何文件都要能跳回它的物理位置。
#[tauri::command]
fn desktop_reveal_path(p: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&p)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(format!("/select,{p}"))
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        let dir = std::path::Path::new(&p)
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .to_path_buf();
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 用系统默认应用打开文件（PDF/图片/Office 文档交给系统，不塞 webview）。
#[tauri::command]
fn desktop_open_path(p: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&p)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", &p])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&p)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 应用入口：桌面由 src/main.rs 调用；移动端由 tauri mobile_entry_point 调用。
/// 内核启动搬进 setup：移动端默认库要等 app 数据目录就绪后再解析。
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 告诉内核：这是 App 壳，开启 /desktop/* 独占端点。
    std::env::set_var("THIRDC_DESKTOP", "1");
    let vault_override = std::env::var_os("THIRDC_VAULT").map(PathBuf::from);

    let builder = tauri::Builder::default().manage(RenderSlot(StdMutex::new(None)));

    // 在线更新器仅桌面端可用（移动端商店更新，不走自更新）。
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        quick_capture(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            desktop_vaults,
            desktop_create_vault,
            desktop_switch_vault,
            desktop_render_page,
            ego_render_result,
            desktop_prefs_save,
            desktop_focus_main,
            desktop_app_version,
            desktop_check_update,
            desktop_install_update,
            desktop_pick_folder,
            desktop_reveal_path,
            desktop_open_path,
            desktop_kernel_api
        ]);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        desktop_vaults,
        desktop_create_vault,
        desktop_switch_vault,
        desktop_render_page,
        ego_render_result,
        desktop_prefs_save,
        desktop_app_version
    ]);

    builder
        .setup(move |app| {
            let vault_path = vault_override.unwrap_or_else(|| {
                #[cfg(mobile)]
                {
                    app.path()
                        .app_data_dir()
                        .map(|d| d.join("vault"))
                        .expect("app 数据目录不可用")
                }
                #[cfg(desktop)]
                {
                    // 恢复上次使用的库（注册表）；首次启动回落到默认库。
                    let reg = load_registry(app.handle());
                    reg.current
                        .map(|c| PathBuf::from(c.path))
                        .filter(|p| p.is_dir())
                        .unwrap_or_else(default_vault)
                }
            });
            let url = match boot_kernel(vault_path.clone()) {
        Ok((port, token)) => {
            wait_ready(port);
            /* 全局快捷键：Alt+Space 剪贴板 → 3 秒落库（内核就绪后注册，失败不阻塞启动） */
            #[cfg(desktop)]
            {
                use tauri_plugin_global_shortcut::GlobalShortcutExt;
                if let Err(e) = app.handle().global_shortcut().register("Alt+Space") {
                    eprintln!("[shortcut] 全局快捷键注册失败（可能被其他应用占用）：{e}");
                }
            }
            // token 走 query：前端首帧就存进 localStorage，省掉桌面端再登录一次。
            format!("http://127.0.0.1:{port}/?token={token}")
        }
            Err(reason) => {
                eprintln!("[thirdc-desktop] {reason}");
                match error_page(&vault_path, &reason) {
                    Some(u) => u,
                    None => {
                        eprintln!("[thirdc-desktop] 兜底页面也写不出来");
                        std::process::exit(1);
                    }
                }
            }
        };

            #[allow(unused_mut)]
            let mut builder =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
                    .title("ThirdC Studio")
                    .resizable(true)
                    /* 同 sticky：关掉系统拖放拦截，还网页原生 HTML5 拖拽 */
                    .disable_drag_drop_handler();

            /* 偏好初始化脚本：每次页面加载（含换库导航）把 prefs.json 灌进 localStorage，
               主题/昵称/树状态等跨库保留。token 除外——每个 origin 的内核 token 不同，
               页面首帧会从 URL 写入正确值。 */
            #[cfg(desktop)]
            {
                let prefs_txt = prefs_path(app.handle())
                    .ok()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .filter(|s| serde_json::from_str::<serde_json::Value>(s).map(|v| v.is_object()).unwrap_or(false))
                    .unwrap_or_else(|| "{}".into());
                let init = format!(
                    r#"try{{var __prefs={prefs};Object.keys(__prefs).forEach(function(k){{if(k==='thirdc_token')return;try{{localStorage.setItem(k,__prefs[k])}}catch(e){{}}}})}}catch(e){{}}"#,
                    prefs = prefs_txt
                );
                builder = builder.initialization_script(&init);
            }

            /* 桌面专属的窗口几何/装饰：移动端 API 不存在 */
            #[cfg(desktop)]
            {
                builder = builder
                    .inner_size(1440.0, 900.0)
                    .min_inner_size(960.0, 640.0)
                    .decorations(true);
            }

            #[cfg(target_os = "macos")]
            {
                builder = builder
                    .hidden_title(true)
                    .title_bar_style(tauri::TitleBarStyle::Overlay);
            }

            builder.build()?;

            /* 菜单栏图标：内核起成功才有 origin/token——兜底错误页时不建 tray。 */
            #[cfg(desktop)]
            if url.starts_with("http") {
                let origin = url.split("/?").next().unwrap_or("").to_string();
                let token = url.split("token=").nth(1).unwrap_or("").to_string();
                app.manage(TrayCtx {
                    origin,
                    token,
                    vault: vault_path.clone(),
                    recents: StdMutex::new(ui_state_recents(&vault_path)),
                });
                if let Err(e) = setup_tray(app) {
                    eprintln!("[tray] 菜单栏图标初始化失败：{e}");
                }
            }
            Ok(())
        })
        // 菜单栏应用语义：主窗口点关闭 → 隐藏到菜单栏（tray「退出」才真正退出）；
        // 主窗口被真正销毁时退出进程。macOS Dock 点击（Reopen）恢复主窗口。
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                tauri::WindowEvent::Destroyed if window.label() == "main" => {
                    window.app_handle().exit(0);
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("ThirdC Desktop 启动失败")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                show_main(app);
            }
            let _ = (app, &event);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_page_escapes_and_writes_file() {
        let url = error_page(Path::new("/tmp/库 <a>"), "打不开：<script>x</script>")
            .expect("应写出兜底页面");
        assert!(url.starts_with("file://"), "应是 file:// URL，实际 {url}");
        let path = std::env::temp_dir().join("thirdc-desktop-error.html");
        let html = std::fs::read_to_string(path).unwrap();
        assert!(html.contains("&lt;script&gt;"), "原因需转义");
        assert!(!html.contains("<script>"), "不得注入可执行标签");
        assert!(html.contains("/tmp/库 &lt;a&gt;"), "库路径需转义后出现");
    }

    #[test]
    fn open_or_init_creates_then_reopens() {
        let dir = std::env::temp_dir().join(format!("thirdc-desk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let v1 = open_or_init(&dir).expect("首次应 init");
        assert!(dir.join("thirdc.toml").is_file());
        let v2 = open_or_init(&dir).expect("再次应 open");
        assert_eq!(v1.root, v2.root);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
