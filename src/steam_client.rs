//! Steam Web API 客户端（design.md C1 / §7.2 steam_client）。
//! 本里程碑实现 Tier A（GetOwnedGames）、Tier B（GetPlayerAchievements）与 vanity 解析；
//! 统一走 governor 限流（8 req/s，远低于 100k/天）+ 429/5xx 指数退避。

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::clock::{QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::InMemoryState;
use governor::state::direct::NotKeyed;
use governor::{Quota, RateLimiter};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::models::{Achievement, OwnedGame};

const API_BASE: &str = "https://api.steampowered.com";
const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum SteamError {
    #[error("缺少 Steam Web API Key：请设置环境变量 {0}（可写入 .env，参考 .env.example）")]
    NoKey(String),
    #[error("Steam API HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("Steam API 网络错误: {0}")]
    Network(String),
    #[error("Steam API 响应解析失败: {msg}")]
    Parse { msg: String },
    #[error("代理配置无效: {0}")]
    BadProxy(String),
}

/// 把文本中 `key=<32位凭证>` 替换为 `key=***`，避免密钥进入日志与错误信息。
pub fn redact(s: &str) -> String {
    if let Some(pos) = s.find("key=") {
        let rest = &s[pos + 4..];
        if rest.len() >= 32 && rest[..32].chars().all(|c| c.is_ascii_alphanumeric()) {
            return format!("{}key=***{}", &s[..pos], &rest[32..]);
        }
    }
    s.to_string()
}

/// 代理解析优先级：显式配置 > 环境变量 > Windows 系统代理（注册表）。
/// 大陆网络环境下 api.steampowered.com 可能被干扰，走系统代理（如 Clash）通常即可恢复。
pub fn resolve_proxy(explicit: Option<&str>) -> Option<String> {
    if let Some(p) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(normalize_proxy(p));
    }
    for var in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(normalize_proxy(&v));
            }
        }
    }
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        if let Ok(settings) = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        {
            let enable: u32 = settings.get_value("ProxyEnable").unwrap_or(0);
            if enable == 1 {
                if let Ok(server) = settings.get_value::<String, _>("ProxyServer") {
                    if !server.trim().is_empty() {
                        return Some(normalize_proxy(&server));
                    }
                }
            }
        }
    }
    None
}

/// 补全 scheme；兼容 "http=h1:p1;https=h2:p2" 形式（优先 https 项）。
fn normalize_proxy(p: &str) -> String {
    let p = p.trim();
    if !p.contains("://") {
        if let Some(part) = p.split(';').find(|s| s.trim().starts_with("https=")) {
            return format!("http://{}", part.trim().trim_start_matches("https="));
        }
    }
    if p.contains("://") {
        p.to_string()
    } else {
        format!("http://{p}")
    }
}

type Limiter = RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

/// 稳态限流配额：均值 N/分钟、突发压到 burst——per_minute 整桶突发（容量=分钟配额）
/// 会瞬间打满代理出口触发风控，压小突发才是贴近均值的匀速
fn steady_per_minute(per_min: NonZeroU32, burst: NonZeroU32) -> Quota {
    Quota::per_minute(per_min).allow_burst(burst)
}

/// Web API 客户端：Clone 廉价（reqwest 内部 Arc + 限流器共享），并行取数时按任务克隆
#[derive(Clone)]
pub struct SteamClient {
    http: reqwest::Client,
    api_key: Option<String>,
    key_env: String,
    limiter: Arc<Limiter>,
}

impl SteamClient {
    pub fn new(
        api_key: Option<String>,
        key_env: String,
        proxy: Option<String>,
    ) -> Result<Self, SteamError> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("tonight/", env!("CARGO_PKG_VERSION")));
        if let Some(p) = proxy {
            let proxy = reqwest::Proxy::all(&p)
                .map_err(|e| SteamError::BadProxy(format!("{p}: {e}")))?;
            builder = builder.proxy(proxy);
        }
        Ok(SteamClient {
            http: builder.build().map_err(|e| SteamError::Network(redact(&e.to_string())))?,
            api_key,
            key_env,
            limiter: Arc::new(RateLimiter::direct(Quota::per_second(
                NonZeroU32::new(8).expect("非零"),
            ))),
        })
    }

    fn key(&self) -> Result<&str, SteamError> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| SteamError::NoKey(self.key_env.clone()))
    }

    /// GET 拿原始文本；限流 + 对 429/5xx 指数退避重试。
    async fn get_text(&self, url: &str) -> Result<String, SteamError> {
        let mut attempt = 0u32;
        loop {
            self.limiter.until_ready().await;
            let resp = self
                .http
                .get(url)
                .send()
                .await
                .map_err(|e| SteamError::Network(redact(&e.to_string())))?;
            let status = resp.status().as_u16();
            let body = resp
                .text()
                .await
                .map_err(|e| SteamError::Network(redact(&e.to_string())))?;
            if (200..300).contains(&status) {
                return Ok(body);
            }
            if status == 429 || (500..600).contains(&status) {
                attempt += 1;
                if attempt >= MAX_ATTEMPTS {
                    return Err(SteamError::Http { status, body: redact(&body) });
                }
                let wait = Duration::from_millis(400u64 << attempt);
                tracing::warn!("Steam API HTTP {status}，第 {attempt} 次退避 {:?} 后重试", wait);
                tokio::time::sleep(wait).await;
                continue;
            }
            return Err(SteamError::Http { status, body: redact(&body) });
        }
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, SteamError> {
        let text = self.get_text(url).await?;
        serde_json::from_str(&text).map_err(|e| SteamError::Parse {
            msg: redact(&format!("{e}；响应片段: {}", truncate(&text, 300))),
        })
    }

    /// 自定义 URL 名 → SteamID64；未设置自定义 URL 或名称不符时返回 None（附录 A-1 实测）。
    pub async fn resolve_vanity(&self, vanity: &str) -> Result<Option<String>, SteamError> {
        let key = self.key()?;
        let v: serde_json::Value = self
            .get_json(&format!(
                "{API_BASE}/ISteamUser/ResolveVanityURL/v0001/?key={key}&vanityurl={vanity}"
            ))
            .await?;
        let resp = &v["response"];
        if resp["success"].as_i64() == Some(1) {
            Ok(resp["steamid"].as_str().map(str::to_string))
        } else {
            Ok(None)
        }
    }

    /// Tier A：游戏库清单。
    pub async fn get_owned_games(&self, steamid: &str) -> Result<Vec<OwnedGame>, SteamError> {
        let key = self.key()?;
        #[derive(Deserialize)]
        struct Resp {
            response: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            #[serde(default)]
            games: Option<Vec<RawGame>>,
        }
        #[derive(Deserialize)]
        struct RawGame {
            appid: u32,
            #[serde(default)]
            name: Option<String>,
            playtime_forever: u32,
            #[serde(default)]
            playtime_2weeks: Option<u32>,
            #[serde(default)]
            rtime_last_played: Option<u64>,
        }
        let resp: Resp = self
            .get_json(&format!(
                "{API_BASE}/IPlayerService/GetOwnedGames/v0001/?key={key}&steamid={steamid}\
                 &include_appinfo=true&include_played_free_games=true"
            ))
            .await?;
        Ok(resp
            .response
            .games
            .unwrap_or_default()
            .into_iter()
            .map(|g| OwnedGame {
                app_id: g.appid,
                name: g.name.unwrap_or_else(|| format!("#{}", g.appid)),
                playtime_min: g.playtime_forever,
                playtime_2weeks_min: g.playtime_2weeks.unwrap_or(0),
                last_played: g.rtime_last_played.filter(|&t| t > 0),
            })
            .collect())
    }

    /// Tier B：玩家成就。返回 None 表示该应用无成就/数据不可用（永久跳过，记入 skipped_apps）。
    pub async fn get_player_achievements(
        &self,
        steamid: &str,
        app_id: u32,
    ) -> Result<Option<Vec<Achievement>>, SteamError> {
        let key = self.key()?;
        let url = format!(
            "{API_BASE}/ISteamUserStats/GetPlayerAchievements/v0001/?key={key}\
             &steamid={steamid}&appid={app_id}"
        );
        match self.get_text(&url).await {
            Ok(text) => {
                let resp: RawStats = serde_json::from_str(&text).map_err(|e| SteamError::Parse {
                    msg: redact(&format!("{e}；响应片段: {}", truncate(&text, 300))),
                })?;
                if resp.playerstats.success == Some(false) {
                    return Ok(None);
                }
                Ok(Some(
                    resp.playerstats
                        .achievements
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| Achievement {
                            api_name: a.api_name,
                            achieved: a.achieved != 0,
                            unlock_time: a.unlock_time.filter(|&t| t > 0),
                        })
                        .collect(),
                ))
            }
            // 400/403，以及带 {success:false} 错误体的 500（如 Don't Starve 这类无成就游戏），
            // 都是“该应用无成就/数据不可用”的永久情况，跳过并记录（附录 A-3 实测口径）
            Err(SteamError::Http { status, body }) => {
                let parsed: Option<RawStats> = serde_json::from_str(&body).ok();
                let permanent = matches!(status, 400 | 403)
                    || parsed.as_ref().and_then(|r| r.playerstats.success) == Some(false);
                if permanent {
                    let reason = parsed
                        .and_then(|r| r.playerstats.error)
                        .unwrap_or_else(|| format!("HTTP {status}"));
                    tracing::debug!(app_id, %reason, "跳过成就拉取");
                    Ok(None)
                } else {
                    Err(SteamError::Http { status, body })
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Tier C：成就全球完成度（免 Key）。注意：参数是 gameid、percent 为字符串（附录 A-4 勘误）。
    /// 返回 None 表示该应用无成就数据（永久跳过）。
    pub async fn get_global_achievement_percentages(
        &self,
        app_id: u32,
    ) -> Result<Option<Vec<(String, f32)>>, SteamError> {
        #[derive(Deserialize)]
        struct Resp {
            achievementpercentages: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            #[serde(default)]
            achievements: Option<Vec<Raw>>,
        }
        #[derive(Deserialize)]
        struct Raw {
            name: String,
            percent: Percent,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Percent {
            S(String),
            F(f64),
        }
        let url = format!(
            "{API_BASE}/ISteamUserStats/GetGlobalAchievementPercentagesForApp/v0002/?gameid={app_id}&format=json"
        );
        match self.get_text(&url).await {
            Ok(text) => {
                let resp: Resp = serde_json::from_str(&text).map_err(|e| SteamError::Parse {
                    msg: redact(&format!("{e}；响应片段: {}", truncate(&text, 300))),
                })?;
                Ok(Some(
                    resp.achievementpercentages
                        .achievements
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| {
                            let p = match a.percent {
                                Percent::S(s) => s.trim().parse().unwrap_or(0.0),
                                Percent::F(f) => f as f32,
                            };
                            (a.name, p)
                        })
                        .collect(),
                ))
            }
            Err(SteamError::Http { status: s @ (400 | 403), body }) => {
                tracing::debug!(app_id, status = s, body = %truncate(&body, 120), "无全球成就数据，跳过");
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

const STORE_BASE: &str = "https://store.steampowered.com";

/// Steam 商店接口（Tier D，非官方端点）：限流远严于 Web API（约 200 次/5 分钟），长期缓存。
/// v0.47 起双通道独立限速：/api/* 接口与商店页 HTML 分开——页面是 CDN 静态页、容忍度更高
/// （1/s 稳态），appdetails 走 36/min（200/5min 社区口径留余量、突发压小）；
/// 两条通道并行取数，首次同步墙钟时间由"串行 2N 次/20 每分"降为 max(N/36, N/60) 分钟。
pub struct SteamStore {
    http: reqwest::Client,
    api_limiter: Arc<Limiter>,
    page_limiter: Arc<Limiter>,
}

impl SteamStore {
    pub fn new(proxy: Option<String>) -> Result<Self, SteamError> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("tonight/", env!("CARGO_PKG_VERSION")));
        if let Some(p) = proxy {
            let proxy =
                reqwest::Proxy::all(&p).map_err(|e| SteamError::BadProxy(format!("{p}: {e}")))?;
            builder = builder.proxy(proxy);
        }
        Ok(SteamStore {
            http: builder
                .build()
                .map_err(|e| SteamError::Network(redact(&e.to_string())))?,
            api_limiter: Arc::new(RateLimiter::direct(steady_per_minute(
                NonZeroU32::new(36).expect("非零"),
                NonZeroU32::new(5).expect("非零"),
            ))),
            page_limiter: Arc::new(RateLimiter::direct(steady_per_minute(
                NonZeroU32::new(60).expect("非零"),
                NonZeroU32::new(2).expect("非零"),
            ))),
        })
    }

    /// 统一的商店 GET：限流 + 重试。429/403/5xx 与网络错误（超时/连接重置）都按指数退避
    /// 重试——Steam 风控对代理出口常见**间歇性** 403/429，这正是"app 报错、浏览器打开
    /// 同一链接却正常"的原因（两者出口与端点都不同）；单次失败即报错只会刷屏。
    async fn get_with_retry(
        &self,
        url: &str,
        cookie: Option<&str>,
        limiter: &Limiter,
    ) -> Result<String, SteamError> {
        let mut attempt = 0u32;
        loop {
            limiter.until_ready().await;
            let req = self.http.get(url);
            let req = match cookie {
                Some(c) => req.header("Cookie", c),
                None => req,
            };
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    attempt += 1;
                    if attempt >= 4 {
                        return Err(SteamError::Network(redact(&e.to_string())));
                    }
                    let wait = store_backoff(attempt);
                    tracing::warn!("store API 网络错误，退避 {wait:?} 后重试");
                    tokio::time::sleep(wait).await;
                    continue;
                }
            };
            let status = resp.status().as_u16();
            let text = resp
                .text()
                .await
                .map_err(|e| SteamError::Network(redact(&e.to_string())))?;
            if (200..300).contains(&status) {
                return Ok(text);
            }
            if status == 429 || status == 403 || (500..600).contains(&status) {
                attempt += 1;
                if attempt >= 4 {
                    return Err(SteamError::Http { status, body: redact(&truncate(&text, 200)) });
                }
                let wait = store_backoff(attempt);
                tracing::warn!("store API HTTP {status}，退避 {wait:?} 后重试");
                tokio::time::sleep(wait).await;
                continue;
            }
            return Err(SteamError::Http { status, body: redact(&truncate(&text, 200)) });
        }
    }

    /// 单应用详情（type / genres / categories，中文本地化）。success=false 返回 None。
    /// filters 裁字段：默认全量响应 ~27KB/款，只要 basic+genres+categories 压到 ~2.5KB
    /// （多 appid 批量仅对 filters=price_overview 有效——官方 wiki 明说其余批量返回 null，
    /// 实测 400，故详情仍是每款一请求）
    pub async fn get_app_details(
        &self,
        app_id: u32,
    ) -> Result<Option<crate::models::AppDetail>, SteamError> {
        let url = format!(
            "{STORE_BASE}/api/appdetails?appids={app_id}&l=schinese&filters=basic,genres,categories"
        );
        let text = self.get_with_retry(&url, None, &self.api_limiter).await?;
                #[derive(Deserialize)]
                struct Entry {
                    success: bool,
                    #[serde(default)]
                    data: Option<Data>,
                }
                #[derive(Deserialize)]
                struct Data {
                    #[serde(default, rename = "type")]
                    app_type: Option<String>,
                    #[serde(default)]
                    genres: Option<Vec<TagRaw>>,
                    #[serde(default)]
                    categories: Option<Vec<TagRaw>>,
                }
                #[derive(Deserialize)]
                struct TagRaw {
                    // 实测：不同响应里 id 时而是数字时而是字符串（如 "id":"1"），两者都要兼容
                    #[serde(default)]
                    id: Option<TagId>,
                    description: String,
                }
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum TagId {
                    N(i64),
                    S(String),
                }
                let mut map: std::collections::BTreeMap<String, Entry> =
                    serde_json::from_str(&text).map_err(|e| SteamError::Parse {
                        msg: redact(&format!("{e}；响应片段: {}", truncate(&text, 300))),
                    })?;
                let Some(entry) = map.remove(&app_id.to_string()) else {
                    return Err(SteamError::Parse { msg: format!("appdetails 响应缺少 {app_id}") });
                };
                if !entry.success {
                    return Ok(None);
                }
                let Some(d) = entry.data else {
                    return Ok(None);
                };
                let conv = |t: Option<Vec<TagRaw>>| {
                    t.unwrap_or_default()
                        .into_iter()
                        .map(|r| crate::models::Tag {
                            id: r
                                .id
                                .map(|i| match i {
                                    TagId::N(v) => v,
                                    TagId::S(s) => s.parse().unwrap_or(0),
                                })
                                .unwrap_or(0),
                            description: r.description,
                        })
                        .collect::<Vec<_>>()
                };
                Ok(Some(crate::models::AppDetail {
                    app_type: d.app_type.unwrap_or_else(|| "game".into()),
                    storage_gb: None,
                    genres: conv(d.genres),
                    categories: conv(d.categories),
                }))
    }

    /// 商店页用户投票标签（glance_tags，中文本地化，按热度排序）。
    /// 成熟内容有年龄门：用出生时间 Cookie 绕过；解析不到标签（下架/未过门）返回 None。
    /// 商店页用户投票标签 + 存储空间需求（同一次页面请求，零额外开销）。
    /// 返回 (标签列表, 存储GB)；标签为空返回 None（下架/年龄门）。
    pub async fn get_store_page_info(
        &self,
        app_id: u32,
    ) -> Result<Option<(Vec<String>, Option<f64>)>, SteamError> {
        let url = format!("{STORE_BASE}/app/{app_id}/?l=schinese");
        let html = self
            .get_with_retry(
                &url,
                Some("birthtime=315532800; lastagecheckage=252460800"),
                &self.page_limiter,
            )
            .await?;
        let tags = extract_store_tags(&html);
        let storage_gb = extract_storage_gb(&html);
        Ok(if tags.is_empty() { None } else { Some((tags, storage_gb)) })
    }
}

/// 商店接口退避间隔：3s → 6s → 12s（第 4 次放弃）
fn store_backoff(attempt: u32) -> Duration {
    Duration::from_millis(1500u64 << attempt)
}

/// 从商店页 HTML 系统需求区提取存储空间需求（GB）。
/// 匹配"存储空间"/"Storage:"后的数字+单位（GB/MB），自动换算。
pub fn extract_storage_gb(html: &str) -> Option<f64> {
    for keyword in ["存储空间", "Storage:", "storage"] {
        if let Some(pos) = html.find(keyword) {
            let start = pos + keyword.len();
            let end = html[start..]
                .char_indices()
                .nth(200)
                .map(|(i, _)| start + i)
                .unwrap_or(html.len());
            let after = &html[start..end];
            // 找数字 + 紧随的单位（GB/MB/gb/mb/G/M），如 "60 GB"、"1200 MB"
            let chars: Vec<char> = after.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if chars[i].is_ascii_digit() || chars[i] == '.' {
                    let num_start = i;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                        i += 1;
                    }
                    let num_str: String = chars[num_start..i].iter().collect();
                    // 跳过空格找单位
                    let mut j = i;
                    while j < chars.len() && chars[j] == ' ' {
                        j += 1;
                    }
                    let unit: String = chars[j..(j + 2).min(chars.len())].iter().collect::<String>().to_uppercase();
                    if let Ok(v) = num_str.parse::<f64>() {
                        if v > 0.0 {
                            if unit.starts_with("GB") {
                                if v < 1000.0 {
                                    return Some(v);
                                }
                            } else if unit.starts_with("MB") {
                                let gb = v / 1024.0;
                                if gb > 0.05 && gb < 1000.0 {
                                    return Some((gb * 10.0).round() / 10.0);
                                }
                            }
                        }
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    None
}

/// 从商店页 HTML 提取用户投票标签：glance_tags 区块内 `class="app_tag"` 锚文本。
/// 纯函数，便于单测；取前 20 个（热度序）。
pub fn extract_store_tags(html: &str) -> Vec<String> {
    let Some(anchor) = html.find("popular_tags") else {
        return Vec::new();
    };
    let mut rest = &html[anchor..];
    let mut out = Vec::new();
    while let Some(i) = rest.find("class=\"app_tag\"") {
        let after = &rest[i + "class=\"app_tag\"".len()..];
        let Some(start) = after.find('>') else { break };
        let body = &after[start + 1..];
        let Some(end) = body.find("</a>") else { break };
        let text = body[..end].trim();
        // 展开按钮 "+" 和空文本跳过
        if !text.is_empty() && text != "+" {
            out.push(text.to_string());
        }
        rest = &body[end..];
        if out.len() >= 20 {
            break;
        }
    }
    out
}

#[derive(Deserialize, Default)]
struct RawStats {
    #[serde(default)]
    playerstats: RawPlayerStats,
}

#[derive(Deserialize, Default)]
struct RawPlayerStats {
    #[serde(default)]
    achievements: Option<Vec<RawAchievement>>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct RawAchievement {
    // Steam 文档写 apiName，实测返回 apiname，两个名字都兼容
    #[serde(default, rename = "apiname", alias = "apiName")]
    api_name: String,
    achieved: i32,
    #[serde(default, rename = "unlocktime")]
    unlock_time: Option<u64>,
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_key_in_error_text() {
        // 假密钥（格式同真实 Steam Key，32 位大写十六进制），严禁把真实密钥写进测试
        let s = "error sending request for url (https://api.steampowered.com/x?key=0123456789ABCDEF0123456789ABCDEF&steamid=1)";
        let r = redact(s);
        assert!(r.contains("key=***"));
        assert!(!r.contains("0123456789ABCDEF"));
    }

    #[test]
    fn normalizes_proxy_forms() {
        assert_eq!(normalize_proxy("127.0.0.1:7897"), "http://127.0.0.1:7897");
        assert_eq!(
            normalize_proxy("http=1.1.1.1:80;https=127.0.0.1:7897"),
            "http://127.0.0.1:7897"
        );
        assert_eq!(normalize_proxy("socks5://127.0.0.1:1080"), "socks5://127.0.0.1:1080");
    }

    #[test]
    fn extracts_store_tags_from_html() {
        let html = r#"
        <div class="glance_tags_ctn">
          <div class="glance_tags popular_tags" data-token="x">
            <a class="app_tag" href="..."> 类魂 </a>
            <a class="app_tag" href="...">动作角色扮演</a>
            <a class="app_tag" href="...">困难</a>
            <a class="app_tag" href="...">+</a>
          </div>
        </div>"#;
        let tags = extract_store_tags(html);
        assert_eq!(tags, vec!["类魂".to_string(), "动作角色扮演".to_string(), "困难".to_string()]);
        // 年龄门页面（无 popular_tags 区块）→ 空
        assert!(extract_store_tags("<html>agecheck</html>").is_empty());
    }

    #[test]
    fn extracts_storage_from_html() {
        let zh = r#"<div class="game_area_sys_req"><ul><li><strong>存储空间:</strong> 23 GB 可用空间</li></ul></div>"#;
        assert_eq!(extract_storage_gb(zh), Some(23.0));
        let en = r#"<li><strong>Storage:</strong> 60 GB available space</li>"#;
        assert_eq!(extract_storage_gb(en), Some(60.0));
        let mb = r#"存储空间: 1200 MB"#;
        assert_eq!(extract_storage_gb(mb), Some(1.2)); // MB → GB
        let decimal = r#"存储空间: 1.5 GB"#;
        assert_eq!(extract_storage_gb(decimal), Some(1.5));
        assert_eq!(extract_storage_gb("<html>no storage info</html>"), None);
    }
}
