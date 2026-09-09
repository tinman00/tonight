//! 「今晚玩什么」——Steam 游戏库推荐 Agent。

mod agent;
mod config;
mod llm;
mod models;
mod profiler;
mod recommender;
mod secrets;
mod server;
mod steam_client;
mod steam_local;
mod store;
mod sync;
mod vdf;

use std::path::{Path, PathBuf};

use anyhow::bail;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "tonight", version, about = "「今晚玩什么」——Steam 游戏库推荐 Agent")]
struct Cli {
    /// 配置文件路径
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,

    /// SQLite 数据库路径
    #[arg(long, global = true, default_value = "data/tonight.db3")]
    db: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 同步游戏库：Tier A 库清单 + 本地安装状态 + Tier B 玩家成就 + Tier C/D 补齐 + 成就类型分析
    Sync {
        /// 17 位 SteamID64
        #[arg(long)]
        steamid: Option<String>,
        /// 个人主页自定义 URL 名
        #[arg(long)]
        vanity: Option<String>,
        /// 只刷新本地安装状态，不调用远程 API
        #[arg(long)]
        local_only: bool,
        /// 跳过成就类型分析（不调用 LLM）
        #[arg(long)]
        skip_llm: bool,
    },
    /// 生成玩家画像：四维/深度分布/品类偏好/注水提议
    Profile {
        /// 逐项确认“疑似注水”提议（y 确认 / n 否决 / 其余结束）
        #[arg(long)]
        review_idle: bool,
        /// 启用 LLM 游戏定位增强（生成缓存，一游戏一次；默认取 config [agent] llm_positioning）
        #[arg(long)]
        llm_positioning: bool,
    },
    /// 内部评分推荐 Top-M（纯 Rust 打分，带分数分解；卡片生成在 LLM 里程碑）
    Recommend {
        /// 品类过滤（可多次，命中任一即可）
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// 排除品类（可多次）
        #[arg(long = "exclude-tag")]
        exclude_tags: Vec<String>,
        /// 今晚可玩的时长（分钟），用于会话适配
        #[arg(long = "max-session")]
        max_session_min: Option<u32>,
        /// 只看立即可玩（已安装且无需更新）
        #[arg(long)]
        instant_only: bool,
        /// 候选数量（默认取 config [recommender] top_m）
        #[arg(long)]
        top: Option<u32>,
        /// 启用 LLM 游戏定位增强（生成缓存，一游戏一次）
        #[arg(long)]
        llm_positioning: bool,
    },
    /// 对话式推荐：意图解析 → 确定性评分 → 推荐卡（LLM + 模板兜底）
    Ask,
    /// 清空成就类型缓存并重新在线分析（分类规则升级后手动触发；零 Steam API 调用，仅 LLM）
    Reclassify,
    /// 启动 Web Chat Box（Chat Box 里程碑）
    Serve {
        /// 启动成功后自动用默认浏览器打开页面
        #[arg(long)]
        open: bool,
        /// 监听端口（被占用时自动顺延，最多 +15）
        #[arg(long, default_value_t = 8668)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    align_cwd_to_exe();
    init_tracing();
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    let cfg = config::load(&cli.config)?;

    match cli.cmd {
        Cmd::Sync { steamid, vanity, local_only, skip_llm } => {
            let cancel = tokio_util::sync::CancellationToken::new();
            let c2 = cancel.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                c2.cancel();
            });
            sync::run(
                &cli.db,
                &cfg,
                sync::SyncOptions { steamid, vanity, local_only, skip_llm },
                cancel,
                &|p: &sync::SyncProgress| println!("{}", p.text),
            )
            .await
        }
        Cmd::Profile { review_idle, llm_positioning } => {
            run_profile(&cli.db, &cfg, review_idle, llm_positioning).await
        }
        Cmd::Recommend { tags, exclude_tags, max_session_min, instant_only, top, llm_positioning } => {
            run_recommend(&cli.db, &cfg, tags, exclude_tags, max_session_min, instant_only, top, llm_positioning).await
        }
        Cmd::Ask => agent::run_ask(&cli.db, &cfg).await,
        Cmd::Reclassify => run_reclassify(&cli.db, &cfg).await,
        Cmd::Serve { open, port } => server::run(&cli.db, &cli.config, cfg, open, port).await,
    }
}

/// 清空成就类型缓存并重新在线分析（分类规则升级 / 手动纠偏；不碰 Steam API，仅 LLM 调用）
async fn run_reclassify(db: &std::path::Path, cfg: &config::Config) -> anyhow::Result<()> {
    let store = store::Store::open(db)?;
    let cleared = store.clear_achievement_categories()?;
    store.meta_set("ach_cat_ver", profiler::ACH_CAT_VERSION)?;
    println!("已清空 {cleared} 条旧分类（规则版本 v{}），开始重新分析……", profiler::ACH_CAT_VERSION);
    let llm = llm::LlmClient::from_config(cfg)
        .map_err(|e| anyhow::anyhow!("需要 LLM：{e}"))?;
    println!("（LLM：{}，定价：{}）", llm.model(), llm.price_note());
    let stats = profiler::analyze_missing(Some(&llm), &store, &|s: &str| println!("{s}")).await?;
    println!(
        "\n重分类完成：{} 款、{} 次调用，tokens {}入/{}出，费用 {}",
        stats.games,
        stats.calls,
        stats.prompt_tokens,
        stats.completion_tokens,
        stats.cost_cny.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "未配置".into())
    );
    Ok(())
}

/// 发行包稳健性：config.toml / data/ / web/ / .env 都相对 CWD 解析。
/// 若当前目录没有 web/（不是从项目根或发行目录启动），切到 exe 所在目录，
/// 让所有相对路径落在发行包内；项目根开发时 CWD 已有 web/，行为不变。
fn align_cwd_to_exe() {
    if Path::new("web").is_dir() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("web").is_dir() {
                let _ = std::env::set_current_dir(dir);
            }
        }
    }
}

/// 日志初始化（v0.48）：stdout 照旧 + 追加写 data/logs/tonight.log（发行包用户排障用，
/// README Troubleshooting 指向该文件）。文件超过 1MB 轮转为 tonight.old（只保留一代）；
/// 文件打不开时静默退化为仅 stdout——日志层绝不能拖垮主程序。
fn init_tracing() {
    use std::io::Write as _;
    use tracing_subscriber::fmt::MakeWriter;

    struct LogFile(PathBuf);
    struct LogWriter(Option<std::fs::File>);
    struct TeeWriter { file: LogWriter, out: std::io::Stdout }
    struct Tee(LogFile);

    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match &mut self.0 {
                Some(f) => f.write(buf),
                None => Ok(buf.len()),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            match &mut self.0 {
                Some(f) => f.flush(),
                None => Ok(()),
            }
        }
    }

    impl std::io::Write for TeeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.file.write(buf);
            let _ = self.out.write(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let _ = self.file.flush();
            self.out.flush()
        }
    }

    impl<'a> MakeWriter<'a> for Tee {
        type Writer = TeeWriter;
        fn make_writer(&'a self) -> Self::Writer {
            // 轮转检查：rename 失败（占用等）不阻断，下次事件再试
            if let Ok(md) = std::fs::metadata(&self.0 .0) {
                if md.len() > 1_000_000 {
                    let _ = std::fs::rename(&self.0 .0, self.0 .0.with_extension("old"));
                }
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.0 .0)
                .ok();
            TeeWriter { file: LogWriter(file), out: std::io::stdout() }
        }
    }

    let log_path = PathBuf::from("data/logs/tonight.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(Path::new(".")));
    // 启动即 touch：零事件的干净运行也保证文件存在（README 排障指引指向它）
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&log_path);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(Tee(LogFile(log_path)))
        .init();
}

async fn run_profile(
    db: &std::path::Path,
    cfg: &config::Config,
    review_idle: bool,
    llm_positioning: bool,
) -> anyhow::Result<()> {
    let store = store::Store::open(db)?;
    // 可选：LLM 游戏定位增强（用户开关；生成缓存后 recommend 也能复用）
    if llm_positioning || cfg.agent.llm_positioning {
        match llm::LlmClient::from_config(cfg) {
            Ok(client) => {
                let stats = profiler::llm_position_library(&client, &store, &|s| println!("{s}")).await?;
                if stats.calls > 0 {
                    println!(
                        "LLM 定位：{} 款、{} 次调用，tokens {}入/{}出，费用 {}",
                        stats.games,
                        stats.calls,
                        stats.prompt_tokens,
                        stats.completion_tokens,
                        stats.cost_cny.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "未配置".into())
                    );
                }
            }
            Err(e) => println!("（LLM 定位未启用：{e}）"),
        }
    }
    let p = profiler::compute(&store, cfg)?;
    if p.games.is_empty() {
        bail!("库为空：请先运行 tonight sync");
    }
    let excluded: Vec<(&str, &str)> = p
        .games
        .iter()
        .filter_map(|g| g.exclusion.as_deref().map(|e| (g.name.as_str(), e)))
        .collect();
    println!(
        "玩家画像 —— {} 款游戏，有效时长 {:.1} 小时（剔除 {} 款）",
        p.total_games,
        p.effective_playtime_min as f64 / 60.0,
        excluded.len()
    );
    println!(
        "Bartle 四维：成就完成 {:.2} · 探索发现 {:.2} · 竞争对抗 {:.2} · 社交合作 {:.2}",
        p.axes.achiever, p.axes.explorer, p.axes.killer, p.axes.socializer
    );
    let main_type = [
        ("成就完成型", p.axes.achiever),
        ("探索发现型", p.axes.explorer),
        ("竞争对抗型", p.axes.killer),
        ("社交合作型", p.axes.socializer),
    ]
    .into_iter()
    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    if let Some((label, score)) = main_type {
        println!("主型倾向：{label}（{score:.2}）");
    }
    let depths = [
        profiler::GameDepth::Unplayed,
        profiler::GameDepth::Sampled,
        profiler::GameDepth::Active,
        profiler::GameDepth::Abandoned,
        profiler::GameDepth::Finished,
    ]
    .iter()
    .map(|d| format!("{} {}", d.as_str(), p.depth_counts.get(d).copied().unwrap_or(0)))
    .collect::<Vec<_>>()
    .join(" · ");
    println!("深度分布：{depths}");
    println!(
        "行为特征：活跃度 {:.2} · 深度 {:.2} · 广度 {:.2} · 典型会话 ~{} 分钟 · 积压率 {:.2}",
        p.behavior.activity,
        p.behavior.depth,
        p.behavior.breadth,
        p.behavior.typical_session_min,
        p.behavior.backlog_ratio
    );
    let mut tw: Vec<(&String, &f64)> = p.tag_weights.iter().collect();
    tw.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
    tw.truncate(15);
    let tags = tw.iter().map(|(k, v)| format!("{k} {v:.2}")).collect::<Vec<_>>().join(" · ");
    if !tags.is_empty() {
        println!("品类偏好（有效时长加权，Top 15 / 共 {} 项）：{tags}", p.tag_weights.len());
    }
    for e in &p.evidence {
        println!("证据：{e}");
    }
    if !p.idle_proposals.is_empty() {
        println!("疑似注水提议（机器提议，`tonight profile --review-idle` 逐项确认）：");
        for (_, name, ev) in &p.idle_proposals {
            println!("  - 《{name}》：{ev}");
        }
    }
    if !excluded.is_empty() {
        println!("剔除名单：");
        for (name, reason) in &excluded {
            println!("  - 《{name}》（{reason}）");
        }
    }
    if review_idle {
        use std::io::Write;
        for (app_id, name, ev) in &p.idle_proposals {
            println!("\n《{name}》：{ev}");
            print!("确认注水？[y=确认 n=否决 q=结束] ");
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            match line.trim() {
                "y" | "Y" => {
                    store.set_annotation_status(*app_id, "confirmed")?;
                    println!("已确认：时长从画像统计中剔除（修正层，永远覆盖自动判定）");
                }
                "n" | "N" => {
                    store.set_annotation_status(*app_id, "rejected")?;
                    println!("已否决：恢复计入画像统计");
                }
                _ => break,
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_recommend(
    db: &std::path::Path,
    cfg: &config::Config,
    tags: Vec<String>,
    exclude_tags: Vec<String>,
    max_session_min: Option<u32>,
    instant_only: bool,
    top: Option<u32>,
    llm_positioning: bool,
) -> anyhow::Result<()> {
    let store = store::Store::open(db)?;
    let use_llm = llm_positioning || cfg.agent.llm_positioning;
    if use_llm {
        match llm::LlmClient::from_config(cfg) {
            Ok(client) => {
                let stats = profiler::llm_position_library(&client, &store, &|s| println!("{s}")).await?;
                if stats.calls > 0 {
                    println!(
                        "LLM 定位：{} 款、{} 次调用，费用 {}",
                        stats.games,
                        stats.calls,
                        stats.cost_cny.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "未配置".into())
                    );
                }
            }
            Err(e) => println!("（LLM 定位未启用：{e}）"),
        }
    }
    let profile = profiler::compute(&store, cfg)?;
    if profile.games.is_empty() {
        bail!("库为空：请先运行 tonight sync");
    }
    let intent = recommender::IntentFilters {
        tags,
        exclude_tags,
        max_session_min,
        only_instant: instant_only,
        // CLI --tag 推荐不做本地限制（未安装的也能浏览）
        only_installed: false,
        top_m: top.unwrap_or(cfg.recommender.top_m),
        mood: None,
        exclude_apps: Vec::new(),
    };
    let cands = recommender::recommend(
        &store,
        cfg,
        &profile,
        &intent,
        use_llm,
        &recommender::RecommendOpts { randomness: cfg.recommender.randomness, exploration: false },
        Some(&|s: &str| println!("[探索] {s}")),
    )?;
    if cands.is_empty() {
        println!("没有符合条件的候选（试试放宽 --tag 过滤，或去掉 --instant-only）");
        return Ok(());
    }
    let w = &cfg.recommender;
    println!(
        "Top-{}（权重：动机 {:.2} · 标签 {:.2} · 可达 {:.2} · 会话 {:.2} · 即玩 {:.2} · 积压 {:.2}）",
        cands.len(), w.w_motiv, w.w_tag, w.w_attain, w.w_session, w.w_ready, w.w_backlog
    );
    println!("{:>3}  {:<26} {:<5} {:>5}   动机/标签/可达/会话/即玩/积压", "#", "游戏", "状态", "总分");
    for (i, c) in cands.iter().enumerate() {
        let b = &c.breakdown;
        let name: String = c.name.chars().take(12).collect();
        println!(
            "{:>3}  {:<26} {:<5} {:>5.2}   {:.2}/{:.2}/{:.2}/{:.2}/{:.2}/{:.2}  [{}]{}",
            i + 1,
            name,
            c.badge,
            b.total,
            b.motivation,
            b.tag,
            b.attainability,
            b.session,
            b.instant,
            b.backlog,
            c.genres.join("、"),
            if c.explore { " 「换个口味」" } else { "" }
        );
    }
    println!("\n（2a 纯评分结果；推荐卡文案生成在 LLM 里程碑接入）");
    Ok(())
}
