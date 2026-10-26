# 1) kernel-deploy::gitops（git 开发模式）
p = 'kernel/crates/kernel-deploy/src/lib.rs'
s = open(p, encoding='utf-8').read()
s += '''

/// git 开发模式：把知识库当仓库管理（工作区 = 分支）。
pub mod gitops {
    use super::{DeployError, git as _git};

    pub fn ensure_repo(dir: &std::path::Path) -> Result<(), DeployError> {
        if dir.join(".git").is_dir() {
            return Ok(());
        }
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["init", "-b", "main"])
            .output()?;
        if !out.status.success() {
            return Err(DeployError::Git(String::from_utf8_lossy(&out.stderr).into()));
        }
        Ok(())
    }

    pub fn status(dir: &std::path::Path) -> Result<(String, usize), DeployError> {
        ensure_repo(dir)?;
        let branch = git_stdout(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();
        let n = git_stdout(dir, &["status", "--porcelain"]).lines().count();
        Ok((branch, n))
    }

    pub fn log(dir: &std::path::Path, n: usize) -> Result<Vec<(String, String)>, DeployError> {
        ensure_repo(dir)?;
        let out = git_stdout(dir, &["log", "--oneline", "-n", &n.to_string()]);
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' ').map(|(h, s)| (h.to_string(), s.to_string())))
            .collect())
    }

    pub fn commit_all(dir: &std::path::Path, msg: &str) -> Result<usize, DeployError> {
        ensure_repo(dir)?;
        _git(dir, &["add", "-A"])?;
        let st = git_stdout(dir, &["status", "--porcelain"])?;
        if st.trim().is_empty() {
            return Ok(0);
        }
        _git(
            dir,
            &["-c", "user.name=litmus", "-c", "user.email=litmus@localhost", "commit", "-q", "-m", msg],
        )?;
        Ok(st.lines().count())
    }

    pub fn branches(dir: &std::path::Path) -> Result<Vec<String>, DeployError> {
        ensure_repo(dir)?;
        let out = git_stdout(dir, &["branch", "--format=%(refname:short)"]);
        Ok(out.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
    }

    pub fn create_branch(dir: &std::path::Path, name: &str) -> Result<(), DeployError> {
        ensure_repo(dir)?;
        _git(dir, &["checkout", "-b", name])
    }

    pub fn checkout(dir: &std::path::Path, name: &str) -> Result<(), DeployError> {
        ensure_repo(dir)?;
        // 未提交的改动先兜底提交，防丢
        commit_all(dir, "wip: 切换工作区前自动存档")?;
        _git(dir, &["checkout", name])
    }

    pub fn sync(dir: &std::path::Path, remote: &str) -> Result<(bool, String), DeployError> {
        ensure_repo(dir)?;
        let existing = git_stdout(dir, &["remote", "get-url", "origin"]).unwrap_or_default();
        if existing.trim().is_empty() {
            _git(dir, &["remote", "add", "origin", remote])?;
        } else if existing.trim() != remote {
            _git(dir, &["remote", "set-url", "origin", remote])?;
        }
        let _ = _git(dir, &["pull", "--rebase", "-q", "origin", "HEAD"]);
        let n = commit_all(dir, "sync: 自动存档")?;
        _git(dir, &["push", "-q", "-u", "origin", "HEAD"])?;
        Ok((n > 0, format!("已同步（本地新提交 {n} 个）")))
    }

    fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").current_dir(dir).args(args).output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(_) => String::new(),
        }
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> Result<(), DeployError> {
        let out = std::process::Command::new("git").current_dir(dir).args(args).output()?;
        if !out.status.success() {
            return Err(DeployError::Git(format!(
                "git {} -> {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}
'''
open(p, 'w', encoding='utf-8').write(s)
print('gitops added')

# 2) server：organize plan/apply + git 端点 + presentation
p = 'server/src/lib.rs'
s = open(p, encoding='utf-8').read()

anchor = '/// OpenFlow 主题预设（提取自 ThemeSystem::presets()）。'
add = '''/// 一键整理：按首标签聚类根目录文档（规则版）；AI 可用时让模型给方案。
async fn organize_plan(State(st): State<Arc<AppState>>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
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
            Ok(m) => (m.title.unwrap_or_default(), extract_tags(&kernel_core::to_markdown(&m))),
            Err(_) => continue,
        };
        items.push((rel, title, tags));
    }
    // 规则：按第一个标签聚类；无标签的不动
    let mut moves: Vec<Value> = Vec::new();
    for (rel, _title, tags) in &items {
        if let Some(tag) = tags.first() {
            let fname = rel.split('/').pop().unwrap_or("");
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
    Query(q): Query<HashMap<String, String>>,
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
        let t = html.replace('</body>', "");
        let t = t.replace("<body", "<body style=\\"transform:scale(var(--s));transform-origin:0 0;width:1280px\\"");
        slides.push_str(&format!(
            "<section class=\\"slide\\" data-title=\\"{}\\"><iframe loading=\\"lazy\\" sandbox=\\"\\" srcdoc=\\"{}\\"></iframe></section>\\n",
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
  <section class="slide on cover"><div><div class="mark">3C</div><h1>{vault_name}</h1><div class="sub">{count} 篇知识 · {today} · Litmus Studio</div></div></section>
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

/// OpenFlow 主题预设（提取自 ThemeSystem::presets()）。'''
assert anchor in s
s = s.replace(anchor, add, 1)
s = s.replace('.route("/design/presets", get(design_presets))',
'''.route("/design/presets", get(design_presets))
        .route("/organize/plan", post(organize_plan))
        .route("/organize/apply", post(organize_apply))
        .route("/git/status", get(git_status))
        .route("/git/log", get(git_log))
        .route("/git/commit", post(git_commit))
        .route("/git/branches", get(git_branches))
        .route("/git/checkout", post(git_checkout))
        .route("/git/sync", post(git_sync))
        .route("/presentation", get(presentation))''')
open(p, 'w', encoding='utf-8').write(s)
print('server endpoints added')
