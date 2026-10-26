use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kernel_core::Vault;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "litmus", version, about = "Litmus Studio kernel CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 初始化新库
    Init {
        path: PathBuf,
        #[arg(default_value = "MyVault")]
        name: String,
    },
    /// 库状态：文档数、配置
    Status { path: PathBuf },
    /// 监听 Notes/，外部改动实时合入 op-log 与索引
    Watch { path: PathBuf },
    /// 全文检索（FTS5，中文可查，bm25 排序）
    Search {
        path: PathBuf,
        query: String,
    },
    /// 创建/更新一篇文档
    Put {
        path: PathBuf,
        /// 相对库根的路径，如 Notes/foo.md
        rel: String,
        /// 从文件读取内容，缺省用 stdin
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// 打印一篇文档的当前块模型（调试）
    Doc {
        path: PathBuf,
        rel: String,
    },
    /// 附件入库：写入 Assets/ 并输出可直接粘贴的引用片段
    Asset {
        path: PathBuf,
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// 反查：哪些文档引用了该附件
    Refs { path: PathBuf, hash: String },
    /// 启动本地 daemon（HTTP API + 文件监听）
    Serve {
        path: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7700")]
        addr: String,
    },
    /// 密向量重建：[ai].embedding_model 配置后，把全库增量嵌入 index.db 旁路表
    Reindex { path: PathBuf },
    /// E2EE 备份：运行备份（默认推全部目标）；--list 列举快照；--restore --into 恢复
    Backup {
        path: PathBuf,
        /// 只跑指定目标（按 name）
        #[arg(long)]
        target: Option<String>,
        /// 列举各目标快照
        #[arg(long)]
        list: bool,
        /// 恢复指定快照 id；不带值 = 恢复最新
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        restore: Option<String>,
        /// 恢复到该目录（必须为空或不存在）
        #[arg(long)]
        into: Option<String>,
    },
    /// 以 MCP server 运行（stdio），供 Claude/Cursor 等 agent 使用
    Mcp { path: PathBuf },
    /// 从 Obsidian 库导入（保留目录结构，附件内容寻址）
    ImportObsidian {
        vault_path: PathBuf,
        obsidian: PathBuf,
        #[arg(long, default_value = "attachments")]
        attachments_dir: String,
    },
    /// 主题一键采集（快速建立知识库）
    Ingest {
        path: PathBuf,
        topic: String,
        /// 源列表，缺省 wikipedia+hackernews+arxiv
        #[arg(long, value_delimiter = ',')]
        sources: Vec<String>,
        #[arg(long, default_value = "5")]
        limit: usize,
    },
    /// 发布：构建站点并推送到目标
    Publish {
        path: PathBuf,
        /// 目标名（缺省用 [publish].default_target，否则 local）
        #[arg(long)]
        target: Option<String>,
        /// 只构建站点，不推送
        #[arg(long)]
        build_only: bool,
    },
    /// 设计规范：列表 / 导入 / 激活
    Design {
        #[command(subcommand)]
        cmd: DesignCmd,
    },
    /// 外部数据源连接管理（MCP 入口侧）
    Conn {
        #[command(subcommand)]
        cmd: ConnCmd,
    },
}

#[derive(Subcommand)]
enum DesignCmd {
    /// 列出已导入的设计规范
    List { path: PathBuf },
    /// 导入 design.md / SKILL 目录 / tokens.css / 页面 HTML
    Add {
        path: PathBuf,
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// 导入后立即激活
        #[arg(long)]
        activate: bool,
    },
    /// 激活某个规范（空名恢复内置契约）
    Use {
        path: PathBuf,
        name: String,
        /// 作用域：vault（默认）/ publish / 文档路径 Notes/x.md
        #[arg(long, default_value = "vault")]
        scope: String,
    },
    /// 查看作用域绑定
    Scopes { path: PathBuf },
    /// 消化一个真实页面（ego-lite → headless Chromium → 直接抓取）
    Url {
        path: PathBuf,
        url: String,
        #[arg(long)]
        name: Option<String>,
        /// auto | ego | render | http
        #[arg(long, default_value = "auto")]
        via: String,
        #[arg(long)]
        activate: bool,
    },
}

#[derive(Subcommand)]
enum ConnCmd {
    /// 列出库中配置的连接
    List { path: PathBuf },
    /// 探测连接：握手 + 列出 tools/resources
    Probe { path: PathBuf, name: String },
    /// 采集该连接的全部 resources 入库
    Pull { path: PathBuf, name: String },
    /// 调用连接的某个 tool
    Call {
        path: PathBuf,
        name: String,
        tool: String,
        #[arg(long, default_value = "{}")]
        args: String,
        /// 把返回文本也导入为一篇文档
        #[arg(long)]
        import: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init { path, name } => {
            let v = Vault::init(&path, &name)?;
            println!("vault '{}' initialized at {}", name, v.root.display());
        }
        Cmd::Status { path } => {
            let vault = Vault::open(&path)?;
            let mut k = kernel_core::Kernel::open(vault).context("open kernel")?;
            let docs = kernel_core::list_docs(&k.vault)?;
            println!("vault:   {}", k.vault.config.name);
            println!("root:    {}", k.vault.root.display());
            println!("docs:    {}", docs.len());
            println!("indexed: {}", k.indexed_count()?);
            println!("assets:  {}", k.assets_count()?);
            let synced = k.sync_all()?;
            println!("synced:  {synced} external change(s) merged into op-log");
        }
        Cmd::Watch { path } => {
            let vault = Vault::open(&path)?;
            println!("watching {} — Ctrl-C to stop", vault.notes_dir().display());
            kernel_core::watch::watch(vault, |changed| {
                if changed > 0 {
                    println!("[{}] synced {changed} doc(s)", now_hms());
                }
            })?;
        }
        Cmd::Search { path, query } => {
            let vault = Vault::open(&path)?;
            let k = kernel_core::Kernel::open(vault).context("open kernel")?;
            for (rel, rank) in k.search(&query)? {
                println!("{rank:>8.3}  {rel}");
            }
        }
        Cmd::Put { path, rel, file } => {
            let vault = Vault::open(&path)?;
            let mut k = kernel_core::Kernel::open(vault).context("open kernel")?;
            let md = match file {
                Some(f) => std::fs::read_to_string(f)?,
                None => std::io::read_to_string(std::io::stdin())?,
            };
            k.put_doc(&rel, &md)?;
            println!("written: {rel}");
        }
        Cmd::Doc { path, rel } => {
            let vault = Vault::open(&path)?;
            let mut k = kernel_core::Kernel::open(vault).context("open kernel")?;
            k.sync_all().context("sync")?;
            let m = k.get_doc(&rel)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
        }
        Cmd::Asset { path, file, name } => {
            let vault = Vault::open(&path)?;
            let mut k = kernel_core::Kernel::open(vault).context("open kernel")?;
            let bytes = std::fs::read(&file)?;
            let display = name.unwrap_or_else(|| {
                file.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "asset".into())
            });
            let meta = k.put_asset(&file.to_string_lossy(), &bytes)?;
            println!("hash: {}", meta.hash);
            println!("path: {}", meta.rel);
            println!("mime: {}  size: {}", meta.mime, meta.size);
            println!("\n{}", kernel_core::Kernel::asset_markdown(&meta, &display));
        }
        Cmd::Refs { path, hash } => {
            let vault = Vault::open(&path)?;
            let k = kernel_core::Kernel::open(vault).context("open kernel")?;
            for doc in k.asset_docs(&hash)? {
                println!("{doc}");
            }
        }
        Cmd::Serve { path, addr } => {
            let vault = Vault::open(&path)?;
            let state = litmus_server::build_state(vault).context("build state")?;
            litmus_server::spawn_watcher(state.clone()).context("spawn watcher")?;
            println!("litmus daemon listening on http://{addr}");
            println!("api token: {}", state.token);
            println!("(Authorization: Bearer <token>，或 header x-litmus-token)");
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move { litmus_server::serve(&addr, state).await })
                .context("serve")?;
        }
        Cmd::Reindex { path } => {
            let vault = Vault::open(&path)?;
            let state = litmus_server::build_state(vault).context("build state")?;
            let model = state.kernel.lock().unwrap().embedding_model();
            if model.is_empty() {
                println!("未配置 [ai].embedding_model（litmus.toml），密向量未启用");
                return Ok(());
            }
            println!("密向量重建：模型 {model}，后台批量嵌入中…");
            litmus_server::rag_job_blocking(state.clone());
            let k = state.kernel.lock().unwrap();
            let (covered, docs) = k.vector_stats().unwrap_or((0, 0));
            let err = state.rag.last_error.lock().unwrap().clone();
            println!("完成：{covered}/{docs} 篇已建向量");
            if !err.is_empty() {
                println!("最后错误：{err}");
                std::process::exit(1);
            }
        }
        Cmd::Backup {
            path,
            target,
            list,
            restore,
            into,
        } => {
            let vault = Vault::open(&path)?;
            let cfg = vault
                .config
                .backup
                .clone()
                .ok_or_else(|| anyhow::anyhow!("未配置备份：litmus.toml 加 [backup] 与 [[backup.targets]]"))?;
            let into: Option<String> = into;
            if let Some(ref dir) = into {
                // 恢复模式
                let t = match target.as_ref() {
                    Some(name) => cfg.targets.iter().find(|t| &t.name == name)
                        .ok_or_else(|| anyhow::anyhow!("目标不存在：{name}"))?,
                    None => cfg.targets.first()
                        .ok_or_else(|| anyhow::anyhow!("未配置备份目标"))?,
                };
                let snap = match restore.filter(|s| !s.is_empty()) {
                    Some(id) => id,
                    None => kernel_backup::list_snapshots(t)
                        .ok()
                        .and_then(|l| l.first().map(|m| m.id.clone()))
                        .unwrap_or_default(),
                };
                if snap.is_empty() {
                    anyhow::bail!("目标「{}」没有可用快照", t.name);
                }
                                let passphrase = if cfg.passphrase.is_empty() { std::env::var("LITMUS_BACKUP_PASSPHRASE").or_else(|_| std::env::var("THIRDC_BACKUP_PASSPHRASE")).unwrap_or_default() } else { cfg.passphrase.clone() };
                let (files, bytes) = kernel_backup::restore(t, &snap, std::path::Path::new(dir), &passphrase)?;
                println!("已恢复 {snap}：{files} 个文件，{bytes} 字节 → {dir}");
                return Ok(());
            }
            if list {
                for t in cfg.targets.iter() {
                    match kernel_backup::list_snapshots(t) {
                        Ok(metas) => {
                            println!("◆ {}（{}）", t.name, t.kind);
                            for m in metas {
                                println!("  {}  {} 个文件  {} 字节  @{}", m.id, m.files, m.bytes, m.host);
                            }
                        }
                        Err(e) => println!("◆ {}（{}）：列举失败 {e}", t.name, t.kind),
                    }
                }
                return Ok(());
            }
            let (reports, state) = kernel_backup::run_backup(&vault.root, &cfg, target.as_deref())?;
            let mut any_fail = false;
            for r in &reports {
                if r.ok {
                    println!("✓ {}（{}）→ {}  {} 字节 / {}s，修剪 {} 份",
                        r.target, r.kind, r.snapshot, r.bytes, r.seconds, r.pruned);
                } else {
                    any_fail = true;
                    println!("✗ {}（{}）：{}", r.target, r.kind, r.error);
                }
            }
            let (_, msg) = kernel_backup::compliance(&state);
            println!("{msg}");
            if any_fail {
                std::process::exit(1);
            }
        }
        Cmd::Mcp { path } => {
            use std::io::{BufRead, Write};
            let vault = Vault::open(&path)?;
            let server = litmus_mcp::McpServer::open(vault).context("open mcp server")?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            for line in stdin.lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(resp) = server.handle(&line) {
                    let mut out = stdout.lock();
                    writeln!(out, "{resp}")?;
                    out.flush()?;
                }
            }
        }
        Cmd::ImportObsidian { vault_path, obsidian, attachments_dir } => {
    use std::path::Path;
    let vault = Vault::open(&vault_path)?;
    let src = Path::new(&obsidian);
    if !src.is_dir() { anyhow::bail!("目录不存在：{}", src.display()); }
    let _att_dir = attachments_dir.clone();

    let mut copied = 0usize; let mut assets = 0usize; let mut skipped = 0usize;
    let mut stack = vec![src.to_path_buf()];
    let notes = vault.root.join("Notes");
    let assets_root = vault.root.join("Assets");

    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) { Ok(r) => r, Err(_) => continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == ".obsidian" || name == ".git" || name == ".trash" { continue; }
            if p.is_dir() { stack.push(p); continue; }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            let rel_dir = p.parent().unwrap().strip_prefix(src).unwrap_or(Path::new(""));
            if ext == "md" || ext == "html" {
                let dest_dir = notes.join(rel_dir);
                let _ = std::fs::create_dir_all(&dest_dir);
                let dest = dest_dir.join(&name);
                if dest.exists() { skipped += 1; continue; }
                if std::fs::copy(&p, &dest).is_ok() { copied += 1; }
            } else if ["png","jpg","jpeg","gif","webp","svg","pdf","mp4","mp3"].contains(&ext.as_str()) {
                // 附件 → 内容寻址
                let bytes = match std::fs::read(&p) { Ok(b) => b, Err(_) => continue };
                let h = kernel_core::Cas::hash_hex(&bytes);
                let cas_rel = format!("Assets/{}/{}", &h[..2], &h[2..4]);
                let cas_dir = assets_root.join(&cas_rel[7..]);
                let _ = std::fs::create_dir_all(&cas_dir);
                let dest = cas_dir.join(format!("{h}.{ext}"));
                if !dest.exists() { let _ = std::fs::write(&dest, &bytes); }
                assets += 1;
                // 顺手记录原始文件名 → CAS 路径的映射，后面改写引用
                let map_file = vault.sidecar().join("obsidian-alias.json");
                let mut alias: std::collections::BTreeMap<String,String> = std::fs::read_to_string(&map_file)
                    .ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
                alias.insert(name, format!("{cas_rel}/{h}.{ext}"));
                let _ = std::fs::write(&map_file, serde_json::to_string_pretty(&alias).unwrap_or_default());
            }
        }
    }
    println!("Obsidian 导入完成：{} 篇文档（跳过已有 {}），{} 个附件已内容寻址入库", copied, skipped, assets);
    println!("正在建立索引（第一次可能要几秒）…");
    let mut k = kernel_core::Kernel::open(vault)?;
    let changed = k.sync_all()?;
    println!("索引完成：{} 篇新入索引", changed);
}
        Cmd::Ingest { path, topic, sources, limit } => {
            let vault = Vault::open(&path)?;
            let mut k = kernel_core::Kernel::open(vault).context("open kernel")?;
            let src = if sources.is_empty() {
                vec!["wikipedia".into(), "hackernews".into(), "arxiv".into()]
            } else {
                sources
            };
            println!("采集「{topic}」（{}）…", src.join("/"));
            let docs = kernel_core::ingest::ingest_topic(&topic, &src, limit).map_err(anyhow::Error::msg)?;
            for d in &docs {
                let uri = format!("topic://{}?t={}&src={}", kernel_core::ingest::urlenc_pub(&topic),
                    &kernel_core::Cas::hash_hex(d.title.as_bytes())[..12], kernel_core::ingest::urlenc_pub(&d.source));
                let rel = k.import_capture("topic", &uri, Some(&d.title),
                    &format!("# {}\n\n> 来源：{}（{}）\n\n{}", d.title, d.url, d.source, d.text), &d.mime)?;
                println!("  ✓ {} → {}", d.title, rel);
            }
            println!("共导入 {} 篇", docs.len());
        }
        Cmd::Publish { path, target, build_only } => {
            let vault = Vault::open(&path)?;
            let mut k = kernel_core::Kernel::open(vault).context("open kernel")?;
            let cfg = k.vault.config.publish.clone();
            let name = target
                .clone()
                .or_else(|| cfg.default_target.clone())
                .unwrap_or_else(|| "local".to_string());
            let t = cfg
                .targets
                .iter()
                .find(|t| t.name == name)
                .cloned()
                .unwrap_or(kernel_core::PublishTarget {
                    name: name.clone(),
                    kind: "local".into(),
                    dir: Some(cfg.site_dir.clone()),
                    ..Default::default()
                });
            let dir = k.vault.root.join(kernel_deploy::target_dir(&t, &cfg.site_dir));
            let m = k.build_site(&dir, cfg.base_url.as_deref(), t.icp.as_deref())?;
            println!(
                "站点已构建：{}（{} 个文件，{:.1} KB）",
                dir.display(),
                m.files.len(),
                m.files.iter().map(|f| f.bytes).sum::<u64>() as f64 / 1024.0
            );
            for f in m.files.iter().take(12) {
                println!("  {}", f.path);
            }
            let issues = k.compliance_check(&dir, &t)?;
            let blocking: Vec<_> = issues.iter().filter(|i| i.level == "block").collect();
            for i in &issues {
                println!("  [{}] {}: {}", i.level, i.file, i.detail);
            }
            if !blocking.is_empty() {
                anyhow::bail!("发布前检查未通过（{} 项阻断）", blocking.len());
            }
            if build_only {
                return Ok(());
            }
            let report = kernel_deploy::deploy(&dir, &k.vault.sidecar(), &t)?;
            {
                let dir_ev = k.vault.sidecar().join("events");
                let _ = std::fs::create_dir_all(&dir_ev);
                let line = serde_json::json!({
                    "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                    "kind": "publish",
                    "data": { "target": report.target, "kind": report.kind, "uploaded": report.uploaded, "skipped": report.skipped, "checks": issues.len(), "detail": report.detail }
                });
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir_ev.join("publish.jsonl")) {
                    let _ = writeln!(f, "{line}");
                }
            }
            println!(
                "部署 [{} / {}]：上传 {} · 跳过 {} · {}",
                report.target, report.kind, report.uploaded, report.skipped, report.detail
            );
            if let Some(u) = report.url {
                println!("地址：{u}");
            }
        }
        Cmd::Design { cmd } => match cmd {
            DesignCmd::List { path } => {
                let vault = Vault::open(&path)?;
                let k = kernel_core::Kernel::open(vault).context("open kernel")?;
                let active = k.active_design().map(|p| p.name).unwrap_or_default();
                for p in k.designs()? {
                    let mark = if p.name == active { "●" } else { "○" };
                    println!(
                        "{mark} {:16} {:11} token {:3} · 组件 {:3} · 规则 {:3}  {}",
                        p.name,
                        p.kind,
                        p.tokens.len(),
                        p.archetypes.len(),
                        p.rules.len(),
                        p.source
                    );
                }
            }
            DesignCmd::Add { path, file, name, activate } => {
                let vault = Vault::open(&path)?;
                let k = kernel_core::Kernel::open(vault).context("open kernel")?;
                let p = k.import_design_path(&file, name.as_deref())?;
                println!(
                    "导入「{}」（{}）：token {} · 字体 {} · 组件 {} · 规则 {}",
                    p.name, p.kind, p.tokens.len(), p.fonts.len(), p.archetypes.len(), p.rules.len()
                );
                if activate {
                    k.set_active_design(&p.name)?;
                    println!("已激活 {}", p.name);
                }
            }
            DesignCmd::Use { path, name, scope } => {
                let vault = Vault::open(&path)?;
                let k = kernel_core::Kernel::open(vault).context("open kernel")?;
                let sc = if scope == "vault" {
                    "vault".to_string()
                } else if scope == "publish" {
                    "publish".to_string()
                } else if scope.starts_with("Notes/") {
                    format!("doc:{scope}")
                } else {
                    scope.clone()
                };
                k.set_design_scope(&sc, &name)?;
                println!(
                    "{} -> {}",
                    sc,
                    if name.is_empty() { "（清除，回退下级作用域）" } else { &name }
                );
            }
            DesignCmd::Scopes { path } => {
                let vault = Vault::open(&path)?;
                let k = kernel_core::Kernel::open(vault).context("open kernel")?;
                let sc = k.design_scopes();
                if sc.is_empty() {
                    println!("（无作用域绑定，全部使用内置契约）");
                }
                for (k2, v) in sc {
                    println!("{k2:28} -> {v}");
                }
            }
            DesignCmd::Url { path, url, name, via, activate } => {
                let vault = Vault::open(&path)?;
                let k = kernel_core::Kernel::open(vault).context("open kernel")?;
                let name = name.unwrap_or_else(|| {
                    url.trim_end_matches('/').rsplit('/').next().unwrap_or("page").to_string()
                });
                let ego = k.vault.config.browser.ego.clone();
                let chrome = k.vault.config.browser.chrome.clone();
                let to = std::time::Duration::from_secs(60);
                let mut tried: Vec<String> = Vec::new();

                let make_ego = || kernel_core::ego_digest(&url, ego.as_deref(), to);
                let make_render = || kernel_core::dump_dom(&url, chrome.as_deref(), to);
                let make_http = || kernel_core::fetch_html(&url, to);

                let profile = if via == "ego" || via == "auto" {
                    match make_ego() {
                        Ok(d) => k.import_design_digest(&name, &url, &d).map(|p| (p, "ego")),
                        Err(e) => {
                            tried.push(format!("ego: {e}"));
                            if via == "ego" { Err(kernel_core::SyncError::Md(tried.join("; "))) }
                            else { make_render().map_err(|e2| { tried.push(format!("render: {e2}")); kernel_core::SyncError::Md(tried.join("; ")) }).and_then(|h| k.import_design_html(&name, &url, &h).map(|p| (p, "render"))) }
                        }
                    }
                } else if via == "render" || via == "headless" {
                    make_render().map_err(|e| kernel_core::SyncError::Md(e.to_string())).and_then(|h| {
                        k.import_design_html(&name, &url, &h).map(|p| (p, "render"))
                    })
                } else {
                    make_http().map_err(|e| kernel_core::SyncError::Md(e.to_string())).and_then(|h| {
                        k.import_design_html(&name, &url, &h).map(|p| (p, "http"))
                    })
                };
                match profile {
                    Ok((p, backend)) => {
                        println!(
                            "消化「{}」（{} · {}）：token {} · 字体 {:?} · 规则 {} · 组件 {}",
                            p.name, p.kind, backend, p.tokens.len(), p.fonts, p.rules.len(), p.archetypes.len()
                        );
                        for r in p.rules.iter().take(8) {
                            println!("  · {r}");
                        }
                        if activate {
                            k.set_design_scope("vault", &p.name)?;
                            println!("已激活（vault 作用域）");
                        }
                    }
                    Err(e) => {
                        println!("失败：{e}");
                        if !tried.is_empty() {
                            println!("尝试记录：{}", tried.join("; "));
                        }
                    }
                }
            }
        },
        Cmd::Conn { cmd } => match cmd {
            ConnCmd::List { path } => {
                let vault = Vault::open(&path)?;
                if vault.config.connections.is_empty() {
                    println!("(no connections configured in litmus.toml)");
                }
                for c in &vault.config.connections {
                    println!("{:12}  {} {}", c.name, c.command, c.args.join(" "));
                }
            }
            ConnCmd::Probe { path, name } => {
                let vault = Vault::open(&path)?;
                let cfg = find_conn(&vault, &name)?;
                let (server, tools, resources) = litmus_mcp::pull::probe(&cfg)?;
                println!("server: {server}");
                println!("tools ({}): {}", tools.len(), tools.join(", "));
                println!("resources ({}):", resources.len());
                for r in resources {
                    println!("  {r}");
                }
            }
            ConnCmd::Pull { path, name } => {
                let vault = Vault::open(&path)?;
                let cfg = find_conn(&vault, &name)?;
                let kernel = std::sync::Arc::new(std::sync::Mutex::new(
                    kernel_core::Kernel::open(vault).context("open kernel")?,
                ));
                let report = litmus_mcp::pull::pull_resources(&cfg, &kernel)?;
                println!(
                    "server {} · tools {} · imported {} · skipped {}",
                    report.server,
                    report.tools.len(),
                    report.imported.len(),
                    report.skipped.len()
                );
                for (uri, rel) in &report.imported {
                    println!("  {uri}  ->  {rel}");
                }
                for s in &report.skipped {
                    println!("  skipped: {s}");
                }
            }
            ConnCmd::Call {
                path,
                name,
                tool,
                args,
                import,
            } => {
                let vault = Vault::open(&path)?;
                let cfg = find_conn(&vault, &name)?;
                let parsed: serde_json::Value = serde_json::from_str(&args)?;
                let result = litmus_mcp::pull::call(&cfg, &tool, parsed)?;
                let text = litmus_mcp::client::extract_text(&result)
                    .unwrap_or_else(|| result.to_string());
                println!("{text}");
                if import {
                    let kernel = std::sync::Arc::new(std::sync::Mutex::new(
                        kernel_core::Kernel::open(vault).context("open kernel")?,
                    ));
                    let uri = format!("tool://{name}/{tool}");
                    let mut k = kernel.lock().unwrap();
                    let rel = k.import_capture(&name, &uri, Some(&tool), &text, "text/markdown")?;
                    println!("imported: {rel}");
                }
            }
        },
    }
    Ok(())
}

fn find_conn(vault: &Vault, name: &str) -> Result<kernel_core::ConnectionConfig> {
    vault
        .config
        .connections
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .with_context(|| format!("connection '{name}' not found in litmus.toml"))
}

fn now_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

