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
    /// 把这个库的 MCP 一键写进外部 agent：cursor / claude / claude-code / codex / opencode / dsh / hermes / generic（默认只打印，--write 才落盘写配置）
    McpInstall {
        /// 库根路径
        path: PathBuf,
        /// 目标 agent：cursor | claude | claude-code | codex | opencode | dsh | hermes | obsidian | generic
        #[arg(long, default_value = "cursor")]
        client: String,
        /// 真正写进该 agent 的配置文件（会先备份成 *.bak-litmus-mcp）
        #[arg(long)]
        write: bool,
    },
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
    /// 打印主流设计 / 流程图 / 笔记工具接进来的 TOML 样板（先确认端点与密钥再启用）
    Presets { path: PathBuf },
    /// 把一条连接写进库根 litmus.toml 的 [[connections]]
    Add {
        path: PathBuf,
        /// 连接名（如 canva / notion / obsidian）
        #[arg(long)]
        name: String,
        /// 本地子进程命令（填了 url 就不用）
        #[arg(long)]
        command: Option<String>,
        /// 子进程参数（空格分隔）
        #[arg(long)]
        args: Option<String>,
        /// 远程 MCP（Streamable HTTP）地址
        #[arg(long)]
        url: Option<String>,
        /// 请求头，KEY=VALUE（值可写 {{secret:名字}}），逗号分隔多条
        #[arg(long)]
        header: Option<String>,
        /// 环境变量，KEY=VALUE，逗号分隔多条
        #[arg(long)]
        env: Option<String>,
    },
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
        Cmd::McpInstall { path, client, write } => {
            mcp_install(&path, &client, write)?;
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
            ConnCmd::Presets { path } => {
                conn_presets(&path)?;
            }
            ConnCmd::Add { path, name, command, args, url, header, env } => {
                conn_add(&path, &name, command.as_deref(), args.as_deref(), url.as_deref(), header.as_deref(), env.as_deref())?;
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


// ═══════════ MCP 客户端一键接入（0.30：设计工具、流程图、笔记与各 agent 双向都要接） ═══════════

fn json_mcp_write(file: &PathBuf, outer: &str, inner: Option<&str>, name: &str, entry: serde_json::Value) -> Result<()> {
    if !file.exists() {
        if let Some(dir) = file.parent() { std::fs::create_dir_all(dir)?; }
    }
    let raw = std::fs::read_to_string(file).unwrap_or_else(|_| "{}".to_string());
    let mut cfg: serde_json::Value = serde_json::from_str(&raw).context("该 agent 的配置文件不是合法 JSON")?;
    if cfg.get(outer).map(|v| !v.is_object()).unwrap_or(false) { anyhow::bail!("配置里 {outer} 不是对象：{}", file.display()); }
    let slot = cfg.as_object_mut().unwrap().entry(outer.to_string()).or_insert_with(|| serde_json::json!({}));
    let slot2 = if let Some(inner) = inner {
        let s = slot.as_object_mut().unwrap().entry(inner.to_string()).or_insert_with(|| serde_json::json!({}));
        s
    } else { slot };
    slot2.as_object_mut().unwrap().insert(name.to_string(), entry);
    let expect = format!("{}.bak-litmus-mcp", file.display());
    let bak = std::path::PathBuf::from(&expect);
    if file.exists() { let _ = std::fs::copy(file, &bak); }
    std::fs::write(file, serde_json::to_string_pretty(&cfg)? + "\n")?;
    println!("已写入 {}（原文件备份为 {}）", file.display(), bak.display());
    Ok(())
}

fn mcp_install(path: &PathBuf, client: &str, write: bool) -> Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("litmus"));
    let vault = path.canonicalize().unwrap_or_else(|_| path.clone());
    let args = vec!["mcp".to_string(), vault.display().to_string()];
    println!("库：{}", vault.display());
    println!("入口：{} mcp {}", exe.display(), vault.display());
    println!();
    let write_hint = || println!("确认无误后，加 **--write** 才会真的写进去。");
    match client {
        "cursor" | "claude" => {
            let file = if client == "cursor" {
                PathBuf::from(std::env::var("HOME")?).join(".cursor/mcp.json")
            } else {
                PathBuf::from(std::env::var("HOME")?).join("Library/Application Support/Claude/claude_desktop_config.json")
            };
            println!("目标文件：{}", file.display());
            let entry = serde_json::json!({ "command": exe, "args": args });
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": { "litmus": entry.clone() }}))?);
            if write { json_mcp_write(&file, "mcpServers", None, "litmus", entry)?; } else { write_hint(); }
        }
        "claude-code" => {
            println!("在终端跑一次（避免改坏 ~/.claude.json，交给官方 CLI）：");
            println!("  claude mcp add litmus -- {} mcp {} ", exe.display(), vault.display());
            println!("想全局可用时加 --scope user。");
        }
        "codex" => {
            let file = PathBuf::from(std::env::var("HOME")?).join(".codex/config.toml");
            let block = format!(
                "\n[mcp_servers.litmus]\ncommand = \"{}\"\nargs = [\"mcp\", \"{}\"]\n",
                exe.display(), vault.display()
            );
            println!("目标文件：{}", file.display());
            println!("追加坡段：{block}");
            if write {
                let raw = std::fs::read_to_string(&file)?;
                if raw.contains("[mcp_servers.litmus]") { println!("已有 [mcp_servers.litmus]，跳过（不重复加）。"); }
                else {
                    let bak = PathBuf::from(format!("{}.bak-litmus-mcp", file.display()));
                    let _ = std::fs::copy(&file, &bak);
                    std::fs::write(&file, raw + &block)?;
                    println!("已写入 {}（原文件备份为 {}）", file.display(), bak.display());
                }
            } else { write_hint(); }
        }
        "opencode" => {
            let file = PathBuf::from(std::env::var("HOME")?).join(".config/opencode/opencode.json");
            println!("目标文件：{}", file.display());
            let entry = serde_json::json!({ "type": "local", "command": [exe, "mcp", vault], "enabled": true });
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "mcp": { "servers": { "litmus": entry.clone() } } }))?);
            if write { json_mcp_write(&file, "mcp", Some("servers"), "litmus", entry)?; } else { write_hint(); }
        }
        "dsh" => {
            println!("DSH（DeepSeek Harness）的 MCP 挂载在它自己的设置里（设置 → MCP，配置存在 DSH 的分区配置中，由 App 保管）：");
            println!("  server 名：litmus");
            println!("  方式：stdio 命令  {}", exe.display());
            println!("  参数：mcp {}", vault.display());
            println!(" DSFH? 不改 App 内部状态，往外写请走 DSH 的设置界面或它的配置导入。");
        }
        "hermes" => {
            println!("hermes 本机在 PATH 上（~/.local/bin/hermes）。看它的 MCP 配置方式：");
            println!("  hermes mcp --help   # 有 mcp 子命令则按它的 mcp add 加：");
            println!("  hermes mcp add litmus -- {} mcp {}", exe.display(), vault.display());
            println!("若 hermes 只认通用 mcpServers JSON，用 --client generic 的输出贴进它的配置。");
        }
        "obsidian" => {
            println!("Obsidian 侧作为调用方：装 MCP 插件（如 Local REST API / mcp-obsidian）后，");
            println!("把 litmus 的 stdio 入口填进它：");
            println!("  {} mcp {}", exe.display(), vault.display());
            println!("反过来（Litmus 读 Obsidian 库）：litmus import-obsidian，或 /搬家；");
            println!("走 MCP 的话用 litmus conn add --name obsidian 参照 conn presets 的样板。");
        }
        "generic" => {
            let entry = serde_json::json!({ "command": exe, "args": args });
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": { "litmus": entry } }))?);
            println!("（HTTP 版先起本地 HTTP：另开终端跑 litmus serve {} --addr 127.0.0.1:7700；HTTP MCP 端点在 roadmap 上）", vault.display());
        }
        other => anyhow::bail!("不认识的客户端：{other}。支持：cursor | claude | claude-code | codex | opencode | dsh | hermes | obsidian | generic"),
    }
    Ok(())
}

fn conn_presets(path: &PathBuf) -> Result<()> {
    let vault = Vault::open(path)?;
    let root = vault.root.display().to_string();
    println!("把下面任一段贴进库根 litmus.toml（改动后 litmus conn list 探测）。全部先确认端点/密钥再启用。");
    println!();
    println!("# ── Notion（官方 MCP，两种接法二选一）────────────────────────");
    println!("# 远程托管版（推荐，要 node 的不用）：");
    println!("[[connections]]");
    println!("name = \"notion\"");
    println!("url = \"https://mcp.notion.com/mcp\"");
    println!("headers = {{ Authorization = \"Bearer {{{{secret:notion_token}}}}\" }}");
    println!();
    println!("# 本机 npx 版：");
    println!("[[connections]]");
    println!("name = \"notion\"");
    println!("command = \"npx\"");
    println!("args = [\"-y\", \"@notionhq/notion-mcp-server\", \"--transport=stdio\"]");
    println!("env = {{ NOTION_TOKEN = \"{{{{secret:notion_token}}}}\" }}");
    println!();
    println!("# ── Obsidian（库 ↔ 库；对方要装 Local REST 类社区插件）──────");
    println!("[[connections]]");
    println!("name = \"obsidian\"");
    println!("command = \"uvx\"");
    println!("args = [\"mcp-obsidian\"]");
    println!("env = {{ OBSIDIAN_API_KEY = \"{{{{secret:obsidian_token}}}}\" }}");
    println!();
    println!("# ── Canva（官方 MCP，Beta；先在 Canva 开通）─────────────────");
    println!("[[connections]]");
    println!("name = \"canva\"");
    println!("url = \"https://mcp.canva.com/mcp\"");
    println!("headers = {{ Authorization = \"Bearer {{{{secret:canva_token}}}}\" }}");
    println!();
    println!("# ── Open Design（本机 App 自带 MCP；照 Cursor 里的同款配置）──");
    println!("[[connections]]");
    println!("name = \"open-design\"");
    println!("command = \"/Applications/Open Design.app/Contents/Resources/open-design/bin/node\"");
    println!("args = [\"/Applications/Open Design.app/Contents/Resources/app/prebundled/daemon/daemon-cli.mjs\", \"mcp\"]");
    println!();
    println!("# ── Lovart（占位：等你的 Lovart 开通 MCP / API 后把 url 换成它的端点）──");
    println!("[[connections]]");
    println!("name = \"lovart\"");
    println!("url = \"https://LOVART_MCP_ENDPOINT/mcp\"");
    println!("headers = {{ Authorization = \"Bearer {{{{secret:lovart_token}}}}\" }}");
    println!();
    println!("# 流程图：Mermaid / draw.io / Excalidraw 暂没有公认的标准 MCP server；");
    println!("# 这类文件（.mermaid / .drawio / .excalidraw）直接拖进库，Litmus 已把它们");
    println!("# 当附件收进 Assets/ 并可被引用；等社区 server 成熟后照上面的样板接。");
    let _ = root;
    Ok(())
}

fn conn_add(path: &PathBuf, name: &str, command: Option<&str>, args: Option<&str>, url: Option<&str>, header: Option<&str>, env: Option<&str>) -> Result<()> {
    if name.is_empty() { anyhow::bail!("连接名不能为空"); }
    let vault = Vault::open(path)?;
    if vault.config.connections.iter().any(|c| c.name == name) { anyhow::bail!("已有同名连接：{name}。改名或先在 litmus.toml 里删掉再加"); }
    let toml_path = vault.root.join("litmus.toml");
    let mut text = std::fs::read_to_string(&toml_path).unwrap_or_default();
    let mut block = format!("\n[[connections]]\nname = \"{name}\"\n");
    if let Some(u) = url { block += &format!("url = \"{u}\"\n"); }
    else if let Some(c) = command {
        block += &format!("command = \"{c}\"\n");
        if let Some(a) = args {
            let items: Vec<String> = a.split_whitespace().map(|x| format!("\"{x}\"")).collect();
            block += &format!("args = [{}]\n", items.join(", "));
        }
    } else { anyhow::bail!("url 和 command 至少给一个"); }
    let kv = |v: &str| v.split(',').filter(|x| x.contains('=')).map(|x| {
        let (k, val) = x.split_once('=').unwrap();
        format!("\"{}\" = \"{}\"", k.trim(), val.trim())
    }).collect::<Vec<_>>().join(", ");
    if let Some(h) = header { block += &format!("headers = {{{}}}\n", kv(h)); }
    if let Some(e) = env { block += &format!("env = {{{}}}\n", kv(e)); }
    let bak = PathBuf::from(format!("{}.bak-litmus-conn", toml_path.display()));
    if toml_path.exists() { let _ = std::fs::copy(&toml_path, &bak); }
    text += &block;
    std::fs::write(&toml_path, &text)?;
    println!("已加 [[connections]] {name} 进 {}（备份 {}）", toml_path.display(), bak.display());
    println!("下一步：litmus conn probe {} {name}  →  litmus conn pull {} {name}", path.display(), path.display());
    Ok(())
}
