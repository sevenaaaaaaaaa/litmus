//! kernel-browser: 把真实页面（含已登录会话）消化成排版摘要。
//!
//! 三条后端，按能力递进：
//! - `Http`   直接抓静态 HTML（无 JS、无登录）
//! - `Render` 本地 Chromium headless `--dump-dom`（有 JS 渲染，无登录）
//! - `Ego`    调 `ego-browser` CLI 在 ego-lite 里打开页面并跑 `page.evaluate`，
//!            **复用用户已登录的会话**，拿到 computed style 级别的真实排版
//!
//! 摘要的抽取脚本在页面里执行，返回结构化 JSON（字体/配色/容器宽/字阶/高频组件）。

use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("command failed: {0}")]
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Http,
    Render,
    Ego,
}

impl Backend {
    pub fn parse(s: &str) -> Backend {
        match s.to_ascii_lowercase().as_str() {
            "http" => Backend::Http,
            "render" | "headless" | "dump" => Backend::Render,
            "ego" | "ego-lite" | "egolite" => Backend::Ego,
            _ => Backend::Auto,
        }
    }
}

/// 在页面内执行的摘要脚本（computed style 级）。
pub const DIGEST_JS: &str = r#"() => {
  const fonts = {}, colors = {}, widths = {}, classes = {}, vars = {};
  const bump = (o, k) => { if (k) o[k] = (o[k] || 0) + 1; };
  const els = document.querySelectorAll('body *');
  els.forEach((el) => {
    let s; try { s = getComputedStyle(el); } catch (e) { return; }
    if (s.fontFamily) bump(fonts, s.fontFamily);
    [s.color, s.backgroundColor, s.borderTopColor, s.fill].forEach((c) => {
      if (c && c !== 'rgba(0, 0, 0, 0)' && c !== 'transparent') bump(colors, c);
    });
    if (s.maxWidth && s.maxWidth !== 'none') bump(widths, s.maxWidth);
    el.classList.forEach((c) => bump(classes, c));
  });
  const headings = {};
  document.querySelectorAll('h1,h2,h3,h4,h5,h6').forEach((h) => bump(headings, h.tagName.toLowerCase()));
  try {
    for (const sheet of document.styleSheets) {
      let rules; try { rules = sheet.cssRules; } catch (e) { continue; }
      for (const r of rules) {
        if (r.style) for (const p of r.style) if (p.startsWith('--')) vars[p] = r.style.getPropertyValue(p).trim();
      }
    }
  } catch (e) {}
  const top = (o, n) => Object.entries(o).sort((a, b) => b[1] - a[1]).slice(0, n);
  return {
    title: document.title || '',
    url: location.href,
    elementCount: els.length,
    fonts: top(fonts, 8),
    colors: top(colors, 16),
    widths: top(widths, 6),
    classes: top(classes, 16),
    headings,
    vars,
    lang: document.documentElement.lang || ''
  };
}"#;

/// 直接抓取页面 HTML（静态站点即可，无 JS/登录）。
pub fn fetch_html(url: &str, timeout: Duration) -> Result<String, BrowserError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent("litmus/0.1 (+design ingestion)")
        .build()
        .map_err(|e| BrowserError::Failed(e.to_string()))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| BrowserError::Failed(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(BrowserError::Failed(format!("HTTP {}", resp.status())));
    }
    resp.text().map_err(|e| BrowserError::Failed(e.to_string()))
}

/// 定位 Chromium/Chrome 可执行文件。
pub fn find_chrome(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = configured {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(p) = std::env::var("LITMUS_CHROME").or_else(|_| std::env::var("THIRDC_CHROME")) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    for p in chrome_candidates() {
        if p.is_file() && binary_works(&p) {
            return Some(p);
        }
    }
    None
}

/// 候选顺序：真实 app bundle 优先，PATH 上的 wrapper 脚本最后
/// （homebrew 的 chromium 可能是指向已删除 app 的壳脚本）。
pub fn chrome_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    out.extend(playwright_candidates());
    out.extend(
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Arc.app/Contents/MacOS/Arc",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/opt/homebrew/bin/chromium",
        "/usr/local/bin/chromium",
    ]
    .iter()
    .map(PathBuf::from),
    );
    out
}

/// Playwright 的 Chromium / headless shell（机器上常已存在，且是纯二进制、
/// 不会像 app 壳那样在 --dump-dom 下挂起）。版本号倒序优先。
pub fn playwright_candidates() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let root = PathBuf::from(home).join("Library/Caches/ms-playwright");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs.reverse(); // 版本高的优先
    let mut out = Vec::new();
    for d in dirs {
        let name = d.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with("chromium_headless_shell-") {
            for sub in ["chrome-headless-shell-mac-arm64", "chrome-headless-shell-mac-x64", "chrome-headless-shell-linux64"] {
                out.push(d.join(sub).join("chrome-headless-shell"));
            }
        } else if name.starts_with("chromium-") {
            for sub in ["chrome-mac-arm64", "chrome-mac-x64", "chrome-linux"] {
                out.push(d.join(sub).join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"));
                out.push(d.join(sub).join("chrome"));
            }
        }
    }
    out
}

/// 可执行性校验：跑得起来才算数。
fn binary_works(path: &std::path::Path) -> bool {
    Command::new(path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 定位 ego-browser CLI。
pub fn find_ego(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = configured {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(p) = std::env::var("LITMUS_EGO").or_else(|_| std::env::var("THIRDC_EGO")) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.local/bin/ego-browser"),
        "/usr/local/bin/ego-browser".to_string(),
        "/opt/homebrew/bin/ego-browser".to_string(),
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// 运行命令并在超时后杀掉（std 无 wait_timeout，用轮询 + 超时 kill）。
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<(String, String), BrowserError> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let out = out_handle.join().unwrap_or_default();
                let err = err_handle.join().unwrap_or_default();
                if status.success() {
                    return Ok((out, err));
                }
                return Err(BrowserError::Failed(format!(
                    "exit {:?}: {}",
                    status.code(),
                    err.chars().take(400).collect::<String>()
                )));
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BrowserError::Timeout(timeout));
                }
                std::thread::sleep(Duration::from_millis(120));
            }
        }
    }
}

/// headless 渲染并 dump DOM（有 JS，无登录）。
pub fn dump_dom(url: &str, chrome: Option<&str>, timeout: Duration) -> Result<String, BrowserError> {
    let bin = find_chrome(chrome).ok_or_else(|| {
        BrowserError::Unavailable(
            "未找到 Chromium/Chrome。可用 [browser] chrome = \"…\" 或 LITMUS_CHROME 指定。".into(),
        )
    })?;
    let profile = std::env::temp_dir().join(format!(
        "litmus-chrome-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let mut cmd = Command::new(bin);
    cmd.args([
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--virtual-time-budget=6000",
        &format!("--user-data-dir={}", profile.display()),
        "--dump-dom",
        url,
    ]);
    let (out, _err) = run_with_timeout(cmd, timeout)?;
    Ok(out)
}

/// 用 ego-lite 打开页面并执行摘要脚本（复用已登录会话）。
/// 返回结构化摘要 JSON。
pub fn ego_digest(
    url: &str,
    ego: Option<&str>,
    timeout: Duration,
) -> Result<Value, BrowserError> {
    let bin = find_ego(ego).ok_or_else(|| {
        BrowserError::Unavailable(
            "未找到 ego-browser CLI。安装 ego lite 后可用 [browser] ego = \"…\" 指定路径。".into(),
        )
    })?;
    let script = format!(
        r#"const task = await taskSpace("litmus: digest page layout");
const page = task.page("p1");
await page.goto({url});
const digest = await page.evaluate({js});
console.log("__LITMUS_DIGEST__" + JSON.stringify(digest));
"#,
        url = serde_json::to_string(url)?,
        js = DIGEST_JS,
    );
    let mut cmd = Command::new(bin);
    cmd.arg("nodejs").arg("-e").arg(script);
    let (out, err) = run_with_timeout(cmd, timeout)?;
    // 注意：ego 的 console 输出走 stderr，两路都要找
    let combined = format!("{out}\n{err}");
    for line in combined.lines().rev() {
        if let Some(idx) = line.find("__LITMUS_DIGEST__") {
            let json = &line[idx + "__LITMUS_DIGEST__".len()..];
            return Ok(serde_json::from_str(json)?);
        }
    }
    // 退化：尝试任意 JSON 行
    for line in combined.lines().rev() {
        let t = line.trim();
        if t.starts_with('{') && t.contains("\"fonts\"") {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                return Ok(v);
            }
        }
    }
    Err(BrowserError::Failed(format!(
        "ego 输出中未找到摘要：{}",
        combined
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parsing() {
        assert_eq!(Backend::parse("ego"), Backend::Ego);
        assert_eq!(Backend::parse("EGO-LITE"), Backend::Ego);
        assert_eq!(Backend::parse("headless"), Backend::Render);
        assert_eq!(Backend::parse("whatever"), Backend::Auto);
    }

    #[test]
    fn digest_js_is_self_contained() {
        // 脚本必须是可求值的函数表达式，且不依赖外部变量
        assert!(DIGEST_JS.trim_start().starts_with("() =>"));
        assert!(DIGEST_JS.contains("getComputedStyle"));
        assert!(DIGEST_JS.contains("document.styleSheets"));
        assert!(!DIGEST_JS.contains("require("));
    }

    #[test]
    fn missing_binary_reports_unavailable() {
        let err = dump_dom("https://example.com", Some("/nope/chrome"), Duration::from_millis(200));
        assert!(err.is_err());
        let ego = ego_digest("https://example.com", Some("/nope/ego"), Duration::from_millis(200));
        assert!(ego.is_err());
    }
}
