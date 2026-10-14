//! 最小 ZIP 读写（OOXML / ODF / EPUB 都是 zip）：只依赖 flate2。
//!
//! 读：按中央目录定位（不信本地头的大小，兼容数据描述符），支持 stored / deflate。
//! 写：deflate 压缩 + CRC32；`mimetype` 这类必须 stored 的条目由调用方指定。

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use std::collections::BTreeMap;
use std::io::{Read, Write};

#[derive(Debug, thiserror::Error)]
pub enum ZipError {
    #[error("不是有效的 zip 文件")]
    NotZip,
    #[error("zip 条目损坏：{0}")]
    Corrupt(String),
    #[error("不支持的压缩方式 {0}")]
    Method(u16),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// 已解析的 zip：条目名 → (方法, 压缩数据起点, 压缩长度, 原始长度)。
pub struct ZipReader<'a> {
    data: &'a [u8],
    entries: BTreeMap<String, (u16, usize, usize, usize)>,
    order: Vec<String>,
}

impl<'a> ZipReader<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self, ZipError> {
        // 末尾找 EOCD（最多 64KB 注释）
        let min = data.len().saturating_sub(22 + 65535);
        let mut eocd = None;
        let mut i = data.len().saturating_sub(22);
        loop {
            if u32le(data, i) == Some(0x0605_4b50) {
                eocd = Some(i);
                break;
            }
            if i == 0 || i <= min {
                break;
            }
            i -= 1;
        }
        let eocd = eocd.ok_or(ZipError::NotZip)?;
        let count = u16le(data, eocd + 10).ok_or(ZipError::NotZip)? as usize;
        let cd_off = u32le(data, eocd + 16).ok_or(ZipError::NotZip)? as usize;
        let mut entries = BTreeMap::new();
        let mut order = Vec::new();
        let mut p = cd_off;
        for _ in 0..count {
            if u32le(data, p) != Some(0x0201_4b50) {
                return Err(ZipError::Corrupt("中央目录".into()));
            }
            let method = u16le(data, p + 10).unwrap_or(0);
            let csize = u32le(data, p + 20).unwrap_or(0) as usize;
            let usize_ = u32le(data, p + 24).unwrap_or(0) as usize;
            let nlen = u16le(data, p + 28).unwrap_or(0) as usize;
            let xlen = u16le(data, p + 30).unwrap_or(0) as usize;
            let clen = u16le(data, p + 32).unwrap_or(0) as usize;
            let local = u32le(data, p + 42).unwrap_or(0) as usize;
            let name = String::from_utf8_lossy(data.get(p + 46..p + 46 + nlen).unwrap_or_default()).replace('\\', "/");
            // 本地头：数据起点 = 30 + 名长 + 扩展长
            let lnlen = u16le(data, local + 26).unwrap_or(0) as usize;
            let lxlen = u16le(data, local + 28).unwrap_or(0) as usize;
            let start = local + 30 + lnlen + lxlen;
            if !name.ends_with('/') {
                order.push(name.clone());
                entries.insert(name, (method, start, csize, usize_));
            }
            p += 46 + nlen + xlen + clen;
        }
        Ok(ZipReader { data, entries, order })
    }

    pub fn names(&self) -> &[String] {
        &self.order
    }

    pub fn has(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn read(&self, name: &str) -> Option<Vec<u8>> {
        let (method, start, csize, usize_) = *self.entries.get(name).or_else(|| {
            // 大小写不敏感兜底（部分生成器路径大小写不一致）
            let low = name.to_ascii_lowercase();
            self.entries.iter().find(|(k, _)| k.to_ascii_lowercase() == low).map(|(_, v)| v)
        })?;
        let raw = self.data.get(start..start + csize)?;
        match method {
            0 => Some(raw.to_vec()),
            8 => {
                let mut out = Vec::with_capacity(usize_);
                DeflateDecoder::new(raw).read_to_end(&mut out).ok()?;
                Some(out)
            }
            _ => None,
        }
    }

    pub fn read_string(&self, name: &str) -> Option<String> {
        self.read(name).map(|b| {
            let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&b).to_vec();
            String::from_utf8_lossy(&b).into_owned()
        })
    }
}

// ───────────────────────── 写 ─────────────────────────

fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c = t[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

#[derive(Default)]
pub struct ZipWriter {
    buf: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

impl ZipWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加条目；`store=true` 不压缩（ODF/EPUB 的 mimetype 必须如此且排第一）。
    pub fn add(&mut self, name: &str, data: &[u8], store: bool) {
        let crc = crc32(data);
        let (method, payload) = if store {
            (0u16, data.to_vec())
        } else {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
            let _ = e.write_all(data);
            (8u16, e.finish().unwrap_or_default())
        };
        let off = self.buf.len() as u32;
        let n = name.as_bytes();
        // 固定时间戳 1980-01-01：同内容产物字节一致
        let (time, date) = (0u16, 0x0021u16);
        let mut h = Vec::new();
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&20u16.to_le_bytes());
        h.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 文件名
        h.extend_from_slice(&method.to_le_bytes());
        h.extend_from_slice(&time.to_le_bytes());
        h.extend_from_slice(&date.to_le_bytes());
        h.extend_from_slice(&crc.to_le_bytes());
        h.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        h.extend_from_slice(&(n.len() as u16).to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&h);
        self.buf.extend_from_slice(n);
        self.buf.extend_from_slice(&payload);

        let c = &mut self.central;
        c.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        c.extend_from_slice(&20u16.to_le_bytes());
        c.extend_from_slice(&20u16.to_le_bytes());
        c.extend_from_slice(&0x0800u16.to_le_bytes());
        c.extend_from_slice(&method.to_le_bytes());
        c.extend_from_slice(&time.to_le_bytes());
        c.extend_from_slice(&date.to_le_bytes());
        c.extend_from_slice(&crc.to_le_bytes());
        c.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        c.extend_from_slice(&(data.len() as u32).to_le_bytes());
        c.extend_from_slice(&(n.len() as u16).to_le_bytes());
        c.extend_from_slice(&[0u8; 8]); // extra, comment, disk, int attr
        c.extend_from_slice(&0u32.to_le_bytes()); // ext attr
        c.extend_from_slice(&off.to_le_bytes());
        c.extend_from_slice(n);
        self.count += 1;
    }

    pub fn finish(mut self) -> Vec<u8> {
        let cd_off = self.buf.len() as u32;
        let cd_len = self.central.len() as u32;
        self.buf.extend_from_slice(&self.central);
        self.buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        self.buf.extend_from_slice(&[0u8; 4]);
        self.buf.extend_from_slice(&self.count.to_le_bytes());
        self.buf.extend_from_slice(&self.count.to_le_bytes());
        self.buf.extend_from_slice(&cd_len.to_le_bytes());
        self.buf.extend_from_slice(&cd_off.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_crc() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let mut w = ZipWriter::new();
        w.add("mimetype", b"application/vnd.oasis.opendocument.text", true);
        w.add("内容/中文.xml", "<a>鹿蕊</a>".repeat(50).as_bytes(), false);
        let bytes = w.finish();
        let r = ZipReader::new(&bytes).unwrap();
        assert_eq!(r.names(), &["mimetype".to_string(), "内容/中文.xml".to_string()]);
        assert_eq!(r.read_string("mimetype").unwrap(), "application/vnd.oasis.opendocument.text");
        assert_eq!(r.read_string("内容/中文.xml").unwrap(), "<a>鹿蕊</a>".repeat(50));
        assert!(ZipReader::new(b"not a zip at all").is_err());
    }
}
