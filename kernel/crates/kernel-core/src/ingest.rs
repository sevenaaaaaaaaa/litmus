//! kernel-core::ingest — 主题一键采集（内置源 + 可扩展注册表）。
//!
//! 设计：每个源是一个无密钥的公开 API（ wikipedia / hackernews / arxiv ），
//! 第三方/私有源通过 `[[connections]]`（MCP）无限扩展——"更多 API"的路就是 MCP。

use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, serde::Serialize)]
pub struct IngestDoc {
    pub title: String,
    pub text: String,
    pub url: String,
    pub mime: String,
    pub source: String,
}

pub const SOURCES: &[(&str, &str)] = &[
    ("wikipedia", "维基百科（中文）词条摘要"),
    ("hackernews", "Hacker News 热帖（英文）"),
    ("arxiv", "arXiv 论文摘要"),
];

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("litmus/0.1 (+topic ingest)")
        .build()
        .expect("http client")
}

fn clean(s: &str) -> String {
    s.replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 采集一个主题。返回导入就绪的文档列表。
pub fn ingest_topic(topic: &str, sources: &[String], limit: usize) -> Result<Vec<IngestDoc>, String> {
    let c = client();
    let mut docs = Vec::new();
    let want = |name: &str| sources.iter().any(|s| s == name);

    // ── wikipedia（zh）──
    if want("wikipedia") {
        let url = format!(
            "https://zh.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&srlimit={}&format=json",
            urlenc(topic), limit.min(8)
        );
        if let Ok(v) = c.get(&url).send().and_then(|r| r.json::<Value>()) {
            if let Some(hits) = v.pointer("/query/search").and_then(|x| x.as_array()) {
                for h in hits.iter().take(limit) {
                    let title = h["title"].as_str().unwrap_or("").to_string();
                    let snippet = h["snippet"].as_str().unwrap_or("").to_string();
                    if title.is_empty() {
                        continue;
                    }
                    // 取全文摘要
                    let sum_url = format!(
                        "https://zh.wikipedia.org/api/rest_v1/page/summary/{}",
                        urlenc(&title)
                    );
                    let text = c
                        .get(&sum_url)
                        .send()
                        .ok()
                        .and_then(|r| r.json::<Value>().ok())
                        .and_then(|v| v["extract"].as_str().map(|s| s.to_string()))
                        .unwrap_or(snippet);
                    if text.len() < 40 {
                        continue;
                    }
                    docs.push(IngestDoc {
                        url: format!("https://zh.wikipedia.org/wiki/{}", urlenc(&title)),
                        title,
                        text,
                        mime: "text/markdown".into(),
                        source: "wikipedia".into(),
                    });
                }
            }
        }
    }

    // ── hackernews ──
    if want("hackernews") {
        let url = format!(
            "https://hn.algolia.com/api/v1/search?query={}&hitsPerPage={}",
            urlenc(topic), limit.min(10)
        );
        if let Ok(v) = c.get(&url).send().and_then(|r| r.json::<Value>()) {
            if let Some(hits) = v["hits"].as_array() {
                for h in hits.iter().take(limit) {
                    let title = h["title"].as_str().unwrap_or("").to_string();
                    if title.is_empty() {
                        continue;
                    }
                    let story = h["story_text"].as_str().or_else(|| h["comment_text"].as_str()).unwrap_or("");
                    let link = h["url"].as_str().unwrap_or("");
                    let points = h["points"].as_u64().unwrap_or(0);
                    let text = format!(
                        "{}\n\n- 链接：{}\n- 热度：{} 分\n- 来源：Hacker News",
                        clean(story),
                        link,
                        points
                    );
                    docs.push(IngestDoc {
                        title,
                        text,
                        url: if link.is_empty() {
                            format!("https://news.ycombinator.com/item?id={}", h["objectID"].as_str().unwrap_or(""))
                        } else {
                            link.to_string()
                        },
                        mime: "text/markdown".into(),
                        source: "hackernews".into(),
                    });
                }
            }
        }
    }

    // ── arxiv（Atom XML，手工解析）──
    if want("arxiv") {
        let url = format!(
            "http://export.arxiv.org/api/query?search_query=all:{}&max_results={}",
            urlenc(topic), limit.min(8)
        );
        if let Ok(xml) = c.get(&url).send().and_then(|r| r.text()) {
            for entry in xml.split("<entry>").skip(1) {
                let title = between(entry, "<title>", "</title>").trim().to_string();
                let summary = between(entry, "<summary>", "</summary>").trim().to_string();
                let link = between(entry, "<id>", "</id>").trim().to_string();
                if title.is_empty() || summary.is_empty() {
                    continue;
                }
                docs.push(IngestDoc {
                    title,
                    text: clean(&summary),
                    url: link,
                    mime: "text/markdown".into(),
                    source: "arxiv".into(),
                });
                if docs.iter().filter(|d| d.source == "arxiv").count() >= limit.min(8) {
                    break;
                }
            }
        }
    }

    if docs.is_empty() {
        return Err(format!(
            "主题「{topic}」没有采到内容（源：{}）。检查网络或改用 MCP 连接器扩展源。",
            sources.join("/")
        ));
    }
    Ok(docs)
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> &'a str {
    s.find(open)
        .and_then(|i| s[i + open.len()..].find(close).map(|j| &s[i + open.len()..i + open.len() + j]))
        .unwrap_or("")
}

pub fn urlenc_pub(s: &str) -> String {
    urlenc(s)
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
