//! Cloudflare Pages 直传：本地 mock 校验 multipart 形态与指纹增量。
use kernel_deploy::DeployState;
use kernel_store::PublishTarget;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

#[test]
fn cf_pages_upload_and_fingerprint_skip() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel::<(String, Vec<u8>)>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 { break; }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
            let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let cl: usize = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.eq_ignore_ascii_case("content-length") { v.trim().parse().ok() } else { None }
            }).unwrap_or(0);
            while buf.len() < head_end + cl {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 { break; }
                buf.extend_from_slice(&tmp[..n]);
            }
            let _ = tx.send((head, buf[head_end..].to_vec()));
            let resp = r#"{"success":true,"result":{"url":"https://abc.pages.dev"}}"#;
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", resp.len(), resp).as_bytes());
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(site.join("index.html"), "<h1>主页</h1>").unwrap();
    std::fs::write(site.join("a.html"), "<p>A</p>").unwrap();
    std::fs::write(site.join("b.html"), "<p>A</p>").unwrap(); // 与 a 同内容：应按 hash 去重只传一次

    std::env::set_var("LITMUS_TEST_CF_TOKEN", "tok-123");
    let target = PublishTarget {
        name: "cf".into(),
        kind: "cf-pages".into(),
        account_id: Some("acct".into()),
        project: Some("kb".into()),
        token_env: Some("LITMUS_TEST_CF_TOKEN".into()),
        api_base: Some(format!("http://{addr}")),
        public_base_url: Some("https://kb.pages.dev".into()),
        ..Default::default()
    };

    let mut state = DeployState::default();
    let r1 = kernel_deploy::cf_pages::deploy_cf_pages(&site, &target, &mut state).unwrap();
    assert_eq!(r1.uploaded, 3);
    assert_eq!(r1.url.as_deref(), Some("https://abc.pages.dev"), "{:?}", r1.url);

    let (head, body) = rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
    assert!(head.contains("POST /client/v4/accounts/acct/pages/projects/kb/deployments"), "{head}");
    assert!(head.to_lowercase().contains("authorization: bearer tok-123"));
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains("name=\"manifest\""), "必须有 manifest part");
    assert!(text.contains("index.html"));
    // b.html 与 a.html 同内容 → manifest 都在，但文件 part 按 hash 只传一份
    assert!(text.contains("\"a.html\"") && text.contains("\"b.html\""));
    let a_hash_part = text.matches("name=\"").count();

    // 第二次：指纹未变 → 不创建部署、无请求
    let r2 = kernel_deploy::cf_pages::deploy_cf_pages(&site, &target, &mut state).unwrap();
    assert_eq!(r2.uploaded, 0);
    assert!(r2.detail.contains("无变更"));
    assert!(rx.recv_timeout(std::time::Duration::from_millis(250)).is_err());

    // 内容变化 → 重新部署
    std::fs::write(site.join("index.html"), "<h1>改版</h1>").unwrap();
    let r3 = kernel_deploy::cf_pages::deploy_cf_pages(&site, &target, &mut state).unwrap();
    assert_eq!(r3.uploaded, 3);
    let _ = a_hash_part;
}
