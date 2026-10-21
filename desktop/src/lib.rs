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
    /* 切换库后重建主菜单：Open Recent 的打勾与列表立刻更新 */
    #[cfg(target_os = "macos")]
    if let Ok(menu) = build_app_menu(app) {
        let _ = app.set_menu(menu);
    }
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
    let cfg = path.join("thirdc.toml");
    if cfg.is_file() {
        match kernel_core::Vault::open(path) {
            Ok(v) => return Ok(v),
            Err(e) => {
                // 坏配置自愈：进程上次被杀在 init 写盘途中会留下空的 thirdc.toml
                // （目录项已提交、数据丢失——移动端尤其常见）。空文件直接删掉重建；
                // 非空但解析失败才是真坏了，如实上报，不再盲目回退 open 吞掉原因。
                let empty = std::fs::metadata(&cfg).map(|m| m.len() == 0).unwrap_or(false);
                if !empty {
                    return Err(anyhow::anyhow!("打开库失败：{e}"));
                }
                std::fs::remove_file(&cfg).ok();
            }
        }
    }
    std::fs::create_dir_all(path)?;
    match kernel_core::Vault::init(path, "Litmus") {
        Ok(v) => Ok(v),
        // 目录已被别的实例初始化过（竞态，toml 已存在）：回退到 open。
        Err(e) if cfg.is_file() => kernel_core::Vault::open(path)
            .map_err(|_| anyhow::anyhow!("初始化库时发现竞态，回退打开也失败：{e}")),
        Err(e) => Err(anyhow::anyhow!("初始化库失败：{e}")),
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

/// 内核起不来时的兜底页面：桌面写临时文件用 file:// 打开；移动端 Android WebView
/// 默认禁 file://（net::ERR_ACCESS_DENIED），改走自定义协议 thirdcerror://。
static BOOT_ERROR_HTML: std::sync::OnceLock<String> = std::sync::OnceLock::new();

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
    std::fs::write(&path, html.clone()).ok()?;
    let _ = BOOT_ERROR_HTML.set(html);
    // 移动端：Android WebView 禁 file://，走注册好的自定义协议（http://<scheme>.localhost）
    #[cfg(mobile)]
    return Some("http://thirdcerror.localhost/".into());
    #[cfg(desktop)]
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

/// 让窗口标题跟随当前知识库（Mission Control / 窗口菜单里可见）。
#[cfg(desktop)]
#[tauri::command]
fn desktop_set_title(app: AppHandle, title: String) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_title(&title);
    }
}

/// 窗口几何：像原生 App 一样记住上次的大小与位置（独立于 localStorage）。
#[cfg(desktop)]
fn window_state_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("window.json"))
}

#[cfg(desktop)]
fn load_window_state(app: &AppHandle) -> Option<(f64, f64, f64, f64)> {
    let p = window_state_path(app)?;
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    Some((
        v.get("w")?.as_f64()?,
        v.get("h")?.as_f64()?,
        v.get("x")?.as_f64()?,
        v.get("y")?.as_f64()?,
    ))
}

#[cfg(desktop)]
fn save_window_state(app: &AppHandle, window: &tauri::Window) {
    let (Ok(size), Ok(pos), Ok(scale)) = (
        window.inner_size(),
        window.outer_position(),
        window.scale_factor(),
    ) else {
        return;
    };
    let Some(p) = window_state_path(app) else { return };
    let body = serde_json::json!({
        "w": size.width as f64 / scale,
        "h": size.height as f64 / scale,
        "x": pos.x as f64 / scale,
        "y": pos.y as f64 / scale,
    });
    let _ = std::fs::write(p, body.to_string());
}

/// Resized / Moved 会连续触发：700ms 内只落一次盘，避免拖窗口时频繁写文件。
#[cfg(desktop)]
fn maybe_save_window_state(window: &tauri::Window) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 700 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    save_window_state(&window.app_handle(), window);
}

/// 用系统默认程序打开 URL（帮助 / 发布说明 / 问题反馈）。
#[cfg(desktop)]
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// macOS 主菜单：App / 文件 / 编辑 / 视图 / 窗口 / 帮助。
/// 自定义项只发事件给壳（menu:*），具体动作仍由网页里的同一套函数执行。
#[cfg(desktop)]
fn build_app_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{
        AboutMetadata, CheckMenuItem, MenuBuilder, MenuItemBuilder, PredefinedMenuItem,
        SubmenuBuilder,
    };
    let pkg = app.package_info();

    let about = PredefinedMenuItem::about(
        app,
        Some("关于鹿蕊 Litmus"),
        Some(AboutMetadata {
            name: Some("鹿蕊 Litmus".into()),
            version: Some(pkg.version.to_string()),
            short_version: Some(pkg.version.to_string()),
            copyright: Some("AGPL-3.0-or-later · 林下 Understory".into()),
            credits: Some("本地优先的知识 OS".into()),
            ..Default::default()
        }),
    )?;
    let check_update = MenuItemBuilder::with_id("m-check-update", "检查更新…").build(app)?;
    let settings = MenuItemBuilder::with_id("m-settings", "设置…")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let services = PredefinedMenuItem::services(app, Some("服务"))?;
    let hide = PredefinedMenuItem::hide(app, Some("隐藏鹿蕊 Litmus"))?;
    let hide_others = PredefinedMenuItem::hide_others(app, Some("隐藏其他"))?;
    let show_all = PredefinedMenuItem::show_all(app, Some("显示全部"))?;
    let quit = PredefinedMenuItem::quit(app, Some("退出鹿蕊 Litmus"))?;
    let app_menu = SubmenuBuilder::new(app, "鹿蕊 Litmus")
        .item(&about)
        .separator()
        .item(&check_update)
        .separator()
        .item(&settings)
        .separator()
        .item(&services)
        .separator()
        .item(&hide)
        .item(&hide_others)
        .item(&show_all)
        .separator()
        .item(&quit)
        .build()?;

    let new_doc = MenuItemBuilder::with_id("m-new-doc", "新建文档")
        .accelerator("CmdOrCtrl+N")
        .build(app)?;
    let new_folder = MenuItemBuilder::with_id("m-new-folder", "新建文件夹")
        .accelerator("CmdOrCtrl+Shift+N")
        .build(app)?;
    let sticky = MenuItemBuilder::with_id("m-sticky", "新建便利贴").build(app)?;
    let reveal = MenuItemBuilder::with_id("m-reveal-vault", "在访达中显示知识库").build(app)?;
    let close = PredefinedMenuItem::close_window(app, Some("关闭窗口"))?;

    /* Open Recent：注册表里的库，当前库打勾；切换库后菜单会重建 */
    let reg = load_registry(app);
    let mut recent = SubmenuBuilder::new(app, "最近的知识库");
    if reg.vaults.is_empty() {
        recent = recent.item(&MenuItemBuilder::with_id("m-vault-none", "暂无").build(app)?);
    } else {
        for (i, v) in reg.vaults.iter().take(8).enumerate() {
            let current = reg
                .current
                .as_ref()
                .map(|c| c.path == v.path)
                .unwrap_or(false);
            recent = recent.item(&CheckMenuItem::with_id(
                app,
                format!("m-vault:{i}"),
                &v.name,
                true,
                current,
                None::<&str>,
            )?);
        }
    }
    let recent = recent.build()?;

    let file_menu = SubmenuBuilder::new(app, "文件")
        .item(&new_doc)
        .item(&new_folder)
        .item(&sticky)
        .separator()
        .item(&recent)
        .separator()
        .item(&reveal)
        .separator()
        .item(&close)
        .build()?;

    let undo = PredefinedMenuItem::undo(app, Some("撤销"))?;
    let redo = PredefinedMenuItem::redo(app, Some("重做"))?;
    let cut = PredefinedMenuItem::cut(app, Some("剪切"))?;
    let copy = PredefinedMenuItem::copy(app, Some("复制"))?;
    let paste = PredefinedMenuItem::paste(app, Some("粘贴"))?;
    let select_all = PredefinedMenuItem::select_all(app, Some("全选"))?;
    let edit_menu = SubmenuBuilder::new(app, "编辑")
        .item(&undo)
        .item(&redo)
        .separator()
        .item(&cut)
        .item(&copy)
        .item(&paste)
        .item(&select_all)
        .build()?;

    let search = MenuItemBuilder::with_id("m-search", "搜索与命令")
        .accelerator("CmdOrCtrl+K")
        .build(app)?;
    let toggle_rail = MenuItemBuilder::with_id("m-toggle-rail", "切换侧栏")
        .accelerator("CmdOrCtrl+\\")
        .build(app)?;
    let toggle_theme = MenuItemBuilder::with_id("m-toggle-theme", "切换明暗")
        .accelerator("CmdOrCtrl+Shift+L")
        .build(app)?;
    let fullscreen = PredefinedMenuItem::fullscreen(app, Some("进入全屏"))?;
    let view_menu = SubmenuBuilder::new(app, "视图")
        .item(&search)
        .item(&toggle_rail)
        .separator()
        .item(&toggle_theme)
        .separator()
        .item(&fullscreen)
        .build()?;

    let minimize = PredefinedMenuItem::minimize(app, Some("最小化"))?;
    let zoom = PredefinedMenuItem::maximize(app, Some("缩放"))?;
    let front = PredefinedMenuItem::bring_all_to_front(app, Some("前置全部窗口"))?;
    let window_menu = SubmenuBuilder::new(app, "窗口")
        .item(&minimize)
        .item(&zoom)
        .separator()
        .item(&front)
        .build()?;

    let help = MenuItemBuilder::with_id("m-help", "鹿蕊 Litmus 帮助").build(app)?;
    let notes = MenuItemBuilder::with_id("m-release-notes", "发布说明").build(app)?;
    let issue = MenuItemBuilder::with_id("m-report-issue", "报告问题").build(app)?;
    let help_menu = SubmenuBuilder::new(app, "帮助")
        .item(&help)
        .item(&notes)
        .item(&issue)
        .build()?;

    MenuBuilder::new(app)
        .item(&app_menu)
        .item(&file_menu)
        .item(&edit_menu)
        .item(&view_menu)
        .item(&window_menu)
        .item(&help_menu)
        .build()
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

/// 当前 App 版本（Cargo.toml 版本源，Nightly 构建时注入 -nightly 后缀；package_info），供「关于」页显示。
#[tauri::command]
fn desktop_app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// 原生触觉反馈：按压/成功/警告/错误四档强度。
/// 桌面：macOS 走 NSHapticFeedbackManager（触控板 Force Touch），Windows/Linux 静默降级；
/// 移动：iOS 走 UIImpactFeedbackGenerator，Android 走 Vibrator。
/// Web 层 navigator.vibrate 在 iOS 无效，必须下沉到原生。
#[tauri::command]
fn native_haptic(_app: AppHandle, pattern: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use cocoa::base::id;
        use objc::{class, msg_send, sel, sel_impl};
        unsafe {
            let perf: id = msg_send![class!(NSHapticFeedbackManager), defaultPerformer];
            let pattern_id: i64 = match pattern.as_str() {
                "press" => 1,   // generic
                "success" => 1, // generic
                "warn" => 2,    // alignment
                "error" => 3,   // levelChange
                _ => 1,
            };
            let _: () = msg_send![perf, performFeedbackPattern: pattern_id performanceTime: 0i64];
        }
    }
    #[cfg(target_os = "android")]
    {
        // Android 触觉反馈在 MainActivity.kt 的 JS bridge 里实现（WebView 层），
        // 此处仅作为 Tauri 命令的统一入口，实际调用走 window.__TAURI_HAPTIC__
    }
    #[cfg(target_os = "ios")]
    {
        // iOS 走 UIImpactFeedbackGenerator，需要 Swift bridge（TODO: 加 Swift 包）
        // 当前静默降级，前端探测到 iOS 时继续用 navigator.vibrate（无效但不报错）
    }
    Ok(())
}

/// 原生通知：系统级通知中心推送。
/// 桌面：macOS 走 NSUserNotification，Windows 走 Toast；移动：iOS/Android 走系统通知。
#[tauri::command]
fn native_notify(app: AppHandle, title: String, body: String) -> Result<(), String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_notification::NotificationExt;
        app.notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(|e| format!("通知发送失败：{e}"))?;
    }
    #[cfg(mobile)]
    {
        // 移动端通知走系统通知栏，需要权限（Android 13+ 运行时申请）
        // 当前降级为应用内 toast（前端已实现）
    }
    Ok(())
}

/// 原生分享：系统分享面板（AirDrop / 微信 / 邮件 / 复制链接）。
/// 桌面：macOS 走 NSSharingServicePicker；移动：iOS 走 UIActivityViewController，Android 走 Intent.ACTION_SEND。
#[tauri::command]
fn native_share(_app: AppHandle, title: String, text: String, url: Option<String>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, sel, sel_impl};
        unsafe {
            let items: id;
            let mut vec: Vec<id> = Vec::new();
            let ns_text = NSString::alloc(nil).init_str(&text);
            vec.push(ns_text);
            if let Some(u) = url {
                let ns_url: id = msg_send![class!(NSURL), URLWithString: NSString::alloc(nil).init_str(&u)];
                vec.push(ns_url);
            }
            let arr: id = msg_send![class!(NSArray), arrayWithObjects: vec.as_ptr() count: vec.len()];
            let picker: id = msg_send![class!(NSSharingServicePicker), alloc];
            let picker: id = msg_send![picker, initWithItems: arr];
            let main_win = _app.get_webview_window("main").ok_or("主窗口不存在")?;
            let ns_win: id = main_win.ns_window().map_err(|e| e.to_string())? as id;
            let content_view: id = msg_send![ns_win, contentView];
            let rect: cocoa::foundation::NSRect = msg_send![content_view, bounds];
            let _: () = msg_send![picker, showRelativeToRect: rect ofView: content_view preferredEdge: 0i64];
        }
    }
    #[cfg(mobile)]
    {
        // 移动端分享走系统 Intent（Android）/ UIActivityViewController（iOS）
        // 需要平台 bridge，当前降级为复制到剪贴板（前端已实现）
    }
    Ok(())
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
/// 导出文件落「下载」文件夹（重名自动加序号），返回完整路径；前端随后可 reveal。
/// WKWebView 对 blob 下载的支持因系统版本而异，桌面端统一走这里更可靠。
#[tauri::command]
fn desktop_save_download(app: AppHandle, name: String, data_b64: String) -> Result<String, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64.as_bytes()).map_err(|e| e.to_string())?;
    let dir = app.path().download_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = name.chars().map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c }).collect();
    let (stem, ext) = match safe.rsplit_once('.') {
        Some((a, b)) => (a.to_string(), format!(".{b}")),
        None => (safe.clone(), String::new()),
    };
    let mut target = dir.join(&safe);
    let mut n = 2;
    while target.exists() && n < 1000 {
        target = dir.join(format!("{stem} {n}{ext}"));
        n += 1;
    }
    std::fs::write(&target, bytes).map_err(|e| e.to_string())?;
    Ok(target.to_string_lossy().into_owned())
}

/// 打印 / 存为 PDF：开一个窗口载入打印版页面，加载完成后调起系统打印面板
/// （macOS 面板左下角「PDF → 存储为 PDF」）。
#[tauri::command]
fn desktop_print_url(app: AppHandle, url: String) -> Result<(), String> {
    let parsed: tauri::Url = url.parse().map_err(|e| format!("URL 解析失败：{e}"))?;
    let label = format!(
        "print-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
    );
    let app2 = app.clone();
    app.run_on_main_thread(move || {
        let _ = WebviewWindowBuilder::new(&app2, &label, WebviewUrl::External(parsed))
            .title("打印 / 存为 PDF")
            .inner_size(880.0, 1000.0)
            .on_page_load(|w, payload| {
                if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                    let w2 = w.clone();
                    std::thread::spawn(move || {
                        // 等字体与图片排好版再打印
                        std::thread::sleep(std::time::Duration::from_millis(900));
                        let _ = w2.print();
                    });
                }
            })
            .build();
    })
    .map_err(|e| e.to_string())
}

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

    let builder = tauri::Builder::default()
        .manage(RenderSlot(StdMutex::new(None)))
        // 兜底错误页协议：移动端 WebView 禁 file://，错误页经 thirdcerror:// 提供。
        // 页面 HTML 在 boot 失败时写入 BOOT_ERROR_HTML，见 error_page()。
        .register_uri_scheme_protocol("thirdcerror", |_ctx, _req| {
            let html = BOOT_ERROR_HTML.get().cloned().unwrap_or_else(|| {
                "<!doctype html><meta charset=\"utf-8\"><title>鹿蕊 Litmus</title><p>内核没能启动".into()
            });
            tauri::http::Response::builder()
                .status(200)
                .header("content-type", "text/html; charset=utf-8")
                .body(html.into_bytes())
                .unwrap()
        });

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
        /* 主菜单事件：macOS 走原生 NSMenu；自定义项发 CustomEvent 给壳，
           与网页里的 ⌘N/⌘K/⌘\ 走同一条代码路径。 */
        .on_menu_event(|app, event| {
            let id = event.id().0.clone();
            let action = match id.as_str() {
                "m-new-doc" => "menu:new-doc",
                "m-new-folder" => "menu:new-folder",
                "m-search" => "menu:search",
                "m-settings" => "menu:settings",
                "m-toggle-rail" => "menu:toggle-rail",
                "m-toggle-theme" => "menu:toggle-theme",
                "m-check-update" => "menu:check-update",
                _ => "",
            };
            if !action.is_empty() {
                show_main(app);
                dispatch_main(app, action, "{}");
                return;
            }
            match id.as_str() {
                "m-sticky" => open_sticky(app),
                other if other.starts_with("m-vault:") => {
                    let idx: usize = other["m-vault:".len()..].parse().unwrap_or(usize::MAX);
                    let reg = load_registry(app);
                    if let Some(v) = reg.vaults.get(idx) {
                        let path = PathBuf::from(&v.path);
                        if path.is_dir() {
                            let name = v.name.clone();
                            let app2 = app.clone();
                            std::thread::spawn(move || {
                                let _ = switch_and_navigate(&app2, path, name);
                            });
                        }
                    }
                }
                "m-reveal-vault" => {
                    if let Some(ctx) = app.try_state::<TrayCtx>() {
                        let _ = std::process::Command::new("open")
                            .arg(ctx.vault.display().to_string())
                            .spawn();
                    }
                }
                "m-help" => open_url("https://nownexts.com/thirdc/"),
                "m-release-notes" => open_url("https://github.com/sevenaaaaaaaaa/litmus/releases"),
                "m-report-issue" => open_url("https://github.com/sevenaaaaaaaaa/litmus/issues"),
                _ => {}
            }
        })
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
            desktop_kernel_api,
            native_haptic,
            native_notify,
            native_share,
            desktop_save_download,
            desktop_print_url,
            desktop_set_title
        ]);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        desktop_vaults,
        desktop_create_vault,
        desktop_switch_vault,
        desktop_render_page,
        ego_render_result,
        desktop_prefs_save,
        desktop_app_version,
        native_haptic,
        native_notify,
        native_share
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
                    .title("鹿蕊 Litmus")
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
                /* 像原生 App 一样恢复上次的窗口大小与位置；首次启动 1440×900。 */
                let saved = load_window_state(app.handle());
                let (ww, wh) = saved.map(|s| (s.0, s.1)).unwrap_or((1440.0, 900.0));
                builder = builder
                    .inner_size(ww, wh)
                    .min_inner_size(960.0, 640.0)
                    .decorations(true);
                if let Some((_, _, x, y)) = saved {
                    builder = builder.position(x, y);
                }
            }

            #[cfg(target_os = "macos")]
            {
                /* 红绿灯与 56px 顶栏同一条中线（y=28）。tao 的 inset：标题栏容器高 = 按钮高(16)+y，
                   按钮离容器底 6 → 按钮中心 = y + 2。x=18：三灯落在 18/38/58，止于 72，
                   顶栏首个控件从 88 起（壳里 --tl-inset）。改顶栏高度时两处一起改。 */
                builder = builder
                    .hidden_title(true)
                    .title_bar_style(tauri::TitleBarStyle::Overlay)
                    .traffic_light_position(tauri::Position::Logical(tauri::LogicalPosition::new(18.0, 26.0)));
            }

            builder.build()?;

            /* macOS 原生主菜单（App/文件/编辑/视图/窗口/帮助）。
               构建失败只记日志，绝不因此挡住启动。 */
            #[cfg(target_os = "macos")]
            {
                match build_app_menu(app.handle()) {
                    Ok(menu) => {
                        let n = menu.items().map(|v| v.len()).unwrap_or(0);
                        match app.set_menu(menu) {
                            Ok(_) => eprintln!("[menu] 已安装主菜单（{n} 个顶级菜单）"),
                            Err(e) => eprintln!("[menu] 安装主菜单失败：{e}"),
                        }
                    }
                    Err(e) => eprintln!("[menu] 构建主菜单失败：{e}"),
                }
            }

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
                // 记住窗口几何（Resized / Moved，内部节流到 700ms）
                tauri::WindowEvent::Resized(_) if window.label() == "main" => {
                    maybe_save_window_state(window);
                    // 全屏时系统收起红绿灯：通知壳收回顶栏左侧留白（html.fullscreen）
                    #[cfg(target_os = "macos")]
                    {
                        use std::sync::atomic::{AtomicBool, Ordering};
                        static FS: AtomicBool = AtomicBool::new(false);
                        let fs = window.is_fullscreen().unwrap_or(false);
                        if FS.swap(fs, Ordering::Relaxed) != fs {
                            if let Some(w) = window.app_handle().get_webview_window("main") {
                                let _ = w.eval(&format!("document.documentElement.classList.toggle('fullscreen',{fs})"));
                            }
                        }
                    }
                }
                tauri::WindowEvent::Moved(_) if window.label() == "main" => {
                    maybe_save_window_state(window);
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

    #[test]
    fn open_or_init_selfheals_empty_config() {
        let dir = std::env::temp_dir().join(format!("thirdc-desk-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 模拟上次被杀在 init 写盘途中：目录结构在、thirdc.toml 是空文件。
        std::fs::create_dir_all(dir.join("Notes")).unwrap();
        std::fs::write(dir.join("thirdc.toml"), "").unwrap();
        let v = open_or_init(&dir).expect("空配置应自愈重建");
        assert_eq!(v.config.name, "Litmus");
        let raw = std::fs::read_to_string(dir.join("thirdc.toml")).unwrap();
        assert!(raw.contains("name"), "重建后配置应含 name 字段");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
