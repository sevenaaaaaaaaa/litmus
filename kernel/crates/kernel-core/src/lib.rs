//! kernel-core: 内核门面。组装 store + md + sync + index，对 CLI/桌面端/server 暴露统一操作。

pub mod field;
pub mod ingest;
pub mod watch;

pub use kernel_browser::{
    Backend as BrowserBackend, BrowserError, DIGEST_JS, dump_dom, ego_digest, fetch_html,
    find_chrome, find_ego,
};
pub use kernel_design::{
    Archetype, DesignError, DesignProfile, DesignStore, apply_to_html, digest_html, digest_page,
    import_path, parse_css, parse_design_md,
};
pub use kernel_md::{Block, DocModel, from_markdown, to_html, to_markdown};
pub use kernel_md::tidy;
pub use kernel_store::{
    AiConfig, BrowserConfig, Cas, ConnectionConfig, PublishConfig, PublishTarget, StoreError, Vault,
    VaultConfig, events_dir, list_docs, new_doc_id, refs,
};
pub use kernel_sync::{OpLog, SyncError, VERBATIM_EXTS, is_html_rel, is_verbatim_rel};

use kernel_store::index::Index;
use std::fs;

/// 附件元数据。rel 是相对库根的路径（真相区 Assets/ 下）。
#[derive(Debug, Clone, PartialEq)]
pub struct AssetMeta {
    pub hash: String,
    pub rel: String,
    pub mime: String,
    pub size: usize,
}

/// 高层库操作：一次处理“文件 + op-log + 索引”三层。
pub struct Kernel {
    pub vault: Vault,
    pub log: OpLog,
    index: Index,
    designs: DesignStore,
    /// 上次全量同步时刻（读路径节流用）。
    last_sync: Option<std::time::Instant>,
    /// 文件监听置位的脏标记：Notes/ 下有任何外部改动即置 true，
    /// 读路径据此决定是否真的需要全库 walk（无事发生时零开销）。
    dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 文件监听句柄，存活即监听生效；丢弃即停止。
    _watcher: Option<notify::RecommendedWatcher>,
    /// 数据代数：任何内容变更（sync 命中 / 写 / 删）+1，供上层缓存失效。
    epoch: u64,
    /// 混合检索的 TF-IDF 语料缓存，键为 (文档数, 最大 mtime)。
    hybrid_cache: Option<((usize, i64), rag::TfidfIndex)>,
    /// 密向量缓存，键为 (文档数, 最大 mtime, 模型)。
    dense_cache: Option<((usize, i64), rag::DenseIndex)>,
}

impl Kernel {
    pub fn open(vault: Vault) -> Result<Self, StoreError> {
        let index = Index::open(&vault)?;
        let designs = DesignStore::open(&vault.sidecar())
            .map_err(|e| StoreError::Config(e.to_string()))?;
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _watcher = Self::spawn_watcher(vault.root.join("Notes"), dirty.clone());
        Ok(Kernel {
            vault,
            log: OpLog::new(),
            index,
            designs,
            last_sync: None,
            dirty,
            _watcher,
            epoch: 0,
            hybrid_cache: None,
            dense_cache: None,
        })
    }

    /// Notes/ 递归监听：任何事件置脏标记。监听失败只降级（回到周期兜底），不阻断开库。
    fn spawn_watcher(
        dir: std::path::PathBuf,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<notify::RecommendedWatcher> {
        use notify::Watcher;
        use std::sync::atomic::Ordering;
        let mut watcher = notify::recommended_watcher(move |_res| {
            flag.store(true, Ordering::Relaxed);
        })
        .ok()?;
        watcher.watch(&dir, notify::RecursiveMode::Recursive).ok()?;
        Some(watcher)
    }

    /// 读路径专用：按需同步。三档判定——
    /// ① 监听没报脏且 60s 内同步过 → 直接返回（浏览热路径零开销，不 walk）；
    /// ② 报脏（外部改动）→ 立即全库 walk，改动一次请求内可见；
    /// ③ 兜底：即使监听漏报，每 10 分钟也强制走一遍。
    /// UI 内写入直接更新索引不经过这里；顶栏「同步」（/sync）走 force 的 sync_all。
    pub fn sync_throttled(&mut self) -> Result<usize, SyncError> {
        use std::sync::atomic::Ordering;
        let dirty = self.dirty.load(Ordering::Relaxed);
        let recent = self
            .last_sync
            .map_or(false, |t| t.elapsed() < std::time::Duration::from_secs(60));
        if recent && !dirty {
            return Ok(0);
        }
        let periodic = self
            .last_sync
            .map_or(true, |t| t.elapsed() > std::time::Duration::from_secs(600));
        if !dirty && !periodic {
            return Ok(0);
        }
        let n = self.sync_all()?;
        self.last_sync = Some(std::time::Instant::now());
        Ok(n)
    }

    /// 当前数据代数（memo 等上层缓存的失效依据）。
    pub fn data_epoch(&self) -> u64 {
        self.epoch
    }

    /// 标记「索引刚与磁盘对齐」：启动预热分批同步完成后调用，
    /// 让读路径的 sync_throttled 进入零开销档。
    pub fn mark_synced(&mut self) {
        self.last_sync = Some(std::time::Instant::now());
    }

    /// 全库 path → (mtime, size) 快照（索引表直出，不碰文件）。
    pub fn doc_stats(&self) -> Result<std::collections::HashMap<String, (i64, i64)>, StoreError> {
        self.index.stat_map()
    }

    /// 按修改日期的文档数直方图（memo 热力图数据源）。
    pub fn date_histogram(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.index.date_histogram()
    }

    // ---------- 设计规范（一等知识对象） ----------

    /// 从文本导入设计规范（design.md / css / html 摘要），保存并返回。
    pub fn import_design_text(&self, name: &str, content: &str) -> Result<DesignProfile, SyncError> {
        let looks_html = content.trim_start().starts_with("<!doctype") || content.contains("<html");
        let looks_css = content.contains("--") && content.contains('{') && !content.contains("# ");
        let profile = if looks_html {
            digest_html(name, "(pasted html)", content)
        } else if looks_css && !content.contains("\n## ") {
            parse_css(name, "(pasted css)", content)
        } else {
            parse_design_md(name, "(pasted markdown)", content)
        };
        if profile.is_empty() {
            return Err(SyncError::Md(format!("未从「{name}」解析出任何 token/字体/规则")));
        }
        self.designs
            .save(&profile)
            .map_err(|e| SyncError::Md(e.to_string()))?;
        Ok(profile)
    }

    /// 从路径导入（文件或目录 / skill 包）。
    pub fn import_design_path(&self, path: &std::path::Path, name: Option<&str>) -> Result<DesignProfile, SyncError> {
        let profile = import_path(path, name).map_err(|e| SyncError::Md(e.to_string()))?;
        self.designs
            .save(&profile)
            .map_err(|e| SyncError::Md(e.to_string()))?;
        Ok(profile)
    }

    pub fn designs(&self) -> Result<Vec<DesignProfile>, SyncError> {
        self.designs.list().map_err(|e| SyncError::Md(e.to_string()))
    }

    pub fn active_design(&self) -> Option<DesignProfile> {
        self.designs.active().ok().flatten()
    }

    pub fn set_active_design(&self, name: &str) -> Result<(), SyncError> {
        self.designs
            .set_active(name)
            .map_err(|e| SyncError::Md(e.to_string()))
    }

    pub fn clear_active_design(&self) -> Result<(), SyncError> {
        self.designs
            .clear_active()
            .map_err(|e| SyncError::Md(e.to_string()))
    }

    /// 渲染文档为 HTML，按作用域解析有效规范（doc → publish → vault）。
    pub fn render_doc_html(&mut self, rel: &str) -> Result<String, SyncError> {
        let model = self.get_doc(rel)?;
        let html = to_html(&model);
        let profile = self
            .designs
            .resolve(Some(rel))
            .map_err(|e| SyncError::Md(e.to_string()))?;
        Ok(match profile {
            Some(p) => apply_to_html(&html, &p),
            None => html,
        })
    }

    // ---------- 设计规范作用域（库级 / 文档级 / 发布级） ----------

    pub fn set_design_scope(&self, scope: &str, name: &str) -> Result<(), SyncError> {
        self.designs
            .set_scope(scope, name)
            .map_err(|e| SyncError::Md(e.to_string()))
    }

    pub fn design_scopes(&self) -> std::collections::BTreeMap<String, String> {
        self.designs.scopes()
    }

    pub fn design_for_doc(&self, rel: &str) -> Option<DesignProfile> {
        self.designs.resolve(Some(rel)).ok().flatten()
    }

    /// 从结构化页面摘要（computed style 级）导入规范。
    pub fn import_design_digest(
        &self,
        name: &str,
        source: &str,
        digest: &serde_json::Value,
    ) -> Result<DesignProfile, SyncError> {
        let p = digest_page(name, source, digest);
        if p.is_empty() {
            return Err(SyncError::Md(format!("页面摘要未提取到任何排版信息：{source}")));
        }
        self.designs.save(&p).map_err(|e| SyncError::Md(e.to_string()))?;
        Ok(p)
    }

    /// 从渲染/抓取到的 HTML 导入规范。
    pub fn import_design_html(
        &self,
        name: &str,
        source: &str,
        html: &str,
    ) -> Result<DesignProfile, SyncError> {
        let p = digest_html(name, source, html);
        if p.is_empty() {
            return Err(SyncError::Md(format!("HTML 未提取到排版信息：{source}")));
        }
        self.designs.save(&p).map_err(|e| SyncError::Md(e.to_string()))?;
        Ok(p)
    }

    /// 已登记的非文档文件相对路径（检索结果标注 kind=file 用）。
    pub fn index_files(&self) -> Result<Vec<String>, StoreError> {
        self.index.list_files()
    }

    /// 扫描 Notes/，把所有外部改动导入 op-log 并同步全文索引。
    /// 返回发生内容变化的文档数。已删除的文档自动从索引回收。
    pub fn sync_all(&mut self) -> Result<usize, SyncError> {
        use std::sync::atomic::Ordering;
        self.dirty.store(false, Ordering::Relaxed); // walk 前清脏；walk 中的新改动会重新置位
        let (n, _) = self.sync_limited(usize::MAX)?;
        if n > 0 {
            self.epoch += 1;
        }
        Ok(n)
    }

    /// 分批同步：单次最多处理 `limit` 篇变更，返回 (changed, 是否还有剩余)。
    /// 大库（迁移后首次索引）不再阻塞 daemon——调用方循环多次即可。
    pub fn sync_limited(&mut self, limit: usize) -> Result<(usize, bool), SyncError> {
        let docs = list_docs(&self.vault).map_err(SyncError::Store)?;
        // 批量取出 mtime/size 索引（一次查询代替 N 次）
        let stats = self.index.stat_map().map_err(SyncError::Store)?;
        let mut changed = 0;
        let mut processed = 0usize;
        let mut more = false;
        for rel in &docs {
            if processed >= limit { more = true; break; }
            let rel_str = rel.to_str().unwrap_or("");
            let abs = self.vault.root.join(rel);
            // mtime+size 短路：文件没动就连读都不读（1.2 万篇时这是秒开的关键）
            let (mtime, size) = fs::metadata(&abs)
                .map(|m| (
                    m.modified().ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64).unwrap_or(0),
                    m.len() as i64,
                ))
                .unwrap_or((0, 0));
            if stats.get(rel_str) == Some(&(mtime, size)) {
                continue; // mtime+size 一致 → 文件没动，跳过（不读不哈希不查库）
            }
            let text = match fs::read_to_string(&abs) {
                Ok(t) => t,
                Err(_) => continue,
            };
            // 增量闸门：文件哈希未变 → O(1) 跳过（不再全量对比 op-log，
            // 这是 2 万+ 文档规模下同步可用的前提）
            let hash = kernel_store::Cas::hash_hex(text.as_bytes());
            if self.index.hash_of(rel_str).as_deref() == Some(hash.as_str()) {
                continue;
            }
            processed += 1;
            // 文件变了：写 op-log（失败也继续索引，索引比 op-log 更关键）
            if let Ok(before) = self.log.current_model(&self.vault, rel_str) {
                let before_md = kernel_md::to_markdown(&before);
                if before_md != text && self.log.import_file(&self.vault, rel_str, &text).is_ok() {
                    changed += 1;
                }
            }
            // 索引 upsert
            let reindexed = self
                .index
                .upsert(rel_str, &hash, &text, mtime, size)
                .map_err(SyncError::Store)?;
            if reindexed {
                let refs = refs::find_asset_refs(&text);
                self.index
                    .set_doc_assets(rel_str, &refs)
                    .map_err(SyncError::Store)?;
            }
        }
        if !more {
            // 只在最后一轮做删除回收
        }
        // 回收已删除文档的索引
        let on_disk: std::collections::HashSet<String> = docs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        for rel in self.index.list_paths().map_err(SyncError::Store)? {
            if !on_disk.contains(&rel) {
                self.index.remove(&rel).map_err(SyncError::Store)?;
            }
        }
        // 非文档文件：按文件名进检索（文件即真相——内核不解析 ≠ 检索不到）
        let files = kernel_store::list_non_doc_files(&self.vault).map_err(SyncError::Store)?;
        let mut seen_files = std::collections::HashSet::new();
        for abs in &files {
            let rel = abs
                .strip_prefix(&self.vault.root)
                .unwrap_or(abs)
                .to_string_lossy()
                .replace('\\', "/");
            let (mtime, size) = fs::metadata(abs)
                .map(|m| {
                    (
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                        m.len() as i64,
                    )
                })
                .unwrap_or((0, 0));
            seen_files.insert(rel.clone());
            if self.index.stat_file(&rel) == Some((mtime, size)) {
                continue; // mtime+size 一致 → 跳过
            }
            let _ = self.index.upsert_file(&rel, mtime, size);
        }
        for rel in self.index.list_files().map_err(SyncError::Store)? {
            if !seen_files.contains(&rel) {
                let _ = self.index.remove_file(&rel);
            }
        }
        Ok((changed, more))
    }

    /// 创建或更新一篇文档（同时写文件与 op-log，保持两侧一致）。
    pub fn put_doc(&mut self, rel: &str, md: &str) -> Result<(), SyncError> {
        let abs = self.vault.root.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, md)?;
        self.log.import_file(&self.vault, rel, md)?;
        // 物化：把确定性序列化写回，保证文件即规范形态。
        // HTML 一等文档与 verbatim（代码/结构化文本）保留作者原文（不被规范化改写），Markdown 走规范化。
        if !is_html_rel(rel) && !is_verbatim_rel(rel) {
            self.log.materialize_to_file(&self.vault, rel)?;
        }
        // 索引
        let text = fs::read_to_string(&abs).unwrap_or_default();
        let hash = kernel_store::Cas::hash_hex(text.as_bytes());
        let (mt, sz) = fs::metadata(&abs)
            .map(|m| (
                m.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64).unwrap_or(0),
                m.len() as i64,
            ))
            .unwrap_or((0, 0));
        if self
            .index
            .upsert(rel, &hash, &text, mt, sz)
            .map_err(SyncError::Store)?
        {
            let refs = refs::find_asset_refs(&text);
            self.index
                .set_doc_assets(rel, &refs)
                .map_err(SyncError::Store)?;
        }
        self.epoch += 1;
        self.last_sync = Some(std::time::Instant::now());
        self.dirty.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 附件入库：内容寻址写入真相区 Assets/，登记元数据。返回 Markdown 可直接引用的元数据。
    pub fn put_asset(&mut self, name: &str, bytes: &[u8]) -> Result<AssetMeta, SyncError> {
        let ext = match name.rsplit_once('.') {
            Some((_, e)) if !e.is_empty() && e.len() <= 8 => e.to_ascii_lowercase(),
            _ => String::new(),
        };
        let (hash, abs) = Cas::put_with_ext(&self.vault.assets_dir(), bytes, &ext)
            .map_err(SyncError::Store)?;
        let rel = abs
            .strip_prefix(&self.vault.root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        let mime = refs::mime_for_ext(&ext);
        self.index
            .upsert_asset(&hash, &rel, mime, bytes.len() as i64, now_secs())
            .map_err(SyncError::Store)?;
        Ok(AssetMeta {
            hash,
            rel,
            mime: mime.to_string(),
            size: bytes.len(),
        })
    }

    /// 生成文档内引用片段（稳定路径；换图床时无需改文档）。
    pub fn asset_markdown(meta: &AssetMeta, alt: &str) -> String {
        format!("![{alt}]({})", meta.rel)
    }

    /// 反查：哪些文档引用了该附件。
    pub fn asset_docs(&self, hash: &str) -> Result<Vec<String>, SyncError> {
        Ok(self.index.docs_for_asset(hash).map_err(SyncError::Store)?)
    }

    pub fn assets_count(&self) -> Result<usize, StoreError> {
        self.index.assets_count()
    }

    /// 采集管道落点：把外部来源的内容导入为文档。
    ///
    /// - 同一 `connection + uri` 重复采集会**更新同一篇文档**（不产生副本）。
    /// - 文档落在 `Notes/Sources/<connection>/<slug>.md`，顶部加一行可见来源，机器溯源存 items 表。
    pub fn import_capture(
        &mut self,
        connection: &str,
        uri: &str,
        title: Option<&str>,
        content: &str,
        mime: &str,
    ) -> Result<String, SyncError> {
        let rel = match self.index.item_rel(connection, uri).map_err(SyncError::Store)? {
            Some(existing) => existing,
            None => {
                let slug = slugify(title.unwrap_or(uri));
                let slug = if slug.is_empty() {
                    format!("item-{}", &Cas::hash_hex(uri.as_bytes())[..8])
                } else {
                    slug
                };
                let candidate = format!("Notes/Sources/{connection}/{slug}.md");
                let abs = self.vault.root.join(&candidate);
                if abs.exists() {
                    format!("Notes/Sources/{connection}/{slug}-{}.md", &Cas::hash_hex(uri.as_bytes())[..6])
                } else {
                    candidate
                }
            }
        };

        let mut body = String::new();
        // H1 必须在最前（标题提升依赖首行），来源行紧随其后
        let (heading, rest) = match content.lines().next() {
            Some(first) if first.starts_with("# ") => {
                let h = first[2..].trim().to_string();
                let rest = content[first.len()..].trim_start_matches('\n').to_string();
                (Some(h), rest)
            }
            Some(_) => (title.map(|t| t.to_string()), content.to_string()),
            None => (title.map(|t| t.to_string()), String::new()),
        };
        if let Some(h) = heading {
            body.push_str(&format!("# {h}\n\n"));
        }
        let source_line = format!(
            "> 来源：[{uri}]({uri})　连接：{connection}　类型：{mime}　采集：{}\n\n",
            now_secs()
        );
        body.push_str(&source_line);
        body.push_str(&rest);
        if !body.ends_with('\n') {
            body.push('\n');
        }

        self.put_doc(&rel, &body)?;
        self.index
            .upsert_item(connection, uri, &rel, now_secs())
            .map_err(SyncError::Store)?;
        Ok(rel)
    }

    /// 已采集条目数。
    pub fn items_count(&self) -> Result<usize, StoreError> {
        self.index.items_count()
    }

    // ---------- 内联视图（Views 层） ----------

    /// 解析视图数据。source: here | folder:<path> | query:<词> | all
    pub fn resolve_view(&mut self, spec: &views::ViewSpec, current: Option<&str>) -> Result<Vec<views::ViewRow>, SyncError> {
        self.sync_throttled()?;
        let mut rows: Vec<views::ViewRow> = Vec::new();
        let src = spec.source.trim();
        let root = self.vault.root.clone();

        if src == "here" {
            // 本页列表项作为行（结构化列表 → 视图）
            if let Some(rel) = current {
                if let Ok(m) = self.get_doc(rel) { let md = to_markdown(&m); rows.push(make_view_row(&root, rel, &m, &md)); }
            }
        } else if let Some(q) = src.strip_prefix("query:") {
            for (path, _score) in self.search_hybrid(q, spec.limit.max(20))? {
                if let Ok(m) = self.get_doc(&path) { let md = to_markdown(&m); rows.push(make_view_row(&root, &path, &m, &md)); }
            }
        } else {
            let prefix = src.strip_prefix("folder:").map(|p| p.trim_end_matches('/').to_string());
            for rel in list_docs(&self.vault).map_err(SyncError::Store)? {
                let rel_str = rel.to_string_lossy().into_owned();
                if let Some(pref) = &prefix {
                    if !rel_str.starts_with(pref) { continue; }
                }
                if let Ok(m) = self.get_doc(&rel_str) {
                    let md = to_markdown(&m);
                    rows.push(make_view_row(&root, &rel_str, &m, &md));
                }
            }
        }

        // 过滤：tag:xxx / text:xxx / kind:xxx / key=value（frontmatter 属性）
        if let Some(f) = &spec.filter {
            let f = f.trim();
            if let Some(t) = f.strip_prefix("tag:") {
                rows.retain(|r| r.tags.iter().any(|x| x.eq_ignore_ascii_case(t.trim())));
            } else if let Some(t) = f.strip_prefix("text:") {
                let t = t.trim().to_lowercase();
                rows.retain(|r| r.title.to_lowercase().contains(&t));
            } else if let Some(t) = f.strip_prefix("kind:") {
                rows.retain(|r| r.kind == t.trim());
            } else if let Some((k, v)) = f.split_once('=') {
                let k = k.trim().to_lowercase();
                let v = v.trim().to_lowercase();
                rows.retain(|r| {
                    r.props
                        .get(&k)
                        .map(|x| x.to_lowercase() == v)
                        .unwrap_or(false)
                });
            } else {
                let t = f.to_lowercase();
                rows.retain(|r| r.title.to_lowercase().contains(&t));
            }
        }
        // 排序：updated_asc/title/… 或 <prop>:asc|<prop>:desc（数值优先）
        match spec.sort.as_deref().unwrap_or("updated_desc") {
            "updated_asc" => rows.sort_by_key(|r| r.mtime),
            "title_asc" | "title" => rows.sort_by(|a, b| a.title.cmp(&b.title)),
            s => {
                if let Some((key, dir)) = s.rsplit_once(':') {
                    let key = key.trim().to_lowercase();
                    let asc = dir.trim().eq_ignore_ascii_case("asc");
                    rows.sort_by(|a, b| {
                        let av = a.props.get(&key).cloned().unwrap_or_default();
                        let bv = b.props.get(&key).cloned().unwrap_or_default();
                        let ord = match (av.parse::<f64>(), bv.parse::<f64>()) {
                            (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                            _ => av.to_lowercase().cmp(&bv.to_lowercase()),
                        };
                        if asc { ord } else { ord.reverse() }
                    });
                } else {
                    rows.sort_by(|a, b| b.mtime.cmp(&a.mtime));
                }
            }
        }
        rows.truncate(if spec.limit == 0 { 30 } else { spec.limit });
        // rollup：先于 formula，formula 可引用 rollup 结果
        if !spec.rollup.is_empty() {
            let mut lookup: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
            for p in list_docs(&self.vault).map_err(SyncError::Store)? {
                let rel = p.to_string_lossy().into_owned();
                let stem = rel.rsplit('/').next().unwrap_or(&rel).trim_end_matches(".md").to_lowercase();
                lookup.entry(stem).or_insert_with(|| rel.clone());
                lookup.entry(rel.to_lowercase()).or_insert_with(|| rel.clone());
                let no_ext = rel.trim_end_matches(".md").to_lowercase();
                lookup.entry(no_ext).or_insert_with(|| rel.clone());
                if let Ok(raw) = fs::read_to_string(self.vault.root.join(&rel)) {
                    if let Some(t) = refs::extract_title(&raw) {
                        lookup.entry(t.to_lowercase()).or_insert_with(|| rel.clone());
                    }
                }
            }
            for expr in &spec.rollup {
                let (col, field, prop, agg) = views::parse_rollup(expr);
                if col.is_empty() || field.is_empty() { continue; }
                for r in &mut rows {
                    let rel_val = r.props.get(&field).cloned().unwrap_or_default();
                    let mut vals: Vec<f64> = vec![];
                    let mut resolved = 0usize;
                    for tok in rel_val.split(',') {
                        let tok = tok.trim().trim_start_matches('[').trim_end_matches(']').trim().to_lowercase();
                        if tok.is_empty() { continue; }
                        let Some(target) = lookup.get(&tok) else { continue; };
                        let Ok(raw) = fs::read_to_string(self.vault.root.join(target)) else { continue; };
                        resolved += 1;
                        if let Some(pr) = &prop {
                            if let Some(v) = parse_frontmatter_props(&raw).get(pr).and_then(|s| s.parse::<f64>().ok()) {
                                vals.push(v);
                            }
                        }
                    }
                    let out: Option<f64> = match agg.as_str() {
                        "count" => Some(resolved as f64),
                        "sum" => Some(vals.iter().sum()),
                        "avg" => if vals.is_empty() { None } else { Some(vals.iter().sum::<f64>() / vals.len() as f64) },
                        "min" => vals.iter().cloned().fold(f64::INFINITY, f64::min).into(),
                        "max" => vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max).into(),
                        _ => None,
                    };
                    if let Some(v) = out {
                        let s = if (v - v.trunc()).abs() < 1e-9 { format!("{}", v.trunc() as i64) } else { format!("{v:.2}") };
                        r.props.insert(col.clone(), s);
                    }
                }
            }
        }
        // 计算列：`名称 = 表达式`，变量来自该行 frontmatter 属性
        if let Some(f) = &spec.formula {
            if let Some((name, expr)) = f.split_once('=') {
                let name = name.trim().to_lowercase();
                if !name.is_empty() && !expr.trim().is_empty() {
                    let expr = expr.trim().to_string();
                    for r in &mut rows {
                        if let Some(v) = views::eval_formula(&expr, &r.props) {
                            let s = if (v - v.trunc()).abs() < 1e-9 {
                                format!("{}", v.trunc() as i64)
                            } else {
                                format!("{v:.2}")
                            };
                            r.props.insert(name.clone(), s);
                        }
                    }
                }
            }
        }
        Ok(rows)
    }

    /// 渲染文档，并把 ```view 块替换成真实数据视图。
    pub fn render_doc_html_with_views(&mut self, rel: &str) -> Result<String, SyncError> {
        let mut model = self.get_doc(rel)?;
        let mut specs: Vec<views::ViewSpec> = Vec::new();
        for b in &model.blocks {
            if let kernel_md::Block::Code { lang: Some(l), text, .. } = b {
                if l == "view" {
                    specs.push(views::ViewSpec::from_yamlish(text));
                }
            }
        }
        if specs.is_empty() {
            return self.render_doc_html(rel);
        }
        // 依次把每个 view 块换成占位标记，再整体渲染
        let mut idx = 0usize;
        for b in model.blocks.iter_mut() {
            if let kernel_md::Block::Code { lang: Some(l), .. } = b {
                if l == "view" {
                    let spec = specs.get(idx).cloned().unwrap_or_default();
                    let rows = self.resolve_view(&spec, Some(rel))?;
                    let rendered = format!("<!--KBVIEW:{}-->", views::render_html(&spec, &rows));
                    let block_id = b.id().to_string();
                    *b = kernel_md::Block::Raw { id: block_id, text: rendered };
                    idx += 1;
                }
            }
        }
        let html = to_html(&model);
        let profile = self.active_design().map(|p| apply_to_html(&html, &p)).unwrap_or(html);
        Ok(profile.replace("<!--KBVIEW:", "").replace("-->", ""))
    }

    // ---------- Agent 记忆 ----------

    /// 追加一段对话到当天的对话文件（文件真相，用户可读可编辑）。
    pub fn append_conversation(&mut self, date: &str, role: &str, text: &str) -> Result<String, SyncError> {
        let rel = memory::conversation_path(date).to_string_lossy().into_owned();
        let mut content = fs::read_to_string(self.vault.root.join(&rel)).unwrap_or_default();
        if content.is_empty() {
            content = memory::conversation_header(date);
        }
        let stamp = now_hhmm();
        content.push_str(&format!("## {stamp} {role}

{text}

"));
        self.put_doc(&rel, &content)?;
        Ok(rel)
    }

    /// 写入一条长期记忆（追加到 memory.md，用户可编辑删除）。
    pub fn save_memory(&mut self, kind: &str, text: &str) -> Result<String, SyncError> {
        let rel = memory::MEMORY_FILE;
        let mut content = fs::read_to_string(self.vault.root.join(rel)).unwrap_or_default();
        if content.is_empty() {
            content = memory::memory_header();
        }
        let text = text.replace('\n', " ").trim().to_string();
        if text.is_empty() {
            return Ok(rel.to_string());
        }
        let line = format!("- [{kind}] {text} · {}\n", now_date());
        if content.contains(&text) {
            return Ok(rel.to_string()); // 去重
        }
        content.push_str(&line);
        self.put_doc(rel, &content)?;
        Ok(rel.to_string())
    }

    /// 回忆：在 Agent 目录里做混合检索（TF-IDF 为主，规模小）。
    pub fn recall(&mut self, query: &str, limit: usize) -> Result<Vec<(String, f64, String)>, SyncError> {
        self.sync_throttled()?;
        let dir = self.vault.root.join(memory::DIR);
        let mut corpus: Vec<(String, String)> = Vec::new();
        if dir.is_dir() {
            for e in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
                if !e.file_type().is_file() { continue; }
                if e.path().extension().map_or(false, |x| x == "md") {
                    if let (Some(rel), Ok(text)) = (
                        e.path().strip_prefix(&self.vault.root).ok().and_then(|p| p.to_str()),
                        fs::read_to_string(e.path()),
                    ) {
                        corpus.push((rel.to_string(), text));
                    }
                }
            }
        }
        if corpus.is_empty() { return Ok(Vec::new()); }
        let idx = rag::TfidfIndex::build(&corpus);
        let hits = idx.search(query, limit);
        let by_path: std::collections::HashMap<&str, &str> =
            corpus.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        Ok(hits.into_iter().map(|(p, s)| {
            // 取命中最相关的一行作为摘要
            let body = by_path.get(p.as_str()).copied().unwrap_or("");
            let snippet = body.lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('#') && !l.starts_with("---"))
                .take(3).collect::<Vec<_>>().join(" ");
            (p, s, snippet.chars().take(200).collect())
        }).collect())
    }

    /// 全部附件 (hash, path, mime)。
    pub fn asset_list(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        Ok(self
            .index
            .list_assets()?
            .into_iter()
            .map(|a| (a.hash, a.path, a.mime))
            .collect())
    }

    /// 文档 → 附件引用关系全量。
    pub fn doc_asset_pairs(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.index.list_doc_assets()
    }

    /// 读取一篇文档的当前块模型。
    pub fn get_doc(&mut self, rel: &str) -> Result<DocModel, SyncError> {
        self.log.current_model(&self.vault, rel)
    }

    /// 全文检索（FTS5 trigram，中文子串可查，bm25 排序）。
    pub fn search(&self, query: &str) -> Result<Vec<(String, f64)>, SyncError> {
        Ok(self.index.search(query).map_err(SyncError::Store)?)
    }

    /// 混合检索：FTS5 + TF-IDF 向量 → RRF 合并。
    /// 语料按 (文档数, 最大 mtime) 缓存：库没变就不重建（1.2 万篇全量读一遍是秒级开销）。
    pub fn search_hybrid(&mut self, query: &str, limit: usize) -> Result<Vec<(String, f64)>, SyncError> {
        let fts = self.search(query)?;
        let stats = self.index.stat_map().map_err(SyncError::Store)?;
        let corpus_epoch = (stats.len(), stats.values().map(|(m, _)| *m).max().unwrap_or(0));
        let stale = match &self.hybrid_cache {
            Some((e, _)) => *e != corpus_epoch,
            None => true,
        };
        if stale {
            // 语料从索引库（docs_fts）直读：单文件顺序读，比逐个打开文件快一个数量级
            let corpus: Vec<(String, String)> = self
                .index
                .corpus()
                .map_err(SyncError::Store)?
                .into_iter()
                .filter(|(rel, _)| rel.starts_with("Notes/") && (rel.ends_with(".md") || rel.ends_with(".html")))
                .collect();
            let idx = rag::TfidfIndex::build(&corpus);
            self.hybrid_cache = Some((corpus_epoch, idx));
        }
        let vec_hits = {
            let (_, idx) = self.hybrid_cache.as_ref().expect("hybrid cache just set");
            idx.search(query, limit * 2)
        };
        Ok(rag::rrf_merge(&fts, &vec_hits, limit))
    }

    /// 混合检索语料快照：(epoch, 语料)。锁内只做一次 SQL 顺序读（秒级），
    /// TF-IDF 构建交给调用方在锁外慢慢算——启动预热用，避免部署后站点不可用。
    pub fn hybrid_snapshot(&self) -> Result<((usize, i64), Vec<(String, String)>), SyncError> {
        let stats = self.index.stat_map().map_err(SyncError::Store)?;
        let epoch = (stats.len(), stats.values().map(|(m, _)| *m).max().unwrap_or(0));
        let corpus = self.index.corpus().map_err(SyncError::Store)?
            .into_iter()
            .filter(|(rel, _)| rel.starts_with("Notes/") && (rel.ends_with(".md") || rel.ends_with(".html")))
            .collect();
        Ok((epoch, corpus))
    }

    /// 安装锁外构建好的 TF-IDF 语料缓存。
    pub fn hybrid_install(&mut self, epoch: (usize, i64), idx: rag::TfidfIndex) {
        self.hybrid_cache = Some((epoch, idx));
    }

    // ── 密向量召回（AI embeddings 旁路索引；未配置 embedding_model 时全部退回 TF-IDF）──

    /// [ai] 配置的 embeddings 模型（空 = 未启用）。
    pub fn embedding_model(&self) -> String {
        self.vault.config.ai.as_ref()
            .map(|c| c.embedding_model.clone())
            .unwrap_or_default()
    }

    /// embeddings 端点与密钥（配置优先，密钥兜底环境变量）。
    pub fn embedding_endpoint(&self) -> Option<(String, String)> {
        let ai = self.vault.config.ai.as_ref()?;
        if ai.base_url.is_empty() { return None; }
        Some((ai.base_url.clone(), ai.resolved_key()))
    }

    /// 密向量覆盖：(已建向量数, 文档总数)。
    pub fn vector_stats(&self) -> Result<(usize, usize), StoreError> {
        let model = self.embedding_model();
        let covered = if model.is_empty() { 0 } else { self.index.vector_count(&model)? };
        Ok((covered, self.index.doc_count()?))
    }

    /// 待建/待更新清单：(path, mtime)，mtime 新的在前。
    pub fn vector_stale(&self) -> Result<Vec<(String, i64)>, SyncError> {
        let model = self.embedding_model();
        if model.is_empty() { return Ok(Vec::new()); }
        let _ = self.index.vectors_prune().map_err(SyncError::Store)?;
        self.index.vector_stale(&model).map_err(SyncError::Store)
    }

    /// 落一批向量（旁路表 upsert）。
    pub fn vectors_store(&self, model: &str, rows: &[(String, i64, Vec<f32>)]) -> Result<(), SyncError> {
        self.index.vectors_upsert(model, rows).map_err(SyncError::Store)
    }

    /// 密向量检索：查询向量已由调用方在锁外取好（embedding 是网络调用，不进内核锁）。
    pub fn search_dense(&mut self, q: &[f32], limit: usize) -> Result<Vec<(String, f64)>, SyncError> {
        let model = self.embedding_model();
        if model.is_empty() { return Ok(Vec::new()); }
        let stats = self.index.stat_map().map_err(SyncError::Store)?;
        let epoch = (stats.len(), stats.values().map(|(m, _)| *m).max().unwrap_or(0));
        let stale = match &self.dense_cache {
            Some((e, idx)) => *e != epoch || idx.model != model,
            None => true,
        };
        if stale {
            let vectors = self.index.vectors_all(&model).map_err(SyncError::Store)?;
            self.dense_cache = Some((epoch, rag::DenseIndex::build(&model, vectors)));
        }
        let idx = &self.dense_cache.as_ref().expect("dense cache just set").1;
        Ok(idx.search(q, limit))
    }

    /// 已索引文档数。
    pub fn indexed_count(&self) -> Result<usize, StoreError> {
        self.index.doc_count()
    }
}

/// 今天的日期（YYYY-MM-DD）。
pub fn today_string() -> String {
    now_date()
}

/// 构造视图行（自由函数：避免借用冲突）。
fn make_view_row(root: &std::path::Path, path: &str, model: &DocModel, md: &str) -> views::ViewRow {
    let collection = {
        let rest = path.strip_prefix("Notes/").unwrap_or(path);
        match rest.split_once('/') {
            Some((seg, _)) if seg == "Sources" => "采集".to_string(),
            Some((seg, _)) => seg.to_string(),
            None => "(根)".to_string(),
        }
    };
    let title = model.title.clone().unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_string());
    let tags = kernel_store::refs::find_tag_refs(md);
    let mtime = fs::metadata(root.join(path)).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs()).unwrap_or(0);
    let kind = if path.contains("/Agent/") { "agent".to_string() }
        else if path.contains("/Sources/") { "capture".to_string() }
        else { "doc".to_string() };
    let props = std::fs::read_to_string(root.join(path))
        .map(|raw| parse_frontmatter_props(&raw))
        .unwrap_or_default();
    views::ViewRow { path: path.to_string(), title, tags, mtime, collection, kind, props }
}

/// 解析 frontmatter 顶层属性（标量 + 行内/缩进列表），用于「文件即数据库」的列。
fn parse_frontmatter_props(md: &str) -> std::collections::BTreeMap<String, String> {
    use std::collections::BTreeMap;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut lines = md.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return out;
    }
    let mut list_key: Option<String> = None;
    let mut list_vals: Vec<String> = Vec::new();
    let flush = |out: &mut BTreeMap<String, String>, k: Option<String>, vals: &mut Vec<String>| {
        if let Some(k) = k {
            if !vals.is_empty() {
                out.insert(k, std::mem::take(vals).join(", "));
            }
        }
    };
    for raw in lines {
        let line = raw.trim_end();
        if line.trim() == "---" {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ") {
            if list_key.is_some() {
                let v = trimmed
                    .trim_start_matches("- ")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                if !v.is_empty() {
                    list_vals.push(v.to_string());
                }
            }
            continue;
        }
        flush(&mut out, list_key.take(), &mut list_vals);
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_lowercase();
        if k.is_empty() {
            continue;
        }
        let v = v.trim();
        if v.is_empty() {
            list_key = Some(k); // 后面可能是缩进列表
            continue;
        }
        let v = v.trim_start_matches('[').trim_end_matches(']');
        let v = v
            .split([',', '，'])
            .map(|x| x.trim().trim_matches('"').trim_matches('\''))
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        out.insert(k, v);
    }
    flush(&mut out, list_key.take(), &mut list_vals);
    out
}

fn now_date() -> String {
    let secs = now_secs() as i64;
    let (y, m, d, _, _, _) = kernel_deploy_free_civil(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

fn now_hhmm() -> String {
    let secs = now_secs() as i64;
    let (_, _, _, h, mi, _) = kernel_deploy_free_civil(secs);
    format!("{h:02}:{mi:02}")
}

/// UNIX 秒 → UTC 民用时间（Howard Hinnant 算法，避免依赖）。
fn kernel_deploy_free_civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 文档路径安全校验：仅允许 Notes/ 下的相对路径（防目录穿越）。
/// server 与 mcp 共用同一规则，保证入口一致。
pub fn is_safe_doc_path(p: &str) -> bool {
    !p.is_empty()
        && p.starts_with("Notes/")
        && !p.contains("..")
        && !p.starts_with('/')
        && !p.contains('\\')
}

/// 生成安全、可读的文件名：保留 Unicode 字母数字（含中文），其余转连字符。
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if c.is_ascii_uppercase() {
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_search_and_external_edit() {
        let dir = tempdir().unwrap();
        let vault = Vault::init(dir.path(), "k").unwrap();
        let mut k = Kernel::open(vault).unwrap();

        k.put_doc("Notes/x.md", "# 标题\n\n正文内容\n").unwrap();
        let m = k.get_doc("Notes/x.md").unwrap();
        assert_eq!(m.title.as_deref(), Some("标题"));

        let hits = k.search("正文内容").unwrap();
        assert_eq!(hits[0].0, "Notes/x.md");

        // 外部编辑后 sync_all 拉进 op-log + 索引
        std::fs::write(
            k.vault.root.join("Notes/x.md"),
            "# 标题\n\n改过的内容\n",
        )
        .unwrap();
        assert_eq!(k.sync_all().unwrap(), 1);
        let m = k.get_doc("Notes/x.md").unwrap();
        assert!(kernel_md::to_markdown(&m).contains("改过的内容"));
        assert!(k.search("改过的内容").unwrap().len() == 1);
        assert!(k.search("正文内容").unwrap().is_empty());

        // 删除文档 → 索引回收
        std::fs::remove_file(k.vault.root.join("Notes/x.md")).unwrap();
        k.sync_all().unwrap();
        assert_eq!(k.indexed_count().unwrap(), 0);
    }

    #[test]
    fn verbatim_docs_roundtrip_byte_exact() {
        let dir = tempdir().unwrap();
        let vault = Vault::init(dir.path(), "k").unwrap();
        let mut k = Kernel::open(vault).unwrap();

        // 代码内容含 Markdown 易变形结构（4 空格缩进=代码块、--- 分隔线、# 注释）
        let py = "def f(x):\n    return x * 2\n\n# --- divider ---\nconfig = {'a': 1}\n";
        k.put_doc("Notes/script.py", py).unwrap();
        let on_disk = std::fs::read_to_string(k.vault.root.join("Notes/script.py")).unwrap();
        assert_eq!(on_disk, py, "verbatim 文件必须字节级往返");

        // HTML 一等文档同样原文保留
        let html = "<!doctype html>\n<html><body><h1>Hi</h1></body></html>\n";
        k.put_doc("Notes/page.html", html).unwrap();
        let on_disk = std::fs::read_to_string(k.vault.root.join("Notes/page.html")).unwrap();
        assert_eq!(on_disk, html);

        // verbatim 文件可被检索、被 list_docs 收录
        assert!(k.search("config").unwrap().iter().any(|(p, _)| p == "Notes/script.py"));
    }

    #[test]
    fn views_expose_frontmatter_props_as_columns() {
        let dir = tempdir().unwrap();
        let vault = Vault::init(dir.path(), "k").unwrap();
        let mut k = Kernel::open(vault).unwrap();
        k.put_doc(
            "Notes/a.md",
            "---\nstatus: doing\npriority: 2\ntags: [x, y]\nowner: 我\n---\n# A\n\n正文\n",
        )
        .unwrap();
        k.put_doc("Notes/b.md", "---\nstatus: done\npriority: 10\n---\n# B\n\n正文\n")
            .unwrap();
        k.put_doc("Notes/c.md", "# C\n\n无 frontmatter\n").unwrap();

        // frontmatter 必须落盘（文件即真相：CRDT 往返不能丢元数据）
        let raw_a = std::fs::read_to_string(dir.path().join("Notes/a.md")).unwrap();
        assert!(raw_a.contains("status: doing"), "frontmatter 应回写：{raw_a:?}");

        let spec = crate::views::ViewSpec::from_yamlish(
            "type: table\nsource: folder:Notes\nfields: title, status, priority, owner\nfilter: status=doing\nsort: priority:desc\nlimit: 50",
        );
        let rows = k.resolve_view(&spec, None).unwrap();
        assert_eq!(rows.len(), 1, "只有 a.md 满足 status=doing");
        assert_eq!(rows[0].props.get("status").map(|s| s.as_str()), Some("doing"));
        assert_eq!(rows[0].props.get("priority").map(|s| s.as_str()), Some("2"));
        assert_eq!(rows[0].props.get("tags").map(|s| s.as_str()), Some("x, y"));

        // 按 frontmatter 数值属性排序
        let spec2 = crate::views::ViewSpec::from_yamlish(
            "type: table\nsource: folder:Notes\nfields: title, priority\nsort: priority:desc\nlimit: 50",
        );
        let rows2 = k.resolve_view(&spec2, None).unwrap();
        assert_eq!(
            rows2[0].props.get("priority").map(|s| s.as_str()),
            Some("10"),
            "priority:desc → b.md 在前"
        );
        let html = crate::views::render_html(&spec2, &rows2);
        assert!(html.contains(">10<"), "属性应渲染成表格列：{html}");
    }

    #[test]
    fn views_formula_total_and_relation_links() {
        let dir = tempdir().unwrap();
        let vault = Vault::init(dir.path(), "k").unwrap();
        let mut k = Kernel::open(vault).unwrap();
        k.put_doc(
            "Notes/task-a.md",
            "---\nhours: 2\nrate: 100\nrelated: 客户\n---\n# 任务 A\n\n工作记录\n",
        )
        .unwrap();
        k.put_doc("Notes/task-b.md", "---\nhours: 3.5\nrate: 80\n---\n# B\n\n工作记录\n")
            .unwrap();

        // 计算列：工时 = hours * rate；relation 列渲染为链接
        let spec = crate::views::ViewSpec::from_yamlish(
            "type: table\nsource: folder:Notes\nfields: title, hours, rate, 工时, related\nrelation: related\nformula: 工时 = hours * rate\ntotal: hours\nlimit: 50",
        );
        let rows = k.resolve_view(&spec, None).unwrap();
        let a = rows.iter().find(|r| r.path == "Notes/task-a.md").unwrap();
        assert_eq!(a.props.get("工时").map(|s| s.as_str()), Some("200"), "计算列");
        assert_eq!(
            rows.iter().find(|r| r.path == "Notes/task-b.md").unwrap().props.get("工时").map(|s| s.as_str()),
            Some("280"),
            "3.5*80"
        );
        let html = crate::views::render_html(&spec, &rows);
        assert!(html.contains("合计"), "汇总行：{html}");
        assert!(html.contains("<a data-doc=\"客户\">客户</a>"), "relation 渲染为链接：{html}");
    }

    #[test]
    fn view_spec_accepts_bracket_list_values() {
        // 编辑器模板/用户习惯写法：[a, b, c] 与带引号都要认
        let s = crate::views::ViewSpec::from_yamlish(
            "type: table\nsource: folder:Notes\nfields: [title, tags, updated]\nrelation: [\"related\", owner]\nlimit: 10",
        );
        assert_eq!(s.fields, vec!["title", "tags", "updated"], "fields 应剥掉方括号");
        assert_eq!(s.relation, vec!["related", "owner"], "relation 应剥掉方括号并小写");
        assert_eq!(s.limit, 10);
    }

    #[test]
    fn views_rollup_aggregates_related_docs() {
        let dir = tempdir().unwrap();
        let vault = Vault::init(dir.path(), "k").unwrap();
        let mut k = Kernel::open(vault).unwrap();
        k.put_doc("Notes/客户A.md", "---\nhours: 5\nrate: 100\n---\n# 客户A\n\n工作\n").unwrap();
        k.put_doc("Notes/客户B.md", "---\nhours: 3\nrate: 80\n---\n# 客户B\n\n工作\n").unwrap();
        k.put_doc(
            "Notes/task.md",
            "---\nrelated: 客户A, 客户B\n---\n# 任务\n\n关联两个客户\n",
        )
        .unwrap();

        // sum + count（别名列名）
        let spec = crate::views::ViewSpec::from_yamlish(
            "type: table\nsource: folder:Notes\nfields: title, related, 关联工时, 关联数\nrelation: related\nrollup: 关联工时 = related.hours:sum, 关联数 = related:count\nlimit: 50",
        );
        let rows = k.resolve_view(&spec, None).unwrap();
        let task = rows.iter().find(|r| r.path == "Notes/task.md").unwrap();
        assert_eq!(task.props.get("关联工时").map(|s| s.as_str()), Some("8"), "5+3=8");
        assert_eq!(task.props.get("关联数").map(|s| s.as_str()), Some("2"), "关联两个文档");

        // avg / min / max / 无别名列名默认表达式
        let spec2 = crate::views::ViewSpec::from_yamlish(
            "type: table\nsource: folder:Notes\nfields: title, related.hours:avg\nrollup: related.hours:avg, related.hours:min, related.hours:max\nlimit: 50",
        );
        let rows2 = k.resolve_view(&spec2, None).unwrap();
        let task2 = rows2.iter().find(|r| r.path == "Notes/task.md").unwrap();
        assert_eq!(task2.props.get("related.hours:avg").map(|s| s.as_str()), Some("4"), "(5+3)/2");
        assert_eq!(task2.props.get("related.hours:min").map(|s| s.as_str()), Some("3"));
        assert_eq!(task2.props.get("related.hours:max").map(|s| s.as_str()), Some("5"));

        // 解析器单测
        let (c, f, p, a) = crate::views::parse_rollup("客户工时 = related.hours:sum");
        assert_eq!((c.as_str(), f.as_str(), p.as_deref(), a.as_str()), ("客户工时", "related", Some("hours"), "sum"));
        let (c2, f2, p2, a2) = crate::views::parse_rollup("related:count");
        assert_eq!((c2.as_str(), f2.as_str(), p2.as_deref(), a2.as_str()), ("related:count", "related", None, "count"));
    }

    #[test]
    fn assets_are_deduped_and_backlinked() {
        let dir = tempdir().unwrap();
        let vault = Vault::init(dir.path(), "k").unwrap();
        let mut k = Kernel::open(vault).unwrap();

        let png = b"\x89PNG fake";
        let m1 = k.put_asset("photo.png", png).unwrap();
        let m2 = k.put_asset("copy.png", png).unwrap();
        assert_eq!(m1.hash, m2.hash, "同内容必须去重");
        assert!(m1.rel.ends_with(".png") && m1.rel.starts_with("Assets/"));
        assert!(k.vault.root.join(&m1.rel).is_file());
        assert_eq!(k.assets_count().unwrap(), 1);

        let snippet = Kernel::asset_markdown(&m1, "照片");
        k.put_doc("Notes/with-asset.md", &format!("# 带图\n\n{snippet}\n"))
            .unwrap();
        assert_eq!(k.asset_docs(&m1.hash).unwrap(), vec!["Notes/with-asset.md"]);

        // 移除引用后反查清空
        k.put_doc("Notes/with-asset.md", "# 带图\n\n无图了\n").unwrap();
        assert!(k.asset_docs(&m1.hash).unwrap().is_empty());
    }
}

/// 站点文件条目（增量部署用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SiteFile {
    pub path: String,
    pub hash: String,
    pub bytes: u64,
}

/// 站点清单。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SiteManifest {
    pub title: String,
    pub generated_at: u64,
    pub files: Vec<SiteFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl Kernel {
    /// 构建静态站点：把库中每篇文档渲成自包含 AI-HTML（套用有效设计规范），
    /// 复制附件，生成 index / llms.txt / sitemap.xml / manifest.json。
    ///
    /// 只覆盖不清理（便于 git 目标：`.git` 必须留着）；上一版清单里消失的文件会被删除。
    pub fn build_site(
        &mut self,
        out: &std::path::Path,
        base_url: Option<&str>,
        icp: Option<&str>,
    ) -> Result<SiteManifest, SyncError> {
        self.sync_all()?;
        std::fs::create_dir_all(out)?;
        let previous = read_manifest(out);

        let docs = list_docs(&self.vault).map_err(SyncError::Store)?;
        let mut entries: Vec<(String, String, String)> = Vec::new(); // (slug, title, excerpt)
        let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
        for rel in &docs {
            let rel_str = rel.to_string_lossy().into_owned();
            let model = match self.get_doc(&rel_str) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let title = model.title.clone().unwrap_or_else(|| {
                rel.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| rel_str.clone())
            });
            let html = match self.render_doc_html(&rel_str) {
                Ok(h) => h,
                Err(_) => to_html(&model),
            };
            let mut slug = slugify(&title);
            if slug.is_empty() {
                slug = slugify(rel.file_stem().unwrap_or_default().to_string_lossy().as_ref());
            }
            if slug.is_empty() {
                slug = format!("doc-{}", &Cas::hash_hex(rel_str.as_bytes())[..8]);
            }
            // slug 去重
            let mut candidate = slug.clone();
            let mut n = 2;
            while used.contains(&candidate) {
                candidate = format!("{slug}-{n}");
                n += 1;
            }
            used.insert(candidate.clone());
            let fname = format!("{candidate}.html");
            write_if_changed(&out.join(&fname), html.as_bytes())?;
            let excerpt: String = to_markdown(&model)
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#') && !t.starts_with('>') && !t.starts_with('-')
                })
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(140)
                .collect();
            entries.push((fname, title, excerpt));
        }

        // 附件：保持 Assets/ 相对路径，文档里的引用才能直接工作
        let assets_root = self.vault.assets_dir();
        if assets_root.is_dir() {
            for entry in walkdir::WalkDir::new(&assets_root).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry.path().strip_prefix(&self.vault.root).unwrap_or(entry.path());
                let dest = out.join(rel);
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let bytes = std::fs::read(entry.path())?;
                write_if_changed(&dest, &bytes)?;
            }
        }

        // index.html
        let profile_css = self.active_design().map(|p| p.to_css()).unwrap_or_default();
        let mut list = String::new();
        for (fname, title, excerpt) in &entries {
            list.push_str(&format!(
                "<li><a href=\"{fname}\">{}</a><p>{}</p></li>\n",
                esc_html(title),
                esc_html(excerpt)
            ));
        }
        let index = format!(
            "<!doctype html>\n<html lang=\"zh\"><head><meta charset=\"utf-8\">\n\
<title>{}</title>\n<style>\n{}\n\
body{{max-width:46rem;margin:2rem auto;padding:0 1rem;font:16px/1.7 system-ui,sans-serif}}\n\
ul{{list-style:none;padding:0}}li{{padding:12px 0;border-bottom:1px solid #0002}}\n\
a{{text-decoration:none;font-weight:600}}p{{margin:4px 0 0;opacity:.72;font-size:14px}}\n\
</style></head><body><h1>{}</h1><ul>\n{}\n</ul></body></html>\n",
            esc_html(&self.vault.config.name),
            profile_css,
            esc_html(&self.vault.config.name),
            list
        );
        write_if_changed(&out.join("index.html"), index.as_bytes())?;

        // llms.txt：让 agent 发现站点内容
        let mut llms = format!("# {}\n\n", self.vault.config.name);
        for (fname, title, excerpt) in &entries {
            llms.push_str(&format!("- [{title}]({fname}) — {excerpt}\n"));
        }
        write_if_changed(&out.join("llms.txt"), llms.as_bytes())?;

        // sitemap.xml（有 base_url 才生成）
        if let Some(base) = base_url {
            let mut sm = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
            sm.push_str(&format!("  <url><loc>{}/index.html</loc></url>\n", base.trim_end_matches('/')));
            for (fname, _, _) in &entries {
                sm.push_str(&format!("  <url><loc>{}/{fname}</loc></url>\n", base.trim_end_matches('/')));
            }
            sm.push_str("</urlset>\n");
            write_if_changed(&out.join("sitemap.xml"), sm.as_bytes())?;
        }

        // 清理上一版残留（保留 .git 等非站点文件）
        let manifest = collect_manifest(out, &self.vault.config.name, base_url)?;
        let manifest_before_icp = manifest.clone();
        let _ = manifest_before_icp;
        if let Some(prev) = previous {
            let now: std::collections::HashSet<&str> =
                manifest.files.iter().map(|f| f.path.as_str()).collect();
            for f in prev.files {
                if !now.contains(f.path.as_str()) && !f.path.starts_with(".git") {
                    let _ = std::fs::remove_file(out.join(&f.path));
                }
            }
        }
        // 国内合规：页脚注入 ICP
        if let Some(icp) = icp {
            self.inject_icp_footer(out, icp)?;
        }
        let manifest = collect_manifest(out, &self.vault.config.name, base_url)?;
        std::fs::write(
            out.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        )?;
        Ok(manifest)
    }
}

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> Result<(), SyncError> {
    if let Ok(existing) = std::fs::read(path) {
        if existing == bytes {
            return Ok(());
        }
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

fn read_manifest(out: &std::path::Path) -> Option<SiteManifest> {
    let t = std::fs::read_to_string(out.join("manifest.json")).ok()?;
    serde_json::from_str(&t).ok()
}

/// 遍历站点目录，生成带内容哈希的清单（跳过 manifest.json 自身）。
pub fn collect_manifest(
    out: &std::path::Path,
    title: &str,
    base_url: Option<&str>,
) -> Result<SiteManifest, SyncError> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(out).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(out)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with(".git/") || rel == "manifest.json" {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        files.push(SiteFile {
            path: rel,
            hash: Cas::hash_hex(&bytes),
            bytes: bytes.len() as u64,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(SiteManifest {
        title: title.to_string(),
        generated_at: now_secs() as u64,
        files,
        base_url: base_url.map(|s| s.to_string()),
    })
}

/// 发布前检查结果（PUB-4）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ComplianceIssue {
    /// block（阻断）| warn（仅告警）
    pub level: String,
    pub file: String,
    pub detail: String,
}

impl Kernel {
    /// 发布前检查：国内合规（ICP 必填 + 页脚注入由 build_site 负责）、敏感词、
    /// 内链/图片完整性、外部机审 API。返回 issues；空 = 通过。
    pub fn compliance_check(
        &mut self,
        site_dir: &std::path::Path,
        target: &PublishTarget,
    ) -> Result<Vec<ComplianceIssue>, SyncError> {
        let mut issues = Vec::new();
        let cfg = self.vault.config.publish.checks.clone();

        // 1) 国内目标：ICP 备案号必填
        if target.domestic
            && target.icp.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true)
        {
            issues.push(ComplianceIssue {
                level: "block".into(),
                file: target.name.clone(),
                detail: "国内发布目标必须填写 ICP 备案号（target.icp），并确认已备案".into(),
            });
        }

        // 2) 敏感词 + 内链完整性（扫描站点 HTML）
        for f in site_files(site_dir)? {
            if !f.ends_with(".html") {
                continue;
            }
            let content = std::fs::read_to_string(site_dir.join(&f)).unwrap_or_default();
            for w in &cfg.sensitive_words {
                if !w.is_empty() && content.contains(w.as_str()) {
                    issues.push(ComplianceIssue {
                        level: "block".into(),
                        file: f.clone(),
                        detail: format!("命中敏感词「{w}」"),
                    });
                }
            }
            // 内链与图片：引用的本地文件必须存在
            for (attr, val) in extract_refs(&content) {
                let path = val.split(['?', '#']).next().unwrap_or("").to_string();
                if path.starts_with("http://") || path.starts_with("https://") || path.starts_with('#') {
                    continue;
                }
                let clean = path.trim_start_matches('/');
                if clean.is_empty() {
                    continue;
                }
                if !site_dir.join(clean).is_file() {
                    issues.push(ComplianceIssue {
                        level: "block".into(),
                        file: f.clone(),
                        detail: format!("{attr} 引用缺失：{path}"),
                    });
                }
            }
        }

        // 3) 外部机审 API（契约：POST {files:[{path,sha256}]} → {ok, reasons?}）
        if let Some(api) = &cfg.audit_api {
            let mut files = Vec::new();
            for f in site_files(site_dir)? {
                let bytes = std::fs::read(site_dir.join(&f)).unwrap_or_default();
                files.push(serde_json::json!({
                    "path": f, "sha256": Cas::hash_hex(&bytes), "bytes": bytes.len()
                }));
            }
            match reqwest::blocking::Client::new()
                .post(api)
                .json(&serde_json::json!({ "target": target.name, "files": files }))
                .timeout(std::time::Duration::from_secs(20))
                .send()
            {
                Ok(resp) => match resp.json::<serde_json::Value>() {
                    Ok(v) if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) => {}
                    Ok(v) => issues.push(ComplianceIssue {
                        level: "block".into(),
                        file: "audit".into(),
                        detail: format!("机审未通过：{}", v),
                    }),
                    Err(e) => issues.push(issue_for_audit_error(&cfg, e.to_string())),
                },
                Err(e) => issues.push(issue_for_audit_error(&cfg, e.to_string())),
            }
        }
        Ok(issues)
    }

    /// 把机审结果合并进站点页面：ICP 页脚注入到每个 html。
    pub fn inject_icp_footer(&self, site_dir: &std::path::Path, icp: &str) -> Result<(), SyncError> {
        for f in site_files(site_dir)? {
            if !f.ends_with(".html") {
                continue;
            }
            let path = site_dir.join(&f);
            let html = std::fs::read_to_string(&path).unwrap_or_default();
            if html.contains("icp-footer") {
                continue;
            }
            let footer = format!(
                "<footer class=\"icp-footer\" style=\"text-align:center;padding:24px 0;color:#8888;font-size:12px\">{}</footer>",
                esc_html(icp)
            );
            let patched = match html.rfind("</body>") {
                Some(i) => format!("{}{}{}", &html[..i], footer, &html[i..]),
                None => html,
            };
            std::fs::write(&path, patched)?;
        }
        Ok(())
    }
}

fn issue_for_audit_error(cfg: &kernel_store::ChecksConfig, msg: String) -> ComplianceIssue {
    ComplianceIssue {
        level: if cfg.fail_on_audit_error { "block".into() } else { "warn".into() },
        file: "audit".into(),
        detail: format!("机审不可达：{msg}"),
    }
}

/// 站点内 HTML 文件清单（相对路径）。
pub fn site_files(dir: &std::path::Path) -> Result<Vec<String>, SyncError> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = std::fs::read_dir(&d).map_err(SyncError::Io)?;
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if name.ends_with(".html") {
                out.push(p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 抽取 href/src 引用（值为相对路径的）。
fn extract_refs(html: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for attr in ["href", "src"] {
        let pat = format!("{attr}=\"");
        let mut rest = html;
        while let Some(i) = rest.find(&pat) {
            let after = &rest[i + pat.len()..];
            if let Some(end) = after.find('"') {
                out.push((if attr == "href" { "链接" } else { "图片" }, after[..end].to_string()));
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
    }
    out
}

/// RAG 语义检索：TF-IDF 向量（离线零依赖）+ AI embeddings 升级路径。
pub mod rag {
    use std::collections::HashMap;

    /// 分词：英文按词，中日韩按字符二元组（捕捉子词语义）。
    pub fn tokenize(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim_start().starts_with('#') { continue; }
            let chars: Vec<char> = line.chars().collect();
            let mut word = String::new();
            for &c in &chars {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    if c.is_ascii() {
                        word.push(c.to_ascii_lowercase());
                    } else {
                        // CJK：先冲掉 word，再二元组
                        if !word.is_empty() { out.push(word.clone()); word.clear(); }
                        // 单字本身
                        out.push(c.to_string());
                    }
                } else {
                    if !word.is_empty() { out.push(word.clone()); word.clear(); }
                }
            }
            if !word.is_empty() { out.push(word); }
            // CJK 二元组
            for w in chars.windows(2) {
                if w[0] != ' ' && w[1] != ' ' && !w[0].is_ascii() && !w[1].is_ascii()
                    && w[0].is_alphanumeric() && w[1].is_alphanumeric()
                {
                    out.push(w.iter().collect());
                }
            }
        }
        out
    }

    /// 稀疏 TF-IDF 向量。
    pub struct TfidfIndex {
        /// doc_path → (term → tfidf_weight)
        pub vectors: HashMap<String, HashMap<String, f64>>,
        pub df: HashMap<String, usize>,
        pub n_docs: usize,
    }

    impl TfidfIndex {
        pub fn build(docs: &[(String, String)]) -> Self {
            let n = docs.len();
            let mut df: HashMap<String, usize> = HashMap::new();
            let mut vectors = HashMap::new();
            for (path, text) in docs {
                let tokens = tokenize(text);
                let mut tf: HashMap<String, usize> = HashMap::new();
                for t in &tokens { *tf.entry(t.clone()).or_insert(0) += 1; }
                let total = tokens.len().max(1) as f64;
                let mut vec = HashMap::new();
                for (term, count) in &tf {
                    let weight = (*count as f64 / total) * (1.0 + (n as f64 / (1.0 + *df.get(term).unwrap_or(&0) as f64)).ln());
                    vec.insert(term.clone(), weight);
                    *df.entry(term.clone()).or_insert(0) += 1;
                }
                vectors.insert(path.clone(), vec);
            }
            TfidfIndex { vectors, df, n_docs: n }
        }

        pub fn embed_query(&self, text: &str) -> HashMap<String, f64> {
            let tokens = tokenize(text);
            let mut tf: HashMap<String, usize> = HashMap::new();
            for t in &tokens { *tf.entry(t.clone()).or_insert(0) += 1; }
            let total = tokens.len().max(1) as f64;
            let mut vec = HashMap::new();
            for (term, count) in &tf {
                let idf = (1.0 + (self.n_docs as f64 / (1.0 + *self.df.get(term).unwrap_or(&0) as f64)).ln()).max(0.1);
                vec.insert(term.clone(), (*count as f64 / total) * idf);
            }
            vec
        }

        /// 余弦相似度，返回 top-k (path, score)。
        pub fn search(&self, query: &str, k: usize) -> Vec<(String, f64)> {
            let qv = self.embed_query(query);
            let qn: f64 = qv.values().map(|v| v * v).sum::<f64>().sqrt();
            if qn < 1e-9 { return vec![]; }
            let mut scores: Vec<(String, f64)> = self.vectors.iter()
                .map(|(path, vec)| {
                    let dot: f64 = qv.iter()
                        .filter_map(|(t, w)| vec.get(t).map(|v| w * v))
                        .sum();
                    let vn: f64 = vec.values().map(|v| v * v).sum::<f64>().sqrt();
                    (path.clone(), if vn < 1e-9 { 0.0 } else { dot / (qn * vn) })
                })
                .collect();
            scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scores.into_iter().take(k).filter(|(_, s)| *s > 0.01).collect()
        }
    }

    /// RRF（Reciprocal Rank Fusion）：把 FTS5 和向量检索的结果合并。
    pub fn rrf_merge(fts_hits: &[(String, f64)], vec_hits: &[(String, f64)], k: usize) -> Vec<(String, f64)> {
        rrf_merge_n(&[fts_hits, vec_hits], k)
    }

    /// RRF 多路合并：FTS5 / TF-IDF / 密向量各贡献一路排名。
    pub fn rrf_merge_n(lists: &[&[(String, f64)]], k: usize) -> Vec<(String, f64)> {
        let mut scores: HashMap<String, f64> = HashMap::new();
        for hits in lists {
            for (rank, (path, _)) in hits.iter().enumerate() {
                *scores.entry(path.clone()).or_insert(0.0) += 1.0 / (60.0 + rank as f64);
            }
        }
        let mut out: Vec<(String, f64)> = scores.into_iter().collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.into_iter().take(k).collect()
    }

    /// f32 余弦相似度（维度不齐按 0 处理——不同代模型混排时防 panic）。
    pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..n {
            dot += a[i] * b[i];
            na += a[i] * a[i];
            nb += b[i] * b[i];
        }
        if na < 1e-9 || nb < 1e-9 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
    }

    /// 内存密向量索引（向量来自 doc_vectors 旁路表，可整表重建）。
    pub struct DenseIndex {
        pub model: String,
        /// (path, 向量)。1 万篇 × 1k 维 f32 ≈ 40MB，可承受；更高规模再换 HNSW。
        pub vectors: Vec<(String, Vec<f32>)>,
    }

    impl DenseIndex {
        pub fn build(model: &str, vectors: Vec<(String, Vec<f32>)>) -> Self {
            DenseIndex { model: model.to_string(), vectors }
        }

        /// 余弦 top-k。0.25 以下视为不相关（不同供应商的绝对分值不可比，取保守下限）。
        pub fn search(&self, q: &[f32], k: usize) -> Vec<(String, f64)> {
            let mut scores: Vec<(String, f64)> = self.vectors.iter()
                .map(|(path, v)| (path.clone(), cosine(q, v) as f64))
                .filter(|(_, s)| *s > 0.25)
                .collect();
            scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scores.into_iter().take(k).collect()
        }
    }

    /// embedding 输入预算：标题行在前 + 正文截断到 4000 字符（各家 8k token 上限内）。
    pub fn embed_text(text: &str) -> String {
        let mut out = String::new();
        for line in text.lines() {
            if line.trim_start().starts_with('#') {
                out.push_str(line.trim());
                out.push('\n');
            }
            if out.len() > 500 { break; }
        }
        let body: String = text.lines()
            .filter(|l| !l.trim_start().starts_with("---") && !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>().join("\n");
        out.push_str(&body);
        out.chars().take(4000).collect()
    }

    /// OpenAI 兼容 /embeddings 调用（blocking——调用方放后台线程或 spawn_blocking）。
    pub fn embed_batch(
        base_url: &str,
        api_key: &str,
        model: &str,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, String> {
        let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build().map_err(|e| e.to_string())?;
        let mut req = client.post(&url).json(&serde_json::json!({
            "model": model,
            "input": inputs,
        }));
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("embeddings HTTP {status}: {}", resp.text().unwrap_or_default().chars().take(200).collect::<String>()));
        }
        let body: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
        let mut rows: Vec<(usize, Vec<f32>)> = body.get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| "embeddings 响应缺 data 数组".to_string())?
            .iter()
            .filter_map(|item| {
                let i = item.get("index")?.as_u64()? as usize;
                let v: Vec<f32> = item.get("embedding")?.as_array()?
                    .iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
                Some((i, v))
            })
            .collect();
        rows.sort_by_key(|(i, _)| *i);
        if rows.len() != inputs.len() {
            return Err(format!("embeddings 返回 {} 条，期望 {} 条", rows.len(), inputs.len()));
        }
        Ok(rows.into_iter().map(|(_, v)| v).collect())
    }
}

#[cfg(test)]
mod rag_tests {
    use super::rag::*;

    #[test]
    fn tfidf_finds_relevant_docs() {
        let docs = vec![
            ("a.md".to_string(), "知识库 CRDT 块级合并 融合内核 同步协议".to_string()),
            ("b.md".to_string(), "发布 S3 Git 目标 对象存储".to_string()),
            ("c.md".to_string(), "CRDT 融合内核 Automerge 增量".to_string()),
        ];
        let idx = TfidfIndex::build(&docs);
        let hits = idx.search("CRDT 合并", 2);
        assert!(hits.len() >= 2);
        assert!(hits[0].0 == "a.md" || hits[0].0 == "c.md", "hits: {:?}", hits);
        assert!(hits[0].1 > 0.05);
    }

    #[test]
    fn cjk_tokenizes() {
        let tokens = tokenize("知识库的块级合并");
        assert!(!tokens.is_empty(), "CJK 分词不应为空");
        assert!(tokens.iter().any(|t| t.contains("知识")));
    }

    #[test]
    fn rrf_merges_rankings() {
        let fts = vec![("a.md".into(), 1.0), ("b.md".into(), 0.5)];
        let vec = vec![("b.md".into(), 1.0), ("c.md".into(), 0.5)];
        let merged = rrf_merge(&fts, &vec, 3);
        assert_eq!(merged[0].0, "b.md", "两路都命中的排最前");
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn cosine_and_dense_search() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.9f32, 0.1, 0.0];
        let c = vec![0.0f32, 1.0, 0.0];
        assert!(cosine(&a, &b) > 0.99);
        assert!(cosine(&a, &c).abs() < 1e-6);
        let idx = DenseIndex::build("test", vec![
            ("near.md".into(), b), ("far.md".into(), c),
        ]);
        let hits = idx.search(&a, 2);
        assert_eq!(hits[0].0, "near.md");
        assert!(!hits.iter().any(|(p, _)| p == "far.md"), "零相关要被阈值滤掉");
    }

    #[test]
    fn rrf_n_merges_three_legs() {
        let fts = vec![("a.md".into(), 1.0)];
        let tfidf = vec![("b.md".into(), 1.0)];
        let dense = vec![("a.md".into(), 1.0), ("b.md".into(), 0.9)];
        let merged = rrf_merge_n(&[&fts, &tfidf, &dense], 5);
        // a 两路命中（fts+dense），b 两路命中（tfidf+dense），并列时秩和定序
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|(p, _)| p == "a.md") && merged.iter().any(|(p, _)| p == "b.md"));
    }

    #[test]
    fn embed_text_keeps_headings_and_truncates() {
        let long = "# 标题\n\n".to_string() + &"正文内容。".repeat(2000);
        let out = embed_text(&long);
        assert!(out.starts_with("# 标题"));
        assert!(out.chars().count() <= 4000, "要截到预算内");
    }
}

/// Agent 记忆：对话与决策落库（文件真相），回答前自动回忆。
pub mod memory {
    use std::path::PathBuf;

    pub const DIR: &str = "Notes/Agent";
    pub const MEMORY_FILE: &str = "Notes/Agent/memory.md";

    /// 记忆文件头部（用户可直接编辑/删除条目）。
    pub fn memory_header() -> String {
        "---\ntype: memory\n---\n\n# Agent 记忆\n\n> 每条一行，用户可直接编辑或删除。`[偏好]` `[事实]` `[决策]` 三种类型。\n\n".into()
    }

    pub fn conversation_path(date: &str) -> PathBuf {
        PathBuf::from(format!("Notes/Agent/{date} 对话.md"))
    }

    pub fn conversation_header(date: &str) -> String {
        format!("---\ntype: conversation\ndate: {date}\n---\n\n# {date} 对话\n\n")
    }
}

/// 内联视图（Views 层）：在笔记里声明表格/看板/日历，数据源可以是文件夹、
/// 当前页列表、或检索结果。声明用 ```view 代码块（YAML 风格），不破坏 Markdown。
///
/// 关键字（table）：
/// - `fields: title, status, hours` —— 任意 frontmatter 属性可当列
/// - `filter: status=doing` / `sort: priority:desc`（数值优先）
/// - `formula: 工时 = hours * rate` —— 计算列（+−×÷ 括号）
/// - `total: hours` —— 表尾合计行
/// - `relation: related` —— 该列渲染为文档链接
/// - `rollup: related.hours:sum` —— 按关联聚合目标文档属性（sum/avg/count/min/max）
pub mod views {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
    pub struct ViewSpec {
        /// table | kanban | calendar | list
        pub kind: String,
        /// here（本页列表）| folder:Notes/x | query:关键词 | all
        pub source: String,
        #[serde(default)]
        pub fields: Vec<String>,
        #[serde(default)]
        pub filter: Option<String>,
        #[serde(default)]
        pub sort: Option<String>,
        #[serde(default)]
        pub group_by: Option<String>,
        #[serde(default)]
        pub limit: usize,
        /// 计算列：`工时 = hours * rate`（+ - * / 括号，变量取 frontmatter 属性）
        #[serde(default)]
        pub formula: Option<String>,
        /// 汇总：对某属性求和，输出表尾合计行
        #[serde(default)]
        pub total: Option<String>,
        /// 关系列：这些字段渲染为文档链接（relation 最小形态）
        #[serde(default)]
        pub relation: Vec<String>,
        /// rollup 按关联聚合：`related.hours:sum`、`related:count`、
        /// 别名 `客户工时 = related.hours:sum`（sum/avg/count/min/max）
        #[serde(default)]
        pub rollup: Vec<String>,
    }

    impl ViewSpec {
        pub fn from_yamlish(text: &str) -> Self {
            let mut s = ViewSpec { kind: "table".into(), source: "all".into(), limit: 30, ..Default::default() };
            for line in text.lines() {
                let Some((k, v)) = line.split_once(':') else { continue };
                let key = k.trim().to_lowercase();
                let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
                if val.is_empty() { continue; }
                match key.as_str() {
                    "type" | "kind" => s.kind = val,
                    "source" | "来源" => s.source = val,
                    "filter" | "筛选" => s.filter = Some(val),
                    "sort" | "排序" => s.sort = Some(val),
                    "group_by" | "group" | "分组" => s.group_by = Some(val),
                    "limit" | "条数" => s.limit = val.parse().unwrap_or(30),
                    "fields" | "字段" => {
                        s.fields = split_list(&val)
                    }
                    "formula" | "公式" => s.formula = Some(val),
                    "total" | "汇总" => s.total = Some(val),
                    "relation" | "relations" | "关联" => {
                        s.relation = split_list(&val).into_iter().map(|x| x.to_lowercase()).collect()
                    }
                    "rollup" | "聚合" => {
                        s.rollup = split_list(&val)
                    }
                    _ => {}
                }
            }
            s
        }
    }

    /// 列表值解析：`title, tags` 与 `[title, tags]` / `"a", "b"` 都认（兼容 YAML 风格写法）。
    fn split_list(val: &str) -> Vec<String> {
        let cleaned = val.trim().trim_start_matches('[').trim_end_matches(']').to_string();
        cleaned
            .split([',', '，'])
            .map(|x| x.trim().trim_matches('"').trim_matches('\'').trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    }

    /// 一行视图数据。
    #[derive(Debug, Clone, Serialize)]
    pub struct ViewRow {
        pub path: String,
        pub title: String,
        pub tags: Vec<String>,
        pub mtime: u64,
        pub collection: String,
        pub kind: String,
        /// frontmatter 顶层属性（文件即数据库：任意 key 都能当列）
        #[serde(default)]
        pub props: std::collections::BTreeMap<String, String>,
    }

    /// 渲染视图为静态 HTML（发布/预览用；行可点击，由客户端桥接打开文档）。
    pub fn render_html(spec: &ViewSpec, rows: &[ViewRow]) -> String {
        let title = format!(
            "{} · {} · {} 行",
            match spec.kind.as_str() { "kanban" => "看板", "calendar" => "日历", "list" => "列表", _ => "表格" },
            spec.source,
            rows.len()
        );
        let mut out = format!("<section class=\"kb-view\" data-kind=\"{}\"><header>{}</header>", spec.kind, esc(&title));
        match spec.kind.as_str() {
            "kanban" => {
                let mut groups: std::collections::BTreeMap<String, Vec<&ViewRow>> = Default::default();
                for r in rows {
                    let g = spec.group_by.as_deref().and_then(|g| match g {
                        "tag" => r.tags.first().cloned(),
                        "collection" => Some(r.collection.clone()),
                        "kind" => Some(r.kind.clone()),
                        _ => None,
                    }).unwrap_or_else(|| "(未分组)".into());
                    groups.entry(g).or_default().push(r);
                }
                for (g, items) in groups {
                    out.push_str(&format!("<div class=\"kb-col\"><h4>{} <span>{}</span></h4>", esc(&g), items.len()));
                    for r in items {
                        out.push_str(&format!("<a class=\"kb-card\" data-doc=\"{}\">{}</a>", esc(&r.path), esc(&r.title)));
                    }
                    out.push_str("</div>");
                }
            }
            "calendar" => {
                let mut by_day: std::collections::BTreeMap<String, Vec<&ViewRow>> = Default::default();
                for r in rows {
                    let day = fmt_day(r.mtime);
                    by_day.entry(day).or_default().push(r);
                }
                for (day, items) in by_day {
                    out.push_str(&format!("<div class=\"kb-day\"><h4>{}</h4>", esc(&day)));
                    for r in items {
                        out.push_str(&format!("<a class=\"kb-card\" data-doc=\"{}\">{}</a>", esc(&r.path), esc(&r.title)));
                    }
                    out.push_str("</div>");
                }
            }
            "list" => {
                out.push_str("<ul>");
                for r in rows {
                    out.push_str(&format!("<li><a data-doc=\"{}\">{}</a></li>", esc(&r.path), esc(&r.title)));
                }
                out.push_str("</ul>");
            }
            _ => {
                let fields = if spec.fields.is_empty() {
                    vec!["title".to_string(), "tags".to_string(), "updated".to_string()]
                } else {
                    spec.fields.clone()
                };
                out.push_str("<table><thead><tr>");
                for f in &fields { out.push_str(&format!("<th>{}</th>", esc(f))); }
                out.push_str("</tr></thead><tbody>");
                for r in rows {
                    out.push_str("<tr>");
                    for f in &fields {
                        let cell = match f.as_str() {
                            "title" | "标题" => format!("<a data-doc=\"{}\">{}</a>", esc(&r.path), esc(&r.title)),
                            "tags" | "标签" => r.tags.iter().map(|t| format!("<span class=\"kb-tag\">{}</span>", esc(t))).collect::<Vec<_>>().join(" "),
                            "updated" | "更新时间" => fmt_day(r.mtime),
                            "collection" | "集合" => esc(&r.collection),
                            "path" | "路径" => format!("<code>{}</code>", esc(&r.path)),
                            other => {
                                let key = other.to_lowercase();
                                let v = r.props.get(&key).map(|s| s.as_str()).unwrap_or("");
                                if spec.relation.iter().any(|x| x == &key) {
                                    if v.trim().is_empty() { String::new() } else {
                                        format!("<a data-doc=\"{}\">{}</a>", esc(v), esc(v))
                                    }
                                } else {
                                    render_prop_value(v)
                                }
                            }
                        };
                        out.push_str(&format!("<td>{cell}</td>"));
                    }
                    out.push_str("</tr>");
                }
                // 汇总行：total 指定属性求和 + 行数
                if let Some(t) = &spec.total {
                    let tk = t.trim().to_lowercase();
                    let sum: f64 = rows
                        .iter()
                        .filter_map(|r| r.props.get(&tk).and_then(|s| s.parse::<f64>().ok()))
                        .sum();
                    let s = if (sum - sum.trunc()).abs() < 1e-9 { format!("{}", sum.trunc() as i64) } else { format!("{sum:.1}") };
                    out.push_str(&format!(
                        "<tfoot><tr><td colspan=\"{}\">共 {} 行 · {} 合计：{}</td></tr></tfoot>",
                        fields.len().max(1),
                        rows.len(),
                        esc(&tk),
                        s
                    ));
                }
                out.push_str("</tbody></table>");
            }
        }
        out.push_str("</section>");
        out
    }

    /// 计算列求值：+ - * / 与括号，变量 = frontmatter 属性（小写键，空值/缺失按 0）。
    pub(crate) fn eval_formula(expr: &str, vars: &std::collections::BTreeMap<String, String>) -> Option<f64> {
        let tokens = tokenize_formula(expr)?;
        let mut p = 0usize;
        let v = parse_expr(&tokens, &mut p, vars)?;
        if p != tokens.len() {
            return None;
        }
        Some(v.0)
    }

    /// 解析 rollup 条目 → (列名, 关联字段, 目标属性?, 聚合函数)。
    /// 支持 `related.hours:sum`、`related:count`、别名 `客户工时 = related.hours:sum`。
    /// 无别名时列名 = 完整表达式小写（如 `related.hours:avg`）。
    pub(crate) fn parse_rollup(s: &str) -> (String, String, Option<String>, String) {
        let s = s.trim();
        let (alias, body) = match s.split_once('=') {
            Some((a, b)) => (a.trim().to_lowercase(), b.trim().to_string()),
            None => (String::new(), s.to_string()),
        };
        let (left, agg) = match body.rsplit_once(':') {
            Some((l, a)) if matches!(a, "sum" | "avg" | "count" | "min" | "max") => {
                (l.to_string(), a.to_string())
            }
            _ => {
                let agg = if body.contains('.') { "sum".to_string() } else { "count".to_string() };
                (body.clone(), agg)
            }
        };
        let col = if alias.is_empty() { body.to_lowercase() } else { alias };
        let (field, prop) = match left.split_once('.') {
            Some((f, p)) => (f.to_lowercase(), Some(p.to_lowercase())),
            None => (left.to_lowercase(), None),
        };
        (col, field, prop, agg)
    }

    #[derive(Debug, Clone)]
    enum FTok {
        Num(f64),
        Var(String),
        Op(char),
        LP,
        RP,
    }

    fn tokenize_formula(expr: &str) -> Option<Vec<FTok>> {
        let mut out = Vec::new();
        let chars: Vec<char> = expr.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            if "+-*/".contains(c) {
                out.push(FTok::Op(c));
                i += 1;
                continue;
            }
            if c == '(' {
                out.push(FTok::LP);
                i += 1;
                continue;
            }
            if c == ')' {
                out.push(FTok::RP);
                i += 1;
                continue;
            }
            if c.is_ascii_digit() || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                out.push(FTok::Num(s.parse().ok()?));
                continue;
            }
            if c.is_alphabetic() {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                out.push(FTok::Var(chars[start..i].iter().collect::<String>().to_lowercase()));
                continue;
            }
            return None; // 未知字符
        }
        Some(out)
    }

    struct FVal(f64);

    fn parse_expr(t: &[FTok], p: &mut usize, vars: &std::collections::BTreeMap<String, String>) -> Option<FVal> {
        let mut left = parse_term(t, p, vars)?;
        while *p < t.len() {
            if let FTok::Op(op) = &t[*p] {
                if *op == '+' || *op == '-' {
                    let op = *op;
                    *p += 1;
                    let right = parse_term(t, p, vars)?;
                    left = FVal(match op {
                        '+' => left.0 + right.0,
                        _ => left.0 - right.0,
                    });
                    continue;
                }
            }
            break;
        }
        Some(left)
    }

    fn parse_term(t: &[FTok], p: &mut usize, vars: &std::collections::BTreeMap<String, String>) -> Option<FVal> {
        let mut left = parse_factor(t, p, vars)?;
        while *p < t.len() {
            if let Some(FTok::Op(op)) = t.get(*p) {
                if *op == '*' || *op == '/' {
                    let op = *op;
                    *p += 1;
                    let right = parse_factor(t, p, vars)?;
                    left = FVal(if op == '*' { left.0 * right.0 } else { left.0 / right.0 });
                    continue;
                }
            }
            break;
        }
        Some(left)
    }

    fn parse_factor(t: &[FTok], p: &mut usize, vars: &std::collections::BTreeMap<String, String>) -> Option<FVal> {
        if *p >= t.len() {
            return None;
        }
        match &t[*p] {
            FTok::LP => {
                *p += 1;
                let v = parse_expr(t, p, vars)?;
                if *p >= t.len() || !matches!(t[*p], FTok::RP) {
                    return None;
                }
                *p += 1;
                Some(v)
            }
            FTok::Num(n) => {
                let v = *n;
                *p += 1;
                Some(FVal(v))
            }
            FTok::Var(name) => {
                *p += 1;
                let s = vars.get(&name.to_lowercase()).map(|s| s.as_str()).unwrap_or("");
                Some(FVal(s.trim().parse::<f64>().unwrap_or(0.0)))
            }
            FTok::Op('-') => {
                *p += 1;
                let v = parse_factor(t, p, vars)?;
                Some(FVal(-v.0))
            }
            FTok::Op('+') => {
                *p += 1;
                parse_factor(t, p, vars)
            }
            _ => None,
        }
    }

    /// 表格里的属性值：`[[X]]` 渲染为可点链接（relation 最小形态）。
    fn render_prop_value(v: &str) -> String {
        if !v.contains("[[") {
            return esc(v);
        }
        let mut out = String::new();
        let mut rest = v;
        while let Some(i) = rest.find("[[") {
            out.push_str(&esc(&rest[..i]));
            let after = &rest[i + 2..];
            match after.find("]]") {
                Some(j) => {
                    let target = &after[..j];
                    out.push_str(&format!("<a data-doc=\"{}\">{}</a>", esc(target), esc(target)));
                    rest = &after[j + 2..];
                }
                None => {
                    out.push_str("[[");
                    rest = after;
                    break;
                }
            }
        }
        out.push_str(&esc(rest));
        out
    }

    fn fmt_day(secs: u64) -> String {
        if secs == 0 { return "—".into(); }
        let (y, m, d, _, _, _) = civil(secs as i64);
        format!("{y:04}-{m:02}-{d:02}")
    }

    fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = if m <= 2 { y + 1 } else { y };
        (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32)
    }

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }
}
