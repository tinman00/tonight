//! 玩家画像（design.md 需求 1 / C2 / C4 / §7.4）。
//! C4：成就类型在线分析——LLM 判类型、代码算分数；一游戏一次、永久缓存、计入用量（R6）。
//! 画像计算：纯读 SQLite 的确定性统计；注水提议会写 annotations（机器提议、人类确认、修正永远赢）。

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;

use crate::config::Config;
use crate::llm::{ChatMessage, LlmClient};
use crate::models::AchievementCategory;
use crate::store::Store;

// ============ C4：成就类型在线分析 ============

/// api_name 是否可表意：过滤 `ACHIEVEMENT_113` / `ACH_9` 这类纯内部编号命名（实测库中存在）。
pub fn is_expressive(name: &str) -> bool {
    let n = name.trim();
    if n.len() < 3 {
        return false;
    }
    if !n.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let stem = n.strip_prefix("NEW_").unwrap_or(n);
    for prefix in ["ACHIEVEMENT_", "ACH_"] {
        if let Some(rest) = stem.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '_') {
                return false;
            }
        }
    }
    true
}

/// 成就分类规则版本：升级提示词/输入信号时递增，sync 检测到旧版缓存会清空并在线重分析
pub const ACH_CAT_VERSION: &str = "3";

const TYPE_SYSTEM: &str = "你是 Steam 成就分类器。输入为成就内部标识名（api_name）与其全球达成率，把每个成就分到恰好一个类别：\n\
story=剧情/关卡推进（完成任务目标、到达剧情节点，**含终章之前的推进**）；challenge=高难度挑战（无伤/速通/高难操作/完成一项考验/生存试炼）；\n\
collect=收集齐 N 个物品/图鉴；explore=探索发现（地图/隐藏区域/秘密）；competitive=多人对战/排名/竞技；\n\
coop=多人合作相关（合作/双人模式的推进与目标都算——合作战役的章节推进也是 coop，即便带剧情色彩也不是 story）；grind=长期重复劳动或累计数量；other=无法判断。\n\
completion=**游戏主线战役的最终终点**——击败最终 Boss、到达结局、通关制作人员名单。只有\"游戏走到头\"的标志才归此类：\n\
终章高潮（最终决战、逃离设施、通关字幕，含终幕标志性动作，如\"射向月亮\"式结局事件）算；章节中途推进、支线/可选结局、生存或试炼类条件达成、完成若干挑战**都不是**。\n\
completion 误判的代价很高（游戏会被直接标记为已完成）：拿不准就不给，story 是安全落点。\n\
全球达成率仅供参考：前期/教程成就普遍 >60%，主线终点多在 15–50%，彩蛋与极限挑战 <10%。\n\
输出严格的 JSON 对象 {\"<api_name>\":\"<类别小写>\"}，必须覆盖输入的每一个名字，不要输出任何其他文字。";

/// 解析模型输出（纯函数）。返回 (映射, 是否成功解析出 JSON 对象)；缺失/未知类别 → other。
pub fn parse_type_response(
    content: &str,
    requested: &[String],
) -> (BTreeMap<String, AchievementCategory>, bool) {
    let text = extract_json_object(content);
    let parsed: Option<BTreeMap<String, String>> = serde_json::from_str(&text).ok();
    let ok = parsed.is_some();
    let mut out = BTreeMap::new();
    let source = parsed.unwrap_or_default();
    for name in requested {
        let cat = source
            .get(name)
            .and_then(|s| AchievementCategory::parse(s))
            .unwrap_or(AchievementCategory::Other);
        out.insert(name.clone(), cat);
    }
    (out, ok)
}

/// 剥掉 Markdown 围栏，取首个 `{` 到末个 `}` 之间的内容。
fn extract_json_object(s: &str) -> String {
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

#[derive(Debug, Default, Clone, Copy)]
pub struct AnalyzeStats {
    pub games: u32,
    pub calls: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_cny: Option<f64>,
}

impl AnalyzeStats {
    fn add_call(&mut self, model: &str, prompt: u64, completion: u64, cost: Option<f64>) {
        self.calls += 1;
        self.prompt_tokens += prompt;
        self.completion_tokens += completion;
        // 价格未知的调用让整体费用不可知（None）；其余正常累计
        self.cost_cny = match (self.cost_cny, cost) {
            (_, None) => None,
            (Some(a), Some(b)) => Some(a + b),
            (None, Some(b)) => Some(b),
        };
        let _ = model;
    }

    fn merge(&mut self, other: AnalyzeStats) {
        self.games += other.games;
        self.calls += other.calls;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        // 零调用的游戏（如无可表意成就被跳过）不参与费用合并
        if other.calls > 0 {
            self.cost_cny = match (self.cost_cny, other.cost_cny) {
                (_, None) => None,
                (Some(a), Some(b)) => Some(a + b),
                (None, Some(b)) => Some(b),
            };
        }
    }
}

/// 纯 LLM 侧的类型分析（不碰 DB，可在并行任务中运行）。
/// 返回 (分类行, 每次调用的用量记录)；分类行为 None 表示 LLM 调用失败——
/// 不落库、下次同步重试。不可表意成就不足 3 条或不足三成 → 整组 other 落库
/// （作为"已处理"标记，下次同步不再调用）。
async fn analyze_names(
    llm: &LlmClient,
    app_id: u32,
    names: &[String],
    global_map: &HashMap<String, f32>,
) -> (Option<Vec<(String, AchievementCategory)>>, Vec<(String, u64, u64, Option<f64>)>) {
    let mut cats: BTreeMap<String, AchievementCategory> =
        names.iter().map(|n| (n.clone(), AchievementCategory::Other)).collect();
    let mut usage: Vec<(String, u64, u64, Option<f64>)> = Vec::new();
    let expressive: Vec<&String> = names.iter().filter(|n| is_expressive(n)).collect();

    if expressive.len() >= 3 && expressive.len() * 10 >= names.len() * 3 {
        // 全球达成率注入：终点成就的分布信号（教程 >60%、主线终点 15–50%、彩蛋 <10%）
        let pct = |n: &str| -> String {
            global_map.get(n).map(|p| format!("（全球 {p:.0}%）")).unwrap_or_default()
        };
        for chunk in expressive.chunks(150) {
            let requested: Vec<String> = chunk.iter().map(|s| (*s).clone()).collect();
            let listing = requested.iter().map(|n| format!("{n}{}", pct(n))).collect::<Vec<_>>().join("\n");
            let ask = format!(
                "成就 api_name 列表（每行一个，附全球达成率）：\n{listing}\n\n请输出覆盖以上全部 {} 个名字的 JSON 对象。",
                requested.len()
            );
            let out = match llm
                .chat(&[ChatMessage::system(TYPE_SYSTEM), ChatMessage::user(ask)], 0.1, true, 8192)
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!("app {app_id} 成就类型分析失败（跳过，下次同步重试）: {e}");
                    return (None, usage);
                }
            };
            usage.push((out.model, out.usage.prompt_tokens, out.usage.completion_tokens, out.cost_cny));
            let (mut map, ok) = parse_type_response(&out.content, &requested);
            if !ok {
                let retry = format!(
                    "上一次输出不是合法 JSON。请只输出 JSON 对象，覆盖以下全部名字：\n{listing}"
                );
                let out2 = llm
                    .chat(&[ChatMessage::system(TYPE_SYSTEM), ChatMessage::user(retry)], 0.1, true, 8192)
                    .await;
                if let Ok(o2) = out2 {
                    usage.push((o2.model, o2.usage.prompt_tokens, o2.usage.completion_tokens, o2.cost_cny));
                    let (m2, _) = parse_type_response(&o2.content, &requested);
                    map = m2;
                }
            }
            for (k, v) in map {
                cats.insert(k, v);
            }
        }
    }
    (Some(cats.into_iter().collect()), usage)
}

/// 对一个游戏的全部成就做类型分析并落库（单游戏入口：读库 → LLM → 写库）。
pub async fn analyze_game(
    llm: &LlmClient,
    store: &Store,
    app_id: u32,
    names: &[String],
) -> Result<AnalyzeStats> {
    let mut stats = AnalyzeStats { games: 1, ..Default::default() };
    let global_map: HashMap<String, f32> =
        store.global_achievements(app_id)?.into_iter().collect();
    let (rows, usage) = analyze_names(llm, app_id, names, &global_map).await;
    for (model, pin, pout, cost) in &usage {
        stats.add_call(model, *pin, *pout, *cost);
        store.record_usage("ach_type_analysis", model, *pin, *pout, *cost)?;
    }
    if let Some(rows) = rows {
        store.upsert_achievement_categories(app_id, &rows)?;
    }
    Ok(stats)
}

/// 补齐缺失的成就类型分析（Tier F：缓存命中则跳过）。LLM 未配置时提示并跳过。
/// 范围：库内所有有成就数据的游戏——玩过的用玩家成就名，未玩候选用全球成就名
/// （全球 API 免玩家数据，候选侧定位由此获得与已玩游戏相同的信号，design.md §7.5 三源策略②）。
pub async fn analyze_missing(
    llm: Option<&LlmClient>,
    store: &Store,
    progress: &(dyn Fn(&str) + Send + Sync),
) -> Result<AnalyzeStats> {
    let owned = store.all_owned_games()?;
    let mut todo = Vec::new();
    for g in &owned {
        let total = store.achievement_count(g.app_id)?.max(store.global_count(g.app_id)?);
        if total > 0 && store.categorized_count(g.app_id)? < total {
            todo.push(g.clone());
        }
    }
    let Some(llm) = llm else {
        if !todo.is_empty() {
            progress(&format!(
                "[6/6] 成就类型分析：{}/{} 款待分析，但未配置 LLM，已跳过（画像将仅用 tag 信号）",
                todo.len(),
                owned.len()
            ));
        }
        return Ok(AnalyzeStats::default());
    };
    if todo.is_empty() {
        progress("[6/6] 成就类型分析：全部缓存命中，无需重新分析");
        return Ok(AnalyzeStats::default());
    }
    progress(&format!("[6/6] 成就类型分析（{} 款，缓存命中跳过，4 路并行）：", todo.len()));
    // 预取每款游戏的名字与全球达成率：DB 读集中在主循环，并行任务里只做 LLM 调用
    let mut jobs: Vec<(u32, String, Vec<String>, HashMap<String, f32>)> = Vec::new();
    for g in &todo {
        // 名称源：玩家成就（玩过）→ 全球成就名（候选）
        let mut names: Vec<String> =
            store.achievements(g.app_id)?.into_iter().map(|a| a.api_name).collect();
        if names.is_empty() {
            names = store.global_achievements(g.app_id)?.into_iter().map(|(n, _)| n).collect();
        }
        if names.is_empty() {
            continue;
        }
        let global_map: HashMap<String, f32> =
            store.global_achievements(g.app_id)?.into_iter().collect();
        jobs.push((g.app_id, g.name.clone(), names, global_map));
    }
    // LLM 并行（chat 自带 429 退避；完成顺序不定，进度行按完成序输出）
    const CONCURRENCY: usize = 4;
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(u32, String, Option<Vec<(String, AchievementCategory)>>, Vec<(String, u64, u64, Option<f64>)>)>(16);
    for (app_id, name, names, global_map) in jobs {
        let llm = LlmClient::clone(llm);
        let sem = sem.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.expect("信号量未关闭");
            let (rows, usage) = analyze_names(&llm, app_id, &names, &global_map).await;
            let _ = tx.send((app_id, name, rows, usage)).await;
        });
    }
    drop(tx);
    // 单消费者串行写库（rusqlite 连接不跨任务共享）
    let mut total = AnalyzeStats::default();
    let mut done = 0usize;
    while let Some((app_id, name, rows, usage)) = rx.recv().await {
        done += 1;
        let mut stats = AnalyzeStats { games: 1, ..Default::default() };
        for (model, pin, pout, cost) in &usage {
            stats.add_call(model, *pin, *pout, *cost);
            store.record_usage("ach_type_analysis", model, *pin, *pout, *cost)?;
        }
        if let Some(rows) = rows {
            store.upsert_achievement_categories(app_id, &rows)?;
        }
        total.merge(stats);
        progress(&format!("  [{:>3}/{}] {}", done, todo.len(), name));
    }
    Ok(total)
}

// ============ 可选增强：LLM 游戏定位（用户开关；一游戏一次、永久缓存） ============

const POSITION_SYSTEM: &str = "你是游戏定位分析器。基于给定的游戏名称、官方类目、用户投票标签和成就样本，\
判断这款游戏主要满足玩家的哪类动机，输出 Bartle 四维分数（0-1）：\
achiever=成就完成（追求完成度/全成就/挑战自我）、explorer=探索发现（世界观/地图/收集与发现）、\
killer=竞争对抗（PvP 竞技/排名/操作对抗）、socializer=社交合作（与朋友合作/社交体验）。\n\
规则：只依据给定信息推断，不要编造；证据必须引用输入内容；四个分数独立打分，不必求和为 1。\
输出严格 JSON：{\"achiever\":0.0,\"explorer\":0.0,\"killer\":0.0,\"socializer\":0.0,\"evidence\":\"一句话\"}";

/// 解析 LLM 定位输出（纯函数）。四维缺失或非法 → None。
pub fn parse_position_response(content: &str) -> Option<(BartleAxes, String)> {
    let text = extract_json_object(content);
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let get = |k: &str| -> Option<f64> {
        v[k].as_f64().map(|x| x.clamp(0.0, 1.0)).filter(|x| !x.is_nan())
    };
    Some((
        BartleAxes {
            achiever: get("achiever")?,
            explorer: get("explorer")?,
            killer: get("killer")?,
            socializer: get("socializer")?,
        },
        v["evidence"].as_str().unwrap_or("").to_string(),
    ))
}

/// 为库内游戏生成 LLM 定位（缺缓存的才生成；非游戏跳过）。design.md §7.5 可选增强③。
pub async fn llm_position_library(
    llm: &LlmClient,
    store: &Store,
    progress: &(dyn Fn(&str) + Send + Sync),
) -> Result<AnalyzeStats> {
    let owned = store.all_owned_games()?;
    let mut todo = Vec::new();
    for g in &owned {
        if store.game_position(g.app_id)?.is_none() {
            let d = store.app_detail(g.app_id)?;
            let tags = store.store_tags(g.app_id)?.unwrap_or_default();
            let is_game = d
                .as_ref()
                .map(|d| {
                    !crate::models::is_non_game_type(&d.app_type)
                        && !crate::models::is_software_only(&d.genres)
                        && !crate::models::is_software_by_tags(&tags)
                })
                .unwrap_or(true);
            if is_game {
                todo.push(g.clone());
            }
        }
    }
    if todo.is_empty() {
        progress("（LLM 定位：全部已缓存）");
        return Ok(AnalyzeStats::default());
    }
    progress(&format!("LLM 定位（{} 款，缓存命中跳过）：", todo.len()));
    let mut total = AnalyzeStats::default();
    for (i, g) in todo.iter().enumerate() {
        let detail = store.app_detail(g.app_id)?;
        let tags = store.store_tags(g.app_id)?.unwrap_or_default();
        let genres: Vec<String> = detail
            .as_ref()
            .map(|d| d.genres.iter().map(|t| t.description.clone()).collect())
            .unwrap_or_default();
        let categories: Vec<String> = detail
            .as_ref()
            .map(|d| d.categories.iter().map(|t| t.description.clone()).collect())
            .unwrap_or_default();
        let global = store.global_achievements(g.app_id)?;
        let sample: Vec<String> = global.iter().take(30).map(|(n, _)| n.clone()).collect();
        let mut cat_dist: BTreeMap<&str, u32> = BTreeMap::new();
        for (_, c) in store.achievement_categories(g.app_id)? {
            *cat_dist.entry(c.as_str()).or_insert(0) += 1;
        }
        let dist_text = if cat_dist.is_empty() {
            String::new()
        } else {
            cat_dist.iter().map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join("、")
        };
        let mut official = genres;
        official.extend(categories);
        let ask = format!(
            "游戏名称：{}\n官方类目：{}\n用户标签：{}\n成就类型分布：{}\n成就样本（内部名，每行一个）：\n{}",
            g.name,
            official.join("、"),
            tags.join("、"),
            if dist_text.is_empty() { "（无）".to_string() } else { dist_text },
            if sample.is_empty() { "（无）".to_string() } else { sample.join("\n") }
        );
        let out = llm
            .chat(&[ChatMessage::system(POSITION_SYSTEM), ChatMessage::user(ask)], 0.2, true, 512)
            .await;
        match out {
            Ok(o) => {
                total.add_call(&o.model, o.usage.prompt_tokens, o.usage.completion_tokens, o.cost_cny);
                store.record_usage("game_position", &o.model, o.usage.prompt_tokens, o.usage.completion_tokens, o.cost_cny)?;
                if let Some((axes, ev)) = parse_position_response(&o.content) {
                    store.upsert_game_position(
                        g.app_id,
                        (axes.achiever, axes.explorer, axes.killer, axes.socializer),
                        &ev,
                    )?;
                } else {
                    tracing::warn!("{}：LLM 定位输出无法解析，跳过", g.name);
                }
            }
            Err(e) => tracing::warn!("{}：LLM 定位失败（跳过）: {e}", g.name),
        }
        total.games += 1;
        progress(&format!("  [{:>3}/{}] {}", i + 1, todo.len(), g.name));
    }
    Ok(total)
}

// ============ 画像计算（确定性，纯读库 + 写注水提议） ============

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GameDepth {
    Unplayed,
    Sampled,
    Active,
    /// 暂离（无终局或叙事中断）：网游/竞技/长线策略（文明、帝国）没有"通关弃坑"概念，
    /// 文字小说/步行模拟读一半是"读到一半"而非"弃坑"。判定 = 自动信号
    /// （is_service_game / is_narrative_game）或手动标注；在线与策略类有 2 小时试玩门槛
    /// （浅尝辄止的仍归"试玩即弃"），叙事类无门槛（开个头放下也是暂离）。
    Service,
    Abandoned,
    /// 已完成：措辞通用（打完主线/读完故事/全成就都算），"通关"对叙事型不贴（v0.36 更名）
    Finished,
}

impl GameDepth {
    pub fn as_str(&self) -> &'static str {
        match self {
            GameDepth::Unplayed => "未开封",
            GameDepth::Sampled => "试玩即弃",
            GameDepth::Active => "活跃中",
            GameDepth::Service => "暂离",
            GameDepth::Abandoned => "弃坑",
            GameDepth::Finished => "已完成",
        }
    }

    /// 全部档位中文（手动标注菜单与取值校验用；与 as_str 一一对应）
    pub fn all_str() -> &'static [&'static str] {
        &["未开封", "试玩即弃", "活跃中", "暂离", "弃坑", "已完成"]
    }

    pub fn parse(s: &str) -> Option<GameDepth> {
        // 兼容旧值："长期在线"→"暂离"（v0.35 当天更名）、"已通关"→"已完成"（v0.36 更名）
        let canon = match s {
            "长期在线" => "暂离",
            "已通关" => "已完成",
            other => other,
        };
        GameDepth::all_str()
            .iter()
            .position(|t| *t == canon)
            .map(|i| match i {
                0 => GameDepth::Unplayed,
                1 => GameDepth::Sampled,
                2 => GameDepth::Active,
                3 => GameDepth::Service,
                4 => GameDepth::Abandoned,
                _ => GameDepth::Finished,
            })
    }
}

/// 暂离信号①（无终局游戏，纯函数，商店用户标签前 8）：
/// 在线对抗类（强词 ≥2 / 免费开玩+多人：雀魂/WarThunder）、策略长线类（≥1：文明/帝国2·4/欧陆/钢4，
/// 真库词形：回合战略/大战略/4X/即时战略）、合作开黑类（在线合作+多人：L4D2/致命公司——
/// 双人成行若命中代价也低：已完成优先级在前）。
/// 调用侧另有 2 小时试玩门槛。
const SERVICE_TAGS: &[&str] = &["大型多人在线", "MMO", "大逃杀", "竞技", "电竞", "玩家对战", "在线对战"];
const LONG_STRATEGY_TAGS: &[&str] = &["回合战略", "大战略", "即时战略", "RTS", "4X"];

pub fn is_service_game(tags: &[String]) -> bool {
    let head: Vec<&String> = tags.iter().take(8).collect();
    let has = |word: &str| head.iter().any(|t| t.contains(word));
    let strong = head
        .iter()
        .filter(|t| SERVICE_TAGS.iter().any(|s| t.contains(s)))
        .count();
    strong >= 2
        || (has("免费开玩") && has("多人"))
        || LONG_STRATEGY_TAGS.iter().any(|s| has(s))
        || (has("在线合作") && has("多人"))
}

/// 暂离信号②（叙事体验类）：视觉小说/步行模拟等——中断不是"弃坑"是"读到一半"，
/// 无试玩门槛（开个头放下也归暂离；读完由成就识别归"已完成"）。
/// "剧情丰富"太宽（大量动作游戏命中）不采用；GTA5 类主线动作不命中（边界上不自动归暂离）。
const NARRATIVE_TAGS: &[&str] = &["视觉小说", "文字游戏", "互动小说", "电影式", "步行模拟"];

pub fn is_narrative_game(tags: &[String]) -> bool {
    tags.iter()
        .take(8)
        .any(|t| NARRATIVE_TAGS.iter().any(|s| t.contains(s)))
}

#[derive(Debug, Clone)]
pub struct GameStat {
    pub app_id: u32,
    pub name: String,
    pub playtime_min: u32,
    pub last_played: Option<u64>,
    #[allow(dead_code)] // P1 画像页逐项证据展示使用
    pub completion: Option<f32>,
    /// 成就总数（<10 时 completion 为统计噪音，展示层据此隐藏）
    pub total_achievements: u32,
    pub depth: GameDepth,
    /// 计入画像统计（玩过、type==game、未被注水剔除）
    pub effective: bool,
    /// 剔除原因（非游戏 / 注水），None = 未剔除
    pub exclusion: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct BartleAxes {
    pub achiever: f64,
    pub explorer: f64,
    pub killer: f64,
    pub socializer: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct BehaviorTraits {
    pub activity: f64,
    pub depth: f64,
    pub breadth: f64,
    pub typical_session_min: u32,
    pub backlog_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct PlayerProfile {
    pub total_games: usize,
    pub effective_playtime_min: u64,
    pub axes: BartleAxes,
    pub behavior: BehaviorTraits,
    pub depth_counts: BTreeMap<GameDepth, usize>,
    pub tag_weights: BTreeMap<String, f64>,
    #[allow(dead_code)] // P1 画像页展示使用
    pub rare_achievements_owned: u32,
    pub games: Vec<GameStat>,
    pub evidence: Vec<String>,
    /// 疑似注水提议：(app_id, 名称, 证据)
    pub idle_proposals: Vec<(u32, String, String)>,
}

/// 识别异常（库存页异常管理区，显式提示哪个游戏的自动识别可能有问题）：
/// - no_detail：商店详情拉取失败（type=unknown：合集包/下架/风控）——定位、标签、存储全缺，可自动修复
/// - no_ach：玩过但无成就数据（成就拉取失败，如传送门2 案例）——无法自动判"已完成"，可自动修复
/// - suspect_finished：≥6 小时且 ≥90 天彻底没玩、又不是长线/暂离类——大概率是玩完了。
///   门槛校准：20h 时传送门2（6.5h 通关单机、合作成就没做、结局成就未被 LLM 分到 completion 类）
///   够不着；降到 6h 后全库候选 ~7 项（含 Hades 等真弃坑），由「没玩完」一键忽略，
///   忽略标记（anomaly_done）持久化、不再打扰。
///   v0.46 扩到「暂离」档：叙事类游戏（通关但成就覆盖不足的漏网形态）自动判完成
///   失败时落暂离而非弃坑，旧条件对它们永远沉默——只能逐个翻深度角标手动标注。
///   现在"弃坑/暂离"两档都提示，配合异常清单的「全部已玩完」一键批量标注。
/// 已有手动深度标注的游戏视为已处理，不再提示。
pub fn detect_anomalies(store: &Store, cfg: &Config) -> Result<Vec<(u32, String, String, String)>> {
    let profile = compute(store, cfg)?;
    let overridden: std::collections::HashSet<u32> = store
        .overrides()?
        .into_iter()
        .filter(|(_, kind, _)| kind == "depth")
        .map(|(id, _, _)| id)
        .collect();
    // 「没玩完」= 用户确认不是漏识别 → 忽略提示（不改深度，保持自动判定）
    let anomaly_done: std::collections::HashSet<u32> = store
        .overrides()?
        .into_iter()
        .filter(|(_, kind, _)| kind == "anomaly_done")
        .map(|(id, _, _)| id)
        .collect();
    let skipped: std::collections::HashSet<u32> =
        store.skipped_apps()?.into_iter().map(|(id, _)| id).collect();
    let now = now_secs();
    let mut out = Vec::new();
    for g in &profile.games {
        if overridden.contains(&g.app_id) || anomaly_done.contains(&g.app_id) {
            continue;
        }
        // 已剔除（非游戏/注水）不做深度判定，不进识别异常（Aseprite/tModLoader 类）
        if g.exclusion.is_some() {
            continue;
        }
        let detail = store.app_detail(g.app_id)?;
        if detail.as_ref().map(|d| d.app_type == "unknown").unwrap_or(false) {
            out.push((
                g.app_id,
                g.name.clone(),
                "no_detail".into(),
                "商店详情拉取失败（合集包/下架/风控），品类定位与标签缺失——可尝试自动修复".into(),
            ));
            continue;
        }
        if g.playtime_min == 0 {
            continue;
        }
        // 无成就数据：全球完成度表有数据说明游戏确有成就系统，玩家侧却为空——无论是否被标
        // skip（Steam 说"无成就或不可用"的档案也会这样返回）都是可疑的拉取失败
        if g.playtime_min >= 30 && g.total_achievements == 0 && {
            let global_known = store.global_count(g.app_id).unwrap_or(0) > 0;
            global_known || !skipped.contains(&g.app_id)
        } {
            out.push((
                g.app_id,
                g.name.clone(),
                "no_ach".into(),
                format!(
                    "玩过 {} 小时但没有成就数据，无法自动判断是否已完成（成就拉取失败？）——可尝试自动修复",
                    g.playtime_min / 60
                ),
            ));
            continue;
        }
        let days_since = g
            .last_played
            .map(|t| (now.saturating_sub(t)) / 86_400)
            .unwrap_or(u64::MAX);
        // 成就类型标注缺失：成就已拉到但 LLM 类型分析没覆盖（分析中断/失败遗留）——
        // 深度判定与成就维评分缺信号。0 成就游戏（无成就系统）不在此列。
        if g.total_achievements > 0 {
            let cats = store.categorized_count(g.app_id).unwrap_or(0);
            let ach = store.achievement_count(g.app_id).unwrap_or(0);
            if ach > cats {
                out.push((
                    g.app_id,
                    g.name.clone(),
                    "ach_uncat".into(),
                    format!(
                        "成就已拉取但类型标注不全（{cats}/{ach}）——深度判定与成就维评分缺信号，可尝试自动修复"
                    ),
                ));
                continue;
            }
        }
        // 弃坑/暂离两档都提示（v0.46）：长线/叙事类的"暂离"里藏着通关但成就信号不足的漏网形态
        if matches!(g.depth, GameDepth::Abandoned | GameDepth::Service)
            && g.playtime_min >= 360
            && days_since >= 90
        {
            out.push((
                g.app_id,
                g.name.clone(),
                "suspect_finished".into(),
                format!(
                    "玩了 {} 小时后 {} 天没再碰——大概率已经玩完，只是没被自动识别",
                    g.playtime_min / 60,
                    days_since
                ),
            ));
        }
    }
    // 时长注水嫌疑（机器提议、待确认）：判断类异常，不可自动修复——
    // 注水游戏已照常进推荐（卡片无总时长内容），这里请用户确认/否决；否决后不再提示
    for (id, kind, status, note) in store.annotations()? {
        if kind == "idle_mark" && status == "proposed" {
            if let Some(g) = profile.games.iter().find(|g| g.app_id == id) {
                out.push((
                    id,
                    g.name.clone(),
                    "idle_proposed".into(),
                    format!(
                        "疑似挂卡/挂机注水（{}）——总时长不可信，请确认",
                        note.unwrap_or_else(|| "证据见画像页".into())
                    ),
                ));
            }
        }
    }
    Ok(out)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 加权平均（权重为 0 时返回 None）。
fn weighted_mean(items: &[(f64, u32)]) -> Option<f64> {
    let w: u64 = items.iter().map(|(_, w)| *w as u64).sum();
    if w == 0 {
        return None;
    }
    Some(items.iter().map(|(v, w)| v * *w as f64).sum::<f64>() / w as f64)
}

/// 可用信号加权合成（缺信号的项不参与，权重自动归一）。
fn combine(parts: Vec<(Option<f64>, f64)>) -> f64 {
    let avail: Vec<(f64, f64)> = parts.into_iter().filter_map(|(v, w)| v.map(|v| (v, w))).collect();
    let wsum: f64 = avail.iter().map(|(_, w)| *w).sum();
    if wsum == 0.0 {
        return 0.0;
    }
    avail.iter().map(|(v, w)| v * w).sum::<f64>() / wsum
}

fn median(vals: &mut Vec<f32>) -> Option<f32> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = vals.len();
    Some(if n % 2 == 1 { vals[n / 2] } else { (vals[n / 2 - 1] + vals[n / 2]) / 2.0 })
}

/// 计算玩家画像。会顺带把新的注水提议写入 annotations（已有 confirmed/rejected 的不覆盖）。
pub fn compute(store: &Store, cfg: &Config) -> Result<PlayerProfile> {
    let owned = store.all_owned_games()?;
    let annotations: HashMap<u32, (String, String, Option<String>)> = store
        .annotations()?
        .into_iter()
        .map(|(id, kind, status, note)| (id, (kind, status, note)))
        .collect();
    // 手动覆盖层：kind='game' value='force' → 强制视为游戏（跳过非游戏剔除）；
    // kind='depth' → 手动深度档（覆盖自动判定，无成就游戏标"已通关"的主路径）
    let overrides = store.overrides()?;
    let force_game: std::collections::HashSet<u32> = overrides
        .iter()
        .filter(|(_, kind, v)| kind == "game" && v == "force")
        .map(|(id, _, _)| *id)
        .collect();
    let depth_override: HashMap<u32, GameDepth> = overrides
        .iter()
        .filter(|(_, kind, _)| kind == "depth")
        .filter_map(|(id, _, v)| GameDepth::parse(v).map(|d| (*id, d)))
        .collect();

    let mut games: Vec<GameStat> = Vec::new();
    let mut evidence: Vec<String> = Vec::new();
    let mut idle_proposals = Vec::new();
    let mut depth_counts: BTreeMap<GameDepth, usize> = BTreeMap::new();

    let mut completions: Vec<f32> = Vec::new();
    let mut full_completions = 0usize;
    let mut rare_owned: u32 = 0;
    let mut explore_items: Vec<(f64, u32)> = Vec::new(); // (倾向, 时长权重)
    let mut comp_items: Vec<(f64, u32)> = Vec::new();
    let mut coop_items: Vec<(f64, u32)> = Vec::new();
    let mut genre_minutes: BTreeMap<String, u64> = BTreeMap::new();
    let mut effective_playtime: u64 = 0;
    let mut pvp_minutes: u64 = 0;
    let mut coop_minutes: u64 = 0;
    let now = now_secs();

    for g in &owned {
        let detail = store.app_detail(g.app_id)?;
        let tags = store.store_tags(g.app_id)?.unwrap_or_default();
        // 非游戏判定：type 走明确非游戏白名单（"unknown"=拉取失败哨兵，不当作非游戏证据，
        // 真机反馈曾因此误剔雀魂/潜龙谍影合集）+ 软件类目兜底（genres 全软件）
        // + 用户标签兜底（头部标签以软件为主，PrprLive 类混合类目应用）
        // + 手动覆盖兜底（画像页"这是游戏"永远赢过自动判定）
        let is_game = force_game.contains(&g.app_id)
            || detail
                .as_ref()
                .map(|d| {
                    !crate::models::is_non_game_type(&d.app_type)
                        && !crate::models::is_software_only(&d.genres)
                        && !crate::models::is_software_by_tags(&tags)
                })
                .unwrap_or(true);
        let mut exclusion: Option<String> = None;
        if !is_game {
            exclusion = Some("非游戏软件".into());
        }

        // 成就与完成度
        let achievements = store.achievements(g.app_id)?;
        let total_ach = achievements.len();
        let achieved: HashSet<&str> = achievements.iter().filter(|a| a.achieved).map(|a| a.api_name.as_str()).collect();
        let completion = if total_ach > 0 {
            Some(achieved.len() as f32 / total_ach as f32)
        } else {
            None
        };

        // 注水判定（仅玩过、有成就、有全球数据、未被玩家否决过）
        let annotation = annotations.get(&g.app_id);
        let idle_confirmed = annotation.map(|a| a.1 == "confirmed").unwrap_or(false);
        let idle_rejected = annotation.map(|a| a.1 == "rejected").unwrap_or(false);
        let mut idle_proposed = annotation.map(|a| a.1 == "proposed").unwrap_or(false);
        if !idle_confirmed && !idle_rejected && !idle_proposed
            && g.playtime_min > 0 && total_ach > 0 && exclusion.is_none()
        {
            let global = store.global_achievements(g.app_id)?;
            if !global.is_empty() {
                let easy: Vec<&str> = global
                    .iter()
                    .filter(|(_, p)| *p >= 40.0)
                    .map(|(n, _)| n.as_str())
                    .collect();
                let mut suspicion = 0u8;
                let mut ev = String::new();
                if !easy.is_empty()
                    && g.playtime_min >= cfg.profile.idle_min_minutes
                {
                    let got = easy.iter().filter(|n| achieved.contains(**n)).count();
                    if (got as f64) < cfg.profile.idle_easy_rate * easy.len() as f64 {
                        suspicion += 50;
                        ev.push_str(&format!(
                            "时长 {} 分钟，但全球≥40% 完成度的入门成就仅 {}/{}",
                            g.playtime_min, got, easy.len()
                        ));
                    }
                }
                if let Some(c) = completion {
                    if g.playtime_min >= 1200 && c < 0.10 {
                        suspicion += 30;
                        if !ev.is_empty() {
                            ev.push_str("；");
                        }
                        ev.push_str(&format!("总完成度仅 {:.0}%", c * 100.0));
                    }
                }
                if suspicion >= 50 {
                    store.propose_idle(g.app_id, &ev)?;
                    idle_proposed = true;
                    idle_proposals.push((g.app_id, g.name.clone(), ev));
                }
            }
        } else if idle_proposed {
            // 已有提议但本次未重算（如数据未变）：保留在提议列表供展示
            if let Some((_, _, note)) = annotation {
                let note = note.clone().unwrap_or_else(|| "注水嫌疑".into());
                idle_proposals.push((g.app_id, g.name.clone(), note));
            }
        }
        if idle_confirmed || idle_proposed {
            if exclusion.is_none() {
                exclusion = Some(if idle_confirmed { "注水（已确认）".into() } else { "注水（提议）".into() });
            }
        }

        // 深度分层（design §7.4）。
        // 已通关三条路径：
        //   a) 完成度 ≥80%——仅当成就总数 ≥10 才可信（防 CS2 这类单成就游戏的 100% 误导）；
        //   b) 里程碑通关（C4 复用，"LLM 判类型、代码算分数"）：玩家获得「通关标记类成就
        //      且其全球完成度 ≤ 阈值」即视为触及游戏终点——空洞骑士如一结局、泰拉瑞亚击败
        //      月主这类节点由成就类型分类识别；完成度百分比低（大量可选成就）不再误判弃坑。
        //      注意：通关节点成就在全球通常 20–35% 完成度（最稀有的是彩蛋/全成就挑战，
        //      不能用"最稀有"口径）。
        //   c) 剧情覆盖率（v0.46，非成就党通关路径）：拿到 ≥story_finish_rate 的剧情/通关类
        //      成就即视为完成主线（合作/竞技不计入分母）。真机案例：传送门2 通关局总完成度
        //      仅 35%、结局成就 SHOOT_THE_MOON 被分到 story 且全库无 completion 项——
        //      a/b 两条路径整体失效；但通关者必然拿到几乎全部主线推进成就，覆盖率能兜住。
        //      成就数 <10 项时信号弱（缺一两条即大幅失真），要求全拿。
        let days_since = g.last_played.map(|t| (now.saturating_sub(t)) / 86_400).unwrap_or(u64::MAX);
        let total_ach = achievements.len() as u32;
        let completion_credible = total_ach >= 10;
        let cat_vec = store.achievement_categories(g.app_id)?;
        let cats: HashMap<&str, AchievementCategory> =
            cat_vec.iter().map(|(n, c)| (n.as_str(), *c)).collect();
        let global_vec = store.global_achievements(g.app_id)?;
        let globals: HashMap<&str, f32> =
            global_vec.iter().map(|(n, p)| (n.as_str(), *p)).collect();
        // 里程碑判定加 2 小时门槛：雨世界真机误判——110 分钟达成"生存通行证"（completion 类、
        // 全球 23.5% ≤ 阈值）被判"已完成"，但那只是早期成就不是终点；2 小时以下不可能"完成"一款游戏
        let milestone_finished = g.playtime_min >= 120 && !achieved.is_empty()
            && achieved.iter().any(|a| {
                cats.get(a).copied() == Some(AchievementCategory::CompletionMark)
                    && globals
                        .get(a)
                        .map(|p| *p <= cfg.profile.finished_mark_max_pct as f32)
                        .unwrap_or(false)
            });
        let story_class =
            |c: &AchievementCategory| matches!(c, AchievementCategory::Story | AchievementCategory::CompletionMark);
        let story_total = cats.values().filter(|c| story_class(c)).count();
        let story_got = achieved
            .iter()
            .filter(|a| cats.get(*a).is_some_and(|c| story_class(c)))
            .count();
        let story_need = if story_total >= 10 {
            cfg.profile.story_finish_rate as f32
        } else {
            1.0
        };
        let story_finished = g.playtime_min >= 120
            && story_total >= 5
            && story_got as f32 >= story_need * story_total as f32;
        let depth = if g.playtime_min == 0 {
            GameDepth::Unplayed
        } else if milestone_finished
            || (completion_credible && completion.map(|c| c >= 0.80).unwrap_or(false))
        {
            GameDepth::Finished
        } else if days_since <= 30 {
            // 活跃期先于覆盖率路径：还在玩的不急着判完成，停了才转"已完成"
            GameDepth::Active
        } else if story_finished {
            GameDepth::Finished
        } else if is_narrative_game(&tags)
            || (g.playtime_min >= 120 && is_service_game(&tags))
        {
            // 暂离：叙事类无门槛（读到一半），无终局类有 2 小时试玩门槛（浅尝的仍归"试玩即弃"）
            GameDepth::Service
        } else if g.playtime_min < 120 {
            GameDepth::Sampled
        } else {
            GameDepth::Abandoned
        };
        // 手动深度标注永远赢过自动判定（修正层原则；value 空/auto 已在写入时删除，无需清除分支）
        let depth = depth_override.get(&g.app_id).copied().unwrap_or(depth);
        if is_game {
            *depth_counts.entry(depth).or_insert(0) += 1;
        }

        let effective = g.playtime_min > 0 && exclusion.is_none() && is_game;
        if effective {
            effective_playtime += g.playtime_min as u64;
            // 成就总数过少的游戏（CS2 单成就 100%）不参与完成度统计
            if let Some(c) = completion {
                if completion_credible {
                    completions.push(c);
                    if c >= 0.95 {
                        full_completions += 1;
                    }
                }
            }
            // 品类偏好：优先用户投票标签（分辨率高），无标签退回 genres；非口味类目不计
            for k in crate::recommender::taste_keys(detail.as_ref(), &tags) {
                *genre_minutes.entry(k).or_insert(0) += g.playtime_min as u64;
            }
            if let Some(d) = &detail {
                let pvp = d.categories.iter().any(|c| c.id == 36 || c.description.contains("对战") || c.description.to_uppercase().contains("PVP"));
                let coop = d.categories.iter().any(|c| matches!(c.id, 9 | 38 | 39) || c.description.contains("合作") || c.description.to_uppercase().contains("CO-OP"));
                if pvp {
                    pvp_minutes += g.playtime_min as u64;
                }
                if coop {
                    coop_minutes += g.playtime_min as u64;
                }
            }
            // 稀有成就 + 各类型达成倾向
            let global_vec = store.global_achievements(g.app_id)?;
            let global: HashMap<&str, f32> =
                global_vec.iter().map(|(n, p)| (n.as_str(), *p)).collect();
            for name in &achieved {
                if let Some(p) = global.get(name) {
                    if *p < 10.0 {
                        rare_owned += 1;
                    }
                }
            }
            let cat_vec = store.achievement_categories(g.app_id)?;
            let cats: HashMap<&str, AchievementCategory> =
                cat_vec.iter().map(|(n, c)| (n.as_str(), *c)).collect();
            if !cats.is_empty() {
                let tendency = |set: &[AchievementCategory]| -> Option<f64> {
                    let total = cats.values().filter(|c| set.contains(c)).count();
                    if total == 0 {
                        return None;
                    }
                    let got = achieved
                        .iter()
                        .filter(|n| cats.get(*n).map(|c| set.contains(c)).unwrap_or(false))
                        .count();
                    Some(got as f64 / total as f64)
                };
                if let Some(t) = tendency(&[AchievementCategory::Explore, AchievementCategory::Collect]) {
                    explore_items.push((t, g.playtime_min));
                }
                if let Some(t) = tendency(&[AchievementCategory::Competitive]) {
                    comp_items.push((t, g.playtime_min));
                }
                if let Some(t) = tendency(&[AchievementCategory::Coop]) {
                    coop_items.push((t, g.playtime_min));
                }
            }
        }

        games.push(GameStat {
            app_id: g.app_id,
            name: g.name.clone(),
            playtime_min: g.playtime_min,
            last_played: g.last_played,
            completion,
            total_achievements: total_ach,
            depth,
            effective,
            exclusion,
        });
    }

    let total_games = games.iter().filter(|g| g.exclusion.as_deref() != Some("非游戏软件")).count();

    // ---- 四维 ----
    let with_ach = completions.len();
    let a1 = median(&mut completions).map(|c| c as f64);
    let a2 = if with_ach > 0 { Some(full_completions as f64 / with_ach as f64) } else { None };
    let a3 = Some((rare_owned as f64 / 20.0).min(1.0));
    let achiever = combine(vec![(a1, 0.5), (a2, 0.3), (a3, 0.2)]);

    let e1 = weighted_mean(&explore_items);
    let distinct_genres = genre_minutes.len() as f64;
    let e2 = Some((distinct_genres / 8.0).min(1.0));
    let explorer = combine(vec![(e1, 0.6), (e2, 0.4)]);

    let k1 = weighted_mean(&comp_items);
    let k2 = if effective_playtime > 0 { Some(pvp_minutes as f64 / effective_playtime as f64) } else { None };
    let killer = combine(vec![(k1, 0.6), (k2, 0.4)]);

    let s1 = weighted_mean(&coop_items);
    let s2 = if effective_playtime > 0 { Some(coop_minutes as f64 / effective_playtime as f64) } else { None };
    let socializer = combine(vec![(s1, 0.6), (s2, 0.4)]);

    // ---- 行为特征 ----
    let two_weeks: u64 = owned.iter().map(|g| g.playtime_2weeks_min as u64).sum();
    let activity = (two_weeks as f64 / 600.0).min(1.0);
    let mut mins: Vec<u64> = games
        .iter()
        .filter(|g| g.effective)
        .map(|g| g.playtime_min as u64)
        .collect();
    mins.sort_unstable_by(|a, b| b.cmp(a));
    let top5: u64 = mins.iter().take(5).sum();
    let depth_conc = if effective_playtime > 0 { top5 as f64 / effective_playtime as f64 } else { 0.0 };
    let typical_session_min = if depth_conc >= 0.55 { 120 } else if e2.unwrap_or(0.0) >= 0.6 { 45 } else { 75 };
    let backlog_n = [GameDepth::Unplayed, GameDepth::Sampled, GameDepth::Abandoned]
        .iter()
        .map(|d| depth_counts.get(d).copied().unwrap_or(0))
        .sum::<usize>();
    let backlog_ratio = if total_games > 0 { backlog_n as f64 / total_games as f64 } else { 0.0 };

    // ---- 品类偏好归一化 ----
    let mut tag_weights: BTreeMap<String, f64> = BTreeMap::new();
    if effective_playtime > 0 {
        for (g, m) in &genre_minutes {
            let w = *m as f64 / effective_playtime as f64;
            if w >= 0.02 {
                tag_weights.insert(g.clone(), (w * 1000.0).round() / 1000.0);
            }
        }
    }

    // ---- 证据 ----
    if let Some(a1v) = a1 {
        evidence.push(format!("成就完成度中位数 {:.0}%，全成就（≥95%）{} 款", a1v * 100.0, full_completions));
    }
    if let Some(e1v) = e1 {
        evidence.push(format!("探索/收集类成就时长加权达成率 {:.0}%", e1v * 100.0));
    }
    if let Some(k1v) = k1 {
        evidence.push(format!("竞技类成就达成率 {:.0}%，PvP 类游戏时长占比 {:.0}%", k1v * 100.0, k2.map(|v| v * 100.0).unwrap_or(0.0)));
    }
    if let Some(s1v) = s1 {
        evidence.push(format!("合作类成就达成率 {:.0}%，Co-op 类游戏时长占比 {:.0}%", s1v * 100.0, s2.map(|v| v * 100.0).unwrap_or(0.0)));
    }
    if rare_owned > 0 {
        evidence.push(format!("持有稀有成就（全球 <10%）{rare_owned} 枚"));
    }

    Ok(PlayerProfile {
        total_games,
        effective_playtime_min: effective_playtime,
        axes: BartleAxes { achiever, explorer, killer, socializer },
        behavior: BehaviorTraits {
            activity,
            depth: depth_conc,
            breadth: e2.unwrap_or(0.0),
            typical_session_min,
            backlog_ratio,
        },
        depth_counts,
        tag_weights,
        rare_achievements_owned: rare_owned,
        games,
        evidence,
        idle_proposals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_unexpressive_api_names() {
        assert!(is_expressive("DEFEAT_QUEEN_SLIME"));
        assert!(is_expressive("TIMBER"));
        assert!(!is_expressive("ACHIEVEMENT_113"));
        assert!(!is_expressive("ACH_9"));
        assert!(!is_expressive("12345"));
        assert!(!is_expressive("ab"));
    }

    #[test]
    fn parses_type_response_with_fences_and_unknowns() {
        let requested = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let content = "```json\n{\"A\":\"story\",\"B\":\"challenge\",\"C\":\"nonsense\"}\n```";
        let (map, ok) = parse_type_response(content, &requested);
        assert!(ok);
        assert_eq!(map["A"], AchievementCategory::Story);
        assert_eq!(map["B"], AchievementCategory::Challenge);
        assert_eq!(map["C"], AchievementCategory::Other); // 未知类别 → other
        // 非 JSON 输出 → 全 other 且 ok=false
        let (map, ok) = parse_type_response("抱歉，我无法输出 JSON", &requested);
        assert!(!ok);
        assert!(map.values().all(|v| *v == AchievementCategory::Other));
    }

    #[test]
    fn weighted_combination_handles_missing_signals() {
        // 只有 a1 可用时，结果就是 a1（权重自动归一）
        assert!((combine(vec![(Some(0.4), 0.5), (None, 0.3), (None, 0.2)]) - 0.4).abs() < 1e-9);
        assert_eq!(combine(vec![(None, 0.5), (None, 0.5)]), 0.0);
    }

    #[test]
    fn parses_llm_position_response() {
        let content = "```json\n{\"achiever\":0.8,\"explorer\":0.5,\"killer\":0.1,\"socializer\":0.2,\"evidence\":\"成就以全收集为主\"}\n```";
        let (axes, ev) = parse_position_response(content).unwrap();
        assert!((axes.achiever - 0.8).abs() < 1e-9);
        assert_eq!(ev, "成就以全收集为主");
        // 越界值被钳制到 [0,1]
        let content = "{\"achiever\":1.7,\"explorer\":0.5,\"killer\":0.1,\"socializer\":0.2,\"evidence\":\"\"}";
        let (axes, _) = parse_position_response(content).unwrap();
        assert_eq!(axes.achiever, 1.0);
        // 缺字段 / 非 JSON → None
        assert!(parse_position_response("{\"achiever\":0.5}").is_none());
        assert!(parse_position_response("无法输出").is_none());
    }

    // ===== 非游戏判定：unknown 哨兵不剔除 + force 覆盖（真机反馈：雀魂/MGSV 合集误剔）=====

    fn owned(app_id: u32, playtime_min: u32) -> crate::models::OwnedGame {
        crate::models::OwnedGame {
            app_id,
            name: format!("G{app_id}"),
            playtime_min,
            playtime_2weeks_min: 0,
            last_played: None,
        }
    }

    fn detail_of(app_type: &str) -> crate::models::AppDetail {
        crate::models::AppDetail {
            app_type: app_type.into(),
            genres: vec![crate::models::Tag { id: 1, description: "动作".into() }],
            categories: vec![],
            storage_gb: None,
            platforms: None,
        }
    }

    #[test]
    fn unknown_app_type_is_not_excluded_but_application_is() {
        // "unknown"（appdetails 拉取失败哨兵，sub 合集包/403 风控）不再被当非游戏剔除
        let store = Store::open_in_memory().unwrap();
        store.upsert_owned_games(&[owned(1151640, 300)]).unwrap();
        store.upsert_app_detail(1151640, &detail_of("unknown")).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 1151640).unwrap();
        assert_eq!(g.exclusion, None, "unknown 不应剔除");
        assert!(g.effective, "应计入画像");

        // 明确非游戏值仍剔除
        let store = Store::open_in_memory().unwrap();
        store.upsert_owned_games(&[owned(2, 300)]).unwrap();
        store.upsert_app_detail(2, &detail_of("application")).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 2).unwrap();
        assert_eq!(g.exclusion.as_deref(), Some("非游戏软件"));

        // force 覆盖永远赢：手动"这是游戏"找回被误剔的真游戏
        store.set_override(2, "game", "force").unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 2).unwrap();
        assert_eq!(g.exclusion, None, "force 覆盖应跳过非游戏剔除");
    }

    // ===== 长期在线档（服务型游戏：无"通关/弃坑"概念）=====

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn service_signal_matches_online_games_not_4x_or_coop() {
        // CS2 真库标签序：竞技第 4、电竞第 7（无"免费开玩"标签）→ 前 8 窗口两强词命中
        assert!(is_service_game(&tags(&["第一人称射击", "射击", "多人", "竞技", "动作", "团队导向", "电竞", "战术"])));
        // SF6 真库标签序：玩家对战第 7、竞技第 8
        assert!(is_service_game(&tags(&["2D 格斗", "格斗", "街机", "角色自定义", "多人", "动作", "玩家对战", "竞技"])));
        // 雀魂/WarThunder 式：免费开玩 + 多人
        assert!(is_service_game(&tags(&["免费开玩", "休闲", "牌类", "多人"])));
        assert!(is_service_game(&tags(&["免费开玩", "多人", "载具", "军事", "模拟", "射击"])));
        // MMO：大型多人在线 + MMORPG(contains MMO) → 2 强词
        assert!(is_service_game(&tags(&["大型多人在线", "MMORPG", "多人", "奇幻"])));
        // 文明6 真库标签序：回合战略第 2、大战略第 5、4X 第 8 → 策略长线词命中
        assert!(is_service_game(&tags(&["策略", "回合战略", "多人", "历史", "大战略", "单人", "回合制", "4X"])));
        // 帝国4 真库标签序：即时战略第 2 → 策略长线词命中
        assert!(is_service_game(&tags(&["策略", "即时战略", "基地建设", "多人", "战争", "中世纪", "单人", "资源管理"])));
        // 欧陆风云4/钢铁雄心4：大战略开头
        assert!(is_service_game(&tags(&["大战略", "策略", "历史", "模拟", "多人", "战争"])));
        // L4D2/致命公司式：在线合作 + 多人（买断合作开黑，无强词非免费）
        assert!(is_service_game(&tags(&["合作", "僵尸", "恐怖", "第一人称射击", "多人", "在线合作"])));
        assert!(is_service_game(&tags(&["合作", "恐怖", "多人", "在线合作", "生存", "悬疑", "第一人称"])));
        // GTA5（主线型：有主线可完成，边界上不自动归暂离）
        assert!(!is_service_game(&tags(&["动作", "冒险", "开放世界", "单人玩家", "多人", "第三人称射击", "犯罪", "驾驶"])));
        // Aimlabs：免费开玩但无"多人"标签（训练工具，不判暂离）
        assert!(!is_service_game(&tags(&["第一人称射击", "免费开玩", "射击", "模拟", "动作", "第一人称", "第三人称射击", "软件"])));
        // VA-11 类文字小说：叙事信号（is_narrative_game）而非本函数——见独立测试
        // 无标签 / 标签太少 → 不判
        assert!(!is_service_game(&[]));
        assert!(!is_service_game(&tags(&["免费开玩"])));
    }

    #[test]
    fn narrative_signal_matches_vn_and_walking_sim() {
        // VA-11 HALL A（文字小说）：叙事词命中 → 暂离（读到一半，不是弃坑）
        assert!(is_narrative_game(&tags(&["视觉小说", "剧情丰富", "赛博朋克", "选择取向", "文字游戏", "氛围"])));
        // Edith Finch / Detroit（步行模拟 / 电影式）
        assert!(is_narrative_game(&tags(&["剧情丰富", "氛围", "步行模拟", "悬疑", "第一人称"])));
        assert!(is_narrative_game(&tags(&["选择取向", "剧情丰富", "电影式", "多结局"])));
        // 普通动作/RPG 不命中（"剧情丰富"太宽，不进叙事词表）
        assert!(!is_narrative_game(&tags(&["剧情丰富", "开放世界", "角色扮演", "动作"])));
        assert!(!is_narrative_game(&[]));
    }

    #[test]
    fn idle_online_game_lands_in_service_not_abandoned() {
        // 400 小时、半年没玩的网游 → 长期在线（不是"弃坑"）
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 730,
                name: "CS2".into(),
                playtime_min: 24_000,
                playtime_2weeks_min: 0,
                last_played: Some(now_secs() - 180 * 86_400),
            }])
            .unwrap();
        store.upsert_store_tags(730, &tags(&["免费开玩", "多人", "竞技", "射击"])).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 730).unwrap();
        assert_eq!(g.depth, GameDepth::Service);
        assert_eq!(p.depth_counts.get(&GameDepth::Service), Some(&1));
        assert_eq!(p.depth_counts.get(&GameDepth::Abandoned), None); // 不再计入"弃坑"
    }

    #[test]
    fn manual_depth_override_wins_and_auto_restores() {
        // 无成就游戏自动判不进"已通关"（两条 Finished 路径都要成就）——手动标注是主路径
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 10,
                name: "NoAch".into(),
                playtime_min: 3_000,
                playtime_2weeks_min: 0,
                last_played: Some(now_secs() - 200 * 86_400),
            }])
            .unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 10).unwrap();
        assert_eq!(g.depth, GameDepth::Abandoned, "无成就 + 半年没玩 → 自动判弃坑");

        // 手动标注"已完成"：覆盖自动判定（GameDepth::parse 全档位往返，旧值"已通关"兼容）
        store.set_override(10, "depth", "已完成").unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 10).unwrap();
        assert_eq!(g.depth, GameDepth::Finished);
        assert_eq!(p.depth_counts.get(&GameDepth::Finished), Some(&1));

        // 清除（value 空）→ 恢复自动判定
        store.set_override(10, "depth", "").unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 10).unwrap();
        assert_eq!(g.depth, GameDepth::Abandoned);
    }

    #[test]
    fn depth_str_roundtrip_all_variants() {
        for s in GameDepth::all_str() {
            assert_eq!(GameDepth::parse(s).as_ref().map(GameDepth::as_str), Some(*s));
        }
        assert_eq!(GameDepth::parse("不存在"), None);
        // 更名前的旧值兼容（存量手动标注直接映射）
        assert_eq!(GameDepth::parse("已通关"), Some(GameDepth::Finished));
        assert_eq!(GameDepth::parse("长期在线"), Some(GameDepth::Service));
    }

    #[test]
    fn detect_anomalies_covers_three_kinds() {
        let store = Store::open_in_memory().unwrap();
        let now = now_secs();
        // no_detail：appdetails 失败哨兵
        store.upsert_owned_games(&[owned(1, 300)]).unwrap();
        store.upsert_app_detail(1, &detail_of("unknown")).unwrap();
        // no_ach：玩过但无成就数据（未 skip）
        store.upsert_owned_games(&[owned(2, 600)]).unwrap();
        store.upsert_app_detail(2, &detail_of("game")).unwrap();
        // suspect_finished：30h + 200 天没玩、非暂离类（无标签）、非 Finished
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 3,
                name: "Portal 2".into(),
                playtime_min: 1_800,
                playtime_2weeks_min: 0,
                last_played: Some(now - 200 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(3, &detail_of("game")).unwrap();
        // 有成就但完成度 0（未通关证据），避免落入 no_ach；类型标注补全，避免落入 ach_uncat
        store
            .upsert_achievements(
                3,
                &[crate::models::Achievement { api_name: "A".into(), achieved: false, unlock_time: None }],
            )
            .unwrap();
        store
            .upsert_achievement_categories(3, &[("A".into(), AchievementCategory::Other)])
            .unwrap();
        // 正常游戏：不产生异常
        store.upsert_owned_games(&[owned(4, 60)]).unwrap();
        store.upsert_app_detail(4, &detail_of("game")).unwrap();
        store
            .upsert_achievements(
                4,
                &[crate::models::Achievement { api_name: "A".into(), achieved: false, unlock_time: None }],
            )
            .unwrap();
        store
            .upsert_achievement_categories(4, &[("A".into(), AchievementCategory::Other)])
            .unwrap();

        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        let kinds: Vec<(u32, &str)> = anomalies.iter().map(|(id, _, k, _)| (*id, k.as_str())).collect();
        assert!(kinds.contains(&(1, "no_detail")), "{kinds:?}");
        assert!(kinds.contains(&(2, "no_ach")), "{kinds:?}");
        assert!(kinds.contains(&(3, "suspect_finished")), "{kinds:?}");
        assert!(!kinds.iter().any(|(id, _)| *id == 4), "{kinds:?}");

        // ach_uncat：成就已拉取但类型标注缺失（0 成就游戏不触发）
        store.upsert_owned_games(&[owned(5, 120)]).unwrap();
        store.upsert_app_detail(5, &detail_of("game")).unwrap();
        store
            .upsert_achievements(
                5,
                &[
                    crate::models::Achievement { api_name: "X".into(), achieved: false, unlock_time: None },
                    crate::models::Achievement { api_name: "Y".into(), achieved: false, unlock_time: None },
                ],
            )
            .unwrap();
        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        assert!(anomalies.iter().any(|(id, _, k, _)| *id == 5 && k == "ach_uncat"));
        // 补齐标注后不再提示
        store
            .upsert_achievement_categories(
                5,
                &[("X".into(), AchievementCategory::Other), ("Y".into(), AchievementCategory::Other)],
            )
            .unwrap();
        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        assert!(!anomalies.iter().any(|(id, ..)| *id == 5), "{anomalies:?}");

        // 手动标注后视为已处理，不再提示（suspect_finished → 标已完成）
        store.set_override(3, "depth", "已完成").unwrap();
        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        assert!(!anomalies.iter().any(|(id, ..)| *id == 3));

        // 「没玩完」= 忽略提示（anomaly_done），不改深度、保持自动判定
        store.set_override(3, "depth", "").unwrap(); // 撤销深度标注
        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        assert!(anomalies.iter().any(|(id, ..)| *id == 3));
        store.set_override(3, "anomaly_done", "done").unwrap();
        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        assert!(!anomalies.iter().any(|(id, ..)| *id == 3));
    }

    #[test]
    fn suspect_finished_also_flags_idle_service_games() {
        // v0.46：暂离档也进疑似完成——通关但成就覆盖不足的叙事类漏网形态落"暂离"而非"弃坑"，
        // 旧条件（只查弃坑档）对它们永远沉默，用户只能逐个翻深度角标手动标注
        let store = Store::open_in_memory().unwrap();
        let now = now_secs();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 730,
                name: "CS2".into(),
                playtime_min: 2_000,
                playtime_2weeks_min: 0,
                last_played: Some(now - 200 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(730, &detail_of("game")).unwrap();
        store.upsert_store_tags(730, &tags(&["免费开玩", "多人", "竞技", "射击"])).unwrap();
        // 有成就但完成度 0（未通关证据），避免落入 no_ach；类型标注补全，避免落入 ach_uncat
        store
            .upsert_achievements(
                730,
                &[crate::models::Achievement { api_name: "A".into(), achieved: false, unlock_time: None }],
            )
            .unwrap();
        store
            .upsert_achievement_categories(730, &[("A".into(), AchievementCategory::Other)])
            .unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 730).unwrap();
        assert_eq!(g.depth, GameDepth::Service, "长线类 2000 分钟 → 暂离档");
        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        assert!(
            anomalies.iter().any(|(id, _, k, _)| *id == 730 && k == "suspect_finished"),
            "暂离档 + ≥6h + ≥90 天没玩 → 也应进疑似完成提示"
        );
    }

    #[test]
    fn no_ach_detects_global_known_games_even_if_skipped() {
        // 传送门2 类：成就拉取失败被标 skip（Steam 返回"不可用"），但全球完成度表有数据
        // （游戏确有成就系统）→ 仍应提示 no_ach（旧条件 !skipped 会漏掉）
        let store = Store::open_in_memory().unwrap();
        store.upsert_owned_games(&[owned(620, 390)]).unwrap();
        store.upsert_app_detail(620, &detail_of("game")).unwrap();
        store.mark_skipped(620, "无成就或不可用").unwrap();
        store.upsert_global_achievements(620, &[("ACH_SURVIVE".into(), 23.5)]).unwrap();
        let anomalies = detect_anomalies(&store, &Config::default()).unwrap();
        assert!(anomalies.iter().any(|(id, _, k, _)| *id == 620 && k == "no_ach"));

        // 真·无成就游戏（skip 且全球也无数据）：不提示
        let store2 = Store::open_in_memory().unwrap();
        store2.upsert_owned_games(&[owned(9, 300)]).unwrap();
        store2.upsert_app_detail(9, &detail_of("game")).unwrap();
        store2.mark_skipped(9, "无成就或不可用").unwrap();
        let anomalies = detect_anomalies(&store2, &Config::default()).unwrap();
        assert!(!anomalies.iter().any(|(id, _, k, _)| *id == 9 && k == "no_ach"));
    }

    #[test]
    fn milestone_finish_requires_two_hours_playtime() {
        // 雨世界真机案例：110 分钟达成"生存通行证"（completion 类、全球 23.5% ≤ 阈值）
        // ——不是终点成就，2 小时门槛挡下 → 落"试玩即弃"而非"已完成"
        let store = Store::open_in_memory().unwrap();
        let now = now_secs();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 312520,
                name: "Rain World".into(),
                playtime_min: 110,
                playtime_2weeks_min: 0,
                last_played: Some(now - 400 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(312520, &detail_of("game")).unwrap();
        store
            .upsert_achievements(
                312520,
                &[crate::models::Achievement {
                    api_name: "PassageSurvivor".into(),
                    achieved: true,
                    unlock_time: Some(now - 400 * 86_400),
                }],
            )
            .unwrap();
        store
            .upsert_achievement_categories(
                312520,
                &[("PassageSurvivor".into(), AchievementCategory::CompletionMark)],
            )
            .unwrap();
        store
            .upsert_global_achievements(312520, &[("PassageSurvivor".into(), 23.5)])
            .unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 312520).unwrap();
        assert_eq!(g.depth, GameDepth::Sampled, "110min 不应判已完成");
    }

    /// 构造"剧情类 N 拿 M + 干扰类别"的成就夹具：返回 (成就列表, 分类列表)
    fn story_fixture(
        story_got: usize,
        story_miss: usize,
        challenge: usize,
        coop: usize,
    ) -> (Vec<crate::models::Achievement>, Vec<(String, AchievementCategory)>) {
        let mut achs = Vec::new();
        let mut cats = Vec::new();
        let mut push = |achs: &mut Vec<_>, cats: &mut Vec<_>, name: String, got: bool, c: AchievementCategory| {
            achs.push(crate::models::Achievement {
                api_name: name.clone(),
                achieved: got,
                unlock_time: None,
            });
            cats.push((name, c));
        };
        for i in 0..story_got {
            push(&mut achs, &mut cats, format!("STORY_GOT_{i}"), true, AchievementCategory::Story);
        }
        for i in 0..story_miss {
            push(&mut achs, &mut cats, format!("STORY_MISS_{i}"), false, AchievementCategory::Story);
        }
        for i in 0..challenge {
            push(&mut achs, &mut cats, format!("CHAL_{i}"), false, AchievementCategory::Challenge);
        }
        for i in 0..coop {
            push(&mut achs, &mut cats, format!("COOP_{i}"), false, AchievementCategory::Coop);
        }
        (achs, cats)
    }

    #[test]
    fn story_coverage_finishes_portal2_like_games() {
        // 传送门2 真机形状（v3 重分类后）：20 项 story 拿 17（85%）、4 项合作全没拿、
        // 27 项挑战全没拿——总完成度仅 33% 且全库无 completion 项，a/b 两条老路径都失效，
        // 剧情覆盖率路径兜住（合作/竞技不计入分母，没拿不影响）
        let store = Store::open_in_memory().unwrap();
        let now = now_secs();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 620,
                name: "Portal 2".into(),
                playtime_min: 390,
                playtime_2weeks_min: 0,
                last_played: Some(now - 600 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(620, &detail_of("game")).unwrap();
        let (achs, cats) = story_fixture(17, 3, 27, 4);
        store.upsert_achievements(620, &achs).unwrap();
        store.upsert_achievement_categories(620, &cats).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 620).unwrap();
        assert_eq!(g.depth, GameDepth::Finished, "剧情类 85% 覆盖 + 停玩 600 天 → 已完成");
    }

    #[test]
    fn story_coverage_blocked_while_active_and_small_sets() {
        // ① 85% 覆盖但 30 天内还在玩 → 活跃中（活跃期先于覆盖率路径，停了才转已完成）
        let store = Store::open_in_memory().unwrap();
        let now = now_secs();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 1,
                name: "Active".into(),
                playtime_min: 3_000,
                playtime_2weeks_min: 90,
                last_played: Some(now - 5 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(1, &detail_of("game")).unwrap();
        let (achs, cats) = story_fixture(17, 3, 5, 2);
        store.upsert_achievements(1, &achs).unwrap();
        store.upsert_achievement_categories(1, &cats).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 1).unwrap();
        assert_eq!(g.depth, GameDepth::Active, "还在玩的不急着判完成");

        // ② story 类 <10 项：信号弱，要求全拿。TUNIC 形状（5/6=83%）不判完成
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 2,
                name: "TUNIC".into(),
                playtime_min: 695,
                playtime_2weeks_min: 0,
                last_played: Some(now - 300 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(2, &detail_of("game")).unwrap();
        let (achs, cats) = story_fixture(5, 1, 3, 0);
        store.upsert_achievements(2, &achs).unwrap();
        store.upsert_achievement_categories(2, &cats).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 2).unwrap();
        assert_eq!(g.depth, GameDepth::Abandoned, "5/6 story（小成就集）不足以判完成");

        // ③ 小成就集全拿（6/6）→ 已完成（upsert 按 api_name 合并不清除旧行，换独立库验证）
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 2,
                name: "TUNIC".into(),
                playtime_min: 695,
                playtime_2weeks_min: 0,
                last_played: Some(now - 300 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(2, &detail_of("game")).unwrap();
        let (achs, cats) = story_fixture(6, 0, 3, 0);
        store.upsert_achievements(2, &achs).unwrap();
        store.upsert_achievement_categories(2, &cats).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 2).unwrap();
        assert_eq!(g.depth, GameDepth::Finished, "小成就集全拿 → 已完成");
    }

    #[test]
    fn story_coverage_partial_progress_does_not_finish() {
        // 中途弃坑形状：20 项 story 拿 12（60% < 80%）、挑战全没拿——覆盖不了，仍判弃坑
        let store = Store::open_in_memory().unwrap();
        let now = now_secs();
        store
            .upsert_owned_games(&[crate::models::OwnedGame {
                app_id: 5,
                name: "MidDrop".into(),
                playtime_min: 1_800,
                playtime_2weeks_min: 0,
                last_played: Some(now - 200 * 86_400),
            }])
            .unwrap();
        store.upsert_app_detail(5, &detail_of("game")).unwrap();
        let (achs, cats) = story_fixture(12, 8, 30, 2);
        store.upsert_achievements(5, &achs).unwrap();
        store.upsert_achievement_categories(5, &cats).unwrap();
        let p = compute(&store, &Config::default()).unwrap();
        let g = p.games.iter().find(|g| g.app_id == 5).unwrap();
        assert_eq!(g.depth, GameDepth::Abandoned, "剧情类 60% 覆盖是玩到中途，不是通关");
    }
}
