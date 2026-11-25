//! Remote account metadata stays on this device; credentials use the encrypted vault.
use super::*;
use std::path::{Component, Path, PathBuf};

const SECRET: &str = "litmus_remote_token";

pub(super) fn account(root: &Path) -> Value {
    std::fs::read(root.join(".litmus/local/remote.json"))
        .ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}))
}

fn token_path(root: &Path) -> PathBuf {
    root.join(".litmus/local/remote-token")
}

fn write_private(path: &Path, value: &str) -> Result<(), String> {
    let parent = path.parent().ok_or("没有父目录")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(".{}.tmp-{}", path.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    std::fs::write(&tmp, value).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| { let _ = std::fs::remove_file(&tmp); e.to_string() })
}

/// 登录 token 单独放一份（0600），不进加密凭证库：
/// 凭证库锁着也照样能用账号密码登录、下次打开直接复用 token，不会退化成每次都要恢复码。
pub(super) fn token(root: &Path) -> Option<String> {
    if let Ok(s) = std::fs::read_to_string(token_path(root)) {
        let t = s.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    // 旧安装：token 在加密凭证库里。读到就搬出来，之后不再依赖凭证库是否锁着。
    let old = kernel_core::secrets::get(root, SECRET)?;
    let _ = write_private(&token_path(root), &old);
    Some(old)
}

pub(super) fn remember(root: &Path, server: &str, user: &str, token: &str) -> Result<bool, String> {
    write_private(&token_path(root), token)?;
    std::fs::write(root.join(".litmus/local/remote.json"), serde_json::to_vec(&json!({"server":server, "user":user, "token_saved": true})).unwrap())
        .map_err(|e| e.to_string())?;
    Ok(true)
}

pub(super) async fn status(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    let k = st.kernel.lock().unwrap();
    let mut a = account(&k.vault.root);
    let has_token = token(&k.vault.root).is_some();
    a["connected"] = json!(has_token);
    // token_saved=false：这次登录成功但凭证库锁着，token 只在内存，重开要再登录一次。
    if a.get("token_saved").is_none() { a["token_saved"] = json!(has_token); }
    Json(a).into_response()
}

pub(super) async fn disconnect(State(st): State<Arc<AppState>>, h: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_token(&st, &h) { return e.into_response(); }
    if let Err(e) = desktop_guard(&st) { return e.into_response(); }
    if st.pull.lock().unwrap().active { return err(StatusCode::CONFLICT, "正在同步，请完成后再退出登录").into_response(); }
    let k = st.kernel.lock().unwrap();
    // 旧副本可能在凭证库里：顺手清掉；锁着就跳过（文件里的那份才是现在用的）。
    let rows: Vec<Value> = secrets_load(&k.vault.root).into_iter()
        .filter(|r| r["name"].as_str() != Some(SECRET)).collect();
    let _ = secrets_save(&k.vault.root, &rows);
    let _ = std::fs::remove_file(token_path(&k.vault.root));
    let path = k.vault.root.join(".litmus/local/remote.json");
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound { return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(); }
    }
    Json(json!({"ok":true})).into_response()
}

pub(super) fn valid_path(path: &str) -> bool {
    path.starts_with("Notes/") && !path.contains('\\') && !path.contains('\0')
        && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)))
        && Path::new(path).extension().is_some_and(|x| x.eq_ignore_ascii_case("md") || x.eq_ignore_ascii_case("html"))
}

fn hash(bytes: &[u8]) -> String { hex::encode(Sha256::digest(bytes)) }

/// 原子落盘：先写同目录临时文件再 rename，内核随时读到的都是完整文件。
fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = dest.parent().ok_or("目标没有父目录")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(".{}.tmp-{}", dest.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| { let _ = std::fs::remove_file(&tmp); e.to_string() })
}

fn fingerprint_path(root: &Path, server: &str, path: &str) -> PathBuf {
    root.join(".litmus/local/remote-meta").join(hash(format!("{server}\n{path}").as_bytes()))
}

/// 上次同步记下的远端指纹（size:mtime）与本地文件都在，且远端没变 → 不必再下载。
pub(super) fn unchanged(root: &Path, server: &str, path: &str, remote: &str) -> bool {
    if !root.join(path).is_file() { return false; }
    let baseline = root.join(".litmus/local/remote-hashes").join(hash(format!("{server}\n{path}").as_bytes()));
    baseline.is_file() && std::fs::read_to_string(fingerprint_path(root, server, path)).ok().as_deref() == Some(remote)
}

pub(super) fn save_fingerprint(root: &Path, server: &str, path: &str, remote: &str) -> Result<(), String> {
    let p = fingerprint_path(root, server, path);
    if let Some(d) = p.parent() { std::fs::create_dir_all(d).map_err(|e| e.to_string())?; }
    std::fs::write(p, remote).map_err(|e| e.to_string())
}

/// Returns 0=unchanged, 1=downloaded, 3=conflict copy. Never overwrite local edits.
/// The caller holds the kernel lock so editor saves cannot race this write.
pub(super) fn store(root: &Path, server: &str, path: &str, source: &str) -> Result<u8, String> {
    if !valid_path(path) { return Err("远端文档路径无效".into()); }
    let dest = root.join(path);
    // Reject symlink ancestors, including an existing file, before creating anything.
    let mut check = PathBuf::from(root);
    for part in Path::new(path).components() {
        check.push(part);
        if std::fs::symlink_metadata(&check).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("远端文档路径包含符号链接".into());
        }
    }
    let hashes = root.join(".litmus/local/remote-hashes");
    std::fs::create_dir_all(&hashes).map_err(|e| e.to_string())?;
    let baseline = hashes.join(hash(format!("{server}\n{path}").as_bytes()));
    let previous = std::fs::read_to_string(&baseline).ok();
    let incoming = hash(source.as_bytes());
    let existing = match std::fs::read(&dest) {
        Ok(bytes) => Some(hash(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    if existing.as_deref() == Some(&incoming) || previous.as_deref() == Some(&incoming) && existing.is_some() {
        std::fs::write(baseline, &incoming).map_err(|e| e.to_string())?;
        return Ok(0);
    }
    let conflict = existing.is_some() && existing != previous;
    let target = if conflict {
        let stem = dest.file_stem().unwrap().to_string_lossy();
        let ext = dest.extension().unwrap().to_string_lossy();
        dest.with_file_name(format!("{stem}-远端冲突-{}.{}", &incoming[..12], ext))
    } else { dest };
    std::fs::create_dir_all(target.parent().unwrap()).map_err(|e| e.to_string())?;
    // create_new also protects an edited conflict copy on later pulls.
    if conflict {
        use std::io::Write;
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(mut file) => file.write_all(source.as_bytes()).map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err("已有同名冲突副本，请整理后重试".into()),
            Err(e) => return Err(e.to_string()),
        }
    } else { write_atomic(&target, source.as_bytes())?; }
    std::fs::write(baseline, incoming).map_err(|e| e.to_string())?;
    Ok(if conflict { 3 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    async fn request(st: &Arc<AppState>, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let req = axum::http::Request::builder().method(method).uri(path)
            .header("authorization", format!("Bearer {}", st.token))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string())).unwrap();
        let res = router(st.clone()).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn wait_pull(st: &Arc<AppState>) -> Value {
        for _ in 0..200 {
            let (_, job) = request(st, "GET", "/desktop/pull/status", json!({})).await;
            if job["active"] == false { return job; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("pull did not finish");
    }

    #[tokio::test]
    async fn account_login_refresh_conflict_and_disconnect() {
        let content = Arc::new(Mutex::new("first".to_string()));
        let content2 = content.clone();
        let upstream = Router::new()
            .route("/litmus/auth/login", post(|Json(body): Json<Value>| async move {
                if body["username"] == "alice" && body["password"] == "correct" {
                    Json(json!({"token":"remote-test-token"}))
                } else { Json(json!({"error":"账号或密码错误"})) }
            }))
            .route("/litmus/docs", get(|h: HeaderMap| async move {
                assert_eq!(h["x-litmus-token"], "remote-test-token");
                Json(json!({"docs":["Notes/a.md", "Notes/../../escape.md", "Notes/a.md"]}))
            }))
            .route("/litmus/doc", get(move |Query(q): Query<HashMap<String, String>>| {
                let content = content2.clone();
                async move {
                    assert_eq!(q["path"], "Notes/a.md");
                    Json(json!({"source":*content.lock().unwrap()}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/litmus", listener.local_addr().unwrap());
        let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap(); });
        let dir = tempfile::tempdir().unwrap();
        let mut state = Arc::try_unwrap(build_state(Vault::init(dir.path(), "remote-test").unwrap()).unwrap()).ok().unwrap();
        state.desktop = true;
        let st = Arc::new(state);
        let (status, _) = request(&st, "POST", "/desktop/pull", json!({"server":server, "user":"alice", "pass":"wrong"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!st.pull.lock().unwrap().active);
        let (_, account) = request(&st, "GET", "/desktop/account", json!({})).await;
        assert_eq!(account["connected"], false);

        let (status, _) = request(&st, "POST", "/desktop/pull", json!({"server":format!("{server}/field/"), "user":"alice", "pass":"correct"})).await;
        assert_eq!(status, StatusCode::OK);
        let job = wait_pull(&st).await;
        assert_eq!(job["total"], 1);
        assert_eq!(job["pulled"], 1);
        assert!(!dir.path().parent().unwrap().join("escape.md").exists());
        let (_, account) = request(&st, "GET", "/desktop/account", json!({})).await;
        assert_eq!(account["connected"], true);
        assert_eq!(account["server"], server);
        assert_eq!(account["user"], "alice");
        assert!(!account.to_string().contains("remote-test-token"));
        // token 存在独立的 0600 文件里，不进加密凭证库；凭证库文件此时可能根本不存在。
        let token_path = dir.path().join(".litmus/local/remote-token");
        assert!(std::fs::read_to_string(&token_path).unwrap().contains("remote-test-token"));
        if let Ok(secrets) = std::fs::read_to_string(dir.path().join(".litmus/local/secrets.json")) {
            assert!(!secrets.contains("remote-test-token"));
            assert!(!secrets.contains("correct"));
        }

        // A fresh AppState models reopening the app; no password is sent again.
        let mut reopened = Arc::try_unwrap(build_state(Vault::open(dir.path()).unwrap()).unwrap()).ok().unwrap();
        reopened.desktop = true;
        let st = Arc::new(reopened);
        *content.lock().unwrap() = "second".into();
        assert_eq!(request(&st, "POST", "/desktop/pull", json!({})).await.0, StatusCode::OK);
        assert_eq!(wait_pull(&st).await["pulled"], 1);
        assert_eq!(std::fs::read_to_string(dir.path().join("Notes/a.md")).unwrap(), "second");
        std::fs::write(dir.path().join("Notes/a.md"), "local edit").unwrap();
        *content.lock().unwrap() = "third".into();
        assert_eq!(request(&st, "POST", "/desktop/pull", json!({})).await.0, StatusCode::OK);
        assert_eq!(wait_pull(&st).await["conflicts"], 1);
        assert_eq!(std::fs::read_to_string(dir.path().join("Notes/a.md")).unwrap(), "local edit");
        assert_eq!(request(&st, "DELETE", "/desktop/account", json!({})).await.0, StatusCode::OK);
        assert_eq!(request(&st, "GET", "/desktop/account", json!({})).await.1["connected"], false);
        assert!(token(dir.path()).is_none());
        assert_eq!(std::fs::read_to_string(dir.path().join("Notes/a.md")).unwrap(), "local edit");
        upstream_task.abort();
    }

    #[test]
    fn downloads_updates_and_preserves_local_edits() {
        let dir = tempfile::tempdir().unwrap(); let root = dir.path();
        let pull = |source| store(root, "https://example.com/litmus", "Notes/a.md", source).unwrap();
        assert_eq!(pull("one"), 1);
        assert_eq!(pull("one"), 0);
        assert_eq!(pull("two"), 1);
        std::fs::write(root.join("Notes/a.md"), "my edit").unwrap();
        assert_eq!(pull("two"), 0);
        assert_eq!(pull("three"), 3);
        assert_eq!(pull("three"), 0);
        assert_eq!(std::fs::read_to_string(root.join("Notes/a.md")).unwrap(), "my edit");
        let copies: Vec<_> = std::fs::read_dir(root.join("Notes")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(copies.len(), 2);
        assert!(copies.iter().any(|p| std::fs::read_to_string(p).unwrap() == "three"));
    }

    #[test]
    fn fingerprint_marks_synced_files() {
        let dir = tempfile::tempdir().unwrap(); let root = dir.path();
        assert_eq!(store(root, "s", "Notes/a.md", "one").unwrap(), 1);
        assert!(!unchanged(root, "s", "Notes/a.md", "3:123"));
        save_fingerprint(root, "s", "Notes/a.md", "3:123").unwrap();
        assert!(unchanged(root, "s", "Notes/a.md", "3:123"));
        assert!(!unchanged(root, "s", "Notes/a.md", "4:124"));
        assert!(!unchanged(root, "other", "Notes/a.md", "3:123"));
    }

    #[test]
    fn first_pull_and_different_server_preserve_existing_notes() {
        let dir = tempfile::tempdir().unwrap(); let root = dir.path();
        std::fs::create_dir(root.join("Notes")).unwrap();
        std::fs::write(root.join("Notes/a.md"), "local").unwrap();
        assert_eq!(store(root, "server-a", "Notes/a.md", "remote-a").unwrap(), 3);
        assert_eq!(store(root, "server-b", "Notes/a.md", "remote-b").unwrap(), 3);
        assert_eq!(std::fs::read_to_string(root.join("Notes/a.md")).unwrap(), "local");
    }

    #[test]
    fn rejects_traversal_and_symlinks() {
        for p in ["Notes/../../escape.md", "/Notes/a.md", "Notes/../a.md", "Notes/a\\b.md", "Assets/a.md", "Notes/a.png"] {
            assert!(!valid_path(p), "{p}");
        }
        assert!(valid_path("Notes/目录/a.md"));
        #[cfg(unix)] {
            let dir = tempfile::tempdir().unwrap(); let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("Notes")).unwrap();
            assert!(store(dir.path(), "server", "Notes/a.md", "content").is_err());
            assert!(!outside.path().join("a.md").exists());
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn token_lives_outside_the_vault_and_survives_a_locked_vault() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".litmus/local")).unwrap();
        assert!(token(d.path()).is_none());
        assert!(remember(d.path(), "https://s", "u", "tok123").unwrap());
        let p = token_path(d.path());
        assert!(p.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(token(d.path()).as_deref(), Some("tok123"));
        assert_eq!(account(d.path())["token_saved"], json!(true));
        // 凭证库锁着（另一台设备的密文）也读得到 token：它不在库里。
        std::fs::write(d.path().join(".litmus/local/secrets.json"), r#"{"v":2,"enc":"aes-256-gcm","kid":"x","nonce":"000000000000000000000000","data":"00"}"#).unwrap();
        assert_eq!(token(d.path()).as_deref(), Some("tok123"));
    }
}

