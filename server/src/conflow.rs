//! ConFlow 加工适配层。
//!
//! 生成方法论和抓取/生成逻辑来自固定版本的 `third_party/conflow`；这里仅负责把它作为
//! Litmus 后台任务运行、持久化状态，并将产物通过 Kernel 安全写回知识库。

use super::{audit_log, check_token, err, now_secs, AppState};
use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use include_dir::{include_dir, Dir};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

static CONFLOW: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../third_party/conflow");
const BRIDGE: &str = include_str!("../conflow_bridge.py");
const UPSTREAM_SHA: &str = "960e8fdbdc16aaa5515e41efe494a3c56cdab2ba";

fn yes() -> bool {
    true
}
fn default_genre() -> String {
    "auto".into()
}
fn default_language() -> String {
    "zh".into()
}
fn default_theme() -> String {
    "auto".into()
}
fn default_frames() -> usize {
    24
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessingOptions {
    #[serde(default = "default_genre")]
    pub genre: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "yes")]
    pub article: bool,
    #[serde(default = "yes")]
    pub slides: bool,
    #[serde(default)]
    pub script: bool,
    #[serde(default)]
    pub storyboard: bool,
    #[serde(default)]
    pub html_animation: bool,
    #[serde(default = "default_frames")]
    pub frames: usize,
    #[serde(default)]
    pub duration: usize,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            genre: default_genre(),
            language: default_language(),
            theme: default_theme(),
            article: true,
            slides: true,
            script: false,
            storyboard: false,
            html_animation: false,
            frames: default_frames(),
            duration: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessingRequest {
    #[serde(default)]
    pub source_kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub options: ProcessingOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessingJob {
    pub id: String,
    pub source_kind: String,
    pub source: String,
    #[serde(default)]
    pub sources: Vec<String>,
    pub title: String,
    pub status: String,
    pub phase: String,
    #[serde(default)]
    pub steps: Vec<String>,
    pub done: usize,
    pub total: usize,
    pub options: ProcessingOptions,
    #[serde(default)]
    pub output_paths: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub qc: Option<Value>,
    pub created: u64,
    pub started: u64,
    pub finished: u64,
}

fn stores() -> &'static Mutex<HashMap<PathBuf, Vec<ProcessingJob>>> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Vec<ProcessingJob>>>> = OnceLock::new();
    STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stops() -> &'static Mutex<HashMap<(PathBuf, String), Arc<AtomicBool>>> {
    static STOPS: OnceLock<Mutex<HashMap<(PathBuf, String), Arc<AtomicBool>>>> = OnceLock::new();
    STOPS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn local_dir(root: &Path) -> PathBuf {
    root.join(".litmus/local/conflow")
}
fn jobs_path(root: &Path) -> PathBuf {
    local_dir(root).join("jobs.json")
}

fn ensure_loaded(root: &Path) {
    let mut all = stores().lock().unwrap();
    all.entry(root.to_path_buf()).or_insert_with(|| {
        std::fs::read_to_string(jobs_path(root))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    });
}

fn persist(root: &Path, jobs: &[ProcessingJob]) {
    let dir = local_dir(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = jobs_path(root);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(jobs).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

fn mutate(root: &Path, id: &str, f: impl FnOnce(&mut ProcessingJob)) {
    ensure_loaded(root);
    let mut all = stores().lock().unwrap();
    let jobs = all.get_mut(root).unwrap();
    if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
        f(job);
    }
    persist(root, jobs);
}

pub fn jobs_snapshot(root: &Path) -> Vec<ProcessingJob> {
    ensure_loaded(root);
    stores()
        .lock()
        .unwrap()
        .get(root)
        .cloned()
        .unwrap_or_default()
}

fn safe_rel_doc(path: &str) -> bool {
    let p = Path::new(path);
    !path.contains('\\')
        && !p.is_absolute()
        && path.starts_with("Notes/")
        && matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("md" | "html" | "htm")
        )
        && p.components().all(|c| matches!(c, Component::Normal(_)))
}

fn safe_name(s: &str) -> String {
    let out: String = s
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '/' | '\\'
                    | ':'
                    | '*'
                    | '?'
                    | '"'
                    | '<'
                    | '>'
                    | '|'
                    | '#'
                    | '\n'
                    | '\r'
                    | '\t'
                    | '['
                    | ']'
            )
        })
        .take(60)
        .collect();
    let out = out.trim();
    if out.is_empty() {
        "加工任务".into()
    } else {
        out.into()
    }
}

fn validate(mut req: ProcessingRequest) -> Result<ProcessingRequest, String> {
    if req.source_kind.is_empty() {
        req.source_kind = if req.source.starts_with("http://") || req.source.starts_with("https://")
        {
            "url".into()
        } else {
            "doc".into()
        };
    }
    match req.source_kind.as_str() {
        "url" if req.source.starts_with("http://") || req.source.starts_with("https://") => {}
        "doc" if safe_rel_doc(&req.source) => {}
        "selection" if !req.sources.is_empty() && req.sources.iter().all(|p| safe_rel_doc(p)) => {}
        _ => return Err("来源无效：URL 只接受 http(s)，文档只接受 Notes/ 内安全路径".into()),
    }
    if !["auto", "tutorial", "science", "commentary", "other"].contains(&req.options.genre.as_str())
    {
        return Err("不支持的文体".into());
    }
    if !["zh", "en", "ja", "ko", "es", "fr", "de", "pt", "ru", "ar"]
        .contains(&req.options.language.as_str())
    {
        return Err("不支持的产出语言".into());
    }
    req.options.frames = req.options.frames.clamp(1, 120);
    req.options.duration = req.options.duration.clamp(15, 600);
    Ok(req)
}

fn extract_embedded(dest: &Path) -> std::io::Result<()> {
    fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                std::fs::create_dir_all(&target)?;
                copy_tree(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }
    fn walk(dir: &Dir<'_>, base: &Path) -> std::io::Result<()> {
        for file in dir.files() {
            let target = base.join(file.path());
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, file.contents())?;
        }
        for child in dir.dirs() {
            walk(child, base)?;
        }
        Ok(())
    }
    if let Some(home) = std::env::var_os("LITMUS_CONFLOW_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
    {
        copy_tree(&home, dest)?;
    } else {
        walk(&CONFLOW, dest)?;
    }
    std::fs::write(dest.join("litmus_bridge.py"), BRIDGE)
}

fn python() -> String {
    std::env::var("LITMUS_PYTHON").unwrap_or_else(|_| "python3".into())
}

fn ai_env(st: &AppState) -> Result<(String, String, String), String> {
    let cfg = st
        .kernel
        .lock()
        .unwrap()
        .vault
        .config
        .ai
        .clone()
        .ok_or_else(|| "ConFlow 加工需要模型：先在 /设置 配置".to_string())?;
    let key = if cfg.api_key.is_empty() {
        std::env::var("LITMUS_AI_API_KEY").unwrap_or_default()
    } else {
        cfg.api_key
    };
    if key.is_empty() {
        return Err("ConFlow 加工需要模型 API Key：先在 /设置 配置".into());
    }
    Ok((key, cfg.base_url, cfg.model))
}

async fn run_process(
    root: &Path,
    id: &str,
    mut cmd: tokio::process::Command,
    stop: Arc<AtomicBool>,
) -> Result<String, String> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("无法启动 ConFlow：{e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取 ConFlow 输出".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取 ConFlow 进度".to_string())?;
    let root2 = root.to_path_buf();
    let id2 = id.to_string();
    let progress = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut recent = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {
                recent.push(line.clone());
                if recent.len() > 8 {
                    recent.remove(0);
                }
            }
            if let Some(phase) = line.strip_prefix("[conflow] ") {
                mutate(&root2, &id2, |j| {
                    let phase: String = phase.chars().take(160).collect();
                    j.phase.clone_from(&phase);
                    if j.steps.last() != Some(&phase) {
                        j.steps.push(phase);
                    }
                });
            }
        }
        recent.join("\n")
    });
    let output = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let status = loop {
        if stop.load(Ordering::Relaxed) {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = progress.await;
            let _ = output.await;
            return Err("任务已停止".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            Err(e) => return Err(format!("等待 ConFlow 失败：{e}")),
        }
    };
    let progress_text = progress.await.unwrap_or_default();
    let bytes = output
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    if !status.success() {
        let detail = if progress_text.is_empty() {
            text.lines().last().unwrap_or("")
        } else {
            progress_text.lines().last().unwrap_or("")
        };
        return Err(format!(
            "ConFlow 退出（{}）：{detail}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(text)
}

fn json_output(text: &str) -> Result<Value, String> {
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| "ConFlow 没有返回有效的 JSON 摘要".into())
}

fn summary_path(runtime: &Path, value: &Value) -> Option<PathBuf> {
    let raw = value.as_str()?;
    let p = PathBuf::from(raw);
    let candidate = if p.is_absolute() { p } else { runtime.join(p) };
    let canonical = candidate.canonicalize().ok()?;
    let base = runtime.canonicalize().ok()?;
    canonical.starts_with(base).then_some(canonical)
}

fn artifact_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("读取产物 {}：{e}", path.display()))
}

fn import_outputs(
    st: &AppState,
    job: &ProcessingJob,
    runtime: &Path,
    summary: &Value,
) -> Result<Vec<String>, String> {
    let folder = format!(
        "Notes/加工/ConFlow/{}",
        safe_name(
            summary
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&job.title)
        )
    );
    let mut written = Vec::new();
    let mut links = Vec::new();
    let mut k = st.kernel.lock().unwrap();
    let mut put = |name: &str, text: String, k: &mut kernel_core::Kernel| -> Result<(), String> {
        let path = format!("{folder}/{name}");
        k.put_doc(&path, &text).map_err(|e| e.to_string())?;
        written.push(path.clone());
        links.push(format!("- [{name}]({name})"));
        Ok(())
    };
    for (key, name, wanted) in [
        ("doc", "文章.md", job.options.article),
        ("slides", "幻灯片.html", job.options.slides),
        ("script", "短视频脚本.md", job.options.script),
        ("animation", "HTML动画.html", job.options.html_animation),
    ] {
        if wanted {
            if let Some(p) = summary.get(key).and_then(|v| summary_path(runtime, v)) {
                let mut text = artifact_text(&p)?;
                if key == "slides" {
                    if let Some(parent) = p.parent() {
                        let frames = parent.join("frames");
                        if frames.is_dir() {
                            if let Ok(entries) = std::fs::read_dir(frames) {
                                for entry in entries.flatten().filter(|e| e.path().is_file()) {
                                    let file = entry.file_name().to_string_lossy().to_string();
                                    if let Ok(bytes) = std::fs::read(entry.path()) {
                                        if let Ok(meta) =
                                            k.put_asset(&format!("conflow-{file}"), &bytes)
                                        {
                                            text = text.replace(
                                                &format!("frames/{file}"),
                                                &format!("../../../../{}", meta.rel),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                put(name, text, &mut k)?;
            }
        }
    }
    if let Some(dir) = summary
        .get("storyboard")
        .and_then(|v| summary_path(runtime, v))
    {
        for (src, dest) in [
            ("storyboard.json", "分镜/storyboard.json"),
            ("narration.md", "分镜/旁白.md"),
            ("subs.srt", "分镜/字幕.srt"),
            ("README.md", "分镜/说明.md"),
        ] {
            let p = dir.join(src);
            if p.is_file() {
                put(dest, artifact_text(&p)?, &mut k)?;
            }
        }
    }
    let transcript = summary
        .get("video_id")
        .and_then(Value::as_str)
        .map(|id| runtime.join("output").join(id).join("transcript.md"));
    if let Some(p) = transcript.filter(|p| p.is_file()) {
        put("转写稿.md", artifact_text(&p)?, &mut k)?;
    }
    if let Some(qc) = summary.get("qc").filter(|v| !v.is_null()) {
        put(
            "幻灯片质检.json",
            serde_json::to_string_pretty(qc).unwrap_or_default(),
            &mut k,
        )?;
    }
    let manifest =
        format!(
        "---\nsource: {}\njob: {}\nupstream: {}\ncreated: {}\nmodes: {}\n---\n# 加工清单\n\n{}\n",
        serde_json::to_string(&job.source).unwrap_or_else(|_| "\"\"".into()),
        job.id, UPSTREAM_SHA, now_secs(),
        serde_json::to_string(&job.options).unwrap_or_else(|_| "{}".into()),
        if links.is_empty() { "- 没有选中可回写的产物".into() } else { links.join("\n") },
    );
    let manifest_path = format!("{folder}/加工清单.md");
    k.put_doc(&manifest_path, &manifest)
        .map_err(|e| e.to_string())?;
    written.insert(0, manifest_path);
    k.sync_all().map_err(|e| e.to_string())?;
    drop(k);
    audit_log(
        &st.kernel.lock().unwrap().vault.root.join(".litmus/local"),
        "conflow-processing",
        &json!({"job": job.id, "source": job.source, "paths": written, "upstream": UPSTREAM_SHA}),
    );
    Ok(written)
}

fn command_env(cmd: &mut tokio::process::Command, env: &(String, String, String)) {
    cmd.env("CONFLOW_LLM_API_KEY", &env.0)
        .env("CONFLOW_LLM_BASE_URL", &env.1)
        .env("CONFLOW_LLM_MODEL", &env.2);
    if let Ok(chrome) = std::env::var("LITMUS_CHROME") {
        cmd.env("CONFLOW_CHROME", chrome);
    }
}

async fn command_ok(program: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn preflight(root: &Path, id: &str, source_kind: &str) -> Result<(), String> {
    let py = python();
    if !command_ok(&py, &["--version"]).await {
        return Err(format!(
            "找不到 Python（当前：{py}）。安装 Python 3，或设置 LITMUS_PYTHON 指向可执行文件"
        ));
    }
    if source_kind != "url" {
        return Ok(());
    }
    if !command_ok(&py, &["-c", "import yt_dlp"]).await {
        return Err("缺少视频抓取依赖 yt-dlp：运行 `python3 -m pip install --user yt-dlp`".into());
    }
    if !command_ok("ffmpeg", &["-version"]).await {
        return Err(
            "缺少 ffmpeg：macOS 可运行 `brew install ffmpeg`，Linux 请用系统包管理器安装".into(),
        );
    }
    let whisper = command_ok(
        &py,
        &[
            "-c",
            "import importlib.util as u; raise SystemExit(0 if any(u.find_spec(x) for x in ('mlx_whisper','faster_whisper','whisper')) else 1)",
        ],
    )
    .await;
    if !whisper {
        mutate(root, id, |j| {
            j.steps.push(
                "提示：未安装本地 Whisper；有字幕的视频仍可加工，无字幕视频会给出安装指引".into(),
            )
        });
    }
    Ok(())
}

fn start(st: Arc<AppState>, job: ProcessingJob) {
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let id = job.id.clone();
    let stop = Arc::new(AtomicBool::new(false));
    stops()
        .lock()
        .unwrap()
        .insert((root.clone(), id.clone()), stop.clone());
    tokio::spawn(async move {
        mutate(&root, &id, |j| {
            j.status = "running".into();
            j.started = now_secs();
            j.finished = 0;
            j.error = None;
            j.phase = "准备 ConFlow 引擎".into();
        });
        let result: Result<Vec<String>, String> = async {
            let runtime = local_dir(&root).join("runs").join(&id).join("runtime");
            if runtime.exists() { std::fs::remove_dir_all(&runtime).map_err(|e| e.to_string())?; }
            std::fs::create_dir_all(&runtime).map_err(|e| e.to_string())?;
            extract_embedded(&runtime).map_err(|e| format!("展开 ConFlow 资源：{e}"))?;
            let output = runtime.join("litmus-output");
            std::fs::create_dir_all(&output).map_err(|e| e.to_string())?;
            let options_path = runtime.join("litmus-options.json");
            let mut opt = serde_json::to_value(&job.options).unwrap_or(json!({}));
            opt["title"] = json!(job.title);
            opt["source"] = json!(job.source);
            std::fs::write(&options_path, serde_json::to_vec(&opt).unwrap_or_default()).map_err(|e| e.to_string())?;
            let fake = std::env::var("LITMUS_CONFLOW_RUNNER").ok();
            if fake.is_none() {
                preflight(&root, &id, &job.source_kind).await?;
            }
            let ai = if fake.is_some() { ("test".into(), "http://127.0.0.1".into(), "test".into()) } else { ai_env(&st)? };
            let text = if let Some(runner) = fake {
                let spec = runtime.join("litmus-job.json");
                std::fs::write(&spec, serde_json::to_vec(&job).unwrap_or_default()).map_err(|e| e.to_string())?;
                let mut cmd = tokio::process::Command::new(runner);
                cmd.arg("--spec").arg(spec).arg("--output").arg(&output).current_dir(&runtime);
                run_process(&root, &id, cmd, stop.clone()).await?
            } else if job.source_kind == "url" {
                let mut cmd = tokio::process::Command::new(python());
                cmd.arg(runtime.join("bin/conflow.py")).arg("convert").arg(&job.source)
                    .arg("--json").arg("--doc-type").arg(&job.options.genre)
                    .arg("--language").arg(&job.options.language)
                    .arg("--theme").arg(&job.options.theme)
                    .arg("--max-frames").arg(job.options.frames.to_string())
                    .arg("--sb-duration").arg(job.options.duration.to_string())
                    .current_dir(&runtime);
                if !job.options.slides { cmd.arg("--no-slides"); }
                if job.options.script { cmd.arg("--script"); }
                if job.options.storyboard { cmd.arg("--storyboard"); }
                command_env(&mut cmd, &ai);
                run_process(&root, &id, cmd, stop.clone()).await?
            } else {
                let docs = if job.source_kind == "selection" { &job.sources } else { std::slice::from_ref(&job.source) };
                let mut joined = String::new();
                for rel in docs {
                    let text = std::fs::read_to_string(root.join(rel)).map_err(|e| format!("读取 {rel}：{e}"))?;
                    joined.push_str(&format!("\n\n# 来源：{rel}\n\n{text}"));
                }
                let source = runtime.join("litmus-source.md");
                std::fs::write(&source, joined).map_err(|e| e.to_string())?;
                let mut cmd = tokio::process::Command::new(python());
                cmd.arg(runtime.join("litmus_bridge.py")).arg("--source").arg(source)
                    .arg("--output").arg(&output).arg("--options").arg(&options_path)
                    .current_dir(&runtime);
                command_env(&mut cmd, &ai);
                run_process(&root, &id, cmd, stop.clone()).await?
            };
            let mut summary = json_output(&text)?;
            if job.source_kind == "url" && job.options.html_animation {
                if let Some(doc) = summary.get("doc").and_then(|v| summary_path(&runtime, v)) {
                    let aug = runtime.join("litmus-animation-options.json");
                    let v = json!({"genre": summary.get("doc_type").and_then(Value::as_str).unwrap_or("other"),
                        "language": job.options.language, "title": job.title, "html_animation": true});
                    std::fs::write(&aug, serde_json::to_vec(&v).unwrap_or_default()).map_err(|e| e.to_string())?;
                    let aug_out = output.join("animation");
                    let mut cmd = tokio::process::Command::new(python());
                    cmd.arg(runtime.join("litmus_bridge.py")).arg("--existing-doc")
                        .arg("--source").arg(doc).arg("--output").arg(&aug_out).arg("--options").arg(aug)
                        .current_dir(&runtime);
                    command_env(&mut cmd, &ai);
                    let text = run_process(&root, &id, cmd, stop.clone()).await?;
                    let extra = json_output(&text)?;
                    summary["animation"] = extra["animation"].clone();
                }
            }
            mutate(&root, &id, |j| j.qc = summary.get("qc").cloned());
            import_outputs(&st, &job, &runtime, &summary)
        }.await;
        match result {
            Ok(paths) => mutate(&root, &id, |j| {
                j.status = "done".into();
                j.phase = "已写回知识库".into();
                j.output_paths = paths;
                j.done = j.total;
                j.finished = now_secs();
            }),
            Err(e) => mutate(&root, &id, |j| {
                j.status = if e == "任务已停止" {
                    "stopped".into()
                } else {
                    "error".into()
                };
                j.phase.clear();
                j.error = Some(e);
                j.finished = now_secs();
            }),
        }
        stops().lock().unwrap().remove(&(root, id));
    });
}

pub async fn list(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let mut jobs = jobs_snapshot(&root);
    jobs.sort_by(|a, b| b.created.cmp(&a.created));
    Json(json!({"jobs": jobs, "upstream": UPSTREAM_SHA})).into_response()
}

pub async fn create(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let req: ProcessingRequest = match serde_json::from_slice(&body)
        .map_err(|e| e.to_string())
        .and_then(validate)
    {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let root = st.kernel.lock().unwrap().vault.root.clone();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let id = format!(
        "{nonce:x}-{}",
        &kernel_core::Cas::hash_hex(format!("{}{:?}", req.source, req.sources).as_bytes())[..8]
    );
    let fallback = if req.source_kind == "selection" {
        format!("{} 篇文档", req.sources.len())
    } else {
        req.source
            .rsplit('/')
            .next()
            .unwrap_or("加工任务")
            .to_string()
    };
    let title = safe_name(if req.title.trim().is_empty() {
        &fallback
    } else {
        &req.title
    });
    let job = ProcessingJob {
        id,
        source_kind: req.source_kind,
        source: req.source,
        sources: req.sources,
        title,
        status: "queued".into(),
        phase: "等待开始".into(),
        steps: vec![],
        done: 0,
        total: 1,
        options: req.options,
        output_paths: vec![],
        error: None,
        qc: None,
        created: now_secs(),
        started: 0,
        finished: 0,
    };
    ensure_loaded(&root);
    {
        let mut all = stores().lock().unwrap();
        let jobs = all.get_mut(&root).unwrap();
        jobs.push(job.clone());
        persist(&root, jobs);
    }
    start(st, job.clone());
    (StatusCode::ACCEPTED, Json(json!(job))).into_response()
}

pub async fn get(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    match jobs_snapshot(&root).into_iter().find(|j| j.id == id) {
        Some(j) => Json(json!(j)).into_response(),
        None => err(StatusCode::NOT_FOUND, "加工任务不存在").into_response(),
    }
}

pub fn control(root: &Path, id: &str, action: &str, st: Option<Arc<AppState>>) -> bool {
    match action {
        "stop" => stops()
            .lock()
            .unwrap()
            .get(&(root.to_path_buf(), id.to_string()))
            .map(|f| {
                f.store(true, Ordering::Relaxed);
                true
            })
            .unwrap_or(false),
        "retry" => {
            let Some(state) = st else { return false };
            let Some(mut job) = jobs_snapshot(root)
                .into_iter()
                .find(|j| j.id == id && matches!(j.status.as_str(), "error" | "stopped" | "done"))
            else {
                return false;
            };
            mutate(root, id, |j| {
                j.status = "queued".into();
                j.phase = "等待重试".into();
                j.done = 0;
                j.started = 0;
                j.finished = 0;
                j.error = None;
                j.output_paths.clear();
                j.steps.clear();
                j.qc = None;
            });
            job.status = "queued".into();
            job.phase = "等待重试".into();
            job.done = 0;
            job.started = 0;
            job.finished = 0;
            job.error = None;
            job.output_paths.clear();
            job.steps.clear();
            job.qc = None;
            start(state, job);
            true
        }
        _ => false,
    }
}

pub async fn control_api(
    State(st): State<Arc<AppState>>,
    h: HeaderMap,
    AxumPath((id, action)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) {
        return e.into_response();
    }
    let root = st.kernel.lock().unwrap().vault.root.clone();
    if !control(&root, &id, &action, Some(st)) {
        return err(StatusCode::CONFLICT, "任务已结束，或不支持这个操作").into_response();
    }
    Json(json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn validates_sources_and_options() {
        let req = ProcessingRequest {
            source_kind: "doc".into(),
            source: "Notes/a.md".into(),
            sources: vec![],
            title: "".into(),
            options: ProcessingOptions::default(),
        };
        assert!(validate(req).is_ok());
        for bad in [
            "/tmp/a.md",
            "Notes/../secret.md",
            "Notes\\a.md",
            "Assets/a.md",
        ] {
            let req = ProcessingRequest {
                source_kind: "doc".into(),
                source: bad.into(),
                sources: vec![],
                title: "".into(),
                options: ProcessingOptions::default(),
            };
            assert!(validate(req).is_err(), "{bad}");
        }
    }

    #[test]
    fn embedded_upstream_is_complete() {
        assert!(CONFLOW.get_file("bin/conflow.py").is_some());
        assert!(CONFLOW.get_file("engine/generate.py").is_some());
        assert!(CONFLOW.get_file("prompts/reverse-htmlanim.md").is_some());
        assert!(CONFLOW.get_file("templates/slides.html").is_some());
        let upstream =
            std::str::from_utf8(CONFLOW.get_file("UPSTREAM").unwrap().contents()).unwrap();
        assert!(upstream.contains(UPSTREAM_SHA));
    }

    #[test]
    fn persistence_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let job = ProcessingJob {
            id: "j".into(),
            source_kind: "url".into(),
            source: "https://example.com/v".into(),
            sources: vec![],
            title: "t".into(),
            status: "queued".into(),
            phase: "".into(),
            steps: vec![],
            done: 0,
            total: 1,
            options: ProcessingOptions::default(),
            output_paths: vec![],
            error: None,
            qc: None,
            created: 1,
            started: 0,
            finished: 0,
        };
        persist(root, std::slice::from_ref(&job));
        stores().lock().unwrap().remove(root);
        assert_eq!(jobs_snapshot(root)[0].id, "j");
    }

    #[tokio::test]
    async fn running_process_can_be_stopped() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let job = ProcessingJob {
            id: "stop-job".into(),
            source_kind: "url".into(),
            source: "https://example.com".into(),
            sources: vec![],
            title: "停止测试".into(),
            status: "running".into(),
            phase: "".into(),
            steps: vec![],
            done: 0,
            total: 1,
            options: ProcessingOptions::default(),
            output_paths: vec![],
            error: None,
            qc: None,
            created: 1,
            started: 1,
            finished: 0,
        };
        persist(root, &[job]);
        stores().lock().unwrap().remove(root);
        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            flag2.store(true, Ordering::Relaxed);
        });
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 5"]);
        let err = run_process(root, "stop-job", cmd, flag).await.unwrap_err();
        assert_eq!(err, "任务已停止");
    }

    #[tokio::test]
    async fn fake_runner_writes_artifacts_back_to_the_vault() {
        static ENV: Mutex<()> = Mutex::new(());
        let _guard = ENV.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let vault = kernel_core::Vault::init(d.path(), "conflow-test").unwrap();
        let state = super::super::build_state(vault).unwrap();
        let fake = d.path().join("fake-conflow.sh");
        std::fs::write(&fake, r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output" ]; then out="$2"; shift 2; else shift; fi
done
mkdir -p "$out/storyboard"
printf '# 固定文章\n\n来自夹具。' > "$out/doc.md"
printf '<!doctype html><title>固定幻灯片</title>' > "$out/slides.html"
printf '# 固定脚本' > "$out/script.md"
printf '{"shots":[]}' > "$out/storyboard/storyboard.json"
printf '# 旁白' > "$out/storyboard/narration.md"
printf '1\n00:00:00,000 --> 00:00:01,000\n你好\n' > "$out/storyboard/subs.srt"
printf '# 分镜说明' > "$out/storyboard/README.md"
printf '{"title":"固定加工","doc":"%s/doc.md","slides":"%s/slides.html","script":"%s/script.md","storyboard":"%s/storyboard","qc":{"mode":"fixture","ok":1,"warn":0,"fail":0}}\n' "$out" "$out" "$out" "$out"
"#).unwrap();
        let mut perms = std::fs::metadata(&fake).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake, perms).unwrap();
        std::env::set_var("LITMUS_CONFLOW_RUNNER", &fake);
        let root = d.path().to_path_buf();
        let job = ProcessingJob {
            id: "fixture-job".into(),
            source_kind: "url".into(),
            source: "https://example.com/video".into(),
            sources: vec![],
            title: "固定加工".into(),
            status: "queued".into(),
            phase: "".into(),
            steps: vec![],
            done: 0,
            total: 1,
            options: ProcessingOptions {
                script: true,
                storyboard: true,
                ..Default::default()
            },
            output_paths: vec![],
            error: None,
            qc: None,
            created: now_secs(),
            started: 0,
            finished: 0,
        };
        ensure_loaded(&root);
        {
            let mut all = stores().lock().unwrap();
            all.get_mut(&root).unwrap().push(job.clone());
        }
        start(state.clone(), job);
        for _ in 0..100 {
            if jobs_snapshot(&root)
                .iter()
                .any(|j| j.id == "fixture-job" && j.status == "done")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let done = jobs_snapshot(&root)
            .into_iter()
            .find(|j| j.id == "fixture-job")
            .unwrap();
        assert_eq!(done.status, "done", "{:?}", done.error);
        let folder = root.join("Notes/加工/ConFlow/固定加工");
        assert!(folder.join("加工清单.md").is_file());
        assert!(folder.join("文章.md").is_file());
        assert!(folder.join("幻灯片.html").is_file());
        assert!(folder.join("短视频脚本.md").is_file());
        assert!(folder.join("分镜/storyboard.json").is_file());
        let manifest = std::fs::read_to_string(folder.join("加工清单.md")).unwrap();
        assert!(manifest.contains("upstream: 960e8f"));
        assert!(manifest.contains("[文章.md](文章.md)"));
        assert!(control(&root, "fixture-job", "retry", Some(state)));
        for _ in 0..100 {
            if jobs_snapshot(&root)
                .iter()
                .any(|j| j.id == "fixture-job" && j.status == "done" && j.started > 0)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        std::env::remove_var("LITMUS_CONFLOW_RUNNER");
        assert_eq!(
            jobs_snapshot(&root)
                .into_iter()
                .find(|j| j.id == "fixture-job")
                .unwrap()
                .status,
            "done"
        );
    }
}
