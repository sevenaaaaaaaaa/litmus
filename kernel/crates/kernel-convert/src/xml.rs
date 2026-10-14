//! 极简 XML → DOM（OOXML / ODF / XHTML 都是良构 XML）。
//! 保留前缀名（`w:p`），提供按本地名 / 全名查找的便捷方法。容错：遇到坏标签尽量继续。

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Child>,
}

#[derive(Debug, Clone)]
pub enum Child {
    E(Node),
    T(String),
}

impl Node {
    /// 本地名（去掉前缀）。
    pub fn local(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }
    pub fn is(&self, name: &str) -> bool {
        self.name == name
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    /// 按本地名取属性（忽略前缀）。
    pub fn attr_local(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.rsplit(':').next() == Some(local))
            .map(|(_, v)| v.as_str())
    }
    pub fn elems(&self) -> impl Iterator<Item = &Node> {
        self.children.iter().filter_map(|c| if let Child::E(n) = c { Some(n) } else { None })
    }
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.elems().find(|n| n.name == name)
    }
    pub fn child_local(&self, local: &str) -> Option<&Node> {
        self.elems().find(|n| n.local() == local)
    }
    /// 深度优先找第一个同名后代。
    pub fn find(&self, name: &str) -> Option<&Node> {
        for c in self.elems() {
            if c.name == name {
                return Some(c);
            }
            if let Some(x) = c.find(name) {
                return Some(x);
            }
        }
        None
    }
    pub fn find_local(&self, local: &str) -> Option<&Node> {
        for c in self.elems() {
            if c.local() == local {
                return Some(c);
            }
            if let Some(x) = c.find_local(local) {
                return Some(x);
            }
        }
        None
    }
    /// 所有同名后代（文档序）。
    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        for c in self.elems() {
            if c.name == name {
                out.push(c);
            }
            c.find_all(name, out);
        }
    }
    pub fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            match c {
                Child::T(t) => s.push_str(t),
                Child::E(n) => s.push_str(&n.text()),
            }
        }
        s
    }
}

pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(j) = tail[..tail.len().min(12)].find(';') {
            let ent = &tail[1..j];
            let rep = match ent {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
                e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(c) = rep {
                out.push(c);
                rest = &tail[j + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            // XML 1.0 不允许的控制字符直接丢
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' && c != '\r' => {}
            c => o.push(c),
        }
    }
    o
}

/// 解析为根节点（返回一个虚拟根，children 是文档顶层元素）。
pub fn parse(src: &str) -> Node {
    let b = src.as_bytes();
    let mut stack: Vec<Node> = vec![Node { name: "#root".into(), ..Default::default() }];
    let mut i = 0;
    let n = b.len();
    while i < n {
        if b[i] == b'<' {
            if src[i..].starts_with("<!--") {
                i = src[i..].find("-->").map(|j| i + j + 3).unwrap_or(n);
                continue;
            }
            if src[i..].starts_with("<![CDATA[") {
                let end = src[i..].find("]]>").map(|j| i + j).unwrap_or(n);
                let t = &src[i + 9..end];
                stack.last_mut().unwrap().children.push(Child::T(t.to_string()));
                i = (end + 3).min(n);
                continue;
            }
            if src[i..].starts_with("<?") || src[i..].starts_with("<!") {
                i = src[i..].find('>').map(|j| i + j + 1).unwrap_or(n);
                continue;
            }
            // 找标签结束（属性值里可能有 >）
            let mut j = i + 1;
            let mut q: u8 = 0;
            while j < n {
                let c = b[j];
                if q != 0 {
                    if c == q {
                        q = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    q = c;
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            let inner = &src[i + 1..j.min(n)];
            i = (j + 1).min(n);
            if let Some(name) = inner.strip_prefix('/') {
                let name = name.trim();
                // 弹到匹配的开标签（容错：中间未闭合的一并收）
                if stack.iter().skip(1).any(|x| x.name == name) {
                    while stack.len() > 1 {
                        let node = stack.pop().unwrap();
                        let done = node.name == name;
                        stack.last_mut().unwrap().children.push(Child::E(node));
                        if done {
                            break;
                        }
                    }
                }
                continue;
            }
            let self_close = inner.ends_with('/');
            let inner = inner.trim_end_matches('/');
            let (name, attr_src) = match inner.find(|c: char| c.is_whitespace()) {
                Some(k) => (&inner[..k], &inner[k..]),
                None => (inner, ""),
            };
            let node = Node { name: name.to_string(), attrs: parse_attrs(attr_src), children: Vec::new() };
            if self_close {
                stack.last_mut().unwrap().children.push(Child::E(node));
            } else {
                stack.push(node);
            }
        } else {
            let end = src[i..].find('<').map(|j| i + j).unwrap_or(n);
            let t = &src[i..end];
            if !t.is_empty() {
                stack.last_mut().unwrap().children.push(Child::T(unescape(t)));
            }
            i = end;
        }
    }
    while stack.len() > 1 {
        let node = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(Child::E(node));
    }
    stack.pop().unwrap()
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let ks = i;
        while i < b.len() && b[i] != b'=' && !(b[i] as char).is_whitespace() {
            i += 1;
        }
        let key = s[ks..i].to_string();
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            if !key.is_empty() {
                out.push((key, String::new()));
            }
            continue;
        }
        i += 1;
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let q = b[i];
        let val = if q == b'"' || q == b'\'' {
            let vs = i + 1;
            let ve = s[vs..].find(q as char).map(|k| vs + k).unwrap_or(b.len());
            i = (ve + 1).min(b.len());
            &s[vs..ve]
        } else {
            let vs = i;
            while i < b.len() && !(b[i] as char).is_whitespace() {
                i += 1;
            }
            &s[vs..i]
        };
        if !key.is_empty() {
            out.push((key, unescape(val)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_ooxml_like() {
        let r = parse(r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body><w:p><w:r><w:t xml:space="preserve">鹿 &amp; 蕊 &#x4E2D;</w:t></w:r></w:p><w:p/><!-- c --><w:x a='1>2'/></w:body></w:document>"#);
        let doc = r.child("w:document").unwrap();
        let body = doc.child_local("body").unwrap();
        assert_eq!(body.elems().count(), 3);
        assert_eq!(body.text(), "鹿 & 蕊 中");
        assert_eq!(body.find("w:x").unwrap().attr("a"), Some("1>2"));
        assert_eq!(body.find("w:t").unwrap().attr_local("space"), Some("preserve"));
    }
}
