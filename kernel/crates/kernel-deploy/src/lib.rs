//! kernel-deploy: 把构建好的站点推送到目标。
//!
//! 目标适配器：
//! - `local`    本地目录（sidecar 站点，daemon 直接托管）
//! - `git`      git push 到仓库/分支 —— **GitHub Pages 与 Cloudflare Pages 的 Git 集成都走这条**
//! - `s3`       S3 兼容对象存储（S3 / R2 / OSS / COS / MinIO），SigV4 直传（下一步）
//! - `cf-pages` Cloudflare Pages 直传 API（下一步）
//!
//! 增量：state 文件记录已上传内容哈希；git 由 git 自己算差异。

pub mod cf_pages;
pub mod webdav;
pub mod s3;

use kernel_store::PublishTarget;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("git: {0}")]
    Git(String),
    #[error("unsupported target kind: {0}")]
    Unsupported(String),
    #[error("missing config: {0}")]
    Missing(String),
    #[error("crypto: {0}")]
    Crypto(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployState {
    pub files: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployReport {
    pub target: String,
    pub kind: String,
    pub uploaded: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// 该目标的站点目录（相对库根）。
pub fn target_dir(target: &PublishTarget, default_site_dir: &str) -> PathBuf {
    if let Some(d) = &target.dir {
        return PathBuf::from(d);
    }
    match target.kind.as_str() {
        "git" => PathBuf::from(format!(".litmus/local/deploy/{}", target.name)),
        _ => PathBuf::from(default_site_dir),
    }
}

fn state_path(sidecar: &Path, target: &str) -> PathBuf {
    sidecar.join("deploy").join(format!("{target}.json"))
}

fn load_state(p: &Path) -> DeployState {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_state(p: &Path, s: &DeployState) -> Result<(), DeployError> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(s)?)?;
    Ok(())
}

fn hash_file(p: &Path) -> Result<String, DeployError> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(p)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(hex::encode(h.finalize()))
}

fn walk(root: &Path) -> Result<Vec<(String, PathBuf)>, DeployError> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for entry in walkdir_lite(root) {
        if entry.is_file() {
            let rel = entry
                .strip_prefix(root)
                .unwrap_or(&entry)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, entry));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// 不引入 walkdir 依赖的极简递归遍历（站点规模小）。
fn walkdir_lite(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// 执行一次部署。
pub fn deploy(
    site_dir: &Path,
    sidecar: &Path,
    target: &PublishTarget,
) -> Result<DeployReport, DeployError> {
    match target.kind.as_str() {
        "local" => deploy_local(site_dir, target),
        "git" => deploy_git(site_dir, target),
        "s3" => {
            let mut st = load_state(&state_path(sidecar, &target.name));
            let r = s3::deploy_s3(site_dir, target, &mut st);
            if r.is_ok() {
                let _ = save_state(&state_path(sidecar, &target.name), &st);
            }
            r
        }
        "webdav" => {
            let mut st = load_state(&state_path(sidecar, &target.name));
            let r = webdav::backup_webdav(site_dir, target, &mut st);
            if r.is_ok() {
                let _ = save_state(&state_path(sidecar, &target.name), &st);
            }
            r
        }
        "cf-pages" => {
            let mut st = load_state(&state_path(sidecar, &target.name));
            let r = cf_pages::deploy_cf_pages(site_dir, target, &mut st);
            if r.is_ok() {
                let _ = save_state(&state_path(sidecar, &target.name), &st);
            }
            r
        }
        other => Err(DeployError::Unsupported(other.to_string())),
    }
    .map(|r| {
        // 记录状态（供后续增量与审计）；s3 在适配器内部已按上传结果维护
        if target.kind != "s3" {
            let sp = state_path(sidecar, &target.name);
            let mut st = load_state(&sp);
            if let Ok(files) = walk(site_dir) {
                for (rel, p) in files {
                    if let Ok(h) = hash_file(&p) {
                        st.files.insert(rel, h);
                    }
                }
            }
            let _ = save_state(&sp, &st);
        }
        r
    })
}

fn deploy_local(site_dir: &Path, target: &PublishTarget) -> Result<DeployReport, DeployError> {
    let files = walk(site_dir)?;
    let bytes: u64 = files
        .iter()
        .filter_map(|(_, p)| std::fs::metadata(p).ok().map(|m| m.len()))
        .sum();
    Ok(DeployReport {
        target: target.name.clone(),
        kind: "local".into(),
        uploaded: files.len(),
        skipped: 0,
        bytes,
        detail: format!("站点已在 {}", site_dir.display()),
        url: None,
    })
}

/// git 目标：把站点目录变成仓库并 push。GitHub Pages / Cloudflare Pages 均可由此驱动。
fn deploy_git(site_dir: &Path, target: &PublishTarget) -> Result<DeployReport, DeployError> {
    std::fs::create_dir_all(site_dir)?;
    let remote = target
        .remote
        .clone()
        .ok_or_else(|| DeployError::Missing("git 目标需要 remote".into()))?;
    let branch = target.branch.clone().unwrap_or_else(|| "gh-pages".into());

    let is_repo = site_dir.join(".git").is_dir();
    if !is_repo {
        git(site_dir, &["init", "-b", &branch])?;
    }
    // 身份只用于本次提交，不写用户全局配置
    let existing = git_stdout(site_dir, &["remote", "get-url", "origin"]).unwrap_or_default();
    if existing.trim().is_empty() {
        git(site_dir, &["remote", "add", "origin", &remote])?;
    } else if existing.trim() != remote {
        git(site_dir, &["remote", "set-url", "origin", &remote])?;
    }
    git(site_dir, &["add", "-A"])?;
    let status = git_stdout(site_dir, &["status", "--porcelain"])?;
    let mut committed = false;
    if !status.trim().is_empty() {
        let msg = format!(
            "publish: {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        git(
            site_dir,
            &[
                "-c",
                "user.name=litmus",
                "-c",
                "user.email=litmus@localhost",
                "commit",
                "-q",
                "-m",
                &msg,
            ],
        )?;
        committed = true;
    }
    // 推送：生成站点目标默认允许强推（内容即状态）
    let mut push: Vec<&str> = vec!["push", "-q"];
    if target.force {
        push.push("--force");
    }
    push.push("origin");
    push.push(&branch);
    git(site_dir, &push)?;

    let files = walk(site_dir)?;
    Ok(DeployReport {
        target: target.name.clone(),
        kind: "git".into(),
        uploaded: if committed { files.len() } else { 0 },
        skipped: if committed { 0 } else { files.len() },
        bytes: 0,
        detail: format!(
            "{} -> {} ({}分支{})",
            site_dir.display(),
            remote,
            branch,
            if committed { "，已提交并推送" } else { "，无变更" }
        ),
        url: Some(remote),
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<(), DeployError> {
    let out = Command::new("git").current_dir(dir).args(args).output()?;
    if !out.status.success() {
        return Err(DeployError::Git(format!(
            "git {} -> {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

fn git_stdout(dir: &Path, args: &[&str]) -> Result<String, DeployError> {
    let out = Command::new("git").current_dir(dir).args(args).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn target(name: &str, kind: &str) -> PublishTarget {
        PublishTarget {
            name: name.into(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    #[test]
    fn gitops_never_tracks_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path();
        std::fs::create_dir_all(v.join(".litmus/local")).unwrap();
        std::fs::write(v.join(".litmus/local/machine.toml"), "key = 1").unwrap();
        std::fs::create_dir_all(v.join("Notes")).unwrap();
        std::fs::write(v.join("Notes/a.md"), "# a").unwrap();
        gitops::ensure_repo(v).unwrap();
        gitops::ensure_repo(v).unwrap(); // idempotent
        let gi = std::fs::read_to_string(v.join(".gitignore")).unwrap();
        assert_eq!(gi.lines().filter(|l| *l == ".litmus/local/").count(), 1);
        Command::new("git").current_dir(v).args(["add", "-A"]).output().unwrap();
        let out = Command::new("git").current_dir(v).args(["ls-files", "--cached"]).output().unwrap();
        let tracked = String::from_utf8_lossy(&out.stdout);
        assert!(tracked.contains("Notes/a.md"));
        assert!(!tracked.contains(".litmus/local"));
    }

    /// 新库第一次开分支：原来的 main 要真的存在；切回去文件跟着换；分支名不能被当成选项；
    /// 远端没配时不把地址改成字面的 "origin"。
    #[test]
    fn gitops_branches_on_a_fresh_vault() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path();
        std::fs::create_dir_all(v.join("Notes")).unwrap();
        std::fs::write(v.join("Notes/a.md"), "# a").unwrap();
        gitops::ensure_repo(v).unwrap();
        assert_eq!(gitops::status(v).unwrap().0, "main", "还没存档时也该报 main 而不是 HEAD");
        gitops::create_branch(v, "草稿/十月").unwrap();
        std::fs::write(v.join("Notes/b.md"), "# b").unwrap();
        gitops::commit_all(v, "b").unwrap();
        let bs = gitops::branches(v).unwrap();
        assert!(bs.contains(&"main".to_string()) && bs.contains(&"草稿/十月".to_string()), "{bs:?}");
        gitops::checkout(v, "main").unwrap();
        assert!(!v.join("Notes/b.md").exists());
        for bad in ["-b", "--orphan", "a..b", "x.lock", "/x", ""] {
            assert!(gitops::checkout(v, bad).is_err(), "{bad}");
            assert!(gitops::create_branch(v, bad).is_err(), "{bad}");
        }
        assert!(gitops::sync(v, "origin").is_err());
        assert!(gitops::remote_url(v).is_empty(), "不该把远端改成字面的 origin");
    }

    #[test]
    fn local_target_reports_files() {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path().join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(site.join("llms.txt"), "- a").unwrap();
        let r = deploy(&site, dir.path(), &target("local", "local")).unwrap();
        assert_eq!(r.kind, "local");
        assert_eq!(r.uploaded, 2);
        assert!(r.detail.contains("站点已在"));
    }

    #[test]
    fn git_target_pushes_to_bare_repo_and_is_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("kb.git");
        let out = Command::new("git")
            .args(["init", "--bare", "-b", "gh-pages"])
            .arg(&bare)
            .output()
            .unwrap();
        assert!(out.status.success(), "bare init failed: {}", String::from_utf8_lossy(&out.stderr));

        let site = dir.path().join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("index.html"), "<h1>第一版</h1>").unwrap();
        std::fs::write(site.join("llms.txt"), "- 文档").unwrap();

        let mut t = target("gh-pages", "git");
        t.remote = Some(bare.to_string_lossy().into_owned());
        t.branch = Some("gh-pages".into());
        t.force = true;

        let r1 = deploy(&site, dir.path(), &t).unwrap();
        assert_eq!(r1.uploaded, 2, "首次应推送全部文件: {:?}", r1);
        assert!(r1.detail.contains("已提交并推送"));

        // 裸仓库里确实有内容
        let show = Command::new("git")
            .arg(format!("--git-dir={}", bare.display()))
            .args(["show", "gh-pages:index.html"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&show.stdout).contains("第一版"));

        // 再次部署：无变更
        let r2 = deploy(&site, dir.path(), &t).unwrap();
        assert_eq!(r2.uploaded, 0);
        assert!(r2.detail.contains("无变更"));

        // 内容改变后：再次推送
        std::fs::write(site.join("index.html"), "<h1>第二版</h1>").unwrap();
        let r3 = deploy(&site, dir.path(), &t).unwrap();
        assert!(r3.uploaded > 0);
        let show2 = Command::new("git")
            .arg(format!("--git-dir={}", bare.display()))
            .args(["show", "gh-pages:index.html"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&show2.stdout).contains("第二版"));
    }

    #[test]
    fn s3_without_config_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        let e = deploy(dir.path(), dir.path(), &target("s3", "s3")).unwrap_err();
        assert!(e.to_string().contains("endpoint"), "配置缺失要说清楚：{e}");
    }

    #[test]
    fn git_without_remote_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let e = deploy(dir.path(), dir.path(), &target("g", "git")).unwrap_err();
        assert!(e.to_string().contains("remote"));
    }
}


/// git 开发模式：把知识库当仓库管理（工作区 = 分支）。
pub mod gitops {
    use super::{DeployError, git as _git};

    /// Paths that must never leave the device: `.litmus/local/` holds the op-log, index
    /// and machine.toml (device key / per-vault E2EE key); it is rebuilt from files.
    // `.thirdc/` 是 0.14 及以前的本机目录名：还没升级的设备同步时也不能把它带上。
    const VAULT_GITIGNORE: &[&str] = &[".litmus/local/", ".thirdc/", ".DS_Store"];

    pub fn ensure_repo(dir: &std::path::Path) -> Result<(), DeployError> {
        if !dir.join(".git").is_dir() {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(["init", "-b", "main"])
                .output()?;
            if !out.status.success() {
                return Err(DeployError::Git(String::from_utf8_lossy(&out.stderr).into()));
            }
        }
        ensure_gitignore(dir)
    }

    fn ensure_gitignore(dir: &std::path::Path) -> Result<(), DeployError> {
        let path = dir.join(".gitignore");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        let missing: Vec<&str> = VAULT_GITIGNORE
            .iter()
            .copied()
            .filter(|p| !text.lines().any(|l| l.trim() == *p))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        for p in missing {
            text.push_str(p);
            text.push('\n');
        }
        std::fs::write(&path, text)?;
        // Untrack anything already committed under ignored paths (keeps local files).
        let _ = std::process::Command::new("git")
            .current_dir(dir)
            .args(["rm", "-r", "--cached", "-q", "--ignore-unmatch", ".litmus/local", ".thirdc"])
            .output();
        Ok(())
    }

    pub fn status(dir: &std::path::Path) -> Result<(String, usize), DeployError> {
        ensure_repo(dir)?;
        let branch = current_branch(dir);
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
        let st = git_stdout(dir, &["status", "--porcelain"]);
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
        // 只列真的分支（refs/heads/），游离 HEAD 那一行「(HEAD detached at …)」不算
        let out = git_stdout(dir, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"]);
        Ok(out.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
    }

    /// 分支名只收字母数字和 `._/-`，不能以 `-` 开头（免得被 git 当成选项）。
    pub fn valid_branch(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 100
            && !name.starts_with('-')
            && !name.starts_with('/')
            && !name.ends_with('/')
            && !name.ends_with(".lock")
            && !name.contains("..")
            && !name.contains("//")
            && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    }

    /// 当前分支名。还没有任何存档时 rev-parse 只会说 "HEAD"，这时读 symbolic-ref 拿到 main。
    pub fn current_branch(dir: &std::path::Path) -> String {
        let b = git_stdout(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();
        if !b.is_empty() && b != "HEAD" {
            return b;
        }
        let s = git_stdout(dir, &["symbolic-ref", "--short", "HEAD"]).trim().to_string();
        if s.is_empty() { b } else { s }
    }

    fn has_commits(dir: &std::path::Path) -> bool {
        std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// origin 的地址（没有就是空）。
    pub fn remote_url(dir: &std::path::Path) -> String {
        git_stdout(dir, &["remote", "get-url", "origin"]).trim().to_string()
    }

    pub fn create_branch(dir: &std::path::Path, name: &str) -> Result<(), DeployError> {
        if !valid_branch(name) {
            return Err(DeployError::Git(format!("分支名不合法：{name}")));
        }
        ensure_repo(dir)?;
        // 还没有任何存档：先存一次，原来的分支才真的存在，新分支从它分出去
        if !has_commits(dir) {
            commit_all(dir, "存档：开新分支前")?;
        }
        _git(dir, &["checkout", "-b", name])
    }

    pub fn checkout(dir: &std::path::Path, name: &str) -> Result<(), DeployError> {
        if !valid_branch(name) {
            return Err(DeployError::Git(format!("分支名不合法：{name}")));
        }
        ensure_repo(dir)?;
        // 分支不存在就别先「兜底存档」：打错名字会凭空多一个存档
        if git_stdout(dir, &["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")]).trim().is_empty() {
            return Err(DeployError::Git(format!("没有叫 {name} 的分支")));
        }
        // 那个分支里要是存着本机数据目录（旧版本误提交的），切过去会把正在用的索引换掉
        let tracked = git_stdout(dir, &["ls-tree", "-r", "--name-only", name, "--", ".litmus/local", ".thirdc"]);
        if !tracked.trim().is_empty() {
            return Err(DeployError::Git(format!("分支 {name} 里提交过本机数据目录（.litmus/local），切过去会覆盖正在用的索引：先在那个分支上把它移出 git 再切")));
        }
        // 未提交的改动先兜底提交，防丢
        commit_all(dir, "wip: 切换工作区前自动存档")?;
        _git(dir, &["checkout", name])
    }

    pub fn sync(dir: &std::path::Path, remote: &str) -> Result<(bool, String), DeployError> {
        ensure_repo(dir)?;
        let remote = remote.trim();
        let existing = git_stdout(dir, &["remote", "get-url", "origin"]);
        // 空、或者就是写了「origin」：沿用已经配好的远端，别把地址改成字面的 "origin"
        if remote.is_empty() || remote == "origin" {
            if existing.trim().is_empty() {
                return Err(DeployError::Git("还没有配置远端：填上你自己服务器上的仓库地址".into()));
            }
        } else if remote.starts_with('-') {
            return Err(DeployError::Git(format!("远端地址不合法：{remote}")));
        } else if existing.trim().is_empty() {
            _git(dir, &["remote", "add", "origin", remote])?;
        } else if existing.trim() != remote {
            _git(dir, &["remote", "set-url", "origin", remote])?;
        }
        // 先把本地改动存档，再拉：带着未提交的改动 rebase 必然失败
        let n = commit_all(dir, "sync: 自动存档")?;
        let branch = current_branch(dir);
        // 精确匹配 refs/heads/<分支>：只写分支名时 ls-remote 按后缀匹配，x/main 也会算成 main
        let exact = format!("refs/heads/{branch}");
        let has_remote_branch = git_stdout(dir, &["ls-remote", "--heads", "origin", &exact]).lines().any(|l| l.split_whitespace().nth(1) == Some(exact.as_str()));
        if has_remote_branch {
            if let Err(e) = _git(dir, &["pull", "--rebase", "-q", "origin", &branch]) {
                let gd = dir.join(".git");
                let mid_rebase = gd.join("rebase-merge").exists() || gd.join("rebase-apply").exists();
                if mid_rebase {
                    // 冲突时别把仓库留在 rebase 一半的状态
                    let _ = _git(dir, &["rebase", "--abort"]);
                    return Err(DeployError::Git(format!("远端和本地改了同一处，没有合并（本地改动已存档，没丢）：{e}")));
                }
                return Err(DeployError::Git(format!("没能从远端拉取（本地改动已存档，没丢）：{e}")));
            }
        }
        _git(dir, &["push", "-q", "-u", "origin", "HEAD"])?;
        Ok((n > 0, format!("已同步（本地新存档 {n} 个文件）")))
    }

    fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").current_dir(dir).args(args).output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(_) => String::new(),
        }
    }

    #[allow(dead_code)]
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

/// 加密备份（v2）：真实 AEAD + 慢 KDF + 防篡改清单 + 设备签名。
///
/// 目标：网盘/远端只拿到密文，看不到任何原始数据；任何字节被改动都能被发现。
/// - 机密性：AES-256-GCM（ring）
/// - 抗爆破：PBKDF2-HMAC-SHA256（随机 16B 盐，60 万次迭代）
/// - 完整性：AEAD tag + 每文件 SHA-256 清单（root 哈希）
/// - 来源真实性：Ed25519 设备密钥签名（私钥只在本机 `.litmus/local/`）
///
/// 容器：MAGIC("3CBK") ver kdf iters salt nonce pubkey | sig | ct_len ciphertext
/// 兼容：v1（旧 SHA256-CTR 格式）仍可解密；v1 无清单，不能直接还原文件。
pub mod encrypted_backup {
    use super::DeployError;
    use ring::rand::SecureRandom;
    use ring::signature::KeyPair;
    use ring::{aead, pbkdf2, rand, signature};

    pub const MAGIC: &[u8; 4] = b"3CBK";
    const VERSION: u8 = 2;
    const BODY_MAGIC: &[u8; 4] = b"3CAR";
    const BODY_VERSION: u8 = 2;
    const KDF_PBKDF2_SHA256: u8 = 1;
    const PBKDF2_ITERS: u32 = 600_000;
    const SALT_LEN: usize = 16;
    const NONCE_LEN: usize = 12;
    const ED25519_PUB: usize = 32;
    const ED25519_SIG: usize = 64;
    const KEY_REL: &str = ".litmus/local/backup-signing.key";

    fn crypto<E: std::fmt::Display>(e: E) -> DeployError {
        DeployError::Crypto(format!("{e}"))
    }

    fn sha256(parts: &[&[u8]]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().into()
    }

    /// 绝不进备份包的机密/可再生成内容。
    fn excluded(rel: &str) -> bool {
        rel == ".litmus/local/machine.toml"
            || rel == KEY_REL
            || rel == ".litmus/local/backup-signing.pub"
            || rel.starts_with(".litmus/local/backups/")
    }

    struct Entry {
        path: String,
        sha: [u8; 32],
    }

    struct Body {
        manifest: Vec<Entry>,
        payloads: Vec<(String, Vec<u8>)>,
    }

    fn collect(root: &std::path::Path) -> Result<(Vec<Entry>, Vec<(String, Vec<u8>)>), DeployError> {
        let mut stack = vec![root.to_path_buf()];
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name == ".git" || name.starts_with("target") {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                if excluded(&rel) {
                    continue;
                }
                let content = std::fs::read(&p).unwrap_or_default();
                files.push((rel, content));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let manifest = files
            .iter()
            .map(|(p, c)| Entry { path: p.clone(), sha: sha256(&[c]) })
            .collect();
        Ok((manifest, files))
    }

    fn manifest_root(entries: &[Entry]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for e in entries {
            h.update((e.path.len() as u32).to_le_bytes());
            h.update(e.path.as_bytes());
            h.update(e.sha);
        }
        h.finalize().into()
    }

    fn build_body(manifest: &[Entry], payloads: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(BODY_MAGIC);
        b.push(BODY_VERSION);
        b.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        for e in manifest {
            b.extend_from_slice(&(e.path.len() as u32).to_le_bytes());
            b.extend_from_slice(e.path.as_bytes());
            b.extend_from_slice(&e.sha);
        }
        b.extend_from_slice(&(payloads.len() as u32).to_le_bytes());
        for (p, c) in payloads {
            b.extend_from_slice(&(p.len() as u32).to_le_bytes());
            b.extend_from_slice(p.as_bytes());
            b.extend_from_slice(&(c.len() as u64).to_le_bytes());
            b.extend_from_slice(c);
        }
        b
    }

    struct Reader<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Result<&'a [u8], DeployError> {
            if self.p + n > self.b.len() {
                return Err(DeployError::Crypto("数据截断".into()));
            }
            let s = &self.b[self.p..self.p + n];
            self.p += n;
            Ok(s)
        }
        fn u8(&mut self) -> Result<u8, DeployError> {
            Ok(self.take(1)?[0])
        }
        fn u16(&mut self) -> Result<u16, DeployError> {
            Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
        }
        fn u32(&mut self) -> Result<u32, DeployError> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
        }
        fn u64(&mut self) -> Result<u64, DeployError> {
            Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
        }
    }

    fn parse_body(b: &[u8]) -> Result<Body, DeployError> {
        let mut r = Reader { b, p: 0 };
        if r.take(4)? != &BODY_MAGIC[..] {
            return Err(DeployError::Crypto("备份内容格式错误".into()));
        }
        let _ver = r.u8()?;
        let mc = r.u32()? as usize;
        let mut manifest = Vec::with_capacity(mc);
        for _ in 0..mc {
            let pl = r.u32()? as usize;
            let path = String::from_utf8_lossy(r.take(pl)?).into_owned();
            let sha: [u8; 32] = r.take(32)?.try_into().map_err(crypto)?;
            manifest.push(Entry { path, sha });
        }
        let pc = r.u32()? as usize;
        let mut payloads = Vec::with_capacity(pc);
        for _ in 0..pc {
            let pl = r.u32()? as usize;
            let path = String::from_utf8_lossy(r.take(pl)?).into_owned();
            let cl = r.u64()? as usize;
            let content = r.take(cl)?.to_vec();
            payloads.push((path, content));
        }
        Ok(Body { manifest, payloads })
    }

    /// 设备签名密钥：读 `.litmus/local/backup-signing.key`，没有就现场生成（0600）。
    fn signing_key(root: &std::path::Path) -> Result<signature::Ed25519KeyPair, DeployError> {
        let path = root.join(KEY_REL);
        if let Ok(bytes) = std::fs::read(&path) {
            return signature::Ed25519KeyPair::from_pkcs8(&bytes).map_err(crypto);
        }
        let rng = rand::SystemRandom::new();
        let doc = signature::Ed25519KeyPair::generate_pkcs8(&rng).map_err(crypto)?;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, doc.as_ref())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let kp = signature::Ed25519KeyPair::from_pkcs8(doc.as_ref()).map_err(crypto)?;
        let _ = std::fs::write(root.join(".litmus/local/backup-signing.pub"), kp.public_key().as_ref());
        Ok(kp)
    }

    fn header(kdf_alg: u8, iters: u32, salt: &[u8], nonce: &[u8], pubkey: &[u8]) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.push(VERSION);
        h.push(kdf_alg);
        h.extend_from_slice(&iters.to_le_bytes());
        h.extend_from_slice(salt);
        h.extend_from_slice(nonce);
        h.extend_from_slice(&(pubkey.len() as u16).to_le_bytes());
        h.extend_from_slice(pubkey);
        h
    }

    fn derive_key(password: &str, salt: &[u8], iters: u32) -> Result<[u8; 32], DeployError> {
        let n = std::num::NonZeroU32::new(iters)
            .ok_or_else(|| DeployError::Crypto("迭代次数非法".into()))?;
        let mut key = [0u8; 32];
        pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, n, salt, password.as_bytes(), &mut key);
        Ok(key)
    }

    /// 打包并加密整个 vault（v2）。
    pub fn encrypt_vault(root: &std::path::Path, password: &str) -> Result<Vec<u8>, DeployError> {
        if password.len() < 8 {
            return Err(DeployError::Crypto("备份密码至少 8 位".into()));
        }
        let (manifest, payloads) = collect(root)?;
        let body = build_body(&manifest, &payloads);
        let root_hash = manifest_root(&manifest);

        let rng = rand::SystemRandom::new();
        let mut salt = [0u8; SALT_LEN];
        rng.fill(&mut salt).map_err(crypto)?;
        let mut nonce_b = [0u8; NONCE_LEN];
        rng.fill(&mut nonce_b).map_err(crypto)?;
        let key = derive_key(password, &salt, PBKDF2_ITERS)?;
        let kp = signing_key(root)?;
        let pubkey = kp.public_key().as_ref().to_vec();
        let hdr = header(KDF_PBKDF2_SHA256, PBKDF2_ITERS, &salt, &nonce_b, &pubkey);

        let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key).map_err(crypto)?;
        let lk = aead::LessSafeKey::new(unbound);
        let nonce = aead::Nonce::try_assume_unique_for_key(&nonce_b).map_err(crypto)?;
        let mut ct = body;
        lk.seal_in_place_append_tag(nonce, aead::Aad::from(&hdr[..]), &mut ct)
            .map_err(crypto)?;

        let msg = sha256(&[&hdr, &ct, &root_hash]);
        let sig = kp.sign(&msg);

        let mut out = Vec::with_capacity(hdr.len() + 2 + ED25519_SIG + 8 + ct.len());
        out.extend_from_slice(&hdr);
        out.extend_from_slice(&(ED25519_SIG as u16).to_le_bytes());
        out.extend_from_slice(sig.as_ref());
        out.extend_from_slice(&(ct.len() as u64).to_le_bytes());
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// 解容器 → 明文 body，校验 AEAD + Ed25519 签名。
    fn open_body(data: &[u8], password: &str) -> Result<Vec<u8>, DeployError> {
        let mut r = Reader { b: data, p: 0 };
        if r.take(4)? != &MAGIC[..] {
            return Err(DeployError::Crypto("不是 v2 备份容器".into()));
        }
        let _ver = r.u8()?;
        let kdf_alg = r.u8()?;
        if kdf_alg != KDF_PBKDF2_SHA256 {
            return Err(DeployError::Crypto("不支持的 KDF".into()));
        }
        let iters = r.u32()?;
        let salt = r.take(SALT_LEN)?.to_vec();
        let nonce_b: [u8; NONCE_LEN] = r.take(NONCE_LEN)?.try_into().map_err(crypto)?;
        let pk_len = r.u16()? as usize;
        let pubkey = r.take(pk_len)?.to_vec();
        let header_slice = data[..r.p].to_vec();
        let sig_len = r.u16()? as usize;
        let sig = r.take(sig_len)?.to_vec();
        let ct_len = r.u64()? as usize;
        let ct_start = r.p;
        let mut ct = r.take(ct_len)?.to_vec();

        let key = derive_key(password, &salt, iters)?;
        let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key).map_err(crypto)?;
        let lk = aead::LessSafeKey::new(unbound);
        let nonce = aead::Nonce::try_assume_unique_for_key(&nonce_b).map_err(crypto)?;
        let plain = lk
            .open_in_place(nonce, aead::Aad::from(&header_slice[..]), &mut ct)
            .map_err(crypto)?;
        let body = plain.to_vec();

        // 签名校验：绑定 header + 原始密文（含 tag）+ 清单 root
        let parsed = parse_body(&body)?;
        let root_hash = manifest_root(&parsed.manifest);
        let ct_slice = &data[ct_start..ct_start + ct_len];
        if pubkey.len() == ED25519_PUB && sig.len() == ED25519_SIG {
            let msg = sha256(&[&header_slice, ct_slice, &root_hash]);
            let vk = signature::UnparsedPublicKey::new(&signature::ED25519, &pubkey);
            vk.verify(&msg, &sig)
                .map_err(|_| DeployError::Crypto("签名校验失败（备份可能被篡改）".into()))?;
        }
        Ok(body)
    }

    /// 解密（不落盘）。v2 返回明文 body；v1 走旧格式。
    pub fn decrypt_vault(data: &[u8], password: &str) -> Result<Vec<u8>, DeployError> {
        if data.len() >= 4 && &data[..4] == MAGIC {
            open_body(data, password)
        } else {
            decrypt_legacy(data, password)
        }
    }

    /// 解密 + 校验清单 + 还原文件到 dest_root，返回还原文件数。
    pub fn restore_vault(
        data: &[u8],
        password: &str,
        dest_root: &std::path::Path,
    ) -> Result<usize, DeployError> {
        if data.len() < 4 || &data[..4] != MAGIC {
            let _ = decrypt_legacy(data, password)?;
            return Err(DeployError::Crypto("旧版备份（v1）不含文件清单，无法直接还原".into()));
        }
        let body = open_body(data, password)?;
        let parsed = parse_body(&body)?;
        if parsed.manifest.len() != parsed.payloads.len() {
            return Err(DeployError::Crypto("清单与文件数量不一致".into()));
        }
        let mut n = 0usize;
        for (i, (path, content)) in parsed.payloads.iter().enumerate() {
            if sha256(&[content]) != parsed.manifest[i].sha {
                return Err(DeployError::Crypto(format!("文件校验失败：{path}")));
            }
            // 安全护栏：不覆盖本机机密，不越界写盘
            if path.contains("..")
                || path.starts_with('/')
                || path == ".litmus/local/machine.toml"
                || path.starts_with(".litmus/local/backup-signing")
            {
                continue;
            }
            let dest = dest_root.join(path);
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&dest, content)?;
            n += 1;
        }
        Ok(n)
    }

    /// 旧版 v1：IV(12) + XOR(SHA256 keystream) + SHA256(key‖ct) 标签。
    fn decrypt_legacy(data: &[u8], password: &str) -> Result<Vec<u8>, DeployError> {
        if data.len() < 44 {
            return Err(DeployError::Crypto("数据太短".into()));
        }
        use sha2::{Digest, Sha256};
        let key_bytes = Sha256::digest(password.as_bytes());
        let iv = &data[..12];
        let (payload, tag) = data[12..].split_at(data[12..].len() - 32);
        let mut mac = Sha256::new();
        mac.update(&key_bytes);
        mac.update(&data[..data.len() - 32]);
        let expected = mac.finalize();
        if expected.as_slice() != tag {
            return Err(DeployError::Crypto("密码错误或文件已损坏（校验失败）".into()));
        }
        let mut keystream = Vec::new();
        let mut counter: u64 = 0;
        while keystream.len() < payload.len() {
            let mut h = Sha256::new();
            h.update(&key_bytes);
            h.update(iv);
            h.update(&counter.to_le_bytes());
            keystream.extend_from_slice(&h.finalize());
            counter += 1;
        }
        Ok(payload.iter().zip(keystream.iter()).map(|(a, b)| a ^ b).collect())
    }
}
