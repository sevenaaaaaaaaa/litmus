use kernel_deploy::encrypted_backup;

#[test]
fn encrypt_decrypt_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("Notes/sub")).unwrap();
    std::fs::write(dir.path().join("Notes/a.md"), "# 测试\n\n内容").unwrap();
    std::fs::write(dir.path().join("Notes/sub/b.md"), "更多内容").unwrap();
    std::fs::write(dir.path().join("litmus.toml"), "name = \"kb\"").unwrap();

    let data = encrypted_backup::encrypt_vault(dir.path(), "mypassword123").unwrap();
    assert!(data.len() > 100, "加密后应该有实际内容");
    // 密文不等于明文
    let text = String::from_utf8_lossy(&data);
    assert!(!text.contains("测试"), "密文不应包含明文");

    let decrypted = encrypted_backup::decrypt_vault(&data, "mypassword123").unwrap();
    let s = String::from_utf8_lossy(&decrypted);
    assert!(s.contains("测试"), "解密后应包含明文: {}", &s[..80.min(s.len())]);
    assert!(s.contains("更多内容"));

    // 错误密码 → HMAC 校验失败
    assert!(encrypted_backup::decrypt_vault(&data, "wrongpass").is_err());
}

#[test]
fn v2_restore_manifest_and_tamper_detection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("Notes/sub")).unwrap();
    std::fs::write(dir.path().join("Notes/a.md"), "# 测试\n\n内容").unwrap();
    std::fs::write(dir.path().join("Notes/sub/b.md"), "更多内容").unwrap();
    std::fs::write(dir.path().join("litmus.toml"), "name = \"kb\"").unwrap();
    // 机密文件：绝不进备份包
    std::fs::create_dir_all(dir.path().join(".litmus/local")).unwrap();
    std::fs::write(dir.path().join(".litmus/local/machine.toml"), "token = \"SECRET_TOKEN_X\"").unwrap();

    let data = encrypted_backup::encrypt_vault(dir.path(), "mypassword123").unwrap();
    assert_eq!(&data[..4], b"3CBK", "应使用 v2 容器魔数");
    assert!(dir.path().join(".litmus/local/backup-signing.key").is_file(), "应生成设备签名私钥");
    let raw = String::from_utf8_lossy(&data);
    assert!(!raw.contains("SECRET_TOKEN_X"), "机密文件不得进备份包");

    // 还原到新目录：内容一致，机密文件不出现
    let out = tempfile::tempdir().unwrap();
    let n = encrypted_backup::restore_vault(&data, "mypassword123", out.path()).unwrap();
    assert_eq!(n, 3, "应还原 3 个文件（a.md / sub/b.md / litmus.toml）");
    assert!(std::fs::read_to_string(out.path().join("Notes/a.md")).unwrap().contains("测试"));
    assert!(std::fs::read_to_string(out.path().join("Notes/sub/b.md")).unwrap().contains("更多内容"));
    assert!(!out.path().join(".litmus/local/machine.toml").exists(), "机密文件不应被还原");

    // 篡改任意一个字节 → 必须被发现（AEAD 认证失败）
    let mut bad = data.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x01;
    assert!(encrypted_backup::decrypt_vault(&bad, "mypassword123").is_err(), "篡改必须被发现");
    assert!(encrypted_backup::restore_vault(&bad, "mypassword123", out.path()).is_err());

    // 错误密码
    assert!(encrypted_backup::decrypt_vault(&data, "wrongpass").is_err());
}

