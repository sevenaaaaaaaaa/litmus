//! S3 直传的集成测试：起一个极简 HTTP 服务当"桶"，验证
//! 真的发出了带 SigV4 的 PUT、且内容未变时跳过。
use kernel_deploy::DeployState;
use kernel_store::PublishTarget;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // 读头
    loop {
        let n = stream.read(&mut tmp).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    // 按 Content-Length 读体
    let cl = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let header_len = head.find("\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    let mut got = buf.len().saturating_sub(header_len);
    while got < cl {
        let n = stream.read(&mut tmp).unwrap_or(0);
        if n == 0 {
            break;
        }
        got += n;
    }
    head
}

#[test]
fn s3_upload_signs_every_request_and_is_incremental() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let head = read_request(&mut s);
            let _ = tx.send(head);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    std::fs::create_dir_all(site.join("Assets/ab/cd")).unwrap();
    std::fs::write(site.join("index.html"), "<h1>首页</h1>").unwrap();
    std::fs::write(site.join("Assets/ab/cd/x.png"), b"\x89PNG").unwrap();

    std::env::set_var("LITMUS_TEST_S3_KEY", "AKIDEXAMPLE");
    std::env::set_var("LITMUS_TEST_S3_SECRET", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
    let target = PublishTarget {
        name: "mock".into(),
        kind: "s3".into(),
        endpoint: Some(format!("http://{addr}")),
        bucket: Some("kb".into()),
        region: Some("us-east-1".into()),
        access_key_env: Some("LITMUS_TEST_S3_KEY".into()),
        secret_key_env: Some("LITMUS_TEST_S3_SECRET".into()),
        prefix: Some("site".into()),
        ..Default::default()
    };

    let mut state = DeployState::default();
    let r1 = kernel_deploy::s3::deploy_s3(&site, &target, &mut state).unwrap();
    assert_eq!(r1.uploaded, 2, "{r1:?}");
    assert_eq!(r1.skipped, 0);
    assert_eq!(r1.kind, "s3");

    // 收到的请求确实带 SigV4 签名与内容哈希，且是 path-style + prefix
    let mut seen = Vec::new();
    while let Ok(h) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
        seen.push(h);
    }
    assert_eq!(seen.len(), 2, "应收到两个 PUT");
    for head in &seen {
        let lower = head.to_lowercase();
        assert!(head.starts_with("PUT "), "必须是 PUT：{head}");
        assert!(lower.contains("authorization: aws4-hmac-sha256 credential=akidexample/"));
        assert!(lower.contains("x-amz-content-sha256:"));
        assert!(lower.contains("x-amz-date:"));
        assert!(head.contains("/kb/site/"), "path-style + prefix：{head}");
    }
    assert!(seen.iter().any(|h| h.contains("index.html")));
    // 中文文件名要点：路径里出现的是百分号编码
    assert!(seen.iter().any(|h| h.contains("Assets/ab/cd/x.png")));

    // 再次部署：内容未变 → 全部跳过，且没有新的请求
    let r2 = kernel_deploy::s3::deploy_s3(&site, &target, &mut state).unwrap();
    assert_eq!(r2.uploaded, 0);
    assert_eq!(r2.skipped, 2);
    assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(), "不应再发请求");

    // 改一个文件 → 只上传那一个
    std::fs::write(site.join("index.html"), "<h1>改过了</h1>").unwrap();
    let r3 = kernel_deploy::s3::deploy_s3(&site, &target, &mut state).unwrap();
    assert_eq!(r3.uploaded, 1);
    assert_eq!(r3.skipped, 1);
}
