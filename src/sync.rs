//! 同步编排（design.md §7.7 分层取数）：Tier A（库清单）→ E（本地安装）→
//! [Tier B（玩家成就）∥ Tier C（全球完成度）∥ Tier D（商店详情与用户标签）并行取数]
//! → Tier F（成就类型分析）。
//! 事件化：进度经 `progress` 回调上报（CLI 打印 / Web 转 SSE），`cancel` 支持随时打断（R4）。
//! v0.47 并行化：三条限速域（Web API 8/s ∥ 商店 api 36/min ∥ 商店页 60/min）独立推进；
//! 网络请求在生成者任务里跑（并发上限只藏 RTT），DB 写入集中在主循环——rusqlite 连接不跨任务共享。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::llm::LlmClient;
use crate::models::{Achievement, AppDetail};
use crate::steam_client::{SteamClient, SteamError, SteamStore};
use crate::steam_local::{self, SteamLocal};
use crate::store::Store;

pub struct SyncOptions {
    pub steamid: Option<String>,
    pub vanity: Option<String>,
    pub local_only: bool,
    pub skip_llm: bool,
}

/// 并行取数结果：生成者任务只做网络请求，结果发回主循环由消费者串行写库
enum Fetched {
    PlayerAch { app_id: u32, name: String, res: Result<Option<Vec<Achievement>>, SteamError> },
    Global { app_id: u32, res: Result<Option<Vec<(String, f32)>>, SteamError> },
    Detail { app_id: u32, res: Result<Option<AppDetail>, SteamError> },
    Tags { app_id: u32, res: Result<Option<(Vec<String>, Option<f64>)>, SteamError> },
    Storage { app_id: u32, res: Result<Option<(Vec<String>, Option<f64>)>, SteamError> },
}

/// 结构化同步进度（v0.49）：text 进日志；pct（0-100）驱动前端进度条，
/// None = 本行不改进度（提示/ETA 行）。text 可为空 = 仅推进度条的心跳事件。
pub struct SyncProgress {
    pub text: String,
    pub pct: Option<u8>,
}

impl SyncProgress {
    /// 普通日志行（不动进度条）
    pub fn line(text: &str) -> Self {
        SyncProgress { text: text.to_string(), pct: None }
    }
    /// 带进度的日志行
    pub fn pct(text: impl Into<String>, pct: u8) -> Self {
        SyncProgress { text: text.into(), pct: Some(pct) }
    }
    /// 仅推进度条（不进日志）
    pub fn bar(pct: u8) -> Self {
        SyncProgress { text: String::new(), pct: Some(pct) }
    }
}

pub async fn run(
    db: &Path,
    cfg: &Config,
    opts: SyncOptions,
    cancel: CancellationToken,
    report: &(dyn Fn(&SyncProgress) + Send + Sync),
) -> Result<()> {
    // 行文本转发：run 内部的普通行统一走 line（pct 由关键节点单独上报）
    let line = |text: &str| report(&SyncProgress::line(text));
    let store = Store::open(db).context("打开数据库失败")?;
    let steam_dir = steam_local::resolve_steam_dir(cfg)?;
    let local = SteamLocal::open(&steam_dir);

    if opts.local_only {
        refresh_local(&store, &local, &line)?;
        return Ok(());
    }

    let key_env = if cfg.steam.api_key_env.trim().is_empty() {
        crate::config::DEFAULT_KEY_ENV.to_string()
    } else {
        cfg.steam.api_key_env.trim().to_string()
    };
    let api_key = std::env::var(&key_env)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    let proxy = crate::steam_client::resolve_proxy(cfg.proxy());
    if let Some(p) = &proxy {
        line(&format!("（检测到代理 {p}，Steam API 请求将经由代理）"));
    }
    let client = SteamClient::new(api_key, key_env.clone(), proxy.clone())?;

    let steamid = resolve_steamid(&store, &client, &local, &opts, &line).await?;

    // Tier A：库清单
    report(&SyncProgress::pct(format!("[1/6] 拉取游戏库（SteamID {steamid}）……"), 3));
    let games = client.get_owned_games(&steamid).await.context("拉取游戏库失败")?;
    store.upsert_owned_games(&games)?;
    report(&SyncProgress::bar(5));
    line(&format!(
        "      共 {} 款（含非游戏软件；剔除见画像页）",
        games.len()
    ));

    // E 层：本地安装状态
    refresh_local(&store, &local, &line)?;
    report(&SyncProgress::bar(7));

    // Tier B（玩家成就）∥ Tier C（全球完成度）∥ Tier D（商店详情/标签/存储回填）：
    // 一次性领完所有待办（Tier C/D 原本要等 B 串行跑完，30 分钟的大头就在这里），
    // 三条限速域并行推进；玩过的游戏优先——中断时画像真正要用的数据（口味/深度判定）已先入库
    let played = store.played_games()?;
    let skipped: HashSet<u32> = store.skipped_apps()?.into_iter().map(|(id, _)| id).collect();
    let todo: Vec<(u32, String)> = played
        .iter()
        .filter(|g| !skipped.contains(&g.app_id))
        .map(|g| (g.app_id, g.name.clone()))
        .collect();
    let need_global = store.owned_without_global()?;
    let mut need_details = store.owned_without_details()?;
    let mut need_tags = store.owned_without_tags()?;
    let mut need_storage = store.owned_without_storage()?;
    let played_ids: HashSet<u32> = played.iter().map(|g| g.app_id).collect();
    for v in [&mut need_details, &mut need_tags, &mut need_storage] {
        v.sort_unstable_by_key(|id| !played_ids.contains(id));
    }

    let t0 = Instant::now();
    let mut interrupted = false;
    let (mut b_done, mut b_failed) = (0usize, 0usize);

    if todo.is_empty()
        && need_global.is_empty()
        && need_details.is_empty()
        && need_tags.is_empty()
        && need_storage.is_empty()
    {
        line("[3-5/6] 玩家成就 / 全球完成度 / 商店数据：已是最新");
    } else {
        // 预计时间 = 三条限速域各自的墙钟下限取最大，再放宽 20% 兜 RTT 与重试
        let web_min = (todo.len() + need_global.len()) as f64 / 8.0 / 60.0;
        let api_min = need_details.len() as f64 / 36.0;
        let page_min = (need_tags.len() + need_storage.len()) as f64 / 60.0;
        let eta = (web_min.max(api_min).max(page_min) * 1.2).ceil() as u64;
        report(&SyncProgress::pct(
            format!(
                "[3-5/6] 并行拉取：玩家成就 {} 款（玩过 {} 款，其余已标记跳过）· 全球完成度 {} · 详情 {} · 标签 {} · 存储回填 {}",
                todo.len(),
                played.len(),
                need_global.len(),
                need_details.len(),
                need_tags.len(),
                need_storage.len()
            ),
            7,
        ));
        line(&format!(
            "      预计网络阶段约 {} 分钟（Web API 与商店双通道并行，商店接口限速 36 次/分 + 页面 60 次/分）",
            eta
        ));

        let store_client = Arc::new(SteamStore::new(proxy.clone())?);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Fetched>(64);
        // 并发上限只用来藏 RTT；实际速率由客户端里的限流器约束，不会加重风控压力
        let web_sem = Arc::new(tokio::sync::Semaphore::new(6));
        let api_sem = Arc::new(tokio::sync::Semaphore::new(3));
        let page_sem = Arc::new(tokio::sync::Semaphore::new(3));

        for (app_id, name) in &todo {
            let (client, steamid, tx, sem, cancel) =
                (client.clone(), steamid.clone(), tx.clone(), web_sem.clone(), cancel.clone());
            let (app_id, name) = (*app_id, name.clone());
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                if cancel.is_cancelled() {
                    return;
                }
                let res = client.get_player_achievements(&steamid, app_id).await;
                if cancel.is_cancelled() {
                    return;
                }
                let _ = tx.send(Fetched::PlayerAch { app_id, name, res }).await;
            });
        }
        for app_id in &need_global {
            let (client, tx, sem, cancel, app_id) =
                (client.clone(), tx.clone(), web_sem.clone(), cancel.clone(), *app_id);
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                if cancel.is_cancelled() {
                    return;
                }
                let res = client.get_global_achievement_percentages(app_id).await;
                if cancel.is_cancelled() {
                    return;
                }
                let _ = tx.send(Fetched::Global { app_id, res }).await;
            });
        }
        for app_id in &need_details {
            let (store_client, tx, sem, cancel, app_id) =
                (store_client.clone(), tx.clone(), api_sem.clone(), cancel.clone(), *app_id);
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                if cancel.is_cancelled() {
                    return;
                }
                let res = store_client.get_app_details(app_id).await;
                if cancel.is_cancelled() {
                    return;
                }
                let _ = tx.send(Fetched::Detail { app_id, res }).await;
            });
        }
        let spawn_page = |app_id: u32,
                          tx: tokio::sync::mpsc::Sender<Fetched>,
                          sem: Arc<tokio::sync::Semaphore>,
                          cancel: CancellationToken,
                          store_client: Arc<SteamStore>,
                          is_tag: bool| {
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                if cancel.is_cancelled() {
                    return;
                }
                let res = store_client.get_store_page_info(app_id).await;
                if cancel.is_cancelled() {
                    return;
                }
                let _ = tx
                    .send(if is_tag { Fetched::Tags { app_id, res } } else { Fetched::Storage { app_id, res } })
                    .await;
            });
        };
        for app_id in &need_tags {
            spawn_page(*app_id, tx.clone(), page_sem.clone(), cancel.clone(), store_client.clone(), true);
        }
        for app_id in &need_storage {
            spawn_page(*app_id, tx.clone(), page_sem.clone(), cancel.clone(), store_client.clone(), false);
        }
        drop(tx); // 生成者全数收尾后 recv 返回 None，消费者自然退出

        // 消费者（主循环）：唯一的 DB 写入方
        let total_jobs =
            (todo.len() + need_global.len() + need_details.len() + need_tags.len() + need_storage.len()).max(1);
        let mut c_failed = 0usize;
        let mut d_failed: Vec<u32> = Vec::new();
        let mut t_failed: Vec<u32> = Vec::new();
        let (mut c_done, mut d_done, mut t_done, mut s_done) = (0usize, 0usize, 0usize, 0usize);
        let mut processed = 0usize;
        while let Some(item) = rx.recv().await {
            if cancel.is_cancelled() {
                interrupted = true;
                break;
            }
            processed += 1;
            match item {
                Fetched::PlayerAch { app_id, name, res } => {
                    b_done += 1;
                    match res {
                        Ok(Some(achs)) => store.upsert_achievements(app_id, &achs)?,
                        Ok(None) => store.mark_skipped(app_id, "无成就或不可用")?,
                        Err(e) => {
                            b_failed += 1;
                            tracing::error!("{name}：{e}");
                        }
                    }
                }
                Fetched::Global { app_id, res } => {
                    c_done += 1;
                    match res {
                        Ok(Some(rows)) => store.upsert_global_achievements(app_id, &rows)?,
                        Ok(None) => store.mark_skipped(app_id, "无成就（global）")?,
                        Err(e) => {
                            c_failed += 1;
                            tracing::error!("app {app_id} 全球完成度拉取失败: {e}");
                        }
                    }
                }
                Fetched::Detail { app_id, res } => {
                    d_done += 1;
                    match res {
                        Ok(Some(d)) => store.upsert_app_detail(app_id, &d)?,
                        // success=false（商店已下架等）：记 unknown，避免每次同步重试
                        Ok(None) => store.upsert_app_detail(
                            app_id,
                            &AppDetail { app_type: "unknown".into(), genres: vec![], categories: vec![], storage_gb: None },
                        )?,
                        Err(e) => {
                            tracing::warn!("app {app_id} 商店详情拉取失败（稍后重试）: {e}");
                            d_failed.push(app_id);
                        }
                    }
                }
                Fetched::Tags { app_id, res } => {
                    t_done += 1;
                    match res {
                        Ok(Some((tags, storage_gb))) => {
                            store.upsert_store_tags(app_id, &tags)?;
                            // 同页面顺带回填存储需求（零额外请求）
                            if storage_gb.is_some() {
                                if let Some(mut d) = store.app_detail(app_id)? {
                                    d.storage_gb = storage_gb;
                                    store.upsert_app_detail(app_id, &d)?;
                                }
                            }
                        }
                        // 年龄门未过/下架：存空列表作"已处理"标记，品味键自动退回 genres
                        Ok(None) => store.upsert_store_tags(app_id, &[])?,
                        Err(e) => {
                            tracing::warn!("app {app_id} 用户标签拉取失败（稍后重试）: {e}");
                            t_failed.push(app_id);
                        }
                    }
                }
                Fetched::Storage { app_id, res } => {
                    s_done += 1;
                    // 年龄门/网络失败下次再试；Ok(None) 无存储信息可写
                    if let Ok(Some((_, Some(gb)))) = res {
                        if let Some(mut d) = store.app_detail(app_id)? {
                            d.storage_gb = Some(gb);
                            store.upsert_app_detail(app_id, &d)?;
                        }
                    }
                }
            }
            if processed % 20 == 0 {
                report(&SyncProgress::pct(
                    format!(
                        "  … 成就 {}/{} · 全球 {}/{} · 详情 {}/{} · 标签 {}/{} · 回填 {}/{}",
                        b_done,
                        todo.len(),
                        c_done,
                        need_global.len(),
                        d_done,
                        need_details.len(),
                        t_done,
                        need_tags.len(),
                        s_done,
                        need_storage.len()
                    ),
                    (7.0 + 71.0 * processed as f64 / total_jobs as f64).min(78.0) as u8,
                ));
            }
        }
        report(&SyncProgress::pct(
            format!(
                "      并行阶段完成：成就 {} 款（失败 {}）· 全球 {}（失败 {}）· 详情 {}（失败 {}）· 标签 {}（失败 {}）· 回填 {}，用时 {:.1}s",
                todo.len() - b_failed,
                b_failed,
                need_global.len() - c_failed,
                c_failed,
                need_details.len() - d_failed.len(),
                d_failed.len(),
                need_tags.len() - t_failed.len(),
                t_failed.len(),
                s_done,
                t0.elapsed().as_secs_f32()
            ),
            80,
        ));

        // 间歇性风控（429/403）补试：此时限流窗口已过去，串行小批量即可
        if !interrupted && !cancel.is_cancelled() && (!d_failed.is_empty() || !t_failed.is_empty()) {
            line(&format!(
                "  … {} 款详情 / {} 款标签失败，30 秒后补试一轮",
                d_failed.len(),
                t_failed.len()
            ));
            for _ in 0..30 {
                if cancel.is_cancelled() {
                    interrupted = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            if !interrupted && !cancel.is_cancelled() {
                let mut still = 0usize;
                for app_id in &d_failed {
                    match store_client.get_app_details(*app_id).await {
                        Ok(Some(d)) => {
                            store.upsert_app_detail(*app_id, &d)?;
                        }
                        Ok(None) => {
                            store.upsert_app_detail(
                                *app_id,
                                &AppDetail { app_type: "unknown".into(), genres: vec![], categories: vec![], storage_gb: None },
                            )?;
                        }
                        Err(_) => still += 1,
                    }
                }
                for app_id in &t_failed {
                    match store_client.get_store_page_info(*app_id).await {
                        Ok(Some((tags, storage_gb))) => {
                            store.upsert_store_tags(*app_id, &tags)?;
                            if storage_gb.is_some() {
                                if let Some(mut d) = store.app_detail(*app_id)? {
                                    d.storage_gb = storage_gb;
                                    store.upsert_app_detail(*app_id, &d)?;
                                }
                            }
                        }
                        Ok(None) => {
                            store.upsert_store_tags(*app_id, &[])?;
                        }
                        Err(_) => still += 1,
                    }
                }
                if still > 0 {
                    tracing::error!("商店数据仍有 {still} 款未拉到——多为暂时性风控，下次同步会自动重试");
                    line(&format!("  … 商店数据仍有 {still} 款未拉到（下次同步自动重试）"));
                }
            }
        }
    }

    // Tier F：成就类型在线分析（C4：LLM 判类型、代码算分数；缓存命中则跳过）
    // 分类规则版本检查：提示词/输入信号升级后（雨世界/传送门2 误分类驱动 v2），旧缓存整体失效重分析
    if !opts.skip_llm && !interrupted {
        let cur = store.meta_get("ach_cat_ver")?.unwrap_or_else(|| "1".into());
        if cur != crate::profiler::ACH_CAT_VERSION {
            let cleared = store.clear_achievement_categories()?;
            store.meta_set("ach_cat_ver", crate::profiler::ACH_CAT_VERSION)?;
            if cleared > 0 {
                line(&format!(
                    "成就分类规则升级（v{cur} → v{}）：清空 {} 条旧分类，重新在线分析",
                    crate::profiler::ACH_CAT_VERSION,
                    cleared
                ));
            }
        }
    }
    let llm = if opts.skip_llm || interrupted {
        None
    } else {
        match LlmClient::from_config(cfg) {
            Ok(c) => {
                line(&format!("（LLM：{}，定价：{}）", c.model(), c.price_note()));
                Some(c)
            }
            Err(e) => {
                line(&format!("（LLM 未启用：{e}；成就类型分析跳过，画像将仅用 tag 信号）"));
                None
            }
        }
    };
    // Tier F 的 LLM 阶段按已处理游戏数推进进度（80→97%）：首行带总数、
    // 随后每款一行"  [i/N]"——轻量文本解析（该行格式由 profiler 固定产出）
    let llm_phase = std::sync::Mutex::new((0usize, 0usize)); // (total, done)
    let llm_adapt = |s: &str| {
        let mut st = llm_phase.lock().expect("LLM 阶段进度锁");
        if st.0 == 0 {
            if let Some(rest) = s.split_once("（").map(|(_, r)| r) {
                if let Some(n) = rest.split('款').next().and_then(|x| x.trim().parse::<usize>().ok()) {
                    st.0 = n;
                }
            }
        } else if s.starts_with("  [") {
            st.1 += 1;
        }
        let pct = if st.0 > 0 {
            Some((80.0 + 17.0 * st.1 as f64 / st.0 as f64).min(97.0) as u8)
        } else {
            None
        };
        report(&SyncProgress { text: s.to_string(), pct });
    };
    let llm_stats =
        crate::profiler::analyze_missing(llm.as_ref(), &store, &llm_adapt).await?;
    if llm_stats.calls > 0 {
        line(&format!(
            "      类型分析：{} 款、{} 次调用，tokens {}入/{}出，费用 {}",
            llm_stats.games,
            llm_stats.calls,
            llm_stats.prompt_tokens,
            llm_stats.completion_tokens,
            llm_stats.cost_cny.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "未配置".into())
        ));
    }

    // LLM 游戏定位增强（用户开关）：Web 路径的定位缓存也在这里生成——
    // 否则只有 CLI profile/recommend 会跑，设置页/引导勾选后永远不生效
    if let Some(llm) = llm.as_ref() {
        if cfg.agent.llm_positioning {
            report(&SyncProgress::pct("（LLM 游戏定位增强：为未定位的游戏生成缓存，一游戏一次）", 97));
            let pos_phase = std::sync::Mutex::new((0usize, 0usize));
            let pos_adapt = |s: &str| {
                let mut st = pos_phase.lock().expect("定位阶段进度锁");
                if st.0 == 0 {
                    if let Some(rest) = s.split_once("（").map(|(_, r)| r) {
                        if let Some(n) = rest.split('款').next().and_then(|x| x.trim().parse::<usize>().ok()) {
                            st.0 = n;
                        }
                    }
                } else if s.starts_with("  [") {
                    st.1 += 1;
                }
                let pct = if st.0 > 0 {
                    Some((97.0 + 2.0 * st.1 as f64 / st.0 as f64).min(99.0) as u8)
                } else {
                    None
                };
                report(&SyncProgress { text: s.to_string(), pct });
            };
            let pos = crate::profiler::llm_position_library(llm, &store, &pos_adapt).await?;
            if pos.calls > 0 {
                line(&format!(
                    "      游戏定位：{} 款、{} 次调用，费用 {}",
                    pos.games,
                    pos.calls,
                    pos.cost_cny.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "未配置".into())
                ));
            }
        }
    }

    store.meta_set("steamid", &steamid)?;
    store.meta_set("last_sync", &now_secs().to_string())?;

    if let Ok((calls, pin, pout, cost)) = store.usage_summary() {
        if calls > 0 {
            line(&format!(
                "LLM 累计用量：{calls} 次调用，{pin} 入 / {pout} 出 tokens，累计费用 {}",
                cost.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "部分调用未配置价格".into())
            ));
        }
    }

    if interrupted {
        line(&format!("已中断：成就 {}/{}（已拉取部分均已入库）", b_done, todo.len()));
    } else {
        report(&SyncProgress::pct(
            format!(
                "同步完成：成就 {} 款、失败 {} 款，用时 {:.1}s；数据库 {}",
                todo.len() - b_failed,
                b_failed,
                t0.elapsed().as_secs_f32(),
                db.display()
            ),
            100,
        ));
    }
    Ok(())
}

fn refresh_local(store: &Store, local: &SteamLocal, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    let apps = local.installed_apps()?;
    let total = apps.len();
    let fully: Vec<_> = apps.into_iter().filter(|a| a.fully_installed).collect();
    store.replace_install_state(&fully)?;
    progress(&format!(
        "[2/6] 本地安装状态：{} 款已安装（{} 款下载中/待更新未计入）",
        fully.len(),
        total - fully.len()
    ));
    Ok(())
}

/// SteamID 解析优先级：参数直填 > vanity > 上次记录 > 本机最近登录账号（F1）。
async fn resolve_steamid(
    store: &Store,
    client: &SteamClient,
    local: &SteamLocal,
    opts: &SyncOptions,
    progress: &(dyn Fn(&str) + Send + Sync),
) -> Result<String> {
    if let Some(id) = opts.steamid.as_deref() {
        if id.len() != 17 || !id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("--steamid 应为 17 位 SteamID64（个人主页 URL 里可以找到）");
        }
        return Ok(id.to_string());
    }
    if let Some(v) = opts.vanity.as_deref() {
        if !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            bail!("--vanity 含非法字符（仅限字母数字、下划线、连字符）");
        }
        match client.resolve_vanity(v).await? {
            Some(id) => return Ok(id),
            None => bail!(
                "自定义 URL “{v}”未解析到账号（可能未设置）；请改用 SteamID64"
            ),
        }
    }
    if let Ok(Some(id)) = store.meta_get("steamid") {
        return Ok(id);
    }
    if let Ok(Some(u)) = local.most_recent_user() {
        progress(&format!("检测到本机最近登录账号：{}（{}），直接使用", u.persona_name, u.account_name));
        return Ok(u.steam_id64);
    }
    bail!("无法确定 SteamID：请在「库存」页输入 17 位 SteamID64 后再同步（CLI 用 --steamid/--vanity），成功后会自动记住")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 异常修复（库存页「尝试自动修复」）：对识别异常清单里的**数据类**异常做针对性重拉——
/// no_ach（成就拉取失败）重拉玩家成就 + 全球完成度；no_detail（商店详情失败）重拉详情与标签。
/// suspect_finished 是判断类异常（没有数据可拉），由前端快捷手动标注解决。
/// 与全量 sync 的区别：不重拉库清单、不跑 LLM（成就类型分析留待下次全量同步增量补）。
pub async fn repair(
    db: &Path,
    cfg: &Config,
    cancel: CancellationToken,
    report: &(dyn Fn(&SyncProgress) + Send + Sync),
) -> Result<()> {
    let line = |text: &str| report(&SyncProgress::line(text));
    let store = Store::open(db).context("打开数据库失败")?;

    // 本地安装状态顺带刷新（E 层，零网络成本）
    if let Ok(steam_dir) = steam_local::resolve_steam_dir(cfg) {
        refresh_local(&store, &SteamLocal::open(&steam_dir), &line)?;
    }

    let anomalies = crate::profiler::detect_anomalies(&store, cfg)?;
    let mut need_ach: Vec<u32> = Vec::new();
    let mut need_detail: Vec<u32> = Vec::new();
    let mut need_cat = false;
    for (app_id, _, kind, _) in &anomalies {
        match kind.as_str() {
            "no_ach" => need_ach.push(*app_id),
            "no_detail" => need_detail.push(*app_id),
            "ach_uncat" => need_cat = true,
            _ => {}
        }
    }
    // 进度文案用游戏名更友好
    let names: std::collections::HashMap<u32, String> = store
        .all_owned_games()?
        .into_iter()
        .map(|g| (g.app_id, g.name))
        .collect();
    let judge_only = anomalies.len() - need_ach.len() - need_detail.len();
    if need_ach.is_empty() && need_detail.is_empty() && !need_cat {
        line(&format!(
            "没有可自动修复的数据异常（{} 项为判断类，请用列表中的手动标注处理）",
            judge_only
        ));
        return Ok(());
    }
    line(&format!(
        "待修复：成就 {} 款、商店详情 {} 款、成就类型标注 {}（另有判断类 {} 项需手动标注）",
        need_ach.len(),
        need_detail.len(),
        if need_cat { "有缺失" } else { "无" },
        judge_only
    ));

    // 需要 Steam API Key 与 steamid
    let key_env = if cfg.steam.api_key_env.trim().is_empty() {
        crate::config::DEFAULT_KEY_ENV.to_string()
    } else {
        cfg.steam.api_key_env.trim().to_string()
    };
    let api_key = std::env::var(&key_env)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    let proxy = crate::steam_client::resolve_proxy(cfg.proxy());
    let client = SteamClient::new(api_key, key_env, proxy.clone())?;
    let steamid = store
        .meta_get("steamid")?
        .filter(|v| !v.is_empty())
        .context("尚未记录 SteamID：请先执行一次完整同步")?;

    let mut fixed = 0usize;
    let mut still_failed = 0usize;

    // ① 成就重拉（顺带全球完成度）
    for (i, app_id) in need_ach.iter().enumerate() {
        if cancel.is_cancelled() {
            line("（已中断：已修复部分均已入库）");
            break;
        }
        let name = names.get(app_id).cloned().unwrap_or_else(|| format!("app {app_id}"));
        match client.get_player_achievements(&steamid, *app_id).await {
            Ok(Some(achs)) => {
                store.upsert_achievements(*app_id, &achs)?;
                if let Ok(Some(rows)) = client.get_global_achievement_percentages(*app_id).await {
                    store.upsert_global_achievements(*app_id, &rows)?;
                }
                fixed += 1;
                line(&format!("  [{}] {}：{} 项成就已补齐", i + 1, name, achs.len()));
            }
            Ok(None) => {
                // Steam 明确说无成就/不可用：记跳过 + 忽略该游戏的 no_ach 提示（防修复死循环）
                store.mark_skipped(*app_id, "无成就或不可用")?;
                let _ = store.set_override(*app_id, "anomaly_done", "done");
                fixed += 1;
                line(&format!("  [{}] {}：确认无成就（Steam 返回），不再提示", i + 1, name));
            }
            Err(e) => {
                still_failed += 1;
                line(&format!("  [{}] {}：仍失败（{e}）", i + 1, name));
            }
        }
    }

    // ② 商店详情重拉（先清 unknown 哨兵行，标签同页顺带）
    if !cancel.is_cancelled() && !need_detail.is_empty() {
        let store_client = SteamStore::new(proxy)?;
        for (i, app_id) in need_detail.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let name = names.get(app_id).cloned().unwrap_or_else(|| format!("app {app_id}"));
            match store_client.get_app_details(*app_id).await {
                Ok(Some(d)) => {
                    store.upsert_app_detail(*app_id, &d)?;
                    if let Ok(Some((tags, storage_gb))) = store_client.get_store_page_info(*app_id).await {
                        store.upsert_store_tags(*app_id, &tags)?;
                        if storage_gb.is_some() {
                            let mut d2 = d.clone();
                            d2.storage_gb = storage_gb;
                            store.upsert_app_detail(*app_id, &d2)?;
                        }
                    }
                    fixed += 1;
                    line(&format!("  [{}] {}：商店详情已补齐", i + 1, name));
                }
                Ok(None) => {
                    store.upsert_app_detail(
                        *app_id,
                        &AppDetail { app_type: "unknown".into(), genres: vec![], categories: vec![], storage_gb: None },
                    )?;
                    still_failed += 1;
                    line(&format!("  [{}] {}：商店确认不可用（下架/合集包），维持剔除", i + 1, name));
                }
                Err(e) => {
                    still_failed += 1;
                    line(&format!("  [{}] {}：仍失败（{e}）", i + 1, name));
                }
            }
        }
    }

    // ③ 成就类型标注补齐（重跑 LLM 类型分析；缓存命中跳过，只补缺失款）
    if need_cat && !cancel.is_cancelled() {
        match crate::llm::LlmClient::from_config(cfg) {
            Ok(llm) => {
                line("开始补齐成就类型标注（LLM，4 路并行）……");
                let stats = crate::profiler::analyze_missing(Some(&llm), &store, &|s: &str| line(s)).await?;
                line(&format!(
                    "  类型标注补齐：{} 款、{} 次调用，费用 {}",
                    stats.games,
                    stats.calls,
                    stats.cost_cny.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "未配置".into())
                ));
                fixed += stats.games as usize;
            }
            Err(e) => {
                still_failed += 1;
                line(&format!("  LLM 未配置，无法补齐成就类型标注：{e}"));
            }
        }
    }

    line(&format!(
        "修复完成：成功 {fixed} 款、仍失败 {still_failed} 款{}",
        if judge_only > 0 { format!("；判断类 {judge_only} 项请手动标注") } else { String::new() }
    ));
    Ok(())
}
