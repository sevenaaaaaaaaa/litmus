//! 解锁卡：把凭证库的解锁码做成可以带走的文件。
//!
//! 三种形态，内容相同（都是那一串解锁码）：
//! - txt：纯文本，最容易读；
//! - png：二维码，手机/另一台电脑拍一下或上传即可识别；
//! - pdf：一张 A4 卡，二维码 + 人可读的码，适合打印/归档。
//!
//! 识别（code_from_bytes）不依赖 OCR：PNG/JPEG 走二维码解码，PDF 走文本层，
//! 文本文件直接找 32/64 位十六进制。所以「上传解锁卡」在截图、拍照、打印件上都成立。

use image::{GrayImage, Luma};
use std::path::{Path, PathBuf};

/// 一张卡写出来的三个文件
#[derive(Debug, Clone, serde::Serialize)]
pub struct Card {
    pub code: String,
    pub txt: String,
    pub png: String,
    pub pdf: String,
}

fn esc_pdf(s: &str) -> String {
    s.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)")
}

/// 二维码矩阵 -> 灰度图（每个模块 scale 像素，四周留 4 模块静区）
fn qr_image(code: &str, scale: u32) -> Result<GrayImage, String> {
    let qr = qrcode::QrCode::new(code.as_bytes()).map_err(|e| format!("生成二维码失败：{e}"))?;
    let n = qr.width() as u32;
    let colors = qr.to_colors();
    let quiet = 4u32;
    let side = (n + quiet * 2) * scale;
    let mut img = GrayImage::from_pixel(side, side, Luma([255u8]));
    for y in 0..n {
        for x in 0..n {
            let dark = colors[(y * n + x) as usize] == qrcode::Color::Dark;
            if !dark {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let px = (x + quiet) * scale + dx;
                    let py = (y + quiet) * scale + dy;
                    img.put_pixel(px, py, Luma([0u8]));
                }
            }
        }
    }
    Ok(img)
}

/// 一页 PDF：标题 + 说明 + 大号等宽码 + 右侧二维码。
fn pdf_bytes(code: &str, qr: &GrayImage) -> Result<Vec<u8>, String> {
    let (w, h) = (qr.width(), qr.height());
    let mut raw = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            raw.push(qr.get_pixel(x, y)[0]);
        }
    }
    use std::io::Write;
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&raw).map_err(|e| e.to_string())?;
    let z = enc.finish().map_err(|e| e.to_string())?;

    let grouped = group4(code);
    let lines = [
        "LITMUS  /  CREDENTIALS VAULT UNLOCK CARD",
        "",
        "This file is a key. Anyone who has it can open your credentials vault.",
        "Keep it private. Do not put it in the vault folder, git, or a shared drive.",
        "",
        "On a new device: open Litmus, go to the Vault panel, upload this card.",
        "The device remembers the code after one successful unlock.",
        "",
    ];
    let mut text = String::new();
    let mut y: i64 = 790;
    text.push_str(&format!("BT /F1 13 Tf 56 {y} Td ({}) Tj ET\n", esc_pdf(lines[0])));
    y -= 28;
    for l in &lines[1..] {
        if !l.is_empty() {
            text.push_str(&format!("BT /F1 10 Tf 56 {y} Td ({}) Tj ET\n", esc_pdf(l)));
        }
        y -= 16;
    }
    y -= 10;
    text.push_str(&format!("BT /F1 9 Tf 56 {y} Td (Unlock code:) Tj ET\n"));
    y -= 24;
    for (i, part) in grouped.split('-').enumerate() {
        let row = (i / 4) as i64;
        let col = (i % 4) as i64;
        let px = 56 + col * 132;
        let py = y - row * 22;
        text.push_str(&format!("BT /F2 14 Tf {px} {py} Td ({}) Tj ET\n", esc_pdf(part)));
    }
    // 整串码也写一行：文本层里必须能整串找到，上传 PDF 才能不靠 OCR 认出来
    text.push_str(&format!("BT /F2 10 Tf 56 620 Td (code: {}) Tj ET\n", esc_pdf(&grouped)));
    let draw_w = 150.0f32;
    let draw_h = 150.0f32;
    let ix = 380.0f32;
    let iy = 300.0f32;
    text.push_str(&format!("q {draw_w} 0 0 {draw_h} {ix} {iy} cm /Im0 Do Q\n"));

    let content = text.into_bytes();
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets: Vec<usize> = Vec::new();
    let mut push_obj = |out: &mut Vec<u8>, body: Vec<u8>| {
        offsets.push(out.len());
        let n = offsets.len();
        out.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(b"\nendobj\n");
    };
    push_obj(&mut out, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    push_obj(&mut out, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    push_obj(&mut out, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R /F2 5 0 R >> /XObject << /Im0 6 0 R >> >> /Contents 7 0 R >>".to_vec());
    push_obj(&mut out, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec());
    push_obj(&mut out, b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier-Bold /Encoding /WinAnsiEncoding >>".to_vec());
    let mut img_obj = Vec::new();
    img_obj.extend_from_slice(format!("<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /Length {} >>\nstream\n", z.len()).as_bytes());
    img_obj.extend_from_slice(&z);
    img_obj.extend_from_slice(b"\nendstream");
    push_obj(&mut out, img_obj);
    let mut cont = Vec::new();
    cont.extend_from_slice(format!("<< /Length {} >>\nstream\n", content.len()).as_bytes());
    cont.extend_from_slice(&content);
    cont.extend_from_slice(b"endstream");
    push_obj(&mut out, cont);

    let xref_pos = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n", offsets.len() + 1).as_bytes());
    Ok(out)
}

pub fn hex_only(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_hexdigit()).collect()
}

pub fn group4(code: &str) -> String {
    let h: String = code.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    h.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("-")
}

/// 写一张卡到 dir 下，name 作为文件名前缀。返回三个文件的完整路径。
pub fn write_card(code: &str, dir: &Path, name: &str) -> Result<Card, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建不了目录 {}：{e}", dir.display()))?;
    let txt = dir.join(format!("{name}.txt"));
    let png = dir.join(format!("{name}.png"));
    let pdf = dir.join(format!("{name}.pdf"));
    let bits = if hex_only(code).len() == 64 { "256 位（旧版）" } else { "128 位" };
    let body = format!(
        "Litmus 凭证库 · 解锁卡\n\n解锁码（{}）：\n{}\n\n这串码等于你的钥匙：谁拿到它，谁就能打开这份凭证库。\n不要放进库文件夹、不要提交到 git、不要上传到公开网盘。\n\n换设备时：打开 Litmus → 凭证库 → 上传解锁卡（也可直接粘贴这串码）。\n解开一次，这台设备就会记住。\n",
        bits,
        group4(code)
    );
    std::fs::write(&txt, &body).map_err(|e| e.to_string())?;
    let qr = qr_image(code, 8)?;
    qr.save(&png).map_err(|e| format!("写 PNG 失败：{e}"))?;
    let pdf_data = pdf_bytes(code, &qr)?;
    std::fs::write(&pdf, &pdf_data).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&txt, &png, &pdf] {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(Card { code: code.to_string(), txt: txt.to_string_lossy().into_owned(), png: png.to_string_lossy().into_owned(), pdf: pdf.to_string_lossy().into_owned() })
}

/// 从一串文本里找解锁码：32 位（128 bit，八组四字）或旧的 64 位（256 bit）。
pub fn code_in_text(text: &str) -> Option<String> {
    let mut run: Vec<char> = Vec::new();
    for c in text.chars() {
        if c.is_ascii_hexdigit() || c == '-' {
            run.push(c);
            if run.len() > 120 {
                run.clear();
            }
            continue;
        }
        let h: String = run.drain(..).filter(|c| c.is_ascii_hexdigit()).collect();
        if h.len() == 32 || h.len() == 64 {
            return Some(h);
        }
    }
    let h: String = run.into_iter().filter(|c| c.is_ascii_hexdigit()).collect();
    if h.len() == 32 || h.len() == 64 { Some(h) } else { None }
}

fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut d = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    d.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// PDF：把每条 stream 解出来（FlateDecode 就解压，别的原样），在文本层里找码。
fn code_in_pdf(bytes: &[u8]) -> Option<String> {
    let buf = String::from_utf8_lossy(bytes).into_owned();
    if let Some(c) = code_in_text(&buf) {
        return Some(c);
    }
    let mut out = String::new();
    let mut i = 0usize;
    while let Some(p) = buf[i..].find("stream") {
        let mut start = i + p + "stream".len();
        if buf[start..].starts_with("\r\n") {
            start += 2;
        } else if buf[start..].starts_with('\n') || buf[start..].starts_with('\r') {
            start += 1;
        }
        let Some(e) = buf[start..].find("endstream") else { break; };
        let chunk = &bytes[start.min(bytes.len())..(start + e).min(bytes.len())];
        if let Ok(d) = inflate(chunk) {
            out.push_str(&String::from_utf8_lossy(&d));
            out.push('\n');
        }
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
        i = start + e + "endstream".len();
    }
    code_in_text(&out)
}

fn code_in_image(bytes: &[u8]) -> Option<String> {
    let img = image::load_from_memory(bytes).ok()?.to_luma8();
    let (w, h) = img.dimensions();
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(w as usize, h as usize, |x, y| img.get_pixel(x as u32, y as u32)[0]);
    for grid in prepared.detect_grids() {
        if let Ok((_, s)) = grid.decode() {
            if let Some(c) = code_in_text(&s) {
                return Some(c);
            }
        }
    }
    None
}

/// 识别一个上传的解锁卡文件：PNG/JPEG（二维码）、PDF（文本层）、其它（纯文本）。
pub fn code_from_bytes(name: &str, bytes: &[u8]) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let is_img = lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") || bytes.starts_with(&[0x89, b'P', b'N', b'G']) || bytes.starts_with(&[0xFF, 0xD8]);
    if is_img {
        if let Some(c) = code_in_image(bytes) {
            return Some(c);
        }
    }
    if lower.ends_with(".pdf") || bytes.starts_with(b"%PDF") {
        if let Some(c) = code_in_pdf(bytes) {
            return Some(c);
        }
    }
    code_in_text(&String::from_utf8_lossy(bytes))
}

/// 默认的卡片目录：桌面端放在文稿里，用户找得到、能 AirDrop 走；其它平台放用户目录。
pub fn default_card_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LITMUS_UNLOCK_CARD_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Documents").join("Litmus-解锁卡")
    } else {
        home.join(".config").join("litmus").join("unlock-cards")
    }
}
