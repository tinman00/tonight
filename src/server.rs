//! Web 服务（design.md F13 / R2·R4·R5·R6）：axum + SSE + 静态 Chat Box 页面。
//! 数据库按请求独立开连接（单用户本地应用，busy 语义简单）；LLM 设置热更新存 meta 表。

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, AskState, TurnRecord};
use crate::config::Config;
use crate::llm::LlmClient;
use crate::profiler;
use crate::store::Store;
use crate::sync::{self, SyncOptions};

struct App {
    db: PathBuf,
    config_path: PathBuf,
    cfg: Mutex<Config>,
    sessions_dir: PathBuf,
    ask_states: Mutex<HashMap<String, AskState>>,
    sync_cancel: Mutex<Option<CancellationToken>>,
}

type Shared = Arc<App>;

/// Host 校验：只接受 127.0.0.1 / localhost / [::1]（任意端口）。
/// 服务只绑回环，但浏览器里任意网页都能向 127.0.0.1 发跨站 POST（DNS rebinding 可绕过同源），
/// 会借用户之手改写 .env 密钥或 base_url——Host 校验是一刀切的防线。
async fn localhost_only(req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| {
            let host = h.rsplit_once(':').map(|(x, _)| x).unwrap_or(h);
            matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        })
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "仅限本机访问").into_response()
    }
}

/// 依次尝试绑定 `port..port+tries`，返回（监听器，实际端口）。全部失败才报错。
async fn bind_with_fallback(port: u16, tries: u16) -> Result<(tokio::net::TcpListener, u16)> {
    let mut last_err: Option<std::io::Error> = None;
    for p in port..=port.saturating_add(tries - 1) {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, p)).await {
            Ok(l) => return Ok((l, p)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!(
        "绑定 127.0.0.1:{port}…{} 全部失败（端口被占用？可用 --port 指定其他端口）：{last_err:?}",
        port + tries - 1
    ))
}

/// 系统默认浏览器打开 URL（serve --open；不引第三方依赖）。
fn open_in_browser(url: &str) {
    #[cfg(windows)]
    let spawned = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = spawned {
        println!("（自动打开浏览器失败：{e}，请手动访问 {url}）");
    }
}

pub async fn run(db: &Path, config_path: &Path, cfg: Config, open_browser: bool, port: u16) -> Result<()> {
    let sessions_dir = db
        .parent()
        .map(|p| p.join("sessions"))
        .unwrap_or_else(|| PathBuf::from("sessions"));
    let cfg = effective_config(db, cfg)?;
    let app: Shared = Arc::new(App {
        db: db.to_path_buf(),
        config_path: config_path.to_path_buf(),
        cfg: Mutex::new(cfg),
        sessions_dir,
        ask_states: Mutex::new(HashMap::new()),
        sync_cancel: Mutex::new(None),
    });
    let router = Router::new()
        .route("/api/profile", get(api_profile))
        .route("/api/library", get(api_library))
        .route("/api/usage", get(api_usage))
        .route("/api/settings", get(api_settings).post(api_settings_post))
        .route("/api/secrets", get(api_secrets_get).post(api_secrets_post))
        .route("/api/llm/models", post(api_llm_models))
        .route("/api/llm/test", post(api_llm_test))
        .route("/api/steam/test", post(api_steam_test))
        .route("/api/llm/hint", get(api_llm_hint))
        .route("/api/annotation", post(api_annotation))
        .route("/api/override", post(api_override))
        .route("/api/feedback", post(api_feedback))
        .route("/api/feedback/reset", post(api_feedback_reset))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/{id}", get(api_session_one))
        .route("/api/sync/status", get(api_sync_status))
        .route("/api/sync/start", post(api_sync_start))
        .route("/api/sync/stop", post(api_sync_stop))
        .route("/api/repair", post(api_repair))
        .route("/api/bootstrap", get(api_bootstrap))
        .route("/api/paths", get(api_paths))
        .route("/api/onboarding/complete", post(api_onboarding_complete))
        .route("/api/ask", post(api_ask))
        // fallback 先于 layer 注册：静态文件同样过 Host 校验（DNS rebinding 防线不留豁口）
        .fallback(static_handler)
        .layer(middleware::from_fn(localhost_only))
        .with_state(app);
    // 绑定首选端口；被占用则自动顺延（8787 这类端口在开发机上很常见），最多试 16 个
    let (listener, actual) = bind_with_fallback(port, 16).await?;
    let url = format!("http://127.0.0.1:{actual}");
    if actual != port {
        println!("端口 {port} 被占用，已自动改用 {actual}");
    }
    println!("「今晚玩什么」Chat Box 已启动：{url} （Ctrl-C 停止）");
    if open_browser {
        open_in_browser(&url);
    }
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Web 设置覆盖（存 meta 表，不改动用户手写的 config.toml）。
fn effective_config(db: &Path, mut cfg: Config) -> Result<Config> {
    if let Ok(store) = Store::open(db) {
        if let Ok(Some(v)) = store.meta_get("ui_active_llm") {
            if !v.is_empty() {
                cfg.agent.active_llm = Some(v);
            }
        }
        if let Ok(Some(v)) = store.meta_get("ui_llm_positioning") {
            cfg.agent.llm_positioning = v == "1";
        }
        if let Ok(Some(v)) = store.meta_get("ui_daily_budget") {
            if let Ok(f) = v.parse::<f64>() {
                cfg.agent.daily_budget_cny = f;
            }
        }
        if let Ok(Some(v)) = store.meta_get("ui_randomness") {
            if let Ok(f) = v.parse::<f64>() {
                cfg.recommender.randomness = f.clamp(0.0, 1.0);
            }
        }
        // OpenAI 兼容端点（引导/设置页）：覆盖 active profile 的 base_url / model；
        // 空值 = 回退 config.toml 默认
        if let Ok(Some(v)) = store.meta_get("ui_llm_base_url") {
            if !v.is_empty() {
                if let Some(p) = active_llm_profile_mut(&mut cfg) {
                    p.base_url = v;
                }
            }
        }
        if let Ok(Some(v)) = store.meta_get("ui_llm_model") {
            if !v.is_empty() {
                if let Some(p) = active_llm_profile_mut(&mut cfg) {
                    p.model = v;
                }
            }
        }
        // 密钥变量名（自定义服务时 key 不叫 DEEPSEEK_API_KEY；值仍写本机 .env）
        if let Ok(Some(v)) = store.meta_get("ui_llm_key_env") {
            if !v.is_empty() {
                if let Some(p) = active_llm_profile_mut(&mut cfg) {
                    p.api_key_env = v;
                }
            }
        }
        // 价格覆盖（设置页可视化编辑；任一档填写即整体生效，覆盖内置表）。
        // 覆盖与「保存时的模型名」绑定：换模型后旧覆盖不再生效，自动回退内置表——
        // 否则给旧模型配的价格会一路错算到新模型上（真机反馈）。
        // 匹配规则与遗留清洗见 meta_price_override（hint 端点同一份逻辑）。
        if let Some(model) = cfg
            .agent
            .active_llm
            .as_ref()
            .and_then(|n| cfg.llm.get(n))
            .map(|p| p.model.clone())
        {
            if let Some((i, c, o)) = meta_price_override(&store, &model) {
                if let Some(p) = active_llm_profile_mut(&mut cfg) {
                    p.price_input_per_m = Some(i);
                    p.price_cache_per_m = c;
                    p.price_output_per_m = Some(o);
                }
            }
        }
        // 服务档案名：把覆盖后的 profile 以自定义名重新入表（显示名/配置引用一致）
        if let Ok(Some(v)) = store.meta_get("ui_llm_name") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                if let Some(name) = cfg.agent.active_llm.clone() {
                    if v != name {
                        if let Some(p) = cfg.llm.remove(&name) {
                            cfg.llm.insert(v.clone(), p);
                            cfg.agent.active_llm = Some(v);
                        }
                    }
                }
            }
        }
    }
    Ok(cfg)
}

fn active_llm_profile_mut(cfg: &mut Config) -> Option<&mut crate::config::LlmProfile> {
    let name = cfg.agent.active_llm.clone()?;
    cfg.llm.get_mut(&name)
}

/// meta 价格覆盖（设置页三档）是否适用于该模型，适用则返回 (输入, 缓存, 输出)：
/// - 有绑定（ui_price_model）须与给定模型一致——换模型后旧覆盖失效回退内置表；
/// - 无绑定的历史值保持原行为生效，但恰为旧默认价形态（1.0/2.0 无缓存档，
///   旧版设置页把预填价原样回发的产物）视为未配置；
/// - 输入或输出缺失（半填/空）不算覆盖。
/// effective_config 与 /api/llm/hint 共用，保证展示与实际生效同源。
fn meta_price_override(store: &Store, model: &str) -> Option<(f64, Option<f64>, f64)> {
    let pi = store.meta_get("ui_price_input").ok().flatten().and_then(|v| v.parse::<f64>().ok())?;
    let po = store.meta_get("ui_price_output").ok().flatten().and_then(|v| v.parse::<f64>().ok())?;
    let pc = store.meta_get("ui_price_cache").ok().flatten().and_then(|v| v.parse::<f64>().ok());
    let bound = store.meta_get("ui_price_model").ok().flatten().filter(|v| !v.is_empty());
    let applies = match bound {
        Some(b) => b == model,
        None => !(pi == 1.0 && po == 2.0 && pc.is_none()),
    };
    if !applies {
        return None;
    }
    Some((pi, pc, po))
}

fn err_json(msg: impl std::fmt::Display) -> Json<serde_json::Value> {
    Json(json!({ "error": msg.to_string() }))
}

// ============ 画像 / 用量 / 设置 / 标注 ============

async fn api_profile(State(app): State<Shared>) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    let p = match profiler::compute(&store, &cfg) {
        Ok(p) => p,
        Err(e) => return err_json(format!("{e:#}")),
    };
    let top_tags: Vec<(String, f64)> = {
        let mut tw: Vec<(&String, &f64)> = p.tag_weights.iter().collect();
        tw.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        tw.iter().take(20).map(|(k, v)| (k.to_string(), **v)).collect()
    };
    let annotations: std::collections::HashMap<u32, String> = store
        .annotations()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _k, status, _)| (id, status))
        .collect();
    let idle = p
        .idle_proposals
        .iter()
        .map(|(id, name, ev)| {
            json!({
                "app_id": id,
                "name": name,
                "evidence": ev,
                "status": annotations.get(id).cloned().unwrap_or_else(|| "proposed".into()),
            })
        })
        .collect::<Vec<_>>();
    let excluded = p
        .games
        .iter()
        .filter_map(|g| {
            g.exclusion
                .as_ref()
                .map(|r| json!({"app_id": g.app_id, "name": g.name, "reason": r}))
        })
        .collect::<Vec<_>>();
    // 修正层口味排除（跨会话生效，画像页可撤销）
    let taste_excludes: Vec<serde_json::Value> = store
        .taste_excludes()
        .unwrap_or_default()
        .into_iter()
        .map(|t| json!({"tag": t}))
        .collect();
    // 行为反馈（P1-d）：即玩自适应系数、事件计数、当前带疲劳降权的游戏数
    let fatigue_apps = store
        .fatigue_map()
        .map(|m| m.values().filter(|(p, _)| *p < 0.0).count())
        .unwrap_or(0);
    let events = store.feedback_event_counts().unwrap_or_default();
    let behavior_feedback = json!({
        "install_affinity": store.install_affinity(),
        "fatigue_apps": fatigue_apps,
        "events": {
            "launch": events.get("launch").copied().unwrap_or(0),
            "dismiss": events.get("dismiss").copied().unwrap_or(0),
            "skip": events.get("skip").copied().unwrap_or(0),
            "download": events.get("download").copied().unwrap_or(0),
            "impression": events.get("impression").copied().unwrap_or(0),
        },
    });
    Json(json!({
        "total_games": p.total_games,
        "effective_hours": (p.effective_playtime_min as f64 / 60.0 * 10.0).round() / 10.0,
        "axes": {
            "achiever": p.axes.achiever,
            "explorer": p.axes.explorer,
            "killer": p.axes.killer,
            "socializer": p.axes.socializer,
        },
        "depth_counts": {
            "未开封": p.depth_counts.get(&profiler::GameDepth::Unplayed).copied().unwrap_or(0),
            "试玩即弃": p.depth_counts.get(&profiler::GameDepth::Sampled).copied().unwrap_or(0),
            "活跃中": p.depth_counts.get(&profiler::GameDepth::Active).copied().unwrap_or(0),
            "暂离": p.depth_counts.get(&profiler::GameDepth::Service).copied().unwrap_or(0),
            "弃坑": p.depth_counts.get(&profiler::GameDepth::Abandoned).copied().unwrap_or(0),
            "已完成": p.depth_counts.get(&profiler::GameDepth::Finished).copied().unwrap_or(0),
        },
        "behavior": {
            "activity": p.behavior.activity,
            "depth": p.behavior.depth,
            "breadth": p.behavior.breadth,
            "typical_session_min": p.behavior.typical_session_min,
            "backlog_ratio": p.behavior.backlog_ratio,
        },
        "tag_weights": top_tags,
        "rare_achievements_owned": p.rare_achievements_owned,
        "evidence": p.evidence,
        "idle_proposals": idle,
        "taste_excludes": taste_excludes,
        "behavior_feedback": behavior_feedback,
        "excluded": excluded,
    }))
}

async fn api_library(State(app): State<Shared>) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    let p = match profiler::compute(&store, &cfg) {
        Ok(p) => p,
        Err(e) => return err_json(format!("{e:#}")),
    };
    let installed = store.installed_map().unwrap_or_default();
    // 手动深度标注的 app（chip 高亮 + 可点击菜单）
    let depth_overridden: std::collections::HashSet<u32> = store
        .overrides()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, kind, _)| kind == "depth")
        .map(|(id, _, _)| id)
        .collect();
    // 工具软件（非游戏）不进库存视图；注水标注的游戏保留（它们是真游戏，只是数据可疑）
    let games: Vec<serde_json::Value> = p
        .games
        .iter()
        .filter(|g| g.exclusion.as_deref() != Some("非游戏软件"))
        .map(|g| {
            let (badge, needs_update) = match installed.get(&g.app_id) {
                Some(false) => ("已安装", false),
                Some(true) => ("需更新", true),
                None => ("未安装", false),
            };
            // 成就总数过少时完成度是统计噪音（CS2 单成就 100%），不给前端展示
            let completion = match g.total_achievements >= 10 {
                true => g.completion.map(|c| (c * 100.0).round() as i32),
                false => None,
            };
            json!({
                "app_id": g.app_id,
                "name": g.name,
                "hours": ((g.playtime_min as f64) / 60.0 * 10.0).round() / 10.0,
                "depth": g.depth.as_str(),
                "depth_override": depth_overridden.contains(&g.app_id),
                "completion": completion,
                "excluded": g.exclusion,
                "badge": badge,
                "needs_update": needs_update,
            })
        })
        .collect();
    Json(json!({
        "total": games.len(),
        "last_sync": store.meta_get("last_sync").ok().flatten(),
        "depth_options": profiler::GameDepth::all_str(),
        "anomalies": profiler::detect_anomalies(&store, &cfg)
            .unwrap_or_default()
            .into_iter()
            .map(|(app_id, name, kind, hint)| json!({
                "app_id": app_id,
                "name": name,
                "kind": kind,
                "hint": hint,
            }))
            .collect::<Vec<_>>(),
        "games": games,
    }))
}

async fn api_usage(State(app): State<Shared>) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    let (calls, pt, ct, cost) = store.usage_summary().unwrap_or((0, 0, 0, None));
    let today = store.today_usage_cny().unwrap_or(0.0);
    let unpriced = store.unpriced_call_count().unwrap_or(0);
    let price_note = LlmClient::from_config(&cfg)
        .map(|c| c.price_note())
        .unwrap_or_else(|e| format!("未配置：{e}"));
    Json(json!({
        "calls": calls,
        "prompt_tokens": pt,
        "completion_tokens": ct,
        "cost_cny": cost,
        "today_cost_cny": (today * 10000.0).round() / 10000.0,
        "daily_budget_cny": cfg.agent.daily_budget_cny,
        "price_note": price_note,
        // 价格未知的调用（不在内置表且未配价）：费用与预算均不计，界面明示避免误读
        "unpriced_calls": unpriced,
    }))
}

async fn api_settings(State(app): State<Shared>) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    let llm_info = LlmClient::from_config(&cfg)
        .map(|c| json!({"model": c.model(), "price_note": c.price_note()}))
        .unwrap_or(json!({"model": null, "price_note": "未配置"}));
    // 生效价格三档（编辑预填：直接从 effective profile 解析，不依赖 key 是否已配置）
    let price = cfg
        .agent
        .active_llm
        .as_ref()
        .and_then(|n| cfg.llm.get(n))
        .and_then(|p| crate::llm::resolve_price(p).0)
        .map(|p| (p.input_per_m, p.cache_input_per_m, p.output_per_m));
    let (base_url, key_env) = cfg
        .agent
        .active_llm
        .as_ref()
        .and_then(|n| cfg.llm.get(n))
        .map(|p| (p.base_url.clone(), p.api_key_env.clone()))
        .unwrap_or_default();
    let auto_recommend = Store::open(&app.db)
        .ok()
        .and_then(|s| s.meta_get("ui_auto_recommend").ok().flatten())
        .map(|v| v != "0")
        .unwrap_or(true);
    Json(json!({
        "active_llm": cfg.agent.active_llm,
        "available": cfg.llm.keys().collect::<Vec<_>>(),
        "base_url": base_url,
        "llm_key_env": key_env,
        "llm_positioning": cfg.agent.llm_positioning,
        "daily_budget_cny": cfg.agent.daily_budget_cny,
        "randomness": cfg.recommender.randomness,
        "auto_recommend": auto_recommend,
        "price_input": price.map(|(i, _, _)| i),
        "price_cache": price.and_then(|(_, c, _)| c),
        "price_output": price.map(|(_, _, o)| o),
        "llm": llm_info,
    }))
}

#[derive(serde::Deserialize)]
struct SettingsReq {
    active_llm: Option<String>,
    llm_positioning: Option<bool>,
    daily_budget_cny: Option<f64>,
    randomness: Option<f64>,
    /// 打开页面时自动「猜你想玩」（默认开）
    auto_recommend: Option<bool>,
    /// OpenAI 兼容端点（空串 = 清除覆盖、回退 config.toml）
    llm_base_url: Option<String>,
    llm_model: Option<String>,
    /// 服务档案名（自定义服务起名，如 kimi/glm；空 = 沿用 config 名）
    llm_name: Option<String>,
    /// 密钥环境变量名（自定义服务的 key 变量；空 = 沿用 profile 默认）
    llm_key_env: Option<String>,
    /// 价格覆盖（元/百万 tokens；input+output 都为 Some 才生效，cache 可选；NaN/负数拒绝）
    price_input: Option<f64>,
    price_cache: Option<f64>,
    price_output: Option<f64>,
    /// 清除价格覆盖（回到内置表/config 默认）
    price_reset: Option<bool>,
}

async fn api_settings_post(State(app): State<Shared>, Json(req): Json<SettingsReq>) -> impl IntoResponse {
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    if let Some(v) = &req.active_llm {
        let _ = store.meta_set("ui_active_llm", v);
    }
    if let Some(v) = req.llm_positioning {
        let _ = store.meta_set("ui_llm_positioning", if v { "1" } else { "0" });
    }
    if let Some(v) = req.daily_budget_cny {
        let _ = store.meta_set("ui_daily_budget", &format!("{v}"));
    }
    if let Some(v) = req.randomness {
        let _ = store.meta_set("ui_randomness", &format!("{}", v.clamp(0.0, 1.0)));
    }
    if let Some(v) = req.auto_recommend {
        let _ = store.meta_set("ui_auto_recommend", if v { "1" } else { "0" });
    }
    if let Some(v) = &req.llm_base_url {
        let v = v.trim();
        if !v.is_empty() && !(v.starts_with("http://") || v.starts_with("https://")) {
            return err_json("base_url 必须以 http:// 或 https:// 开头");
        }
        let _ = store.meta_set("ui_llm_base_url", v);
    }
    if let Some(v) = &req.llm_model {
        let _ = store.meta_set("ui_llm_model", v.trim());
    }
    if let Some(v) = &req.llm_name {
        let _ = store.meta_set("ui_llm_name", v.trim());
    }
    if let Some(v) = &req.llm_key_env {
        let v = v.trim();
        // 变量名只允许字母数字下划线（写入 .env 的键名安全性）
        if !v.is_empty() && !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return err_json("密钥变量名只能包含字母、数字、下划线");
        }
        let _ = store.meta_set("ui_llm_key_env", v);
    }
    if req.price_reset == Some(true) {
        let _ = store.meta_set("ui_price_input", "");
        let _ = store.meta_set("ui_price_cache", "");
        let _ = store.meta_set("ui_price_output", "");
        let _ = store.meta_set("ui_price_model", "");
    } else if let (Some(i), Some(o)) = (req.price_input, req.price_output) {
        if !(i.is_finite() && i >= 0.0 && o.is_finite() && o >= 0.0) {
            return err_json("价格必须是非负数字");
        }
        let cache_ok = req.price_cache.map(|c| c.is_finite() && c >= 0.0).unwrap_or(true);
        if !cache_ok {
            return err_json("缓存命中价必须是非负数字");
        }
        // 绑定目标模型：本次请求改了模型就绑新模型，否则绑当前生效模型——
        // 换模型后此覆盖自动失效（effective_config 校验 ui_price_model）
        let bound_model = req
            .llm_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                store
                    .meta_get("ui_llm_model")
                    .ok()
                    .flatten()
                    .filter(|v| !v.is_empty())
                    .or_else(|| {
                        let cfg = app.cfg.lock().unwrap();
                        cfg.agent
                            .active_llm
                            .as_ref()
                            .and_then(|n| cfg.llm.get(n))
                            .map(|p| p.model.clone())
                    })
            });
        let _ = store.meta_set("ui_price_input", &format!("{i}"));
        let _ = store.meta_set("ui_price_output", &format!("{o}"));
        match req.price_cache {
            Some(c) => {
                let _ = store.meta_set("ui_price_cache", &format!("{c}"));
            }
            None => {
                let _ = store.meta_set("ui_price_cache", "");
            }
        }
        if let Some(m) = bound_model {
            let _ = store.meta_set("ui_price_model", &m);
        }
    }
    // 热更新内存配置。必须从 config.toml 重读原始值再套 meta 覆盖——直接在内存 cfg 上叠加
    // 会让"清除覆盖回退默认"失效（内存里已被上一轮覆盖污染，原始值丢失，真机踩过：
    // 改回默认 key_env/价格后仍读到旧覆盖值）。
    // 注意：clone/load 必须先落在独立语句——scrutinee 的临时 MutexGuard 活到 if-let 结束，
    // body 里的二次 lock() 变成同线程重入加锁 → 死锁（真机踩过）。
    let fresh_cfg = crate::config::load(&app.config_path)
        .unwrap_or_else(|_| app.cfg.lock().unwrap().clone());
    if let Ok(new_cfg) = effective_config(&app.db, fresh_cfg) {
        *app.cfg.lock().unwrap() = new_cfg;
    }
    Json(json!({"ok": true}))
}

// ============ 密钥（Web 设置写 .env，安全设计见 secrets.rs / localhost_only）============

/// 当前密钥状态：只暴露「是否已设置 + 尾 4 位」，永不返回明文。
fn key_status(cfg: &Config) -> serde_json::Value {
    let steam_env = cfg.steam.api_key_env.clone();
    let llm_env = cfg
        .agent
        .active_llm
        .as_ref()
        .and_then(|n| cfg.llm.get(n))
        .map(|p| p.api_key_env.clone());
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let tail = |v: &str| {
        let chars: Vec<char> = v.chars().collect();
        if chars.len() >= 4 {
            chars[chars.len() - 4..].iter().collect()
        } else {
            "****".to_string()
        }
    };
    let steam = read(&steam_env);
    let llm = llm_env.as_deref().and_then(read);
    json!({
        "steam_key_env": steam_env,
        "steam_key_set": steam.is_some(),
        "steam_key_tail": steam.as_deref().map(tail),
        "llm_key_env": llm_env,
        "llm_key_set": llm.is_some(),
        "llm_key_tail": llm.as_deref().map(tail),
    })
}

async fn api_secrets_get(State(app): State<Shared>) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    Json(key_status(&cfg))
}

#[derive(serde::Deserialize)]
struct SecretsReq {
    steam_key: Option<String>,
    llm_key: Option<String>,
}

async fn api_secrets_post(State(app): State<Shared>, Json(req): Json<SecretsReq>) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    let mut updates: Vec<(String, String)> = Vec::new();
    if let Some(raw) = req.steam_key {
        match crate::secrets::validate_key(&raw) {
            Ok(v) => updates.push((cfg.steam.api_key_env.clone(), v)),
            Err(e) => return err_json(format!("Steam API Key：{e}")),
        }
    }
    if let Some(raw) = req.llm_key {
        let env = cfg
            .agent
            .active_llm
            .as_ref()
            .and_then(|n| cfg.llm.get(n))
            .map(|p| p.api_key_env.clone());
        let Some(env) = env else {
            return err_json("LLM 未配置：config.toml 缺少 [agent] active_llm 对应段");
        };
        match crate::secrets::validate_key(&raw) {
            Ok(v) => updates.push((env, v)),
            Err(e) => return err_json(format!("LLM API Key：{e}")),
        }
    }
    if updates.is_empty() {
        return err_json("没有要保存的密钥（输入留空 = 不修改）");
    }
    // .env 与 config.toml 同在 CWD（启动时已对齐 exe 目录）；写盘 + set_var 立即生效
    for (name, value) in &updates {
        if let Err(e) = crate::secrets::set_env_var(Path::new(".env"), name, value) {
            return err_json(format!("写入 .env 失败：{e}"));
        }
        std::env::set_var(name, value);
    }
    Json(key_status(&cfg))
}

// ============ LLM / Steam 连接探测（测试连接按钮，v0.48）============

#[derive(serde::Deserialize, Default)]
struct LlmModelsReq {
    /// 刚填还没保存的 base_url / key 优先（引导页主场景：填完直接拉取再保存）；
    /// 留空则用当前生效配置的 base_url 与已存密钥
    base_url: Option<String>,
    key: Option<String>,
}

/// LLM 错误 → 中文排查指引 + 原始错误（有指引才加前缀）
fn llm_err_text(e: &crate::llm::LlmError) -> String {
    match e.friendly_hint() {
        Some(hint) => format!("{hint}（{e}）"),
        None => format!("{e}"),
    }
}

/// anyhow 链上找 LlmError 做 friendly 化（/api/ask 等聚合错误路径用）；找不到保留原链
fn friendly_llm_error(e: &anyhow::Error) -> String {
    let mut cur: Option<&dyn std::error::Error> = Some(e.as_ref());
    while let Some(err) = cur {
        if let Some(le) = err.downcast_ref::<crate::llm::LlmError>() {
            return llm_err_text(le);
        }
        cur = err.source();
    }
    format!("{e:#}")
}

/// 解析 LLM 探测的 base_url/key（请求值优先 → 生效配置/环境变量回退）。
/// 请求携带的临时 key 同样过 validate_key（防脏值直接打向网络）。
fn resolve_llm_probe(
    cfg: &crate::config::Config,
    req_base: Option<&str>,
    req_key: Option<&str>,
) -> Result<(String, String), String> {
    let profile = cfg.agent.active_llm.as_ref().and_then(|n| cfg.llm.get(n));
    let base_url = req_base
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| profile.map(|p| p.base_url.clone()))
        .ok_or_else(|| "未提供 base_url，且当前没有生效的 LLM 服务".to_string())?;
    let key = req_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            profile
                .and_then(|p| std::env::var(&p.api_key_env).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| "请先填写 LLM API Key".to_string())?;
    let key = crate::secrets::validate_key(&key).map_err(|e| format!("Key 格式问题：{e}"))?;
    Ok((base_url, key))
}

async fn api_llm_models(State(app): State<Shared>, body: String) -> impl IntoResponse {
    let req = serde_json::from_str::<LlmModelsReq>(&body).unwrap_or_default();
    let cfg = app.cfg.lock().unwrap().clone();
    let (base_url, key) = match resolve_llm_probe(&cfg, req.base_url.as_deref(), req.key.as_deref()) {
        Ok(v) => v,
        Err(msg) => return err_json(msg),
    };
    match crate::llm::fetch_models(&base_url, &key).await {
        Ok(models) => Json(json!({ "models": models })),
        Err(e) => err_json(llm_err_text(&e)),
    }
}

/// LLM 配置一键测试（免费）：GET /models 探活端点 + Key，顺带核对目标模型是否在列表中。
/// 不发 chat 请求，零 token 消耗、无限流压力（用户反馈：测试要考虑限流和计费）。
async fn api_llm_test(State(app): State<Shared>, body: String) -> impl IntoResponse {
    #[derive(serde::Deserialize, Default)]
    struct LlmTestReq {
        base_url: Option<String>,
        key: Option<String>,
        model: Option<String>,
    }
    let req = serde_json::from_str::<LlmTestReq>(&body).unwrap_or_default();
    let cfg = app.cfg.lock().unwrap().clone();
    let (base_url, key) = match resolve_llm_probe(&cfg, req.base_url.as_deref(), req.key.as_deref()) {
        Ok(v) => v,
        Err(msg) => return Json(json!({ "ok": false, "message": msg })),
    };
    let profile = cfg.agent.active_llm.as_ref().and_then(|n| cfg.llm.get(n));
    let model = req
        .model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| profile.map(|p| p.model.clone()))
        .unwrap_or_default();
    match crate::llm::fetch_models(&base_url, &key).await {
        Ok(models) => {
            let known = models.iter().any(|m| *m == model);
            let note = if model.is_empty() {
                String::new()
            } else if known {
                format!("，模型 {model} 在列表中")
            } else {
                // DeepSeek 的 deepseek-chat 是官方别名，不在 /models 返回里也合法——提示但不判失败
                format!("；注意：模型 {model} 不在返回列表中（官方别名如 deepseek-chat 可能如此，可照常使用）")
            };
            Json(json!({
                "ok": true,
                "message": format!("连接成功：端点与 Key 有效，共 {} 个模型（免费探测，未消耗 tokens）{}", models.len(), note),
                "models": models,
            }))
        }
        Err(e) => Json(json!({ "ok": false, "message": llm_err_text(&e) })),
    }
}

/// Steam Web API Key 一键测试：resolve_vanity 探活（1 次调用、不触碰用户库数据）。
/// 返回 200 即 Key 有效；401 = Key 无效；网络错误提示代理。
async fn api_steam_test(State(app): State<Shared>, body: String) -> impl IntoResponse {
    #[derive(serde::Deserialize, Default)]
    struct SteamTestReq {
        key: Option<String>,
    }
    let req = serde_json::from_str::<SteamTestReq>(&body).unwrap_or_default();
    let cfg = app.cfg.lock().unwrap().clone();
    let key_env = if cfg.steam.api_key_env.trim().is_empty() {
        crate::config::DEFAULT_KEY_ENV.to_string()
    } else {
        cfg.steam.api_key_env.trim().to_string()
    };
    let key = req
        .key
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var(&key_env)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let Some(key) = key else {
        return Json(json!({ "ok": false, "message": format!("尚未填写 Steam Web API Key（{key_env}）") }));
    };
    if let Err(e) = crate::secrets::validate_key(&key) {
        return Json(json!({ "ok": false, "message": format!("Key 格式问题：{e}") }));
    }
    let proxy = crate::steam_client::resolve_proxy(cfg.proxy());
    let client = match crate::steam_client::SteamClient::new(Some(key), key_env, proxy) {
        Ok(c) => c,
        Err(e) => return Json(json!({ "ok": false, "message": format!("{e}") })),
    };
    // Gabe 的 vanity 名——只为验证 Key 能过鉴权，不涉及任何用户数据
    match client.resolve_vanity("gabelogannewell").await {
        Ok(_) => Json(json!({ "ok": true, "message": "连接成功：Steam Web API Key 有效（一次探测调用，未读取你的数据）" })),
        Err(e) => {
            let msg = match &e {
                crate::steam_client::SteamError::Http { status: 401, .. } => {
                    "Steam 返回 401：Key 无效，请到 steamcommunity.com/dev/apikey 重新生成".to_string()
                }
                // 实测：resolve_vanity 对无效 Key 返回 403 且正文注明 verify your key=
                // （GetOwnedGames 才是 401/403=资料不公开），两种可能都提示
                crate::steam_client::SteamError::Http { status: 403, .. } => {
                    "Steam 返回 403：Key 无效，或代理出口受限——先核对 Key，仍失败再检查代理设置".to_string()
                }
                crate::steam_client::SteamError::Http { status: 429, .. } => {
                    "Steam 返回 429：触发限流，稍等片刻再测".to_string()
                }
                crate::steam_client::SteamError::Network(_) => {
                    "网络错误：连不上 api.steampowered.com；大陆网络通常需要代理（config.toml [network] proxy 或系统代理）".to_string()
                }
                other => format!("{other}"),
            };
            Json(json!({ "ok": false, "message": format!("{msg}（{e}）") }))
        }
    }
}

/// 参数约束提示：按内置规则表匹配 base_url/model（省略的参数回退当前生效配置），
/// 前端只展示 notice，不自带匹配逻辑。
async fn api_llm_hint(
    State(app): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    let profile = cfg.agent.active_llm.as_ref().and_then(|n| cfg.llm.get(n));
    let base_url = q
        .get("base_url")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| profile.map(|p| p.base_url.clone()))
        .unwrap_or_default();
    let model = q
        .get("model")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| profile.map(|p| p.model.clone()))
        .unwrap_or_default();
    let rule = crate::llm::builtin_param_rule(&base_url, &model);
    // 该模型将生效的价格：绑定匹配的覆盖优先，否则内置表/本地免费——
    // 设置页/引导页切换模型时价格预填跟随此值（未被手动改过的字段）
    let price = Store::open(&app.db)
        .ok()
        .and_then(|s| meta_price_override(&s, &model))
        .map(|(i, c, o)| json!({"input": i, "cache": c, "output": o}))
        .or_else(|| {
            crate::llm::peek_builtin_price(&base_url, &model)
                .map(|p| json!({"input": p.input_per_m, "cache": p.cache_input_per_m, "output": p.output_per_m}))
        });
    Json(json!({ "notice": rule.notice, "price": price }))
}

// ============ 首次引导（一次性向导）============

/// 引导/启动聚合状态：前端一次拉全「key 是否就绪 + 库里有没有游戏 + 本机 Steam 账号」。
async fn api_bootstrap(State(app): State<Shared>) -> impl IntoResponse {
    let cfg = app.cfg.lock().unwrap().clone();
    let store = Store::open(&app.db).ok();
    // none=从未走完向导；done=走完；skip=用户主动跳过（不再自动弹）
    let onboarded_state = store
        .as_ref()
        .and_then(|s| s.meta_get("onboarded").ok().flatten())
        .map(|v| match v.as_str() {
            "1" => "done",
            "skip" => "skip",
            _ => "none",
        })
        .unwrap_or("none")
        .to_string();
    let onboarded = onboarded_state != "none";
    let owned = store
        .as_ref()
        .and_then(|s| s.owned_count().ok())
        .unwrap_or(0);
    let steamid_set = store
        .as_ref()
        .and_then(|s| s.meta_get("steamid").ok().flatten())
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let llm_ready = LlmClient::from_config(&cfg).is_ok();
    let (base_url, model) = cfg
        .agent
        .active_llm
        .as_ref()
        .and_then(|n| cfg.llm.get(n))
        .map(|p| (p.base_url.clone(), p.model.clone()))
        .unwrap_or_default();
    // 本机 Steam 客户端最近登录的账号（有就给引导页预填，减少手输 17 位 ID）
    let local_account = crate::steam_local::resolve_steam_dir(&cfg)
        .ok()
        .map(crate::steam_local::SteamLocal::open)
        .and_then(|local| local.most_recent_user().ok().flatten())
        .map(|u| json!({"steamid": u.steam_id64, "persona_name": u.persona_name}));
    let mut status = key_status(&cfg);
    status["onboarded"] = json!(onboarded);
    status["onboarded_state"] = json!(onboarded_state);
    status["llm_ready"] = json!(llm_ready);
    status["base_url"] = json!(base_url);
    status["model"] = json!(model);
    status["owned"] = json!(owned);
    status["steamid_set"] = json!(steamid_set);
    status["local_account"] = local_account.unwrap_or(serde_json::Value::Null);
    // 「猜你想玩」自动推荐开关（默认开；meta "0" = 关）——对话页打开时是否自动推一轮
    status["auto_recommend"] = json!(store
        .as_ref()
        .and_then(|s| s.meta_get("ui_auto_recommend").ok().flatten())
        .map(|v| v != "0")
        .unwrap_or(true));
    // LLM 游戏定位增强（引导/设置页勾选；同步时生成缓存）
    status["llm_positioning"] = json!(cfg.agent.llm_positioning);
    // 服务身份与价格（引导页「自定义服务」高级块预填；镜像 /api/settings 的取值，
    // resolve_price 直接从 effective profile 解析，不依赖 key 是否已配置）
    let profile = cfg.agent.active_llm.as_ref().and_then(|n| cfg.llm.get(n));
    let price = profile
        .and_then(|p| crate::llm::resolve_price(p).0)
        .map(|p| (p.input_per_m, p.cache_input_per_m, p.output_per_m));
    status["llm_name"] = json!(cfg.agent.active_llm);
    status["llm_key_env"] = json!(profile.map(|p| p.api_key_env.clone()).unwrap_or_default());
    status["price_input"] = json!(price.map(|(i, _, _)| i));
    status["price_cache"] = json!(price.and_then(|(_, c, _)| c));
    status["price_output"] = json!(price.map(|(_, _, o)| o));
    Json(status)
}

/// 完成或跳过首次引导。body 可省略（老调用 = 完成）；{"skipped":true} = 用户主动跳过，
/// 之后不再自动弹（可从设置页手动重开）。
async fn api_onboarding_complete(State(app): State<Shared>, body: String) -> impl IntoResponse {
    #[derive(serde::Deserialize, Default)]
    struct OnboardingReq {
        #[serde(default)]
        skipped: bool,
    }
    let skipped = serde_json::from_str::<OnboardingReq>(&body)
        .map(|r| r.skipped)
        .unwrap_or(false);
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    let value = if skipped { "skip" } else { "1" };
    match store.meta_set("onboarded", value) {
        Ok(_) => Json(json!({"ok": true})),
        Err(e) => err_json(e),
    }
}

#[derive(serde::Deserialize)]
struct AnnotationReq {
    app_id: u32,
    status: String, // confirmed / rejected
    /// 口味排除撤销：按标签内容（app_id 传 0）
    revoke_tag: Option<String>,
    /// 批量标注注水（库存页勾选模式）：给了 app_ids 时忽略 app_id
    #[serde(default)]
    app_ids: Vec<u32>,
}

async fn api_annotation(State(app): State<Shared>, Json(req): Json<AnnotationReq>) -> impl IntoResponse {
    // 口味排除撤销（按标签）
    if let Some(tag) = &req.revoke_tag {
        let store = match Store::open(&app.db) {
            Ok(s) => s,
            Err(e) => return err_json(e),
        };
        return match store.revoke_taste_exclude(tag) {
            Ok(_) => Json(json!({"ok": true})),
            Err(e) => err_json(e),
        };
    }
    if !matches!(req.status.as_str(), "confirmed" | "rejected") {
        return err_json("status 仅支持 confirmed / rejected");
    }
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    // 批量（库存页勾选确认注水）优先
    if !req.app_ids.is_empty() {
        let mut done = 0usize;
        for id in &req.app_ids {
            match store.set_idle_mark(*id, &req.status) {
                Ok(_) => done += 1,
                Err(e) => return err_json(format!("第 {done} 款（app {id}）失败：{e}")),
            }
        }
        return Json(json!({"ok": true, "count": done}));
    }
    // upsert：机器提议过的更新状态，从未提议过的（库存页手动标注）直接插入
    match store.set_idle_mark(req.app_id, &req.status) {
        Ok(_) => Json(json!({"ok": true})),
        Err(e) => err_json(e),
    }
}

// ============ 手动覆盖层（§7.4 修正层家族：强制视为游戏 / 手动深度档）============

#[derive(serde::Deserialize)]
struct OverrideReq {
    app_id: u32,
    kind: String, // game / depth
    /// game: "force"（空=清除）；depth: 档位中文或空/auto（清除）
    #[serde(default)]
    value: String,
    /// 批量版（v0.46）：异常清单「全部已玩完」一键标注；给了 app_ids 时忽略 app_id
    #[serde(default)]
    app_ids: Vec<u32>,
}

async fn api_override(State(app): State<Shared>, Json(req): Json<OverrideReq>) -> impl IntoResponse {
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    let v = req.value.trim();
    // kind 合法性 + 各 kind 的取值校验（depth 档位集合与 GameDepth::as_str 对齐）
    let ok = match req.kind.as_str() {
        "game" => v.is_empty() || v.eq_ignore_ascii_case("auto") || v == "force",
        "depth" => v.is_empty()
            || v.eq_ignore_ascii_case("auto")
            || crate::profiler::GameDepth::all_str().contains(&v),
        // 「没玩完」忽略识别异常提示（value 任意非空；空 = 撤销忽略）
        "anomaly_done" => true,
        _ => false,
    };
    if !ok {
        return err_json(format!("kind/value 不合法：{} / {}", req.kind, req.value));
    }
    // 批量（app_ids）优先；两条路径共用同一套 kind/value 校验
    if !req.app_ids.is_empty() {
        let mut done = 0usize;
        for id in &req.app_ids {
            match store.set_override(*id, &req.kind, v) {
                Ok(_) => done += 1,
                Err(e) => return err_json(format!("第 {done} 款（app {id}）失败：{e}")),
            }
        }
        return Json(json!({"ok": true, "count": done}));
    }
    match store.set_override(req.app_id, &req.kind, v) {
        Ok(_) => Json(json!({"ok": true})),
        Err(e) => err_json(e),
    }
}

// ============ 行为反馈（P1-d，§7.9②）============

#[derive(serde::Deserialize)]
struct FeedbackReq {
    app_id: u32,
    kind: String, // impression / launch / dismiss / skip / download
    #[serde(default)]
    session_id: Option<String>,
}

async fn api_feedback(State(app): State<Shared>, Json(req): Json<FeedbackReq>) -> impl IntoResponse {
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    let session = req.session_id.unwrap_or_else(|| "web".into());
    if let Err(e) = store.record_feedback(req.app_id, &req.kind, &session) {
        return err_json(e);
    }
    // 启动已安装游戏 → 即玩倾向 +0.15（启动按钮只在已安装卡上出现）
    let affinity = if req.kind == "launch" {
        store.bump_install_affinity(0.15).unwrap_or_else(|_| store.install_affinity())
    } else {
        store.install_affinity()
    };
    Json(json!({"ok": true, "install_affinity": (affinity * 1000.0).round() / 1000.0}))
}

async fn api_feedback_reset(State(app): State<Shared>) -> impl IntoResponse {
    let store = match Store::open(&app.db) {
        Ok(s) => s,
        Err(e) => return err_json(e),
    };
    match store.reset_feedback() {
        Ok(_) => Json(json!({"ok": true})),
        Err(e) => err_json(e),
    }
}

// ============ 会话历史（R5） ============

async fn api_sessions(State(app): State<Shared>) -> impl IntoResponse {
    let mut list = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&app.sessions_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !(name.starts_with("session-") && name.ends_with(".json")) {
                continue;
            }
            let id = name.trim_start_matches("session-").trim_end_matches(".json").to_string();
            let meta = std::fs::read(e.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .map(|v| {
                    // 首轮用户输入截断作列表预览（历史页一眼认出是哪次会话）
                    let preview = v["turns"][0]["user"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(30)
                        .collect::<String>();
                    json!({
                        "created_at": v["created_at"],
                        "turns": v["turns"].as_array().map(|a| a.len()).unwrap_or(0),
                        "preview": preview,
                    })
                })
                .unwrap_or(json!({"created_at": 0, "turns": 0}));
            list.push(json!({"id": id, "meta": meta}));
        }
    }
    list.sort_by(|a, b| {
        let x = a["meta"]["created_at"].as_u64().unwrap_or(0);
        let y = b["meta"]["created_at"].as_u64().unwrap_or(0);
        y.cmp(&x)
    });
    Json(json!({"sessions": list}))
}

async fn api_session_one(State(app): State<Shared>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return err_json("非法会话 id");
    }
    let path = app.sessions_dir.join(format!("session-{id}.json"));
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(v) => Json(v),
                Err(e) => err_json(e),
            }
        }
        Err(_) => err_json("会话不存在"),
    }
}

// ============ 同步（SSE 进度 + 停止，R4） ============

async fn api_sync_status(State(app): State<Shared>) -> impl IntoResponse {
    let store = Store::open(&app.db).ok();
    let (owned, last_sync) = match store {
        Some(s) => (
            s.owned_count().unwrap_or(0),
            s.meta_get("last_sync").ok().flatten(),
        ),
        None => (0, None),
    };
    let syncing = app
        .sync_cancel
        .lock()
        .map(|g| g.as_ref().map(|t| !t.is_cancelled()).unwrap_or(false))
        .unwrap_or(false);
    Json(json!({"owned": owned, "last_sync": last_sync, "syncing": syncing}))
}

#[derive(serde::Deserialize, Default)]
struct SyncStartReq {
    steamid: Option<String>,
    skip_llm: Option<bool>,
    /// 仅刷新本地安装状态（零网络、秒级）——库存页「更新本地状态」按钮
    #[serde(default)]
    local_only: bool,
}

fn sse_event(kind: &str, payload: &serde_json::Value) -> Event {
    Event::default().data(json!({"type": kind, "payload": payload}).to_string())
}

async fn api_sync_start(
    State(app): State<Shared>,
    Json(req): Json<SyncStartReq>,
) -> Sse<UnboundedReceiverStream<Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // 已有同步在跑：拒绝
    {
        let guard = app.sync_cancel.lock().unwrap();
        if let Some(t) = guard.as_ref() {
            if !t.is_cancelled() {
                let _ = tx.send(Ok(sse_event("error", &json!({"message": "已有同步在进行中"}))));
                return Sse::new(UnboundedReceiverStream::new(rx));
            }
        }
    }
    let token = CancellationToken::new();
    *app.sync_cancel.lock().unwrap() = Some(token.clone());
    let db = app.db.clone();
    let cfg = app.cfg.lock().unwrap().clone();
    let app2 = app.clone();
    let progress_tx = tx.clone();
    tokio::spawn(async move {
        let result = sync::run(
            &db,
            &cfg,
            SyncOptions {
                steamid: req.steamid.filter(|s| !s.trim().is_empty()),
                vanity: None,
                local_only: req.local_only,
                skip_llm: req.skip_llm.unwrap_or(false),
            },
            token,
            &move |p: &sync::SyncProgress| {
                let _ = progress_tx.send(Ok(sse_event(
                    "progress",
                    &json!({"text": p.text, "pct": p.pct}),
                )));
            },
        )
        .await;
        let final_event = match result {
            Ok(_) => sse_event("done", &json!({"message": "同步完成"})),
            Err(e) => sse_event("error", &json!({"message": friendly_sync_error(&e)})),
        };
        let _ = tx.send(Ok(final_event));
        *app2.sync_cancel.lock().unwrap() = None;
    });
    Sse::new(UnboundedReceiverStream::new(rx))
}

async fn api_sync_stop(State(app): State<Shared>) -> impl IntoResponse {
    let cancelled = app
        .sync_cancel
        .lock()
        .unwrap()
        .as_ref()
        .map(|t| {
            t.cancel();
            true
        })
        .unwrap_or(false);
    Json(json!({"ok": cancelled, "message": if cancelled { "停止信号已发送，正在保存已拉取数据" } else { "当前没有进行中的同步" }}))
}

/// 异常修复（库存页「尝试自动修复」）：与同步共用互斥槽（不能并行）；SSE 进度同 sync。
async fn api_repair(State(app): State<Shared>) -> Sse<UnboundedReceiverStream<Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let guard = app.sync_cancel.lock().unwrap();
        if let Some(t) = guard.as_ref() {
            if !t.is_cancelled() {
                let _ = tx.send(Ok(sse_event("error", &json!({"message": "同步进行中，请等它结束再修复"}))));
                return Sse::new(UnboundedReceiverStream::new(rx));
            }
        }
    }
    let token = CancellationToken::new();
    *app.sync_cancel.lock().unwrap() = Some(token.clone());
    let db = app.db.clone();
    let cfg = app.cfg.lock().unwrap().clone();
    let app2 = app.clone();
    let progress_tx = tx.clone();
    tokio::spawn(async move {
        let result = sync::repair(
            &db,
            &cfg,
            token,
            &move |p: &sync::SyncProgress| {
                let _ = progress_tx.send(Ok(sse_event(
                    "progress",
                    &json!({"text": p.text, "pct": p.pct}),
                )));
            },
        )
        .await;
        let final_event = match result {
            Ok(_) => sse_event("done", &json!({"message": "修复完成"})),
            Err(e) => sse_event("error", &json!({"message": friendly_sync_error(&e)})),
        };
        let _ = tx.send(Ok(final_event));
        *app2.sync_cancel.lock().unwrap() = None;
    });
    Sse::new(UnboundedReceiverStream::new(rx))
}

/// 同步失败的友好文案：Steam 侧 401（key 无效）/ 403（资料未公开）与缺 key 单独翻译成
/// 可操作指引——引导流程里用户看到裸 HTTP 码和 HTML 无法自救；其余保持 anyhow 完整链。
fn friendly_sync_error(e: &anyhow::Error) -> String {
    use crate::steam_client::SteamError;
    for cause in e.chain() {
        if let Some(se) = cause.downcast_ref::<SteamError>() {
            match se {
                SteamError::Http { status: 401, .. } => {
                    return "Steam 返回 401：Web API Key 无效。请到 steamcommunity.com/dev/apikey \
                            核对或重新生成 Key，重新保存后即可重试。"
                        .to_string();
                }
                SteamError::Http { status: 403, .. } => {
                    return "Steam 返回 403：通常是 Steam 资料未公开。请在 Steam 个人资料 → 隐私设置中，\
                            把「我的资料」与「游戏详情」都设为公开，然后重试（刚改过的话稍等几分钟生效）。"
                        .to_string();
                }
                SteamError::NoKey(_) => {
                    return "缺少 Steam Web API Key：请在「设置 → API 密钥与模型」或首次引导中填写，保存后无需重启即可重试。"
                        .to_string();
                }
                _ => {}
            }
        }
    }
    format!("{e:#}")
}

// ============ 对话（SSE：trace / cards / done / error） ============

#[derive(serde::Deserialize)]
struct AskReq {
    message: String,
    session_id: String,
}

async fn api_ask(
    State(app): State<Shared>,
    Json(req): Json<AskReq>,
) -> Sse<UnboundedReceiverStream<Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let state_store = app.clone();
    let db = app.db.clone();
    let cfg = app.cfg.lock().unwrap().clone();
    let sessions_dir = app.sessions_dir.clone();
    let session_id = req.session_id;
    tokio::spawn(async move {
        let send = |kind: &str, payload: &serde_json::Value| {
            let _ = tx.send(Ok(sse_event(kind, payload)));
        };
        let store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => {
                send("error", &json!({"message": e.to_string()}));
                return;
            }
        };
        let profile = match profiler::compute(&store, &cfg) {
            Ok(p) => p,
            Err(e) => {
                send("error", &json!({"message": format!("画像计算失败：{e:#}（先同步一次？)")}));
                return;
            }
        };
        if profile.games.is_empty() {
            send("error", &json!({"message": "库为空：请先在「设置」页执行同步"}));
            return;
        }
        let llm = match LlmClient::from_config(&cfg) {
            Ok(c) => c,
            Err(e) => {
                send("error", &json!({"message": format!("LLM 未配置：{e}")}));
                return;
            }
        };
        let digest = agent::profile_digest(&profile);
        let mut state = {
            let mut map = state_store.ask_states.lock().unwrap();
            map.entry(session_id.clone()).or_default().clone()
        };
        let trace_tx = tx.clone();
        let result = agent::handle_turn(
            &store,
            &cfg,
            &llm,
            &profile,
            &digest,
            &mut state,
            &req.message,
            &move |s: &str| {
                let _ = trace_tx.send(Ok(sse_event("trace", &json!({"text": s}))));
            },
        )
        .await;
        match result {
            Ok(r) => {
                let _ = agent::append_session_turn(
                    &sessions_dir,
                    &session_id,
                    TurnRecord {
                        user: req.message.clone(),
                        intent: r.intent_desc.clone(),
                        candidate_ids: r.candidate_ids.clone(),
                        cards: r.cards.clone(),
                    },
                );
                state_store.ask_states.lock().unwrap().insert(session_id.clone(), state);
                send("cards", &json!({
                    "cards": r.cards,
                    "empty": r.cards.is_empty(),
                    "relaxable": r.relaxable,   // 无候选且带品类条件、未放宽 → 前端提供「放宽条件」CTA
                    "relaxed": r.relaxed,       // 轨道已放宽 → 尽头提示"整个库都看完了"
                    "exhausted": r.exhausted,   // 本批见底 → 末尾 CTA 直接显示放宽/尽头而非"换一批"
                }));
                send(
                    "done",
                    &json!({"calls": r.llm_calls, "prompt_tokens": r.prompt_tokens, "completion_tokens": r.completion_tokens, "cost_cny": r.cost_cny}),
                );
            }
            Err(e) => {
                send("error", &json!({"message": friendly_llm_error(&e)}));
            }
        }
    });
    Sse::new(UnboundedReceiverStream::new(rx))
}

// ============ 本机文件路径（设置页「数据与文件」面板，v0.50）============

/// 各文件解析后的绝对路径 + 版本号（明确配置和数据文件目录）：
/// 启动时已对齐 exe 目录（发行包任意方式启动都落在包内），这里展示真实落盘位置。
async fn api_paths(State(app): State<Shared>) -> impl IntoResponse {
    let cwd = std::env::current_dir().unwrap_or_default();
    let abs = |p: &std::path::Path| -> String {
        std::fs::canonicalize(p)
            .map(|x| x.display().to_string().replace("\\\\?\\", "")) // 去 Windows 扩展路径前缀，可读性
            .unwrap_or_else(|_| cwd.join(p).display().to_string())
    };
    let logs = app
        .db
        .parent()
        .unwrap_or(std::path::Path::new("data"))
        .join("logs")
        .join("tonight.log");
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "config": abs(&app.config_path),
        "env": abs(std::path::Path::new(".env")),
        "db": abs(&app.db),
        "sessions": abs(&app.sessions_dir),
        "logs": abs(&logs),
        "web": abs(std::path::Path::new("web")),
    }))
}

// ============ 静态文件（web/ 目录，无构建链） ============

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    // 防目录穿越
    if path.contains("..") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match tokio::fs::read(std::path::Path::new("web").join(path)).await {
        Ok(bytes) => {
            let ct = match path.rsplit('.').next().unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "css" => "text/css; charset=utf-8",
                "js" => "application/javascript; charset=utf-8",
                "json" => "application/json",
                "svg" => "image/svg+xml",
                "png" => "image/png",
                _ => "application/octet-stream",
            };
            // 本地单用户应用：禁缓存。升级发行版后浏览器若还用旧 JS，前后端协议会错位
            // （真机踩过：旧前端把「跳过」发成无 body 的旧请求，新服务端语义已变）
            ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "no-store")], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::friendly_sync_error;
    use crate::steam_client::SteamError;

    #[test]
    fn price_override_binds_to_model() {
        // 价格覆盖与保存时的模型绑定：换模型后旧覆盖失效回退内置表；
        // 无绑定键（v0.42 前旧数据）保持原行为继续生效
        let dir = std::env::temp_dir().join(format!("tonight-price-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        let db_path = dir.join("test.db3");
        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&db_path);
        let cfg = crate::config::load(&cfg_path).unwrap();
        let store = super::Store::open(&db_path).unwrap();

        // 换模型 + 覆盖绑在别的模型上：忽略，回退内置表
        store.meta_set("ui_llm_model", "deepseek-v4-pro").unwrap();
        store.meta_set("ui_price_input", "1.0").unwrap();
        store.meta_set("ui_price_output", "2.0").unwrap();
        store.meta_set("ui_price_model", "deepseek-chat").unwrap();
        let eff = super::effective_config(&db_path, cfg.clone()).unwrap();
        let p = eff.llm.get("deepseek").unwrap();
        assert_eq!(p.model, "deepseek-v4-pro");
        assert_eq!(p.price_input_per_m, None, "跨模型的旧覆盖应被忽略");
        let (price, src) = crate::llm::resolve_price(p);
        assert_eq!(src, crate::llm::PriceSource::Builtin);
        assert_eq!(price.unwrap().input_per_m, 9.0, "v4-pro 内置价兜底");

        // 绑定一致：覆盖生效
        store.meta_set("ui_price_model", "deepseek-v4-pro").unwrap();
        let eff2 = super::effective_config(&db_path, cfg.clone()).unwrap();
        assert_eq!(eff2.llm.get("deepseek").unwrap().price_input_per_m, Some(1.0));

        // 清除绑定键（旧数据形态）：自定义值保持原行为继续生效
        // （effective_config 契约：入参必须是刚从 config.toml 读出的 fresh cfg）
        store.meta_set("ui_price_model", "").unwrap();
        store.meta_set("ui_price_input", "5.0").unwrap();
        store.meta_set("ui_price_output", "6.0").unwrap();
        let eff3 = super::effective_config(&db_path, cfg.clone()).unwrap();
        assert_eq!(eff3.llm.get("deepseek").unwrap().price_input_per_m, Some(5.0));

        // 遗留清洗：无绑定 + 恰为旧默认价（1.0/2.0 无缓存档）= 预填回发产物，视为未配置
        store.meta_set("ui_price_input", "1.0").unwrap();
        store.meta_set("ui_price_output", "2.0").unwrap();
        store.meta_set("ui_price_cache", "").unwrap();
        let eff4 = super::effective_config(&db_path, cfg).unwrap();
        assert_eq!(eff4.llm.get("deepseek").unwrap().price_input_per_m, None, "旧默认价形态应回退内置表");

        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn bind_falls_back_to_next_free_port() {
        // 用端口 0 让系统分配一个随机端口并占住，避免固定端口偶发冲突
        let blocker = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let taken = blocker.local_addr().unwrap().port();
        let (_listener, actual) = super::bind_with_fallback(taken, 8).await.unwrap();
        assert!(actual > taken && actual <= taken + 8, "taken={taken} actual={actual}");
    }

    #[test]
    fn http_403_maps_to_privacy_guidance() {
        let e = anyhow::Error::new(SteamError::Http { status: 403, body: "Forbidden".into() })
            .context("拉取游戏库失败");
        let msg = friendly_sync_error(&e);
        assert!(msg.contains("403"));
        assert!(msg.contains("公开"));
        assert!(!msg.contains("Forbidden")); // 原始 body 不再透出
    }

    #[test]
    fn http_401_maps_to_bad_key_guidance() {
        // 真机实测：无效 key 时 Steam 返回 401（403 是「key 有效但资料未公开」）
        let e = anyhow::Error::new(SteamError::Http {
            status: 401,
            body: "<html>Unauthorized</html>".into(),
        })
        .context("拉取游戏库失败");
        let msg = friendly_sync_error(&e);
        assert!(msg.contains("401") && msg.contains("Key 无效"));
        assert!(!msg.contains("html"));
    }

    #[test]
    fn no_key_maps_to_onboarding_hint() {
        let e = anyhow::Error::new(SteamError::NoKey("STEAM_WEB_API_KEY".into()));
        let msg = friendly_sync_error(&e);
        assert!(msg.contains("Steam Web API Key"));
        assert!(msg.contains("无需重启"));
    }

    #[test]
    fn other_errors_keep_full_chain() {
        let e = anyhow::Error::new(SteamError::Network("timeout".into())).context("拉取游戏库失败");
        let msg = friendly_sync_error(&e);
        assert!(msg.contains("网络错误") && msg.contains("拉取游戏库失败"));
    }
}
