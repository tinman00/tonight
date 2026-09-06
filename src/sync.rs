//! 同步编排（design.md §7.7 分层取数）：Tier A（库清单）→ E（本地安装）→ Tier B（玩家成就）
//! → Tier C（全球完成度）→ Tier D（商店详情与用户标签）→ Tier F（成就类型分析）。
//! 事件化：进度经 `progress` 回调上报（CLI 打印 / Web 转 SSE），`cancel` 支持随时打断（R4）。

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::llm::LlmClient;
use crate::models::AppDetail;
use crate::steam_client::{SteamClient, SteamStore};
use crate::steam_local::{self, SteamLocal};
use crate::store::Store;

pub struct SyncOptions {
    pub steamid: Option<String>,
    pub vanity: Option<String>,
    pub local_only: bool,
    pub skip_llm: bool,
}

pub async fn run(
    db: &Path,
    cfg: &Config,
    opts: SyncOptions,
    cancel: CancellationToken,
    progress: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    let store = Store::open(db).context("打开数据库失败")?;
    let steam_dir = steam_local::resolve_steam_dir(cfg)?;
    let local = SteamLocal::open(&steam_dir);

    if opts.local_only {
        refresh_local(&store, &local, progress)?;
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
        progress(&format!("（检测到代理 {p}，Steam API 请求将经由代理）"));
    }
    let client = SteamClient::new(api_key, key_env.clone(), proxy.clone())?;

    let steamid = resolve_steamid(&store, &client, &local, &opts, progress).await?;

    // Tier A：库清单
    progress(&format!("[1/6] 拉取游戏库（SteamID {steamid}）……"));
    let games = client.get_owned_games(&steamid).await.context("拉取游戏库失败")?;
    store.upsert_owned_games(&games)?;
    progress(&format!(
        "      共 {} 款（含非游戏软件；剔除见画像页）",
        games.len()
    ));

    // E 层：本地安装状态
    refresh_local(&store, &local, progress)?;

    // Tier B：玩家成就
    let played = store.played_games()?;
    let skipped: HashSet<u32> = store.skipped_apps()?.into_iter().map(|(id, _)| id).collect();
    let todo: Vec<_> = played.iter().filter(|g| !skipped.contains(&g.app_id)).collect();
    progress(&format!(
        "[3/6] 玩家成就：待拉取 {} 款（玩过 {} 款，其余已标记跳过）",
        todo.len(),
        played.len()
    ));

    let t0 = Instant::now();
    let mut interrupted = false;
    let mut failed = 0usize;
    let mut done = 0usize;
    for (idx, g) in todo.iter().enumerate() {
        if interrupted || cancel.is_cancelled() {
            interrupted = true;
            break;
        }
        match client.get_player_achievements(&steamid, g.app_id).await {
            Ok(Some(achs)) => {
                store.upsert_achievements(g.app_id, &achs)?;
                progress(&format!("  [{:>3}/{}] {}：{} 项成就", idx + 1, todo.len(), g.name, achs.len()));
            }
            Ok(None) => {
                store.mark_skipped(g.app_id, "无成就或不可用")?;
                progress(&format!("  [{:>3}/{}] {}：跳过（无成就或不可用）", idx + 1, todo.len(), g.name));
            }
            Err(e) => {
                failed += 1;
                tracing::error!("{}：{e}", g.name);
            }
        }
        done += 1;
    }

    // Tier C：全球成就完成度（免 Key，注水判定/稀有成就/可达性/候选侧定位的难度信号；范围全库）
    if !interrupted {
        let need_global = store.owned_without_global()?;
        if need_global.is_empty() {
            progress("[4/6] 全球成就完成度：已是最新");
        } else {
            progress(&format!("[4/6] 全球成就完成度：{} 款（免 Key）", need_global.len()));
            for (i, app_id) in need_global.iter().enumerate() {
                if interrupted || cancel.is_cancelled() {
                    interrupted = true;
                    break;
                }
                match client.get_global_achievement_percentages(*app_id).await {
                    Ok(Some(rows)) => {
                        store.upsert_global_achievements(*app_id, &rows)?;
                    }
                    Ok(None) => {
                        store.mark_skipped(*app_id, "无成就（global）")?;
                    }
                    Err(e) => tracing::error!("app {app_id} 全球完成度拉取失败: {e}"),
                }
                if (i + 1) % 10 == 0 {
                    progress(&format!("  … {}/{}", i + 1, need_global.len()));
                }
            }
        }
    }

    // Tier D：商店详情（长期缓存）+ 用户投票标签（glance_tags，一次性缓存）+ 存储需求回填
    if !interrupted {
        let need_details = store.owned_without_details()?;
        let need_tags = store.owned_without_tags()?;
        let need_storage = store.owned_without_storage()?;
        if need_details.is_empty() && need_tags.is_empty() && need_storage.is_empty() {
            progress("[5/6] 商店详情与用户标签：已是最新");
        } else {
            progress(&format!(
                "[5/6] 商店详情与用户标签：详情 {} 款、标签 {} 款、存储回填 {} 款（非官方接口，限速 20 次/分）",
                need_details.len(),
                need_tags.len(),
                need_storage.len()
            ));
            let store_client = SteamStore::new(proxy.clone())?;
            // 间歇性风控（429/403）下的失败先收集，循环结束后统一补试一轮——
            // 不在主循环里 ERROR 刷屏（浏览器打开同一链接正常，就是间歇性的证据）
            let mut failed_details: Vec<u32> = Vec::new();
            let mut failed_tags: Vec<u32> = Vec::new();
            for (i, app_id) in need_details.iter().enumerate() {
                if interrupted || cancel.is_cancelled() {
                    interrupted = true;
                    break;
                }
                match store_client.get_app_details(*app_id).await {
                    Ok(Some(d)) => {
                        store.upsert_app_detail(*app_id, &d)?;
                    }
                    Ok(None) => {
                        // success=false（商店已下架等）：记 unknown，避免每次同步重试
                        store.upsert_app_detail(
                            *app_id,
                            &AppDetail { app_type: "unknown".into(), genres: vec![], categories: vec![], storage_gb: None },
                        )?;
                    }
                    Err(e) => {
                        tracing::warn!("app {app_id} 商店详情拉取失败（稍后重试）: {e}");
                        failed_details.push(*app_id);
                    }
                }
                if (i + 1) % 10 == 0 {
                    progress(&format!("  … 详情 {}/{}", i + 1, need_details.len()));
                }
            }
            for (i, app_id) in need_tags.iter().enumerate() {
                if interrupted || cancel.is_cancelled() {
                    interrupted = true;
                    break;
                }
                match store_client.get_store_page_info(*app_id).await {
                    Ok(Some((tags, storage_gb))) => {
                        store.upsert_store_tags(*app_id, &tags)?;
                        // 同页面顺带回填存储需求（零额外请求）
                        if storage_gb.is_some() {
                            if let Some(mut d) = store.app_detail(*app_id)? {
                                d.storage_gb = storage_gb;
                                store.upsert_app_detail(*app_id, &d)?;
                            }
                        }
                    }
                    Ok(None) => {
                        // 年龄门未过/下架：存空列表作"已处理"标记，品味键自动退回 genres
                        store.upsert_store_tags(*app_id, &[])?;
                    }
                    Err(e) => {
                        tracing::warn!("app {app_id} 用户标签拉取失败（稍后重试）: {e}");
                        failed_tags.push(*app_id);
                    }
                }
                if (i + 1) % 10 == 0 {
                    progress(&format!("  … 标签 {}/{}", i + 1, need_tags.len()));
                }
            }
            // 风控失败补试：此时限流窗口已过去，多数间歇性失败能一次补齐
            if !failed_details.is_empty() || !failed_tags.is_empty() {
                progress(&format!(
                    "  … {} 款详情 / {} 款标签失败，30 秒后补试一轮",
                    failed_details.len(),
                    failed_tags.len()
                ));
                for _ in 0..30 {
                    if interrupted || cancel.is_cancelled() {
                        interrupted = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            if !interrupted && !cancel.is_cancelled() {
                let mut still_details = 0usize;
                for app_id in &failed_details {
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
                        Err(_) => still_details += 1,
                    }
                }
                let mut still_tags = 0usize;
                for app_id in &failed_tags {
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
                        Err(_) => still_tags += 1,
                    }
                }
                if still_details > 0 || still_tags > 0 {
                    tracing::error!(
                        "商店数据仍有 {} 款详情 / {} 款标签未拉到——多为暂时性风控，下次同步会自动重试",
                        still_details, still_tags
                    );
                    progress(&format!(
                        "  … 商店数据仍有 {} 款未拉到（下次同步自动重试）",
                        still_details + still_tags
                    ));
                }
            }
            // 存储需求回填：已有标签但缺 storage_gb 的游戏，重抓商店页补填
            for (i, app_id) in need_storage.iter().enumerate() {
                if interrupted || cancel.is_cancelled() {
                    interrupted = true;
                    break;
                }
                match store_client.get_store_page_info(*app_id).await {
                    Ok(Some((_, storage_gb))) => {
                        if let Some(gb) = storage_gb {
                            if let Some(mut d) = store.app_detail(*app_id)? {
                                d.storage_gb = Some(gb);
                                store.upsert_app_detail(*app_id, &d)?;
                            }
                        }
                    }
                    _ => {} // 年龄门/网络失败：下次再试
                }
                if (i + 1) % 10 == 0 {
                    progress(&format!("  … 存储回填 {}/{}", i + 1, need_storage.len()));
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
                progress(&format!(
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
                progress(&format!("（LLM：{}，定价：{}）", c.model(), c.price_note()));
                Some(c)
            }
            Err(e) => {
                progress(&format!("（LLM 未启用：{e}；成就类型分析跳过，画像将仅用 tag 信号）"));
                None
            }
        }
    };
    let llm_stats =
        crate::profiler::analyze_missing(llm.as_ref(), &store, &|s: &str| progress(s)).await?;
    if llm_stats.calls > 0 {
        progress(&format!(
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
            progress("（LLM 游戏定位增强：为未定位的游戏生成缓存，一游戏一次）");
            let pos = crate::profiler::llm_position_library(llm, &store, &|s: &str| progress(s)).await?;
            if pos.calls > 0 {
                progress(&format!(
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
            progress(&format!(
                "LLM 累计用量：{calls} 次调用，{pin} 入 / {pout} 出 tokens，累计费用 {}",
                cost.map(|c| format!("¥{c:.4}")).unwrap_or_else(|| "部分调用未配置价格".into())
            ));
        }
    }

    if interrupted {
        progress(&format!("已中断：Tier B 完成 {}/{}（已拉取部分均已入库）", done, todo.len()));
    } else {
        progress(&format!(
            "同步完成：成就 {} 款、失败 {} 款，用时 {:.1}s；数据库 {}",
            todo.len() - failed,
            failed,
            t0.elapsed().as_secs_f32(),
            db.display()
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
    progress: &(dyn Fn(&str) + Send + Sync),
) -> Result<()> {
    let store = Store::open(db).context("打开数据库失败")?;

    // 本地安装状态顺带刷新（E 层，零网络成本）
    if let Ok(steam_dir) = steam_local::resolve_steam_dir(cfg) {
        refresh_local(&store, &SteamLocal::open(&steam_dir), progress)?;
    }

    let anomalies = crate::profiler::detect_anomalies(&store, cfg)?;
    let mut need_ach: Vec<u32> = Vec::new();
    let mut need_detail: Vec<u32> = Vec::new();
    for (app_id, _, kind, _) in &anomalies {
        match kind.as_str() {
            "no_ach" => need_ach.push(*app_id),
            "no_detail" => need_detail.push(*app_id),
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
    if need_ach.is_empty() && need_detail.is_empty() {
        progress(&format!(
            "没有可自动修复的数据异常（{} 项为判断类，请用列表中的手动标注处理）",
            judge_only
        ));
        return Ok(());
    }
    progress(&format!(
        "待修复：成就 {} 款、商店详情 {} 款（另有判断类 {} 项需手动标注）",
        need_ach.len(),
        need_detail.len(),
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
            progress("（已中断：已修复部分均已入库）");
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
                progress(&format!("  [{}] {}：{} 项成就已补齐", i + 1, name, achs.len()));
            }
            Ok(None) => {
                // Steam 明确说无成就/不可用：记跳过 + 忽略该游戏的 no_ach 提示（防修复死循环）
                store.mark_skipped(*app_id, "无成就或不可用")?;
                let _ = store.set_override(*app_id, "anomaly_done", "done");
                fixed += 1;
                progress(&format!("  [{}] {}：确认无成就（Steam 返回），不再提示", i + 1, name));
            }
            Err(e) => {
                still_failed += 1;
                progress(&format!("  [{}] {}：仍失败（{e}）", i + 1, name));
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
                    progress(&format!("  [{}] {}：商店详情已补齐", i + 1, name));
                }
                Ok(None) => {
                    store.upsert_app_detail(
                        *app_id,
                        &AppDetail { app_type: "unknown".into(), genres: vec![], categories: vec![], storage_gb: None },
                    )?;
                    still_failed += 1;
                    progress(&format!("  [{}] {}：商店确认不可用（下架/合集包），维持剔除", i + 1, name));
                }
                Err(e) => {
                    still_failed += 1;
                    progress(&format!("  [{}] {}：仍失败（{e}）", i + 1, name));
                }
            }
        }
    }

    progress(&format!(
        "修复完成：成功 {fixed} 款、仍失败 {still_failed} 款{}",
        if judge_only > 0 { format!("；判断类 {judge_only} 项请手动标注") } else { String::new() }
    ));
    Ok(())
}
