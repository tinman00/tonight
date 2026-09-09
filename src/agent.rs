//! Agent 层（design.md C3 / §7.6）：约束式两步管线 + 多轮修正。
//! ① 意图解析：口语 → IntentFilters（结构化输出 + 校验，失败退默认）；
//! ② 推荐卡生成：候选事实包 → 卡片文案（schema 校验 + 带错重试 + Rust 模板兜底）。
//! 原则：数字归代码、人话归模型；每步经 trace 上报（CLI 打印 / Web 转 SSE，R5）；
//! 预算硬停（R6）；会话 JSON 落盘。`handle_turn` 为 CLI（run_ask）与 Web（server）共用管线。

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::config::Config;
use crate::llm::{ChatMessage, LlmClient};
use crate::profiler::PlayerProfile;
use crate::recommender::{recommend, Candidate, IntentFilters};
use crate::store::Store;

// ============ 意图解析 ============

fn intent_system(popular_tags: &str) -> String {
    format!(
        "你是 Steam 游戏推荐的意图解析器。把用户的口语需求转成 JSON 筛选条件，只输出 JSON。\n\
         可用字段：tags(想玩的品类数组，如 解谜/冒险/竞速)、exclude_tags(排除品类数组)、\
         max_session_min(今晚可玩分钟数，整数或 null)、mood(轻松/沉浸/成就向/社交 之一或 null)、\
         instant_only(只玩已安装的，true/false)、exclude_apps(用户明确点名不要的游戏 app_id 数组，一般 [])。\n\
         判定规则（严格）：\n\
         1. 只有用户明确说出品类/玩法/题材词（如 解谜、竞速、恐怖、像素、肉鸽、剧情）时才写入 tags 或 exclude_tags；\n\
         2. 时间与日程信息（“下午有一大段时间”“就半小时”“周末”）只能换算成 max_session_min，\
         严禁因此推断任何品类或心境——有大段时间不等于想玩剧情向；\n\
         3. 心境描述（轻松/想放松/动脑子/刺激/治愈）放 mood，不进 tags；\n\
         4. 组合品类词要拆开：如“回合制策略”拆成 [回合制, 策略]（用户库标签是分开的，\
         组合词整词匹配会全部落空）；\
         4b. tags/exclude_tags/remember_exclude 优先从「用户库常见品类」里选**原词**：\
         用户说“肉鸽”而库里的标签是「类 Rogue」时，写 类 Rogue；说“银河恶魔城”而库里是\
         「类银河战士恶魔城」时，写 类银河战士恶魔城——同义词整词匹配不上库标签；\n\
         5. 不确定的字段一律留空：tags 为空数组是合法且常见的值，绝不要为了“填点什么”而猜测品类；\n\
         6. “随便玩/不知道/帮我挑/都行/来点”这类没有具体指向的请求：全部字段留空（tags=[]、\
         max_session_min=null、mood=null），交给全库推荐；\n\
         7. “睡前/临睡前/睡觉前”默认换算为 max_session_min=30，不推断品类；\n\
         8. “记住我不喜欢X””以后别再推X””讨厌X”这类表达长期口味的否定 → 写入 remember_exclude 数组\
         （品类词，如 [肉鸽, 恐怖]）；不带”记住/以后”的即时否定（”不要恐怖的”）仍走 exclude_tags。\n\
         结合对话历史理解修正（如“不要恐怖的”是对此前的修正）。用户库常见品类：{popular_tags}"
    )
}

/// 意图解析输出（LLM JSON 的镜像）
#[derive(Debug, Default, Clone)]
pub struct ParsedIntent {
    pub tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    pub max_session_min: Option<u32>,
    pub mood: Option<String>,
    pub instant_only: bool,
    pub exclude_apps: Vec<u32>,
    /// 长期口味排除（"记住我不喜欢肉鸽"→ ["肉鸽"]）：写修正层，跨会话生效
    pub remember_exclude: Vec<String>,
}

/// 解析意图输出（纯函数）。字段缺失按默认处理；整体非 JSON → None。
pub fn parse_intent_response(content: &str) -> Option<ParsedIntent> {
    let text = extract_json(content);
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if !v.is_object() {
        return None;
    }
    let strs = |k: &str| -> Vec<String> {
        v[k]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    Some(ParsedIntent {
        tags: strs("tags"),
        exclude_tags: strs("exclude_tags"),
        max_session_min: v["max_session_min"].as_u64().map(|x| x as u32),
        mood: v["mood"].as_str().map(str::to_string).filter(|s| !s.is_empty()),
        instant_only: v["instant_only"].as_bool().unwrap_or(false),
        exclude_apps: v["exclude_apps"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as u32)).collect())
            .unwrap_or_default(),
        remember_exclude: strs("remember_exclude"),
    })
}

fn extract_json(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if a < b => s[a..=b].to_string(),
        _ => s.to_string(),
    }
}

// ============ 事实包与卡片 ============

/// 候选事实包：2a 输出 → 2b 输入（design.md §7.3 GameFacts 的实现）
#[derive(Clone)]
pub struct GameFacts {
    pub app_id: u32,
    pub name: String,
    pub depth: &'static str,
    pub badge: &'static str,
    pub tags: Vec<String>,
    pub days_since_played: Option<u64>,
    pub motivation: f64,
    #[allow(dead_code)] // 卡片理由可引用的补充分量
    pub tag_score: f64,
    #[allow(dead_code)]
    pub attainability: f64,
    pub backlog: f64,
    pub global_median_pct: Option<f32>,
    pub cat_dist: String,
    /// 预约下载信息（badge 为"未安装"时前端显示下载按钮）
    pub download_info: Option<DownloadInfo>,
    /// 探索位注入（前端"换个口味"角标）
    pub explore: bool,
}

/// 下载权衡信息：大小、磁盘余量、估算时长（由 Rust 侧计算，不进 LLM）
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct DownloadInfo {
    pub storage_gb: Option<f64>,
    pub disk_free_gb: Option<f64>,
    pub est_minutes: Option<u32>,
    pub disk_ok: Option<bool>, // Some(false) = 磁盘不足
    pub install_url: String,  // steam://install/<appid>
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn build_facts(
    store: &Store,
    cands: &[Candidate],
    download_speed_mbps: f64,
    steam_dir: Option<&std::path::Path>,
) -> Result<Vec<GameFacts>> {
    let now = now_secs();
    let mut disk_free_bytes: Option<u64> = None;
    // 只查一次磁盘余量（所有候选共享同一物理盘）；steam_dir 由调用方按 config 优先裁决
    if cands.iter().any(|c| c.badge == "未安装") {
        if let Some(steam_dir) = steam_dir {
            disk_free_bytes = crate::steam_local::SteamLocal::open(steam_dir).disk_free_bytes();
        }
    }
    let disk_free_gb = disk_free_bytes.map(|b| b as f64 / 1e9);
    let mut out = Vec::new();
    for c in cands {
        let globals = store.global_achievements(c.app_id)?;
        let global_median_pct = {
            let mut ps: Vec<f32> = globals.iter().map(|(_, p)| *p).collect();
            if ps.is_empty() {
                None
            } else {
                ps.sort_by(|a, b| a.partial_cmp(b).unwrap());
                Some(ps[ps.len() / 2])
            }
        };
        let mut dist: std::collections::BTreeMap<String, u32> = Default::default();
        for (_, cat) in store.achievement_categories(c.app_id)? {
            *dist.entry(cat.as_str().to_string()).or_insert(0) += 1;
        }
        let cat_dist = dist
            .iter()
            .filter(|(k, _)| k.as_str() != "other")
            .map(|(k, v)| format!("{k}×{v}"))
            .collect::<Vec<_>>()
            .join("、");
        // 预约下载信息（仅"未安装"卡生成）
        let download_info = if c.badge == "未安装" {
            let storage_gb = store.app_detail(c.app_id).ok().flatten().and_then(|d| d.storage_gb);
            let est_minutes = storage_gb.map(|gb| {
                if download_speed_mbps > 0.0 {
                    (gb * 8192.0 / download_speed_mbps / 60.0).round() as u32
                } else {
                    0
                }
            });
            Some(DownloadInfo {
                disk_ok: match (storage_gb, disk_free_gb) {
                    (Some(s), Some(f)) => Some(s <= f * 0.95), // 留 5% 余量
                    _ => None,
                },
                storage_gb: storage_gb.map(|v| (v * 10.0).round() / 10.0),
                disk_free_gb: disk_free_gb.map(|v| (v * 10.0).round() / 10.0),
                est_minutes,
                install_url: format!("steam://install/{}", c.app_id),
            })
        } else {
            None
        };
        out.push(GameFacts {
            app_id: c.app_id,
            name: c.name.clone(),
            depth: c.depth,
            badge: c.badge,
            tags: c.genres.clone(),
            // 注水游戏的天数/积压照常（挂卡注水的是总时长，不是游玩时间点——
            // "N 天没玩"与积压分依然真实）；总时长从不进事实包，卡片天然无痕
            days_since_played: c.last_played.map(|t| (now.saturating_sub(t)) / 86_400),
            motivation: c.breakdown.motivation,
            tag_score: c.breakdown.tag,
            attainability: c.breakdown.attainability,
            backlog: c.breakdown.backlog,
            global_median_pct,
            cat_dist,
            download_info,
            explore: c.explore,
        });
    }
    Ok(out)
}

/// 推荐点标签（Rust 从事实包计算，不走 LLM——数字归代码）
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct CardPoint {
    /// 前端配色类别：match=动机 / taste=口味 / backlog=积压 / attain=成就
    pub kind: String,
    pub text: String,
}

/// 从事实包提取推荐点（纯函数）。
fn card_points(g: &GameFacts) -> Vec<CardPoint> {
    let mut out = vec![CardPoint {
        kind: "match".into(),
        text: format!("动机匹配 {:.0}%", g.motivation * 100.0),
    }];
    for t in g.tags.iter().take(2) {
        out.push(CardPoint { kind: "taste".into(), text: t.clone() });
    }
    out.push(CardPoint {
        kind: "backlog".into(),
        text: match g.days_since_played {
            None => "未开封".to_string(),
            Some(0) => "最近在玩".to_string(),
            Some(d) if d >= 365 => format!("沉睡 {} 年", d / 365),
            Some(d) => format!("{d} 天没玩"),
        },
    });
    // 未开封游戏没有"你的成就进度"语境，成就中位数是统计噪音，不生成该推荐点
    if let (Some(m), Some(_)) = (g.global_median_pct, g.days_since_played) {
        out.push(CardPoint { kind: "attain".into(), text: format!("成就中位 {m:.0}%") });
    }
    out
}

/// 推荐卡（展示与存档结构；rank/badge/链接/推荐点由 Rust 侧补齐，LLM 只产 title 与 blurb）
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RecommendationCard {
    pub rank: u8,
    pub app_id: u32,
    pub name: String,
    pub title: String,
    pub blurb: String,
    #[serde(default)]
    pub points: Vec<CardPoint>,
    /// 时长提示短语（"一局约 20 分钟""一个章节约 1 小时""自由节奏"），LLM 产出、Rust 兜底
    #[serde(default)]
    pub session_hint: String,
    #[serde(skip, default = "default_badge")]
    pub badge: &'static str,
    pub suggested_session_min: u32,
    pub launch_url: String,
    /// 预约下载信息（"未安装"卡片显示下载按钮 + 空间/时间权衡提示）
    #[serde(default)]
    pub download_info: Option<DownloadInfo>,
    /// 探索位注入（"换个口味"角标，探索模式强制 1 席）
    #[serde(default)]
    pub explore: bool,
    #[serde(skip, default = "default_card_source")]
    #[allow(dead_code)] // Web UI 后续展示生成来源
    pub source: &'static str, // llm / template
}

fn default_card_source() -> &'static str {
    "llm"
}

fn default_badge() -> &'static str {
    "未知"
}

const CARD_SYSTEM: &str = "你是游戏推荐官，为一位 Steam 玩家从他的库存里挑选“现在玩什么”，并写出让人读完就想双击图标的游戏卡。\n\
规则：只能基于给定的玩家画像与候选事实说话，不得编造任何数字或事实。\n\
title：不超过 14 个字的海报式标语，制造好奇、情绪或共鸣；禁止出现“推荐”二字。\n\
blurb：100～160 字，推荐词的主体。从他的画像口味与动机出发（品类偏好、主型倾向、积压情况、心境需求），\
描绘“坐下来玩它，会经历什么、有什么值得期待的时刻”——把匹配点讲进体验和画面里，制造期待感。\
要像懂他的朋友在安利，而不是产品介绍：禁止罗列标签词、禁止参数式表述（数据已由系统以标签形式展示）、禁止出现“推荐”二字。\n\
session_hint：不超过 20 字的时长提示短语，结合游戏形态给出具体颗粒度——\
肉鸽/竞技类按“一局约 X 分钟”、剧情类按“一个章节约 X 分钟”、沙盒/模拟类用“自由节奏”\
（可带“建议先玩 X 分钟”式后缀）；参考玩家典型会话时长与用户时限，不要只写裸数字。\n\
suggested_session_min：整数分钟，与 session_hint 口径一致。\n\
输出严格 JSON：{\"cards\":[{\"app_id\":0,\"title\":\"\",\"blurb\":\"\",\"session_hint\":\"\",\"suggested_session_min\":0},...]}，\
app_id 必须来自候选列表，恰好输出 3 张（候选不足 3 张则全部输出）。";

fn card_user_prompt(digest: &str, intent_desc: &str, facts: &[GameFacts]) -> String {
    let mut f = String::new();
    for g in facts {
        f.push_str(&fact_line(g));
    }
    format!("玩家画像：{digest}\n用户当前需求：{intent_desc}\n候选事实：\n{f}")
}

/// 候选事实行（主批与探索位专项 prompt 共用，避免口径漂移）。
/// 不含总游玩时长（注水游戏照常推荐且卡片无时长痕迹）；天数是真实行为信号，保留。
fn fact_line(g: &GameFacts) -> String {
    let days = match g.days_since_played {
        Some(d) => format!("距上次游玩 {d} 天"),
        None => "从未玩过（未开封）".to_string(),
    };
    let median = g
        .global_median_pct
        .map(|m| format!("成就全球完成度中位 {m:.0}%"))
        .unwrap_or_else(|| "无成就数据".into());
    let explore_mark = if g.explore {
        "｜探索位（换个口味：与玩家常用品类差异最大的库存游戏，必须保留在输出中，\
         blurb 要点出它和玩家常玩口味的反差、以及值得尝鲜的新鲜点）"
    } else {
        ""
    };
    // 分段拼接（天数字段恒在，保持与旧口径一致的行格式）
    let mut parts = vec![
        format!("app_id {}", g.app_id),
        format!("《{}》", g.name),
        g.depth.to_string(),
        g.badge.to_string(),
        format!("标签：{}", g.tags.join("、")),
        days,
        format!("动机匹配 {:.0}%", g.motivation * 100.0),
        median,
    ];
    if !g.cat_dist.is_empty() {
        parts.push(format!("成就类型：{}", g.cat_dist));
    }
    let line = parts.join("｜");
    format!("- {line}{}{}\n", if explore_mark.is_empty() { "" } else { "｜" }, explore_mark)
}

/// 卡片草稿（LLM 输出的镜像；推荐点标签由 Rust 计算，不要求 LLM 输出）
#[derive(Debug, serde::Deserialize, Clone)]
struct CardDraft {
    app_id: u32,
    title: String,
    blurb: String,
    #[serde(default)]
    session_hint: String,
    suggested_session_min: u32,
}

/// 校验 LLM 卡片输出（纯函数）：JSON 可解析、app_id ∈ 候选、字段非空。
fn parse_cards_response(content: &str, facts: &[GameFacts]) -> std::result::Result<Vec<CardDraft>, String> {
    #[derive(serde::Deserialize)]
    struct Wrap {
        cards: Vec<CardDraft>,
    }
    let text = extract_json(content);
    let w: Wrap = serde_json::from_str(&text).map_err(|e| format!("JSON 解析失败: {e}"))?;
    if w.cards.is_empty() {
        return Err("cards 为空".into());
    }
    if w.cards.len() > 3 {
        return Err("卡片数量超过 3".into());
    }
    let valid_ids: Vec<u32> = facts.iter().map(|f| f.app_id).collect();
    let mut seen = std::collections::HashSet::new();
    for c in &w.cards {
        if !valid_ids.contains(&c.app_id) {
            return Err(format!("app_id {} 不在候选列表中", c.app_id));
        }
        // 同批去重：LLM 偶发对同一游戏输出两张卡（主批+探索位撞车），保留首次出现的
        if !seen.insert(c.app_id) {
            return Err(format!("app_id {} 重复出现", c.app_id));
        }
        if c.title.trim().is_empty() || c.blurb.trim().is_empty() {
            return Err(format!("app_id {} 的 title/blurb 存在空值", c.app_id));
        }
        if c.blurb.chars().count() < 40 {
            return Err(format!("app_id {} 的 blurb 太短（{}字 < 40），请按 100～160 字要求扩写", c.app_id, c.blurb.chars().count()));
        }
        // session_hint 超长不报错：merge 时丢弃该 hint、退回完整兜底短语——
        // 不重试（多花一次调用）、不截断（残句会直接展示给用户）
    }
    Ok(w.cards)
}

/// Rust 模板兜底：LLM 不可用/校验失败/探索位漏选时，用事实包数字生成保底文案。
/// 文案长度向 LLM 卡看齐（用户直接阅读，两三句起），探索位有专属的"换口味"叙事。
pub fn template_cards(facts: &[GameFacts], typical_session_min: u32, mood: Option<&str>) -> Vec<RecommendationCard> {
    facts
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, g)| {
            let title = if g.explore {
                "换个口味试试".to_string()
            } else {
                match g.depth {
                    "未开封" => "新世界开箱",
                    "试玩即弃" => "再给一次机会",
                    "弃坑" => "老朋友回来了",
                    _ => "今晚就玩它",
                }
                .to_string()
            };
            let days = g
                .days_since_played
                .map(|d| {
                    if d > 365 {
                        format!("你已经 {} 年没碰它了", d / 365)
                    } else if d > 0 {
                        format!("你已经 {} 天没碰它了", d)
                    } else {
                        "最近刚玩过，手感还在".to_string()
                    }
                })
                .unwrap_or_else(|| "它还在库里没拆封".into());
            let mood_line = mood.map(|m| format!("今晚想要{m}的话，它正合适——")).unwrap_or_default();
            let tags_txt = match g.tags.len() {
                0 => String::new(),
                1 => format!("「{}」", g.tags[0]),
                _ => format!("「{}」「{}」", g.tags[0], g.tags[1]),
            };
            let blurb = if g.explore {
                // 探索位：讲清楚"为什么它和你的常用品类不同"，别让人一头雾水
                format!(
                    "{days}。这次换个口味——{tags_txt}类的{badge}，\
                     和你常玩的路线差别不小，正好开个新坑。\
                     动机匹配 {:.0}%，先给 {typical_session_min} 分钟试水，不合胃口随时换一批。",
                    g.motivation * 100.0,
                    tags_txt = tags_txt,
                    badge = g.badge,
                )
            } else {
                format!(
                    "{mood_line}{days}。{tags_txt}向的{badge}，动机匹配 {:.0}%，\
                     今晚 {typical_session_min} 分钟正好来一轮，进度和大坑都接得上。",
                    g.motivation * 100.0,
                    tags_txt = tags_txt,
                    badge = g.badge,
                )
            };
            RecommendationCard {
                rank: (i + 1) as u8,
                app_id: g.app_id,
                name: g.name.clone(),
                title: title.into(),
                blurb,
                points: card_points(g),
                session_hint: format!("约 {typical_session_min} 分钟"),
                badge: g.badge,
                suggested_session_min: typical_session_min,
                launch_url: format!("steam://run/{}", g.app_id),
                download_info: g.download_info.clone(),
                explore: g.explore,
                source: "template",
            }
        })
        .collect()
}

/// session_hint 规整：空缺/纯数字/超长（>20 字）一律丢弃 LLM 的 hint、退回完整的
/// 分钟兜底短语——短语直接展示给用户，宁可要完整的"约 N 分钟"也不要残句
fn normalize_session_hint(raw: &str, fallback_min: u32) -> String {
    let t = raw.trim();
    if t.is_empty() || t.chars().all(|c| c.is_ascii_digit()) || t.chars().count() > 20 {
        return format!("约 {fallback_min} 分钟");
    }
    t.to_string()
}

fn merge_drafts(drafts: Vec<CardDraft>, facts: &[GameFacts], source: &'static str) -> Vec<RecommendationCard> {
    drafts
        .into_iter()
        .filter_map(|d| facts.iter().find(|f| f.app_id == d.app_id).map(|f| (d, f)))
        .take(3)
        .enumerate()
        .map(|(i, (d, f))| RecommendationCard {
            rank: (i + 1) as u8,
            app_id: d.app_id,
            name: f.name.clone(),
            title: d.title.trim().to_string(),
            blurb: d.blurb.trim().to_string(),
            points: card_points(f),
            session_hint: normalize_session_hint(
                &d.session_hint,
                if d.suggested_session_min == 0 { 60 } else { d.suggested_session_min },
            ),
            badge: f.badge,
            suggested_session_min: if d.suggested_session_min == 0 { 60 } else { d.suggested_session_min },
            launch_url: format!("steam://run/{}", d.app_id),
            download_info: f.download_info.clone(),
            explore: f.explore,
            source,
        })
        .collect()
}

/// 卡片生成：schema 校验 → 带错误重试 1 次 → 仍失败返回 Err（调用方走模板兜底）。
/// 返回 (卡片, 用量增量)。
/// 单次流式卡片调用（v0.49）：增量文本里数已完成的卡（每张卡恰有一个 "app_id" 字段），
/// 变化时 trace「推荐语 x/total」——对话页首屏生成与 CTA 换批共用此路径，逐张进度实时可见；
/// total=0 时不报逐张（探索位单卡场景）。完成后 trace 本调用用量（工具调用可视化）并入库。
/// 返回完整 content 文本；stats 为 (调用数, 入 tokens, 出 tokens, 费用) 增量累计。
async fn stream_cards_once(
    llm: &LlmClient,
    store: &Store,
    purpose: &str,
    msgs: &[ChatMessage],
    temperature: f32,
    budget: u32,
    total: usize,
    trace: &(dyn Fn(&str) + Send + Sync),
    stats: &mut (u32, u64, u64, Option<f64>),
) -> Result<String> {
    let acc = std::sync::Mutex::new(String::new());
    let done = std::sync::atomic::AtomicUsize::new(0);
    let out = llm
        .chat_stream(msgs, temperature, true, budget, &|delta: &str| {
            let mut a = acc.lock().expect("卡片进度锁");
            a.push_str(delta);
            let n = a.matches("\"app_id\"").count();
            let d = done.load(std::sync::atomic::Ordering::Relaxed);
            if n > d && total > 0 {
                // chunk 合包时可能一次跳多张，逐张补报
                for k in (d + 1)..=n.min(total) {
                    trace(&format!("推荐语 {k}/{total}"));
                }
                done.store(n, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .await?;
    stats.0 += 1;
    stats.1 += out.usage.prompt_tokens;
    stats.2 += out.usage.completion_tokens;
    stats.3 = match (stats.3, out.cost_cny) {
        (Some(a), Some(b)) => Some(a + b),
        (None, b) => b,
        (a, None) => a,
    };
    trace(&format!(
        "LLM 调用（{purpose}）：{}入/{}出{}",
        out.usage.prompt_tokens,
        out.usage.completion_tokens,
        out.cost_cny.map(|c| format!(" · ¥{c:.4}")).unwrap_or_default()
    ));
    store.record_usage(purpose, &out.model, out.usage.prompt_tokens, out.usage.completion_tokens, out.cost_cny)?;
    Ok(out.content)
}

async fn generate_cards(
    llm: &LlmClient,
    store: &Store,
    digest: &str,
    intent_desc: &str,
    facts: &[GameFacts],
    trace: &(dyn Fn(&str) + Send + Sync),
) -> Result<(Vec<RecommendationCard>, (u32, u64, u64, Option<f64>))> {
    let mut stats = (0u32, 0u64, 0u64, None);
    let total = facts.len().min(3); // 提示词约定恰好 3 张（候选不足则全出）
    trace(&format!("正在为挑选出的 {} 款候选写推荐语…", total));
    let sys = ChatMessage::system(CARD_SYSTEM);
    let ask = ChatMessage::user(card_user_prompt(digest, intent_desc, facts));
    let content =
        stream_cards_once(llm, store, "cards", &[sys.clone(), ask.clone()], 0.4, 4096, total, trace, &mut stats).await?;
    match parse_cards_response(&content, facts) {
        Ok(drafts) => Ok((merge_drafts(drafts, facts, "llm"), stats)),
        Err(err) => {
            tracing::warn!("卡片校验失败，带错误重试: {err}");
            let retry = ChatMessage::user(format!("上一次输出校验失败：{err}。请修正后重新输出完整 JSON。"));
            let prev = ChatMessage { role: "assistant", content };
            let content2 =
                stream_cards_once(llm, store, "cards", &[sys, ask, prev, retry], 0.2, 4096, total, trace, &mut stats).await?;
            let drafts =
                parse_cards_response(&content2, facts).map_err(|e| anyhow::anyhow!("重试后仍校验失败: {e}"))?;
            Ok((merge_drafts(drafts, facts, "llm(retry)"), stats))
        }
    }
}

/// 探索位专项提示词：与主批分开的口吻——任务不是"说服这是最好的游戏"，
/// 而是勾起尝鲜欲。三段式：点名画像常用品类制造反差 → 具体画面 → 降门槛试水收尾。
const EXPLORE_CARD_SYSTEM: &str = "你是游戏推荐官的「探索位」专员：为一位 Steam 玩家写一张完全跳出他常用品类的库存游戏卡。\
你的任务不是证明这是最好的游戏，而是勾起尝鲜欲——让人愿意为一个陌生的品类腾出一个晚上。\n\
规则：只能基于给定的玩家画像与候选事实说话，不得编造任何数字或事实。\n\
title：不超过 14 个字，带一点“出逃感”或悬念，让人好奇为什么这张卡出现在这里；禁止“推荐”二字。\n\
blurb：100～160 字，三件事缺一不可：\n\
① 点名玩家画像里的常用品类（画像摘要里有），与这张卡的品类形成反差——反差就是卖点，直说；\n\
② 用一两句具体画面描绘坐下来会经历什么（基于游戏的标签与成就类型事实）；\n\
③ 降低心理门槛的收尾——“先给 N 分钟，不合胃口随时换一批”式的试水邀请，N 参考典型会话时长。\n\
session_hint：不超过 20 字的具体颗粒度短语（同主规则）。\n\
suggested_session_min：整数分钟，与 session_hint 口径一致。\n\
输出严格 JSON：{\"cards\":[{\"app_id\":0,\"title\":\"\",\"blurb\":\"\",\"session_hint\":\"\",\"suggested_session_min\":0}]}，\
只输出 1 张卡。";

fn explore_card_prompt(digest: &str, session_cap: u32, facts: &[GameFacts]) -> String {
    let f: String = facts.iter().map(fact_line).collect();
    format!(
        "玩家画像：{digest}\n\
         本次任务：为探索位写 1 张卡（用户没有指定品类，系统特意从库里挑了与他口味最不重叠的一款）。\n\
         建议试水时长：约 {session_cap} 分钟（blurb 收尾与 suggested_session_min 参考它）。\n\
         候选事实：\n{f}"
    )
}

/// 探索位专项生成：主批漏选探索位时的补偿调用（单卡、高温、专属提示词）。
/// 只尝试一轮 + 校验失败重试一轮；失败由调用方走模板兜底。返回 (卡片, 用量增量)。
async fn generate_explore_cards(
    llm: &LlmClient,
    store: &Store,
    digest: &str,
    session_cap: u32,
    explore_facts: &[GameFacts],
    trace: &(dyn Fn(&str) + Send + Sync),
) -> Result<(Vec<RecommendationCard>, (u32, u64, u64, Option<f64>))> {
    let mut stats = (0u32, 0u64, 0u64, None);
    trace("正在为探索位写一张尝鲜卡…");
    let sys = ChatMessage::system(EXPLORE_CARD_SYSTEM);
    let ask = ChatMessage::user(explore_card_prompt(digest, session_cap, explore_facts));
    let content =
        stream_cards_once(llm, store, "explore_card", &[sys.clone(), ask.clone()], 0.7, 2048, 0, trace, &mut stats).await?;
    let drafts = match parse_cards_response(&content, explore_facts) {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!("探索位卡校验失败，带错误重试: {err}");
            let retry = ChatMessage::user(format!("上一次输出校验失败：{err}。请修正后重新输出完整 JSON。"));
            let prev = ChatMessage { role: "assistant", content };
            let content2 =
                stream_cards_once(llm, store, "explore_card", &[sys, ask, prev, retry], 0.5, 2048, 0, trace, &mut stats).await?;
            parse_cards_response(&content2, explore_facts).map_err(|e| anyhow::anyhow!("重试后仍校验失败: {e}"))?
        }
    };
    Ok((merge_drafts(drafts, explore_facts, "llm(explore)"), stats))
}

// ============ 画像摘要（注入 prompt，≈300 token，不含 SteamID/全库） ============

pub fn profile_digest(p: &PlayerProfile) -> String {
    let main_type = [
        ("成就完成型", p.axes.achiever),
        ("探索发现型", p.axes.explorer),
        ("竞争对抗型", p.axes.killer),
        ("社交合作型", p.axes.socializer),
    ]
    .into_iter()
    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let top_tags: Vec<String> = {
        let mut tw: Vec<(&String, &f64)> = p.tag_weights.iter().collect();
        tw.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        tw.iter().take(8).map(|(k, v)| format!("{k}({v:.2})")).collect()
    };
    format!(
        "主型{}（成就完成 {:.2}/探索发现 {:.2}/竞争对抗 {:.2}/社交合作 {:.2}）；品类偏好：{}；深度分布：未开封 {}、试玩即弃 {}、活跃 {}、暂离 {}、弃坑 {}、已完成 {}；典型会话约 {} 分钟。",
        main_type.map(|(l, _)| l.to_string()).unwrap_or_default(),
        p.axes.achiever,
        p.axes.explorer,
        p.axes.killer,
        p.axes.socializer,
        top_tags.join("、"),
        p.depth_counts.get(&crate::profiler::GameDepth::Unplayed).copied().unwrap_or(0),
        p.depth_counts.get(&crate::profiler::GameDepth::Sampled).copied().unwrap_or(0),
        p.depth_counts.get(&crate::profiler::GameDepth::Active).copied().unwrap_or(0),
        p.depth_counts.get(&crate::profiler::GameDepth::Service).copied().unwrap_or(0),
        p.depth_counts.get(&crate::profiler::GameDepth::Abandoned).copied().unwrap_or(0),
        p.depth_counts.get(&crate::profiler::GameDepth::Finished).copied().unwrap_or(0),
        p.behavior.typical_session_min
    )
}

// ============ 会话（CLI 与 Web 共用结构） ============

#[derive(Serialize, Clone, serde::Deserialize)]
pub struct TurnRecord {
    pub user: String,
    pub intent: String,
    pub candidate_ids: Vec<u32>,
    pub cards: Vec<RecommendationCard>,
}

#[derive(Serialize, Default, serde::Deserialize)]
struct SessionLog {
    created_at: u64,
    turns: Vec<TurnRecord>,
}

/// 跨轮状态（对话历史 / 上次意图 / 已展示候选 / 轨道放宽 / 探索模式）
#[derive(Default, Clone)]
pub struct AskState {
    pub history: Vec<(String, String)>, // (user, assistant 摘要)
    pub last_intent: Option<ParsedIntent>,
    pub shown_apps: Vec<u32>,
    /// 当前轨道已执行过"放宽条件"（新意图出现时重置）
    pub relaxed: bool,
    /// 探索模式（§7.9①"没想法"信号）：连续 2 次换批且无启动 → 本会话提温 + 降即玩权重 + 1 席探索位
    pub exploration: bool,
    /// 连续换批次数（有 launch 即清零，见 handle_turn 开头的事件检查）
    pub switches_since_launch: u32,
    /// 上次轮开始时已知的最近 launch 时间（用于检测轮间新增的启动行为）
    pub last_launch_ts: i64,
}

impl AskState {
    fn recent_history(&self, n: usize) -> String {
        let start = self.history.len().saturating_sub(n);
        self.history[start..]
            .iter()
            .map(|(u, a)| format!("用户：{u}\n助手：{a}"))
            .collect::<Vec<_>>()
            .join("\n---\n")
    }
}

/// 单轮结果：意图描述、候选、卡片、LLM 用量增量、轨道放宽状态
pub struct TurnResult {
    pub intent_desc: String,
    pub candidate_ids: Vec<u32>,
    pub cards: Vec<RecommendationCard>,
    pub llm_calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// 本轮总费用（未计价模型为 None；done 事件透传给前端展示）
    pub cost_cny: Option<f64>,
    /// 本轮无候选且带品类条件、尚未放宽 → 前端 CTA 提供"放宽条件"入口
    pub relaxable: bool,
    /// 当前轨道已放宽（无候选且已放宽 → "整个库都看完了"）
    pub relaxed: bool,
    /// 候选池本批已见底（再换批必为空）→ 轨道末尾 CTA 直接显示放宽/尽头而非"换一批"
    pub exhausted: bool,
}

/// 会话存档追加（R5）：data/sessions/session-<id>.json，读-改-写。
pub fn append_session_turn(dir: &Path, session_id: &str, turn: TurnRecord) -> Result<()> {
    std::fs::create_dir_all(dir).ok();
    let path = dir.join(format!("session-{session_id}.json"));
    let mut log: SessionLog = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if log.created_at == 0 {
        log.created_at = now_secs();
    }
    log.turns.push(turn);
    std::fs::write(&path, serde_json::to_vec_pretty(&log)?)?;
    Ok(())
}

pub fn budget_check(store: &Store, cfg: &Config) -> Result<()> {
    let budget = cfg.agent.daily_budget_cny;
    if budget <= 0.0 {
        return Ok(());
    }
    let used = store.today_usage_cny().context("查询今日用量失败")?;
    if used >= budget {
        bail!("今日 LLM 预算已用尽（¥{used:.2} / ¥{budget:.2}）。可调高 daily_budget_cny，或明天再聊。");
    }
    Ok(())
}

fn default_filters(cfg: &Config) -> IntentFilters {
    IntentFilters {
        tags: vec![],
        exclude_tags: vec![],
        max_session_min: None,
        only_instant: false,
        only_installed: false,
        top_m: cfg.recommender.top_m,
        mood: None,
        exclude_apps: vec![],
    }
}

/// 单轮处理管线（CLI 与 Web 共用）：意图解析 → 确定性评分 → 卡片生成（校验+兜底）。
/// `trace` 上报每步事件；状态就地更新（历史/意图/已展示）。
pub async fn handle_turn(
    store: &Store,
    cfg: &Config,
    llm: &LlmClient,
    profile: &PlayerProfile,
    digest: &str,
    state: &mut AskState,
    input: &str,
    local_first: bool,
    trace: &(dyn Fn(&str) + Send + Sync),
) -> Result<TurnResult> {
    budget_check(store, cfg)?;
    let (mut calls, mut ptoks, mut ctoks) = (0u32, 0u64, 0u64);
    let mut cost: Option<f64> = None;
    let add_cost = |cost: &mut Option<f64>, c: Option<f64>| {
        *cost = match (*cost, c) {
            (Some(a), Some(b)) => Some(a + b),
            (None, b) => b,
            (a, None) => a,
        };
    };

    // 轮间启动检测：前端异步上报 launch（steam:// 协议跳转不打断对话），这里查事件表。
    // 有新启动 → 用户找到了想玩的，换批计数与探索模式一并清零。
    if let Ok(Some(ts)) = store.last_launch_ts() {
        if ts > state.last_launch_ts {
            state.last_launch_ts = ts;
            state.switches_since_launch = 0;
            state.exploration = false;
        }
    }

    // ① 意图解析（“换一批/放宽条件”走 Rust 短路，不花 token）
    let mut relaxed = state.relaxed;
    /// 换批类短路（换一批/放宽条件）共用的探索信号记账
    fn note_switch(store: &Store, state: &mut AskState, trace: &dyn Fn(&str)) {
        state.switches_since_launch += 1;
        if state.switches_since_launch >= 2 && !state.exploration {
            state.exploration = true;
            // "没想法"信号 → 即玩自适应下调（−0.2，画像页可重置）
            let aff = store.bump_install_affinity(-0.2).unwrap_or(0.5);
            trace(&format!(
                "检测到连续换批且无启动——进入探索模式：提高抽样温度、降低即玩权重、注入 1 席「换个口味」（即玩倾向 {:.2}）",
                aff
            ));
        }
    }
    let parsed = if state.last_intent.is_some() && input.contains("放宽") && !state.relaxed {
        // 条件尽头放宽：沿用上次意图、剥离正向品类筛选（保留排除/时长/即玩约束）
        let mut it = state.last_intent.clone().unwrap();
        let dropped = std::mem::take(&mut it.tags);
        it.exclude_apps.extend(state.shown_apps.iter().copied());
        relaxed = true;
        note_switch(store, state, trace);
        trace(&format!(
            "「放宽条件」：已剥离品类筛选「{}」——以下推荐不再限定该条件（排除项/时长/即玩约束保留）",
            if dropped.is_empty() { "无".into() } else { dropped.join("、") }
        ));
        it
    } else if state.last_intent.is_some() && input.contains("换一批") {
        let mut it = state.last_intent.clone().unwrap();
        it.exclude_apps.extend(state.shown_apps.iter().copied());
        note_switch(store, state, trace);
        trace(&format!("「换一批」：沿用上次意图，排除已展示 {} 款", state.shown_apps.len()));
        it
    } else {
        relaxed = false; // 新意图开启新轨道
        state.switches_since_launch = 0; // 明确表达新需求 ≠ 没想法，探索模式退出
        state.exploration = false;
        let popular: Vec<String> = {
            let mut tw: Vec<(&String, &f64)> = profile.tag_weights.iter().collect();
            tw.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
            tw.iter().take(20).map(|(k, _)| k.to_string()).collect()
        };
        let hist = state.recent_history(4);
        let ask = if hist.is_empty() {
            input.to_string()
        } else {
            format!("对话历史：\n{hist}\n---\n用户最新消息：{input}")
        };
        trace("正在理解你想玩什么…");
        let out = llm
            .chat(
                &[ChatMessage::system(intent_system(&popular.join("、"))), ChatMessage::user(ask)],
                0.1,
                true,
                512,
            )
            .await
            .context("意图解析调用失败")?;
        calls += 1;
        ptoks += out.usage.prompt_tokens;
        ctoks += out.usage.completion_tokens;
        add_cost(&mut cost, out.cost_cny);
        trace(&format!(
            "LLM 调用（意图解析）：{}入/{}出{}",
            out.usage.prompt_tokens,
            out.usage.completion_tokens,
            out.cost_cny.map(|c| format!(" · ¥{c:.4}")).unwrap_or_default()
        ));
        store.record_usage("intent", &out.model, out.usage.prompt_tokens, out.usage.completion_tokens, out.cost_cny)?;
        match parse_intent_response(&out.content) {
            Some(f) => {
                trace(&format!(
                    "意图解析 → 品类{:?} 排除{:?} 时长{} 心境{} 仅装好{}{}",
                    f.tags,
                    f.exclude_tags,
                    f.max_session_min.map(|m| m.to_string() + "min").unwrap_or_else(|| "不限".into()),
                    f.mood.clone().unwrap_or_else(|| "不限".into()),
                    f.instant_only,
                    if f.remember_exclude.is_empty() { String::new() } else { format!(" 长期排除{:?}", f.remember_exclude) }
                ));
                // 长期口味排除 → 写修正层（跨会话生效，画像页可撤销）
                for tag in &f.remember_exclude {
                    if let Err(e) = store.propose_taste_exclude(tag) {
                        tracing::warn!("写入口味排除「{tag}」失败: {e}");
                    } else {
                        trace(&format!("已长期排除「{tag}」——修正层标注，画像页可撤销"));
                    }
                }
                f
            }
            None => {
                trace("意图解析失败（输出非 JSON），退回默认全库");
                ParsedIntent::default()
            }
        }
    };

    let mut intent = default_filters(cfg);
    intent.tags = parsed.tags.clone();
    intent.exclude_tags = parsed.exclude_tags.clone();
    intent.max_session_min = parsed.max_session_min;
    intent.mood = parsed.mood.clone();
    intent.only_instant = parsed.instant_only;
    // 本地优先（对话输入区开关，跨换批/放宽持续生效）：未安装游戏不进候选池
    intent.only_installed = local_first;
    intent.exclude_apps = parsed.exclude_apps.clone();

    // ② 2a 确定性评分（数字归代码）；探索感 = 设置页滑条 → softmax 温度，探索模式再提温
    let opts = crate::recommender::RecommendOpts {
        randomness: cfg.recommender.randomness,
        exploration: state.exploration,
    };
    let cands = recommend(store, cfg, profile, &intent, cfg.agent.llm_positioning, &opts, Some(&|s: &str| trace(s)))
        .context("评分失败")?;
    if state.exploration {
        trace("探索模式生效：本批从更大池子里抽样，并带 1 席「换个口味」");
    }
    let fatigued = cands.iter().filter(|c| c.breakdown.fatigue < 0.0).count();
    if fatigued > 0 {
        trace(&format!("行为反馈：{fatigued} 款候选带疲劳降权（近期被跳过/点过不感兴趣，启动后自动清零）"));
    }
    trace(&format!("评分完成：候选 {} 款", cands.len()));
    let intent_desc = format!(
        "品类偏好{:?}、排除{:?}{}、心境：{}",
        intent.tags,
        intent.exclude_tags,
        intent.max_session_min.map(|m| format!("、时长≤{m}分钟")).unwrap_or_default(),
        intent.mood.clone().unwrap_or_else(|| "不限".into())
    );
    if cands.is_empty() {
        let relaxable = !relaxed && !intent.tags.is_empty();
        if relaxable {
            trace("符合条件的游戏已看完——可点 CTA「放宽条件」看库里的其他选择");
        } else if relaxed {
            trace("放宽后也没有新候选了——整个库都看完了，换个说法试试");
        }
        // 空候选也要保存意图（放宽短路需要读取 last_intent）
        state.last_intent = Some(parsed);
        state.relaxed = relaxed;
        state.history.push((input.to_string(), "（无候选）".into()));
        return Ok(TurnResult {
            intent_desc,
            candidate_ids: vec![],
            cards: vec![],
            llm_calls: calls,
            prompt_tokens: ptoks,
            completion_tokens: ctoks,
            cost_cny: cost,
            relaxable,
            relaxed,
            exhausted: true, // 空候选本身就是见底
        });
    }

    // ③ 卡片生成（人话归模型；「换一批」只短路意图解析，卡片仍由 LLM 生成保持文案质量；
    //    失败走 Rust 模板兜底）。建议时长以用户时限为上限
    let steam_dir = crate::steam_local::resolve_steam_dir(cfg).ok();
    let mut facts = build_facts(store, &cands, cfg.network.download_speed_mbps, steam_dir.as_deref())?;
    // 探索位置顶：LLM 每批只产 3 张卡，探索位总分通常靠后，不置顶就永远选不进
    facts.sort_by_key(|f| if f.explore { 0 } else { 1 });
    let session_cap = intent
        .max_session_min
        .unwrap_or(profile.behavior.typical_session_min)
        .min(profile.behavior.typical_session_min.max(30));
    let cards = match generate_cards(llm, store, digest, &intent_desc, &facts, trace).await {
        Ok((c, s)) => {
            calls += s.0;
            ptoks += s.1;
            ctoks += s.2;
            add_cost(&mut cost, s.3);
            c
        }
        Err(e) => {
            trace(&format!("卡片生成失败（{e}）→ Rust 模板兜底"));
            template_cards(&facts, session_cap, intent.mood.as_deref())
        }
    };
    // 探索位兜底：LLM 每批只产 3 张，探索位总分靠后常被漏选——漏选时先追加一次
    // 探索位专项 LLM 生成（单卡、专属提示词），失败才退 Rust 模板；探索位必须每批可见
    let missing_explore: Vec<GameFacts> = facts
        .iter()
        .filter(|f| f.explore && !cards.iter().any(|c| c.app_id == f.app_id))
        .cloned()
        .collect();
    let mut cards = cards;
    if !missing_explore.is_empty() {
        match generate_explore_cards(llm, store, digest, session_cap, &missing_explore, trace).await {
            Ok((mut ec, s)) => {
                calls += s.0;
                ptoks += s.1;
                ctoks += s.2;
                add_cost(&mut cost, s.3);
                trace("探索位未被主批选中 → 探索位专项 LLM 生成补齐");
                cards.append(&mut ec);
            }
            Err(e) => {
                trace(&format!("探索位专项生成失败（{e}）→ Rust 模板兜底"));
                cards.extend(template_cards(&missing_explore, session_cap, intent.mood.as_deref()));
            }
        }
        // 追加卡的 rank 修正为主批之后的全批序号
        for (i, c) in cards.iter_mut().enumerate() {
            c.rank = (i + 1) as u8;
        }
    }

    // 已展示 = 实际渲染的卡片（LLM 每批只产 3 张）；候选池里未被选中的保留给后续换批
    state.shown_apps = cards.iter().map(|c| c.app_id).collect();
    state.last_intent = Some(parsed);
    state.relaxed = relaxed;
    let summary = cards
        .iter()
        .map(|c| format!("《{}》{}", c.name, c.title))
        .collect::<Vec<_>>()
        .join("；");
    state.history.push((input.to_string(), summary));
    // 本批把候选池发完（如品类过滤只剩 1 款）→ 末尾 CTA 直接给放宽/尽头，不再"换一批"
    let exhausted_pool = cards.len() >= cands.len();

    Ok(TurnResult {
        intent_desc,
        candidate_ids: facts.iter().map(|f| f.app_id).collect(),
        cards,
        llm_calls: calls,
        prompt_tokens: ptoks,
        completion_tokens: ctoks,
        cost_cny: cost,
        // 见底且带品类条件未放宽 → 前端末尾 CTA 显示「放宽条件」（与空候选同语义）
        relaxable: exhausted_pool && !relaxed && !intent.tags.is_empty(),
        relaxed,
        exhausted: exhausted_pool,
    })
}

// ============ CLI 入口 ============

fn print_cards(cards: &[RecommendationCard]) {
    for c in cards {
        println!("\n {} 《{}》——{}  〔{}〕", c.rank, c.name, c.title, c.badge);
        println!("   {}", c.blurb);
        let pts = c.points.iter().map(|p| format!("[{}]", p.text)).collect::<Vec<_>>().join(" ");
        println!("   推荐点：{pts}");
        println!("   建议时长 ~{} 分钟｜启动：{}", c.suggested_session_min, c.launch_url);
    }
    println!("\n（说“换一批”看下一组；“不要 XX”修正筛选；q 退出）");
}

/// 对话式推荐入口（CLI 形态；Web 形态见 server.rs，共用 handle_turn）。
pub async fn run_ask(db: &Path, cfg: &Config) -> Result<()> {
    let store = Store::open(db).context("打开数据库失败")?;
    let profile = crate::profiler::compute(&store, cfg)?;
    if profile.games.is_empty() {
        bail!("库为空：请先运行 tonight sync");
    }
    let llm = LlmClient::from_config(cfg).context("ask 需要 LLM：请配置 [agent] active_llm 与对应 key")?;
    let digest = profile_digest(&profile);

    println!("（LLM：{}｜定价：{}）", llm.model(), llm.price_note());
    println!(
        "画像速览：主型探索 {:.2}｜积压 {} 款｜典型会话 ~{} 分钟",
        profile.axes.explorer,
        profile
            .games
            .iter()
            .filter(|g| matches!(
                g.depth,
                crate::profiler::GameDepth::Unplayed
                    | crate::profiler::GameDepth::Abandoned
                    | crate::profiler::GameDepth::Sampled
            ))
            .count(),
        profile.behavior.typical_session_min
    );
    println!("说说今晚想玩什么（q 退出）");

    let mut state = AskState::default();
    let (mut calls, mut ptoks, mut ctoks) = (0u32, 0u64, 0u64);
    let sessions_dir = db.parent().map(|p| p.join("sessions")).unwrap_or_else(|| std::path::PathBuf::from("sessions"));
    let session_id = now_secs().to_string();

    loop {
        print!("\n> ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input.to_lowercase().as_str(), "q" | "quit" | "exit" | "退出") {
            break;
        }
        // CLI 形态不启用本地优先（保持全库候选；Web 由输入区开关控制）
        let result = handle_turn(&store, cfg, &llm, &profile, &digest, &mut state, input, false, &|s| {
            println!("[轨迹] {s}")
        })
        .await;
        match result {
            Ok(r) => {
                calls += r.llm_calls;
                ptoks += r.prompt_tokens;
                ctoks += r.completion_tokens;
                if r.cards.is_empty() {
                    println!("没有符合条件的候选，放宽点要求试试？");
                } else {
                    print_cards(&r.cards);
                }
                append_session_turn(&sessions_dir, &session_id, TurnRecord {
                    user: input.to_string(),
                    intent: r.intent_desc.clone(),
                    candidate_ids: r.candidate_ids.clone(),
                    cards: r.cards.clone(),
                })?;
            }
            Err(e) => println!("出错了：{e:#}"),
        }
    }

    println!("\n本轮会话：{calls} 次调用，{ptoks} 入 / {ctoks} 出 tokens；存档：{}", sessions_dir.join(format!("session-{session_id}.json")).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_intent_json_with_fences() {
        let content = "```json\n{\"tags\":[\"休闲\",\"解谜\"],\"exclude_tags\":[\"恐怖\"],\"max_session_min\":60,\"mood\":\"轻松\",\"instant_only\":true,\"exclude_apps\":[]}\n```";
        let it = parse_intent_response(content).unwrap();
        assert_eq!(it.tags, vec!["休闲", "解谜"]);
        assert_eq!(it.exclude_tags, vec!["恐怖"]);
        assert_eq!(it.max_session_min, Some(60));
        assert_eq!(it.mood.as_deref(), Some("轻松"));
        assert!(it.instant_only);
    }

    #[test]
    fn intent_defaults_on_missing_fields() {
        let it = parse_intent_response("{\"tags\":[\"冒险\"]}").unwrap();
        assert_eq!(it.tags, vec!["冒险"]);
        assert_eq!(it.max_session_min, None);
        assert!(!it.instant_only);
        assert!(parse_intent_response("我不会输出 JSON").is_none());
    }

    fn sample_facts() -> Vec<GameFacts> {
        vec![GameFacts {
            app_id: 105600,
            name: "Terraria".into(),
            depth: "弃坑",
            badge: "未安装",
            tags: vec!["沙盒".into(), "生存".into()],
            days_since_played: Some(300),
            motivation: 0.82,
            tag_score: 1.0,
            attainability: 0.25,
            backlog: 1.0,
            global_median_pct: Some(25.0),
            cat_dist: "explore×12".into(),
            download_info: None,
            explore: false,
        }]
    }

    #[test]
    fn validates_cards_against_candidates() {
        let facts = sample_facts();
        let long_blurb = "三年了，那个属于你的世界还在原地等你。今晚回去挖一次矿、打一次boss，把没修好的桥修好，短会话也尽兴。".to_string();
        let ok = format!(
            "{{\"cards\":[{{\"app_id\":105600,\"title\":\"回去挖矿吧\",\"blurb\":\"{long_blurb}\",\"suggested_session_min\":90}}]}}"
        );
        let drafts = parse_cards_response(&ok, &facts).unwrap();
        assert_eq!(drafts.len(), 1);
        // blurb 过短 → 校验失败（供带错重试）
        let short = "{\"cards\":[{\"app_id\":105600,\"title\":\"x\",\"blurb\":\"太短了\",\"suggested_session_min\":1}]}";
        assert!(parse_cards_response(short, &facts).is_err());
        let bad = "{\"cards\":[{\"app_id\":999,\"title\":\"x\",\"blurb\":\"y\",\"suggested_session_min\":1}]}";
        assert!(parse_cards_response(bad, &facts).is_err());
        assert!(parse_cards_response("不是 JSON", &facts).is_err());
        // 同批重复 app_id → 校验失败（供带错重试），不得输出重复卡
        let dup = format!(
            "{{\"cards\":[{{\"app_id\":105600,\"title\":\"a\",\"blurb\":\"{long_blurb}\",\"suggested_session_min\":60}},{{\"app_id\":105600,\"title\":\"b\",\"blurb\":\"{long_blurb}\",\"suggested_session_min\":60}}]}}"
        );
        assert!(parse_cards_response(&dup, &facts).is_err());
    }

    #[test]
    fn oversized_session_hint_passes_and_falls_back() {
        let facts = sample_facts();
        // 超长 hint 不触发校验失败（避免一次重试调用），merge 时丢弃退完整兜底短语
        let long_hint = "这是一条模型偶尔会输出的、明显超过二十个字长度限制的时长提示短语对吧";
        assert!(long_hint.chars().count() > 20);
        let ok = format!(
            "{{\"cards\":[{{\"app_id\":105600,\"title\":\"回去挖矿吧\",\"blurb\":\"三年了，那个属于你的世界还在原地等你。今晚回去挖一次矿、打一次boss，把没修好的桥修好，短会话也尽兴。\",\"session_hint\":\"{long_hint}\",\"suggested_session_min\":90}}]}}"
        );
        let drafts = parse_cards_response(&ok, &facts).unwrap();
        let cards = merge_drafts(drafts, &facts, "llm");
        assert_eq!(cards[0].session_hint, "约 90 分钟");
        // 空缺/纯数字 → 分钟短语兜底；正常长度原样保留
        assert_eq!(normalize_session_hint("", 45), "约 45 分钟");
        assert_eq!(normalize_session_hint("90", 45), "约 45 分钟");
        assert_eq!(normalize_session_hint("一局约 20 分钟", 45), "一局约 20 分钟");
    }

    #[test]
    fn template_cards_cite_real_numbers() {
        let facts = sample_facts();
        let cards = template_cards(&facts, 120, Some("轻松"));
        assert_eq!(cards.len(), 1);
        assert!(cards[0].blurb.contains("300"));
        assert!(cards[0].points.iter().any(|p| p.kind == "attain" && p.text.contains("25%")));
        assert!(cards[0].points.iter().any(|p| p.kind == "taste" && p.text == "沙盒"));
        assert!(cards[0].points.iter().any(|p| p.kind == "match" && p.text.contains("82%")));
        assert!(cards[0].launch_url.starts_with("steam://run/105600"));
    }

    #[test]
    fn explore_prompt_carries_digest_session_and_facts() {
        let facts = sample_facts();
        let mut e = facts[0].clone();
        e.explore = true;
        let prompt = explore_card_prompt("主型：成就完成型；品类偏好：RPG、动作", 90, std::slice::from_ref(&e));
        assert!(prompt.contains("主型：成就完成型"));
        assert!(prompt.contains("90 分钟"));
        assert!(prompt.contains("《Terraria》"));
        assert!(prompt.contains("探索位"));
        assert!(prompt.contains("只输出 1 张") || prompt.contains("1 张卡"));
    }

    #[test]
    fn explore_template_card_tells_the_story_and_is_not_short() {
        // 探索位兜底卡：用户直接阅读，文案不能比 LLM 卡短一截（真机反馈）
        let mut facts = sample_facts();
        facts[0].explore = true;
        facts[0].days_since_played = None;
        let cards = template_cards(&facts, 90, None);
        assert_eq!(cards[0].title, "换个口味试试");
        assert!(cards[0].explore);
        assert!(cards[0].blurb.contains("换个口味"));
        assert!(cards[0].blurb.contains("「沙盒」"));
        assert!(cards[0].blurb.contains("82%")); // 动机数字仍要引用事实
        assert!(cards[0].blurb.chars().count() >= 60, "blurb too short: {}", cards[0].blurb);
    }
}
