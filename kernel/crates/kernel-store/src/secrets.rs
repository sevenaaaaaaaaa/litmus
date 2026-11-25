//! 凭证库：.litmus/local/secrets.json，AES-256-GCM 加密存放。
//!
//! - 文件里只有密文和一个随机的 kid（钥匙编号）；钥匙本身不在库里：
//!   macOS 放系统钥匙串（security），其他系统放用户目录 ~/.config/litmus/keys/<kid>.key（0600），
//!   也可以用环境变量 LITMUS_SECRETS_KEY 直接给。
//!   所以库文件夹被拷走、备份被拿走，凭证也打不开。
//! - 0.36 起「解锁码」是 128 位（32 个十六进制字符）：它就是随机种子，AES 钥匙由
//!   HKDF(种子, kid) 派生。旧库的 64 位（256 bit）码继续认，需要时可一键升级。
//! - 首次开通就把解锁码落成本地文件（用户目录钥匙文件 + Documents 下的解锁卡），
//!   钥匙串被拒也不会再把人挡在「每次登录都要输一长串码」上。
//! - 在另一台设备上打开（或钥匙丢了）就是「锁着」：用解锁卡（PNG/PDF/TXT）或
//!   那串码解开一次，这台设备就记住；也可以清空重来。

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::hkdf::{KeyType, Salt, HKDF_SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const SERVICE: &str = "com.nownexts.litmus.secrets";
/// 128 位种子派生 AES 钥匙用的 HKDF 上下文
const HKDF_INFO: &[u8] = b"litmus-secrets-128-v1";

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

/// 状态给界面看：加密了没有、钥匙在哪、是不是锁着、解锁码多少位
#[derive(Debug, Clone, serde::Serialize)]
pub struct State {
    pub encrypted: bool,
    pub locked: bool,
    pub key: &'static str,
    pub count: usize,
    /// 128（新）或 256（旧库）
    pub bits: u16,
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

fn hex_only(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_hexdigit()).collect()
}

/// 旧的公开接口：64 位十六进制 -> 32 字节钥匙
pub fn parse_key(s: &str) -> Option<[u8; 32]> {
    let h = hex_only(s);
    if h.len() != 64 {
        return None;
    }
    hex::decode(h).ok()?.try_into().ok()
}

struct KeyLen;
impl KeyType for KeyLen {
    fn len(&self) -> usize {
        32
    }
}

/// 128 位种子 + kid -> AES-256 钥匙
fn derive(seed: &[u8; 16], kid: &str) -> Option<[u8; 32]> {
    let salt = Salt::new(HKDF_SHA256, kid.as_bytes());
    let prk = salt.extract(seed);
    let okm = prk.expand(&[HKDF_INFO], KeyLen).ok()?;
    let mut out = [0u8; 32];
    okm.fill(&mut out).ok()?;
    Some(out)
}

fn use_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("LITMUS_KEYS_DIR").is_none() && Path::new("/usr/bin/security").exists()
}

fn keychain_get(kid: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/security").args(["find-generic-password", "-s", SERVICE, "-a", kid, "-w"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn keychain_put(kid: &str, material: &str) -> bool {
    std::process::Command::new("/usr/bin/security")
        .args(["add-generic-password", "-U", "-s", SERVICE, "-a", kid, "-l", "Litmus 凭证库", "-w", material])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 钥匙材料：32 位（128 bit 种子）或 64 位（旧版裸钥匙），带短横线也认。
fn material_key(material: &str, kid: &str) -> Option<([u8; 32], bool)> {
    let h = hex_only(material);
    match h.len() {
        32 => {
            let seed: [u8; 16] = hex::decode(&h).ok()?.try_into().ok()?;
            Some((derive(&seed, kid)?, true))
        }
        64 => {
            let key: [u8; 32] = hex::decode(&h).ok()?.try_into().ok()?;
            Some((key, false))
        }
        _ => None,
    }
}

/// 找到过的钥匙记在内存里：每次都去问钥匙串太慢（一次几十毫秒）
fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, ([u8; 32], &'static str)>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, ([u8; 32], &'static str)>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}
pub fn forget_cached() {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 读钥匙材料：环境变量 -> 钥匙串（macOS）-> 用户目录钥匙文件
fn read_material(kid: &str) -> Option<(String, &'static str)> {
    if let Ok(v) = std::env::var("LITMUS_SECRETS_KEY") {
        if !v.trim().is_empty() {
            return Some((v, "env"));
        }
    }
    // 文件优先于钥匙串：升级/解锁时文件一定被写成最新的一份，钥匙串更新失败时
    // 读到旧材料会把用户锁在门外；没有文件的旧安装再回落到钥匙串。
    if let Some(f) = key_file(kid) {
        if let Ok(raw) = std::fs::read_to_string(f) {
            if !raw.trim().is_empty() {
                return Some((raw.trim().to_string(), "file"));
            }
        }
    }
    if use_keychain() {
        if let Some(v) = keychain_get(kid) {
            return Some((v, "keychain"));
        }
    }
    None
}

fn find_key(kid: &str) -> Option<([u8; 32], &'static str)> {
    if let Some(x) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(kid).copied() {
        return Some(x);
    }
    let (material, src) = read_material(kid)?;
    let (key, _) = material_key(&material, kid)?;
    // 旧安装的钥匙只在钥匙串里：第一次读到就补一份文件，免得哪天钥匙串被拒又变成
    // 「每次登录都要输一长串码」。
    if src == "keychain" {
        let _ = store_material(kid, &material);
    }
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(kid.to_string(), (key, src));
    Some((key, src))
}

/// 存钥匙材料：钥匙串能存就存，同时始终落一份用户目录文件。
///
/// 只存钥匙串的话，未签名构建、钥匙串权限被收回、换机器还原备份，都会变成
/// 「每次登录都让你输恢复码」；文件是那条路的兜底（0600，且不在库目录里）。
fn store_material(kid: &str, material: &str) -> Result<&'static str, String> {
    if std::env::var_os("LITMUS_SECRETS_KEY").is_some() {
        return Ok("env");
    }
    let keychain_ok = use_keychain() && keychain_put(kid, material);
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
    std::fs::write(&tmp, material).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &f).map_err(|e| e.to_string())?;
    Ok(if keychain_ok { "keychain" } else { "file" })
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

/// 首次开通：128 位种子 + 派生的 AES 钥匙 + 本地钥匙文件 + 一张解锁卡
fn fresh_key(kid: &str) -> Result<[u8; 32], String> {
    let rng = SystemRandom::new();
    let mut seed = [0u8; 16];
    rng.fill(&mut seed).map_err(|_| "取不到随机数")?;
    let key = derive(&seed, kid).ok_or("派生钥匙失败")?;
    let material = hex::encode(seed);
    store_material(kid, &material)?;
    forget_cached();
    // 不默认生成解锁卡/不默认要求恢复码：需要带走钥匙时，用户在「凭证库」里手动保存。
    Ok(key)
}

fn encrypt_rows(root: &Path, rows: &[Value], kid: Option<String>) -> Result<(), String> {
    let (kid, key) = match kid.and_then(|k| find_key(&k).map(|(key, _)| (k, key))) {
        Some(x) => x,
        None => {
            let rng = SystemRandom::new();
            let mut id = [0u8; 8];
            rng.fill(&mut id).map_err(|_| "取不到随机数")?;
            let kid = hex::encode(id);
            let key = fresh_key(&kid)?;
            (kid, key)
        }
    };
    let plain = serde_json::to_vec(&json!({ "secrets": rows })).map_err(|e| e.to_string())?;
    write_file(root, &seal(&key, &kid, &plain)?)
}

/// 读出全部凭证。锁着或坏了返回空（界面另看 state）。读到明文顺手加密。
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
        Load::Locked(_) => Err("凭证库是在另一台设备上加密的，这里打不开：上传在那台设备保存的解锁卡，或者粘贴解锁码".into()),
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
    let bits = open_kid(&read(root))
        .and_then(|kid| read_material(&kid))
        .and_then(|(m, _)| match hex_only(&m).len() { 32 => Some(128u16), 64 => Some(256u16), _ => None })
        .unwrap_or(128);
    match read(root) {
        Load::Empty => State { encrypted: true, locked: false, key: if std::env::var_os("LITMUS_SECRETS_KEY").is_some() { "env" } else if use_keychain() { "keychain" } else { "file" }, count: 0, bits },
        Load::Plain(r) => State { encrypted: false, locked: false, key: "", count: r.len(), bits: 256 },
        Load::Open(r, kid) => State { encrypted: true, locked: false, key: find_key(&kid).map(|x| x.1).unwrap_or("file"), count: r.len(), bits },
        Load::Locked(_) | Load::Broken(_) => State { encrypted: true, locked: true, key: "", count: 0, bits },
    }
}

fn open_kid(l: &Load) -> Option<String> {
    match l {
        Load::Open(_, kid) => Some(kid.clone()),
        _ => None,
    }
}

/// 解锁码：128 位（32 个十六进制字符，四字一组）；旧库返回 64 位。
pub fn recovery_code(root: &Path) -> Result<String, String> {
    match read(root) {
        Load::Open(_, kid) => {
            let (m, _) = read_material(&kid).ok_or("找不到钥匙")?;
            Ok(crate::secrets_card::group4(&hex_only(&m)))
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

/// 用解锁码解开另一台设备加密的凭证库：解得开就把钥匙存到这台设备上（下次不再问）
pub fn unlock(root: &Path, code: &str) -> Result<usize, String> {
    let material = hex_only(code);
    let raw = std::fs::read_to_string(path(root)).map_err(|_| "没有凭证库文件")?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| "凭证库文件坏了")?;
    let kid = v["kid"].as_str().ok_or("凭证库文件里没有钥匙编号")?.to_string();
    let key = match material.len() {
        32 => {
            let seed: [u8; 16] = hex::decode(&material).map_err(|_| "解锁码不对")?.try_into().map_err(|_| "解锁码不对")?;
            derive(&seed, &kid).ok_or("派生钥匙失败")?
        }
        64 => {
            let k: [u8; 32] = hex::decode(&material).map_err(|_| "解锁码不对")?.try_into().map_err(|_| "解锁码不对")?;
            k
        }
        _ => return Err("解锁码不对：新的 128 位码是 32 个字符，旧码是 64 个字符".into()),
    };
    let rows = open_with(&key, &v).ok_or("解锁码对不上这份凭证库")?;
    store_material(&kid, &material)?;
    forget_cached();
    Ok(rows.len())
}

/// 用解锁卡文件解开：PNG/JPEG 走二维码，PDF 走文本层，TXT 直接找码
pub fn unlock_with_file(root: &Path, name: &str, bytes: &[u8]) -> Result<usize, String> {
    let code = crate::secrets_card::code_from_bytes(name, bytes)
        .ok_or("这份文件里没找到解锁码：二维码要完整、别裁掉四角；PDF 要是带文字层的（不是扫描图）")?;
    unlock(root, &code)
}

/// 旧库（256 位）升级成 128 位：换种子、重封、留备份、读回验证，失败自动回滚
pub fn upgrade_to_128(root: &Path) -> Result<String, String> {
    if std::env::var_os("LITMUS_SECRETS_KEY").is_some() {
        return Err("钥匙来自环境变量 LITMUS_SECRETS_KEY，不能在线升级；先在环境里换成新码".into());
    }
    let (rows, kid) = match read(root) {
        Load::Open(rows, kid) => (rows, kid),
        Load::Locked(_) => return Err("先解开凭证库再升级".into()),
        Load::Plain(rows) => {
            encrypt_rows(root, &rows, None)?;
            return upgrade_to_128(root);
        }
        Load::Empty => {
            encrypt_rows(root, &[], None)?;
            return upgrade_to_128(root);
        }
        Load::Broken(e) => return Err(e),
    };
    let (old, _) = read_material(&kid).ok_or("找不到钥匙")?;
    if hex_only(&old).len() == 32 {
        return Ok(crate::secrets_card::group4(&hex_only(&old)));
    }
    let rng = SystemRandom::new();
    let mut seed = [0u8; 16];
    rng.fill(&mut seed).map_err(|_| "取不到随机数")?;
    let key = derive(&seed, &kid).ok_or("派生钥匙失败")?;
    let material = hex::encode(seed);
    let sealed = seal(&key, &kid, &serde_json::to_vec(&json!({ "secrets": rows })).map_err(|e| e.to_string())?)?;
    let p = path(root);
    let bak = p.with_extension(format!("pre128-{}.json", now_ms() / 1000));
    std::fs::copy(&p, &bak).map_err(|e| format!("备份失败，没敢动：{e}"))?;
    write_file(root, &sealed)?;
    store_material(&kid, &material)?;
    forget_cached();
    match read(root) {
        Load::Open(..) => {
            let _ = write_card_for(&kid, &material, None);
            Ok(crate::secrets_card::group4(&material))
        }
        _ => {
            let _ = std::fs::copy(&bak, &p);
            store_material(&kid, &old)?;
            forget_cached();
            Err("升级后读不回来，已回滚到旧钥匙；凭证没有丢".into())
        }
    }
}

/// 把当前解锁码写成本地卡片（默认目录见 secrets_card::default_card_dir）
pub fn write_card(root: &Path, dir: Option<&Path>) -> Result<crate::secrets_card::Card, String> {
    let code = recovery_code(root)?;
    let kid = open_kid(&read(root)).ok_or("凭证库还没建好")?;
    write_card_for(&kid, &hex_only(&code), dir)
}

fn write_card_for(kid: &str, material: &str, dir: Option<&Path>) -> Result<crate::secrets_card::Card, String> {
    let code = crate::secrets_card::group4(material);
    let dir = dir.map(PathBuf::from).unwrap_or_else(crate::secrets_card::default_card_dir);
    crate::secrets_card::write_card(&code, &dir, &format!("Litmus-解锁码-{kid}"))
}

/// 清空重来（锁着又没有解锁码时）
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

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Env {
        _g: std::sync::MutexGuard<'static, ()>,
        keys: tempfile::TempDir,
        cards: tempfile::TempDir,
        vault: tempfile::TempDir,
    }
    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var("LITMUS_KEYS_DIR");
            std::env::remove_var("LITMUS_UNLOCK_CARD_DIR");
            std::env::remove_var("LITMUS_SECRETS_KEY");
        }
    }

    fn with_keys() -> Env {
        let g = env_lock();
        let keys = tempfile::tempdir().unwrap();
        let cards = tempfile::tempdir().unwrap();
        let vault = tempfile::tempdir().unwrap();
        std::env::set_var("LITMUS_KEYS_DIR", keys.path());
        std::env::set_var("LITMUS_UNLOCK_CARD_DIR", cards.path());
        std::env::remove_var("LITMUS_SECRETS_KEY");
        std::fs::create_dir_all(vault.path().join(crate::SIDECAR_DIR)).unwrap();
        forget_cached();
        Env { _g: g, keys, cards, vault }
    }

    #[test]
    fn new_vault_uses_128_bit_code_and_remembers_device() {
        let e = with_keys();
        let root = e.vault.path();
        put(root, "openai", "sk-123", "").unwrap();
        let raw = std::fs::read_to_string(path(root)).unwrap();
        assert!(!raw.contains("sk-123") && raw.contains("aes-256-gcm"), "{raw}");
        let code = recovery_code(root).unwrap();
        assert_eq!(hex_only(&code).len(), 32, "{code}");
        assert_eq!(code.split('-').count(), 8, "{code}");
        assert_eq!(state(root).bits, 128);
        let kid = open_kid(&read(root)).unwrap();
        // 首次开通不再默认写解锁卡（要用时在「凭证库」里手动保存）
        assert!(!e.cards.path().join(format!("Litmus-解锁码-{kid}.pdf")).exists());
        // 另一台设备：钥匙没了，锁着
        for f in std::fs::read_dir(e.keys.path()).unwrap() { std::fs::remove_file(f.unwrap().path()).unwrap(); }
        forget_cached();
        assert!(state(root).locked);
        assert!(put(root, "x", "y", "").is_err(), "锁着时不能写");
        assert!(unlock(root, "00").is_err());
        assert_eq!(unlock(root, &code).unwrap(), 1);
        assert!(!state(root).locked);
        forget_cached();
        assert_eq!(get(root, "openai").as_deref(), Some("sk-123"), "解一次就记住，不用再问");
    }

    #[test]
    fn card_files_can_unlock_on_a_fresh_device() {
        let e = with_keys();
        let root = e.vault.path();
        put(root, "github_token", "ghp_x", "").unwrap();
        let kid = open_kid(&read(root)).unwrap();
        let card = write_card(root, Some(e.cards.path())).unwrap();
        let code = recovery_code(root).unwrap();
        assert_eq!(card.code, code);
        for name in [format!("Litmus-解锁码-{kid}.pdf"), format!("Litmus-解锁码-{kid}.png"), format!("Litmus-解锁码-{kid}.txt")] {
            for f in std::fs::read_dir(e.keys.path()).unwrap() { std::fs::remove_file(f.unwrap().path()).unwrap(); }
            forget_cached();
            assert!(state(root).locked, "{name}");
            let bytes = std::fs::read(e.cards.path().join(&name)).unwrap();
            let n = unlock_with_file(root, &name, &bytes).unwrap();
            assert_eq!(n, 1, "{name}");
            assert!(!state(root).locked, "{name}");
        }
    }

    #[test]
    fn legacy_256_bit_code_still_works_and_upgrades() {
        let e = with_keys();
        let root = e.vault.path();
        put(root, "k", "v", "").unwrap();
        let kid = open_kid(&read(root)).unwrap();
        let rng = SystemRandom::new();
        let mut key = [0u8; 32];
        rng.fill(&mut key).unwrap();
        let old = hex::encode(key);
        let sealed = seal(&key, &kid, &serde_json::to_vec(&json!({"secrets":[{"name":"k","value":"v"}]})).unwrap()).unwrap();
        write_file(root, &sealed).unwrap();
        store_material(&kid, &old).unwrap();
        forget_cached();
        let code = recovery_code(root).unwrap();
        assert_eq!(hex_only(&code).len(), 64);
        assert_eq!(state(root).bits, 256);
        let new = upgrade_to_128(root).unwrap();
        assert_eq!(hex_only(&new).len(), 32);
        assert_eq!(get(root, "k").as_deref(), Some("v"));
        assert!(std::fs::read_dir(root.join(crate::SIDECAR_DIR)).unwrap().any(|f| f.unwrap().file_name().to_string_lossy().contains("pre128-")));
        assert!(unlock(root, &old).is_err());
        for f in std::fs::read_dir(e.keys.path()).unwrap() { std::fs::remove_file(f.unwrap().path()).unwrap(); }
        forget_cached();
        assert!(state(root).locked);
        assert_eq!(unlock(root, &new).unwrap(), 1);
        reset(root).unwrap();
        assert_eq!(state(root).count, 0);
        assert!(!state(root).locked);
    }

    #[test]
    fn text_code_scanner_ignores_noise() {
        let c = "edbc-4dbc-3708-6cd3-d8a3-e38d-bcce-a135";
        assert_eq!(crate::secrets_card::code_in_text("abc def zzz"), None);
        assert_eq!(crate::secrets_card::code_in_text(&format!("x {c} y")).unwrap(), hex_only(c));
    }
}
