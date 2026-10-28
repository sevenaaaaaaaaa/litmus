//! 凭证库：`.litmus/local/secrets.json`，AES-256-GCM 加密存放。
//!
//! - 文件里只有密文和一个随机的 `kid`（钥匙编号）；钥匙本身不在库里：
//!   macOS 放系统钥匙串（`security`），其他系统放用户目录 `~/.config/litmus/keys/<kid>.key`（0600），
//!   也可以用环境变量 `LITMUS_SECRETS_KEY`（64 位十六进制）直接给。
//!   所以库文件夹被拷走、备份被拿走，凭证也打不开。
//! - 0.21 及以前是明文：读到明文就当场加密重写（迁移只发生一次）。
//! - 在另一台设备上打开（或钥匙丢了）就是「锁着」：读出来是空的，写会被拒绝（免得把那边的凭证冲掉）；
//!   用原设备上的恢复码能解开，或者清空重来。

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const SERVICE: &str = "com.nownexts.litmus.secrets";

pub fn path(root: &Path) -> PathBuf {
    root.join(crate::SIDECAR_DIR).join("secrets.json")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Load {
    /// 没有文件或空
    Empty,
    /// 明文（旧版本）
    Plain(Vec<Value>),
    /// 解开了
    Open(Vec<Value>, String),
    /// 有密文但没有钥匙
    Locked(String),
    /// 文件坏了
    Broken(String),
}

/// 状态给界面看：加密了没有、钥匙在哪、是不是锁着
#[derive(Debug, Clone, serde::Serialize)]
pub struct State {
    pub encrypted: bool,
    pub locked: bool,
    pub key: &'static str,
    pub count: usize,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn key_file(kid: &str) -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("LITMUS_KEYS_DIR") {
        return Some(PathBuf::from(d).join(format!("{kid}.key")));
    }
    #[cfg(windows)]
    if let Some(a) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(a).join("Litmus").join("keys").join(format!("{kid}.key")));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")))?;
    Some(base.join("litmus").join("keys").join(format!("{kid}.key")))
}

fn env_key() -> Option<[u8; 32]> {
    let v = std::env::var("LITMUS_SECRETS_KEY").ok()?;
    parse_key(&v)
}

/// 恢复码 / 钥匙：64 位十六进制，可以带空格和短横线
pub fn parse_key(s: &str) -> Option<[u8; 32]> {
    let h: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let b = hex::decode(h).ok()?;
    b.try_into().ok()
}

fn use_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("LITMUS_KEYS_DIR").is_none() && Path::new("/usr/bin/security").exists()
}

fn keychain_get(kid: &str) -> Option<[u8; 32]> {
    let out = std::process::Command::new("/usr/bin/security").args(["find-generic-password", "-s", SERVICE, "-a", kid, "-w"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_key(String::from_utf8_lossy(&out.stdout).trim())
}

fn keychain_put(kid: &str, key: &[u8; 32]) -> bool {
    std::process::Command::new("/usr/bin/security")
        .args(["add-generic-password", "-U", "-s", SERVICE, "-a", kid, "-l", "鹿蕊 Litmus 凭证库", "-w", &hex::encode(key)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 找到过的钥匙记在内存里：每次都去问钥匙串太慢（一次几十毫秒）
fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, ([u8; 32], &'static str)>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, ([u8; 32], &'static str)>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}
pub fn forget_cached() {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 找钥匙：环境变量 → 钥匙串（macOS）→ 用户目录里的钥匙文件
fn find_key(kid: &str) -> Option<([u8; 32], &'static str)> {
    if let Some(k) = env_key() {
        return Some((k, "env"));
    }
    if let Some(x) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(kid).copied() {
        return Some(x);
    }
    let found = find_key_uncached(kid);
    if let Some(x) = found {
        cache().lock().unwrap_or_else(|e| e.into_inner()).insert(kid.to_string(), x);
    }
    found
}

fn find_key_uncached(kid: &str) -> Option<([u8; 32], &'static str)> {
    if use_keychain() {
        if let Some(k) = keychain_get(kid) {
            return Some((k, "keychain"));
        }
    }
    let f = key_file(kid)?;
    let raw = std::fs::read_to_string(f).ok()?;
    parse_key(raw.trim()).map(|k| (k, "file"))
}

/// 存钥匙：钥匙串存得进就用钥匙串，否则写用户目录（0600）
fn store_key(kid: &str, key: &[u8; 32]) -> Result<&'static str, String> {
    if env_key().is_some() {
        return Ok("env");
    }
    if use_keychain() && keychain_put(kid, key) {
        return Ok("keychain");
    }
    let f = key_file(kid).ok_or("找不到用户目录，没法放钥匙：设环境变量 LITMUS_SECRETS_KEY")?;
    if let Some(d) = f.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = f.with_extension("tmp");
    std::fs::write(&tmp, hex::encode(key)).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &f).map_err(|e| e.to_string())?;
    Ok("file")
}

fn seal(key: &[u8; 32], kid: &str, plain: &[u8]) -> Result<Value, String> {
    let rng = SystemRandom::new();
    let mut nonce = [0u8; 12];
    rng.fill(&mut nonce).map_err(|_| "取不到随机数")?;
    let k = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).map_err(|_| "钥匙不对")?);
    let mut buf = plain.to_vec();
    k.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(kid.as_bytes()), &mut buf).map_err(|_| "加密失败")?;
    Ok(json!({ "v": 2, "enc": "aes-256-gcm", "kid": kid, "nonce": hex::encode(nonce), "data": hex::encode(buf), "updatedAt": now_ms() }))
}

fn open_with(key: &[u8; 32], v: &Value) -> Option<Vec<Value>> {
    let kid = v["kid"].as_str()?;
    let nonce: [u8; 12] = hex::decode(v["nonce"].as_str()?).ok()?.try_into().ok()?;
    let mut buf = hex::decode(v["data"].as_str()?).ok()?;
    let k = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).ok()?);
    let plain = k.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(kid.as_bytes()), &mut buf).ok()?;
    let rows: Value = serde_json::from_slice(plain).ok()?;
    rows.get("secrets").and_then(|s| s.as_array()).cloned()
}

fn read(root: &Path) -> Load {
    let Ok(raw) = std::fs::read_to_string(path(root)) else { return Load::Empty };
    if raw.trim().is_empty() {
        return Load::Empty;
    }
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return Load::Broken("凭证库文件坏了".into()) };
    if v.get("enc").is_some() {
        let kid = v["kid"].as_str().unwrap_or("").to_string();
        return match find_key(&kid).and_then(|(k, _)| open_with(&k, &v)) {
            Some(rows) => Load::Open(rows, kid),
            None => Load::Locked(kid),
        };
    }
    match v.get("secrets").and_then(|s| s.as_array()) {
        Some(a) => Load::Plain(a.clone()),
        None => Load::Broken("凭证库文件格式不对".into()),
    }
}

fn write_file(root: &Path, body: &Value) -> Result<(), String> {
    let p = path(root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(body).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

fn encrypt_rows(root: &Path, rows: &[Value], kid: Option<String>) -> Result<(), String> {
    let (kid, key) = match kid.and_then(|k| find_key(&k).map(|(key, _)| (k, key))) {
        Some(x) => x,
        None => {
            let rng = SystemRandom::new();
            let mut key = [0u8; 32];
            let mut id = [0u8; 8];
            rng.fill(&mut key).map_err(|_| "取不到随机数")?;
            rng.fill(&mut id).map_err(|_| "取不到随机数")?;
            let kid = hex::encode(id);
            store_key(&kid, &key)?;
            (kid, key)
        }
    };
    let plain = serde_json::to_vec(&json!({ "secrets": rows })).map_err(|e| e.to_string())?;
    write_file(root, &seal(&key, &kid, &plain)?)
}

/// 读出全部凭证。锁着或坏了返回空（界面另看 [`state`]）。读到明文顺手加密。
pub fn load(root: &Path) -> Vec<Value> {
    match read(root) {
        Load::Open(rows, _) => rows,
        Load::Plain(rows) => {
            let _ = encrypt_rows(root, &rows, None);
            rows
        }
        _ => Vec::new(),
    }
}

/// 写回全部凭证（加密）。锁着时拒绝，免得把另一台设备的凭证冲掉。
pub fn save(root: &Path, rows: &[Value]) -> Result<(), String> {
    match read(root) {
        Load::Locked(_) => Err("凭证库是在另一台设备上加密的，这里打不开：在 /凭证 里输入那台设备的恢复码，或者清空重来".into()),
        Load::Open(_, kid) => encrypt_rows(root, rows, Some(kid)),
        _ => encrypt_rows(root, rows, None),
    }
}

pub fn get(root: &Path, name: &str) -> Option<String> {
    load(root).into_iter().find(|r| r.get("name").and_then(|n| n.as_str()) == Some(name)).and_then(|r| r.get("value").and_then(|v| v.as_str()).map(String::from)).filter(|v| !v.is_empty())
}

/// 存一条（同名覆盖）
pub fn put(root: &Path, name: &str, value: &str, note: &str) -> Result<(), String> {
    let mut rows: Vec<Value> = load(root).into_iter().filter(|r| r.get("name").and_then(|n| n.as_str()) != Some(name)).collect();
    rows.insert(0, json!({ "name": name, "value": value, "note": note, "updated": now_ms() }));
    save(root, &rows)
}

pub fn state(root: &Path) -> State {
    match read(root) {
        Load::Empty => State { encrypted: true, locked: false, key: if env_key().is_some() { "env" } else if use_keychain() { "keychain" } else { "file" }, count: 0 },
        Load::Plain(r) => State { encrypted: false, locked: false, key: "", count: r.len() },
        Load::Open(r, kid) => State { encrypted: true, locked: false, key: find_key(&kid).map(|x| x.1).unwrap_or("file"), count: r.len() },
        Load::Locked(_) | Load::Broken(_) => State { encrypted: true, locked: true, key: "", count: 0 },
    }
}

/// 恢复码：就是钥匙本身，四个一组。带到另一台设备上能解开这份凭证库。
pub fn recovery_code(root: &Path) -> Result<String, String> {
    match read(root) {
        Load::Open(_, kid) => {
            let (k, _) = find_key(&kid).ok_or("找不到钥匙")?;
            let h = hex::encode(k);
            Ok(h.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("-"))
        }
        Load::Plain(rows) => {
            encrypt_rows(root, &rows, None)?;
            recovery_code(root)
        }
        Load::Empty => {
            encrypt_rows(root, &[], None)?;
            recovery_code(root)
        }
        Load::Locked(_) => Err("凭证库锁着，先解开".into()),
        Load::Broken(e) => Err(e),
    }
}

/// 用恢复码解开另一台设备加密的凭证库：解得开就把钥匙存到这台设备上
pub fn unlock(root: &Path, code: &str) -> Result<usize, String> {
    let key = parse_key(code).ok_or("恢复码不对：应该是 64 个 0-9、a-f 的字符")?;
    let raw = std::fs::read_to_string(path(root)).map_err(|_| "没有凭证库文件")?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| "凭证库文件坏了")?;
    let rows = open_with(&key, &v).ok_or("恢复码对不上这份凭证库")?;
    let kid = v["kid"].as_str().unwrap_or("").to_string();
    store_key(&kid, &key)?;
    Ok(rows.len())
}

/// 清空重来（锁着又没有恢复码时）
pub fn reset(root: &Path) -> Result<(), String> {
    let p = path(root);
    if p.exists() {
        let bak = p.with_extension(format!("locked-{}.json", now_ms() / 1000));
        std::fs::rename(&p, bak).map_err(|e| e.to_string())?;
    }
    encrypt_rows(root, &[], None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_keys<T>(f: impl FnOnce(&Path, &Path) -> T) -> T {
        let v = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();
        // 测试不碰真钥匙串、真用户目录
        std::env::set_var("LITMUS_KEYS_DIR", keys.path());
        std::env::remove_var("LITMUS_SECRETS_KEY");
        std::fs::create_dir_all(v.path().join(crate::SIDECAR_DIR)).unwrap();
        f(v.path(), keys.path())
    }

    #[test]
    fn secrets_are_encrypted_migrated_locked_and_recoverable() {
        with_keys(|root, keys| {
            // 旧版明文：读一次就加密
            std::fs::write(path(root), r#"{"secrets":[{"name":"openai","value":"sk-123"}]}"#).unwrap();
            assert_eq!(get(root, "openai").as_deref(), Some("sk-123"));
            let raw = std::fs::read_to_string(path(root)).unwrap();
            assert!(!raw.contains("sk-123") && raw.contains("aes-256-gcm"), "{raw}");
            put(root, "github_token", "ghp_x", "").unwrap();
            assert!(!std::fs::read_to_string(path(root)).unwrap().contains("ghp_x"));
            assert_eq!(state(root).count, 2);
            let code = recovery_code(root).unwrap();
            // 钥匙丢了（另一台设备）：锁着，写被拒，恢复码能解开
            for e in std::fs::read_dir(keys).unwrap() { std::fs::remove_file(e.unwrap().path()).unwrap(); }
            forget_cached();
            assert!(state(root).locked);
            assert!(load(root).is_empty());
            assert!(put(root, "x", "y", "").is_err(), "锁着时不能写");
            assert!(unlock(root, "00").is_err());
            assert_eq!(unlock(root, &code).unwrap(), 2);
            assert_eq!(get(root, "github_token").as_deref(), Some("ghp_x"));
            // 清空重来
            reset(root).unwrap();
            assert_eq!(state(root).count, 0);
            assert!(!state(root).locked);
        });
    }
}
