//! 2a 内部评分推荐（design.md §7.5）：确定性筛选/加权评分/排序，输出带分解的候选。
//! 游戏侧四维 = 静态映射表（tag→Bartle，领域知识固化；AXIS_RULES 为草案，欢迎校对调参）
//!             + 成就类型分布修正；玩家侧四维来自 profiler。

use std::collections::HashMap;

use anyhow::Result;

use crate::config::Config;
use crate::models::{AchievementCategory, AppDetail};
use crate::profiler::{BartleAxes, GameDepth, PlayerProfile};
use crate::store::Store;

pub struct IntentFilters {
    pub tags: Vec<String>,        // 命中任一即可（空 = 不过滤）
    pub exclude_tags: Vec<String>,
    pub max_session_min: Option<u32>,
    pub only_instant: bool,
    pub top_m: u32,
    /// 用户当次心境（轻松/沉浸/成就向/社交…），P0 传给卡片文案
    pub mood: Option<String>,
    /// 明确排除的 app_id（"换一批"排除已展示、用户点名不要的）
    pub exclude_apps: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct ScoreBreakdown {
    pub motivation: f64,
    pub tag: f64,
    pub attainability: f64,
    pub session: f64,
    pub instant: f64,
    pub backlog: f64,
    /// 行为反馈净调整（疲劳惩罚 + 启动加成），计入总分但不进四色推荐点
    pub fatigue: f64,
    pub total: f64,
}

#[derive(Clone)]
pub struct Candidate {
    #[allow(dead_code)] // LLM 里程碑：卡片事实包与 steam:// 启动链接使用
    pub app_id: u32,
    pub name: String,
    #[allow(dead_code)] // LLM 里程碑：卡片文案引用（"弃坑 14 个月"）
    pub depth: &'static str,
    #[allow(dead_code)] // 同上
    pub last_played: Option<u64>,
    pub badge: &'static str, // 立即可玩 / 需更新 / 未安装
    pub genres: Vec<String>,
    pub breakdown: ScoreBreakdown,
    /// 探索位注入（"换个口味"角标）：动机相近、标签未接触的强制席位
    pub explore: bool,
}

/// 探索感与探索模式参数（§7.9①）。
/// randomness 0–1 → softmax 温度；exploration = "没想法"信号触发（提温 + 降即玩权重 + 1 席探索位）。
pub struct RecommendOpts {
    pub randomness: f64,
    pub exploration: bool,
}

impl Default for RecommendOpts {
    fn default() -> Self {
        RecommendOpts { randomness: 0.0, exploration: false }
    }
}

/// genres → 四维 [ach, exp, kill, soc]，按中/英描述双保险匹配（本地化描述稳定，但字段无 id 规范）。
/// 注意官方 genres 天生偏粗（玩法分类仅约十种）；"独立/免费开玩"等发行方式类目不构成动机信号，
/// 已列入 NON_TASTE_GENRES，不参与匹配。更细的定位信号见成就类型分布与（P1）商店用户投票 tags。
const GENRE_RULES: &[(&[&str], [f64; 4])] = &[
    (&["动作", "Action"], [0.35, 0.30, 0.40, 0.10]),
    (&["冒险", "Adventure"], [0.15, 0.65, 0.05, 0.15]),
    (&["策略", "Strategy"], [0.45, 0.45, 0.30, 0.10]),
    (&["角色扮演", "Role"], [0.45, 0.60, 0.10, 0.20]),
    (&["模拟", "Simulation"], [0.35, 0.45, 0.10, 0.20]),
    (&["休闲", "Casual"], [0.20, 0.30, 0.05, 0.30]),
    (&["体育", "Sports"], [0.25, 0.15, 0.55, 0.30]),
    (&["竞速", "Racing"], [0.25, 0.25, 0.55, 0.15]),
    (&["大型多人在线", "Massively"], [0.45, 0.35, 0.45, 0.55]),
];

/// categories → 四维，按稳定 id 匹配（1 多人、36 在线 PvP、9/38/39 合作、24 分屏、37 跨平台、51 创意工坊）。
const CATEGORY_RULES: &[(i64, [f64; 4])] = &[
    (1, [0.10, 0.10, 0.35, 0.40]),
    (36, [0.10, 0.05, 0.85, 0.20]),
    (9, [0.10, 0.10, 0.05, 0.80]),
    (38, [0.10, 0.10, 0.05, 0.80]),
    (39, [0.10, 0.10, 0.05, 0.80]),
    (24, [0.10, 0.10, 0.25, 0.55]),
    (37, [0.10, 0.10, 0.30, 0.40]),
    (51, [0.30, 0.55, 0.05, 0.20]),
];

/// 商店用户投票标签 → 四维（白名单式：未列出的标签不参与动机映射）。
/// 中文标签来自 l=schinese 页面；contains 匹配以兼容长短变体；每个标签至多命中一条规则
/// （规则顺序"具体在前、泛化在后"，如 大型多人在线 先于 多人）。
/// 这是三源定位中覆盖面的主力（design.md §7.5 来源③），欢迎校对调参。
const TAG_RULES: &[(&[&str], [f64; 4])] = &[
    // ---- 探索 / 世界 ----
    (&["开放世界", "Open World"], [0.25, 0.70, 0.10, 0.15]),
    (&["银河恶魔城", "银河城", "Metroidvania", "类银河"], [0.30, 0.70, 0.00, 0.00]),
    (&["沙盒", "Sandbox"], [0.30, 0.60, 0.10, 0.20]),
    (&["探索", "Exploration"], [0.20, 0.75, 0.00, 0.10]),
    (&["生存", "Survival"], [0.35, 0.50, 0.20, 0.20]),
    (&["建造", "基地建设", "Building", "Base"], [0.35, 0.45, 0.00, 0.20]),
    (&["农场", "种田", "农业", "Farming"], [0.30, 0.40, 0.00, 0.30]),
    (&["制作", "工艺", "Crafting"], [0.40, 0.40, 0.00, 0.20]),
    (&["工厂", "自动化", "Automation"], [0.45, 0.35, 0.00, 0.10]),
    (&["编程", "Programming"], [0.50, 0.35, 0.00, 0.10]),
    (&["城市营造", "经营", "模拟经营", "Management"], [0.40, 0.35, 0.05, 0.25]),
    (&["资源管理", "Resource"], [0.45, 0.35, 0.10, 0.10]),
    (&["太空", "Space"], [0.20, 0.55, 0.10, 0.15]),
    // ---- 剧情 / 体验 ----
    (&["剧情丰富", "Story Rich", "叙事"], [0.20, 0.45, 0.00, 0.15]),
    (&["视觉小说", "恋爱", "爱情", "Visual Novel"], [0.25, 0.35, 0.00, 0.35]),
    (&["选择取向", "多结局", "Choices"], [0.30, 0.40, 0.00, 0.30]),
    (&["心理恐怖", "恐怖", "惊悚", "Horror"], [0.15, 0.45, 0.00, 0.10]),
    (&["氛围", "Atmospheric"], [0.15, 0.50, 0.00, 0.10]),
    (&["治愈", "放松", "Relaxing"], [0.10, 0.30, 0.00, 0.35]),
    (&["家庭", "同乐", "Family"], [0.15, 0.15, 0.05, 0.60]),
    // ---- 挑战 / 成就 ----
    (&["类魂", "魂类", "Souls"], [0.60, 0.30, 0.20, 0.00]),
    (&["困难", "Hard", "Challenging"], [0.60, 0.20, 0.10, 0.00]),
    (&["精确平台", "平台跳跃", "Platformer"], [0.55, 0.40, 0.00, 0.00]),
    (&["速通", "Speedrun"], [0.60, 0.30, 0.00, 0.00]),
    (&["解谜", "Puzzle"], [0.40, 0.55, 0.00, 0.05]),
    (&["Roguelike", "Roguelite", "肉鸽"], [0.45, 0.50, 0.10, 0.05]),
    (&["卡牌", "牌组构建", "Deckbuild", "Card"], [0.50, 0.40, 0.10, 0.10]),
    (&["收集", "Collect"], [0.55, 0.50, 0.00, 0.05]),
    (&["刷宝", "刷装备", "Loot", "刷子"], [0.50, 0.30, 0.00, 0.10]),
    // ---- 竞争对抗 ----
    (&["竞技", "Competitive"], [0.15, 0.05, 0.80, 0.20]),
    (&["玩家对战", "PvP", "对战"], [0.10, 0.00, 0.85, 0.20]),
    (&["第一人称射击", "FPS"], [0.20, 0.15, 0.70, 0.15]),
    (&["第三人称射击", "Third-person"], [0.25, 0.20, 0.65, 0.15]),
    (&["射击", "Shooter"], [0.25, 0.20, 0.60, 0.15]),
    (&["格斗", "Fighting", "FTG"], [0.20, 0.05, 0.80, 0.20]),
    (&["弹幕", "Bullet"], [0.35, 0.15, 0.50, 0.00]),
    (&["即时战略", "RTS"], [0.40, 0.30, 0.45, 0.15]),
    (&["大战略", "Grand Strategy"], [0.50, 0.35, 0.30, 0.10]),
    (&["战术", "Tactics"], [0.45, 0.30, 0.40, 0.10]),
    (&["回合制", "回合策略", "Turn-based"], [0.50, 0.35, 0.20, 0.10]),
    (&["潜行", "Stealth"], [0.35, 0.50, 0.10, 0.00]),
    (&["竞速", "Racing"], [0.25, 0.20, 0.55, 0.15]),
    (&["体育", "Sports"], [0.25, 0.10, 0.55, 0.30]),
    // ---- 社交合作（具体在前）----
    (&["在线合作", "联网合作"], [0.10, 0.10, 0.00, 0.85]),
    (&["合作", "Co-op", "Coop"], [0.10, 0.10, 0.00, 0.85]),
    (&["分屏", "Split"], [0.10, 0.10, 0.15, 0.70]),
    (&["团队", "Team"], [0.15, 0.05, 0.35, 0.60]),
    (&["社交", "Social"], [0.10, 0.10, 0.00, 0.70]),
    (&["派对", "Party"], [0.15, 0.10, 0.10, 0.70]),
    (&["大型多人在线", "MMO"], [0.45, 0.35, 0.45, 0.50]),
    (&["多人", "Multiplayer"], [0.10, 0.10, 0.35, 0.45]),
    // ---- 角色 ----
    (&["动作角色扮演", "ARPG"], [0.40, 0.50, 0.25, 0.10]),
    (&["角色扮演", "RPG", "角色"], [0.45, 0.60, 0.10, 0.20]),
];

/// 品味键：优先商店用户标签（分辨率高），无则退回 genres；过滤非口味类目（发行方式/平台功能）。
pub fn taste_keys(detail: Option<&AppDetail>, tags: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = if tags.is_empty() {
        detail
            .map(|d| d.genres.iter().map(|g| g.description.clone()).collect())
            .unwrap_or_default()
    } else {
        tags.to_vec()
    };
    keys.retain(|k| !crate::models::NON_TASTE_GENRES.iter().any(|s| k.contains(s)));
    keys
}

const NEUTRAL_AXES: [f64; 4] = [0.25, 0.25, 0.25, 0.25];

/// 规则轴：genres + categories + 用户标签命中规则取平均；无命中用中性值。
pub fn tag_axes(detail: Option<&AppDetail>, tags: &[String]) -> [f64; 4] {
    let mut acc = [0.0f64; 4];
    let mut n = 0u32;
    if let Some(d) = detail {
        for g in &d.genres {
            if let Some((_, axes)) = GENRE_RULES
                .iter()
                .find(|(keys, _)| keys.iter().any(|k| g.description.contains(k)))
            {
                for i in 0..4 {
                    acc[i] += axes[i];
                }
                n += 1;
            }
        }
        for c in &d.categories {
            if let Some((_, axes)) = CATEGORY_RULES.iter().find(|(id, _)| *id == c.id) {
                for i in 0..4 {
                    acc[i] += axes[i];
                }
                n += 1;
            }
        }
    }
    for t in tags {
        if let Some((_, axes)) = TAG_RULES
            .iter()
            .find(|(keys, _)| keys.iter().any(|k| t.contains(*k)))
        {
            for i in 0..4 {
                acc[i] += axes[i];
            }
            n += 1;
        }
    }
    if n == 0 {
        return NEUTRAL_AXES;
    }
    for a in &mut acc {
        *a /= n as f64;
    }
    acc
}

/// 成就类型分布 → 四维修正（other 不计入）。
fn category_mix_axes(cats: &[(String, AchievementCategory)]) -> Option<[f64; 4]> {
    let mut axes = [0.0f64; 4];
    let mut n = 0.0f64;
    for (_, c) in cats {
        match c {
            AchievementCategory::Story => axes[1] += 0.3,
            AchievementCategory::Challenge => axes[0] += 0.8,
            AchievementCategory::Collect => {
                axes[1] += 0.6;
                axes[0] += 0.2;
            }
            AchievementCategory::Explore => axes[1] += 0.8,
            AchievementCategory::Competitive => axes[2] += 0.9,
            AchievementCategory::Coop => axes[3] += 0.9,
            AchievementCategory::Grind => axes[0] += 0.6,
            AchievementCategory::CompletionMark => axes[0] += 0.7,
            AchievementCategory::Other => continue,
        }
        n += 1.0;
    }
    if n == 0.0 {
        return None;
    }
    for a in &mut axes {
        *a /= n;
    }
    Some(axes)
}

/// 游戏侧四维（三源融合，design.md §7.5）：
/// 规则轴（genres/categories/用户标签平均）0.6 + 成就类型分布 0.4；
/// 启用 LLM 定位时再与缓存中的 LLM 轴各半融合（可选增强，用户开关）。
pub fn game_axes(
    detail: Option<&AppDetail>,
    tags: &[String],
    cats: &[(String, AchievementCategory)],
    llm_axes: Option<[f64; 4]>,
    use_llm: bool,
) -> [f64; 4] {
    let base = tag_axes(detail, tags);
    let mut out = match category_mix_axes(cats) {
        Some(c) => {
            let mut o = [0.0; 4];
            for i in 0..4 {
                o[i] = 0.6 * base[i] + 0.4 * c[i];
            }
            o
        }
        None => base,
    };
    if use_llm {
        if let Some(l) = llm_axes {
            for i in 0..4 {
                out[i] = 0.5 * out[i] + 0.5 * l[i];
            }
        }
    }
    out
}

fn motivation_match(p: &BartleAxes, g: &[f64; 4]) -> f64 {
    let pv = [p.achiever, p.explorer, p.killer, p.socializer];
    let dist: f64 = (0..4).map(|i| (pv[i] - g[i]).abs()).sum();
    (1.0 - dist / 4.0).clamp(0.0, 1.0)
}

/// 品类同义组（小写、去空格/连字符后匹配）：俗名 ↔ 官方词形互认。
/// 意图解析的 LLM 常把「类 Rogue」说成「肉鸽」、把「类银河战士恶魔城」说成「银河恶魔城」——
/// 没有 同义组时这些口语词永远匹配不上库内官方标签。
const TAG_SYNONYM_GROUPS: &[&[&str]] = &[
    &["肉鸽", "rogue", "roguelike", "roguelite"],
    &["银河恶魔城", "银河战士恶魔城", "银河城", "metroidvania"],
    &["类魂", "魂系", "魂类", "souls", "soulslike"],
    &["丧尸", "僵尸"],
    &["第一人称射击", "fps", "射击"],
    &["moba", "多人在线战术竞技"],
    &["农场", "种田", "农耕"],
    &["赛车", "竞速"],
];

fn norm_tag(s: &str) -> String {
    s.to_lowercase().replace([' ', '\u{3000}', '-'], "")
}

/// 意图标签与品味键匹配：小写归一后的双向 contains + 同义组展开
/// （意图"肉鸽"命中标签"类 Rogue"；意图"Rogue"命中"类 Rogue"；意图"牌组构建"命中"牌组构建式类 Rogue"）
pub fn tags_match(intent_tags: &[String], keys: &[String]) -> bool {
    intent_tags.iter().any(|t| {
        let tn = norm_tag(t);
        // 意图词沾边的同义组整体展开（"肉鸽" → rogue/roguelike/roguelite…）
        let syns: Vec<String> = TAG_SYNONYM_GROUPS
            .iter()
            .filter(|g| g.iter().any(|m| tn.contains(&norm_tag(m))))
            .flat_map(|g| g.iter().map(|m| norm_tag(m)))
            .collect();
        keys.iter().any(|k| {
            let kn = norm_tag(k);
            kn.contains(&tn)
                || tn.contains(&kn)
                || syns.iter().any(|s| kn.contains(s.as_str()))
        })
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 内部评分推荐：候选（未开封/试玩即弃/弃坑，剔除非游戏与注水）→ 硬过滤 → 六分量加权 → Top-M。
/// use_llm = 是否融合 LLM 定位缓存（用户开关，design.md §7.5 可选增强③）。
/// opts = 探索感（softmax 温度）与探索模式开关（§7.9①）；trace 上报探索位命中情况。
pub fn recommend(
    store: &Store,
    cfg: &Config,
    profile: &PlayerProfile,
    intent: &IntentFilters,
    use_llm: bool,
    opts: &RecommendOpts,
    trace: Option<&(dyn Fn(&str) + Sync)>,
) -> Result<Vec<Candidate>> {
    let installed: HashMap<u32, bool> = store.installed_map()?;
    let now = now_secs();
    let w = &cfg.recommender;
    let mut out = Vec::new();
    // 修正层口味排除：跨会话生效（"记住我不喜欢肉鸽"），与意图级 exclude_tags 叠加
    let taste_excludes = store.taste_excludes().unwrap_or_default();
    // 行为反馈（P1-d）：疲劳净调整（惩罚+加成）与即玩自适应权重
    let fatigue = store.fatigue_map().unwrap_or_default();
    let w_ready_eff = w.w_ready * store.install_affinity() * 2.0 * if opts.exploration { 0.5 } else { 1.0 };
    // 探索位"标签未接触"判据素材：画像品类偏好的全部键。
    // 不用 tag 分数判——tag 分是权重求和+clamp，库稍大就在 1.0 饱和（tag=0 永远无解）。
    let weight_keys: Vec<String> = profile.tag_weights.keys().cloned().collect();

    for g in &profile.games {
        if g.exclusion.is_some() {
            continue; // 非游戏 / 注水（v0 简化：注水游戏不进候选，数据不可信）
        }
        // 候选：未开封/试玩即弃/弃坑 + 活跃中（玩家正在玩的也该能被推荐——"接着玩街霸6"）；
        // 仅排除已通关（完成度已兑现）。积压分量会自然压低活跃游戏的排名。
        if matches!(g.depth, GameDepth::Finished) {
            continue;
        }
        let detail = store.app_detail(g.app_id)?;
        let tags = store.store_tags(g.app_id)?.unwrap_or_default();
        // 品味键：用户投票标签优先，genres 兜底（非口味类目已过滤）
        let keys = taste_keys(detail.as_ref(), &tags);

        // 硬过滤（意图级排除 + 修正层口味排除叠加；与正向过滤同口径的双向 contains）
        if !intent.exclude_tags.is_empty() && tags_match(&intent.exclude_tags, &keys) {
            continue;
        }
        if !taste_excludes.is_empty() && tags_match(&taste_excludes, &keys) {
            continue;
        }
        if intent.exclude_apps.contains(&g.app_id) {
            continue;
        }
        // 正向品类过滤：用户点名要某类玩法（如"牌组构建式类 Rogue"）→ 只留品味键命中的
        // 游戏（与排除同口径的双向 contains，"Rogue" 能命中 "类 Rogue"）。全库无命中时
        // agent 侧走「放宽条件」CTA。
        if !intent.tags.is_empty() && !tags_match(&intent.tags, &keys) {
            continue;
        }

        // 即点即玩徽标
        let (badge, instant) = match installed.get(&g.app_id) {
            Some(false) => ("立即可玩", 1.0),
            Some(true) => ("需更新", 0.0),
            None => ("未安装", 0.0),
        };
        if intent.only_instant && badge != "立即可玩" {
            continue;
        }

        // 六分量（游戏侧四维 = 规则轴 + 成就分布 [+ LLM 定位]）
        let cats = store.achievement_categories(g.app_id)?;
        let llm_axes = if use_llm {
            store.game_position(g.app_id)?.map(|(a, _)| a)
        } else {
            None
        };
        let gaxes = game_axes(detail.as_ref(), &tags, &cats, llm_axes, use_llm);
        let motivation = motivation_match(&profile.axes, &gaxes);
        // 标签匹配：画像权重键与候选品味键双向 contains（长短变体兼容）
        let tag = profile
            .tag_weights
            .iter()
            .filter(|(k, _)| {
                keys.iter().any(|c| c.contains(k.as_str()) || k.contains(c.as_str()))
            })
            .map(|(_, v)| *v)
            .sum::<f64>()
            .clamp(0.0, 1.0);
        let globals = store.global_achievements(g.app_id)?;
        let attainability = {
            let mut ps: Vec<f32> = globals.iter().map(|(_, p)| *p).collect();
            if ps.is_empty() {
                0.5 // 无成就数据 → 中性
            } else {
                ps.sort_by(|a, b| a.partial_cmp(b).unwrap());
                (ps[ps.len() / 2] / 100.0) as f64
            }
        };
        let typical = profile.behavior.typical_session_min as f64;
        let session = match intent.max_session_min {
            None => 0.7,
            Some(ms) => {
                let r = ms as f64 / typical;
                if r >= 1.5 { 1.0 } else if r >= 0.75 { 0.8 } else { 0.4 }
            }
        };
        let backlog = match g.depth {
            GameDepth::Unplayed => 0.75,
            GameDepth::Active => 0.0, // 正在玩，不需要"捞回来"的积压分
            GameDepth::Service => 0.3, // 暂离的长期在线游戏：中性恒定，不按闲置天数上涨（"暂离"≠"弃坑"）
            _ => g
                .last_played
                .map(|t| (((now.saturating_sub(t)) / 86_400) as f64 / 540.0).min(1.0))
                .unwrap_or(0.75),
        };

        let (fatigue_penalty, fatigue_boost) = fatigue.get(&g.app_id).copied().unwrap_or((0.0, 0.0));
        let fatigue_adj = fatigue_penalty + fatigue_boost;
        let total = w.w_motiv * motivation
            + w.w_tag * tag
            + w.w_attain * attainability
            + w.w_session * session
            + w_ready_eff * instant
            + w.w_backlog * backlog
            + fatigue_adj;

        out.push(Candidate {
            app_id: g.app_id,
            name: g.name.clone(),
            depth: g.depth.as_str(),
            last_played: g.last_played,
            badge,
            genres: keys.iter().take(6).cloned().collect(),
            breakdown: ScoreBreakdown {
                motivation,
                tag,
                attainability,
                session,
                instant,
                backlog,
                fatigue: fatigue_adj,
                total,
            },
            explore: false,
        });
    }

    if !intent.tags.is_empty() {
        if let Some(tr) = trace {
            tr(&format!("品类过滤：{} 款命中 {:?}", out.len(), intent.tags));
        }
    }
    Ok(rank_and_sample(out, intent.top_m as usize, temperature(opts.randomness, opts.exploration), opts.exploration, cfg.recommender.deterministic, &weight_keys, trace))
}

/// 探索感温度映射：0 → 恒定榜单（不抽样）；0–1 线性到 0.03–0.20；探索模式 ×1.6。
fn temperature(randomness: f64, exploration: bool) -> f64 {
    let r = randomness.clamp(0.0, 1.0);
    if r <= 1e-9 {
        return 0.0;
    }
    let mut t = 0.03 + r * 0.17;
    if exploration {
        t *= 1.6;
    }
    t
}

/// 确定性伪随机（xorshift64*）：不引第三方依赖，固定种子可复现（deterministic 配置/测试）。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (v >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// 榜单抽样（§7.9①）：top-⌈1.5M⌉ 池 softmax(score/T) 加权无放回抽 M 席；
/// T→0 退化为纯 top-M。榜单内按真实分数排序展示（分数永远真实，变的只是入场资格）。
/// 探索模式再强制 1 席探索位（动机相近、标签未接触），替换榜内最低分。
fn rank_and_sample(
    mut scored: Vec<Candidate>,
    m: usize,
    t: f64,
    exploration: bool,
    deterministic: bool,
    weight_keys: &[String],
    trace: Option<&(dyn Fn(&str) + Sync)>,
) -> Vec<Candidate> {
    scored.sort_by(|a, b| b.breakdown.total.partial_cmp(&a.breakdown.total).unwrap());
    if scored.len() <= m {
        return scored; // 小库：全部入榜，无需抽样
    }
    let pool_n = (((m as f64) * 1.5).ceil() as usize).min(scored.len());
    let mut selected: Vec<Candidate> = if t <= 1e-9 {
        scored[..m].to_vec()
    } else {
        let pool = &scored[..pool_n];
        // 数值稳定：先减最大分再取指数（低温时 exp(score/T) 会溢出 inf）
        let max_total = pool.iter().map(|c| c.breakdown.total).fold(0.0_f64, f64::max);
        let weights: Vec<f64> =
            pool.iter().map(|c| ((c.breakdown.total - max_total) / t).exp()).collect();
        let seed = if deterministic {
            42
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x1234_5678)
        };
        let mut rng = Rng::new(seed);
        let mut idxs: Vec<usize> = (0..pool.len()).collect();
        let mut picked: Vec<Candidate> = Vec::with_capacity(m);
        while picked.len() < m && !idxs.is_empty() {
            let sum: f64 = idxs.iter().map(|&i| weights[i]).sum();
            let mut r = rng.next_f64() * sum;
            let mut hit = *idxs.last().unwrap();
            for (k, &i) in idxs.iter().enumerate() {
                r -= weights[i];
                if r <= 0.0 {
                    hit = i;
                    idxs.remove(k);
                    break;
                }
            }
            idxs.retain(|&i| i != hit); // 兜底：浮点边缘 r≈sum 未命中时移除末位
            picked.push(pool[hit].clone());
        }
        picked
    };
    // 探索位：动机相近（≥0.5）、品味键与画像偏好键交集最少的最高分候选，替换榜内最低分。
    // 判据 = 双向 contains 的键交集数（tag 分饱和后不可用）；交集相同取总分高者。
    if exploration && !selected.is_empty() {
        let in_board: Vec<u32> = selected.iter().map(|c| c.app_id).collect();
        let inter_cnt = |c: &Candidate| -> usize {
            c.genres
                .iter()
                .filter(|g| {
                    weight_keys.iter().any(|k| g.contains(k.as_str()) || k.contains(g.as_str()))
                })
                .count()
        };
        let slot = scored
            .iter()
            .filter(|c| !in_board.contains(&c.app_id) && c.breakdown.motivation >= 0.5)
            .max_by(|a, b| {
                inter_cnt(b)
                    .cmp(&inter_cnt(a))
                    .then_with(|| a.breakdown.total.partial_cmp(&b.breakdown.total).unwrap())
            })
            .cloned();
        match slot {
            Some(mut s) => {
                if let Some(trace) = trace {
                    trace(&format!(
                        "探索位：注入《{}》（动机 {:.0}%、与你的品类偏好交集最少）",
                        s.name,
                        s.breakdown.motivation * 100.0
                    ));
                }
                s.explore = true;
                let weakest = selected
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| a.breakdown.total.partial_cmp(&b.breakdown.total).unwrap())
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                selected[weakest] = s;
            }
            None => {
                if let Some(trace) = trace {
                    trace("探索位：库里没有「动机相近」的榜外候选，本批跳过");
                }
            }
        }
    }
    selected.sort_by(|a, b| b.breakdown.total.partial_cmp(&a.breakdown.total).unwrap());
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tag;

    fn detail(genres: &[(&str, i64)], categories: &[(&str, i64)]) -> AppDetail {
        AppDetail {
            app_type: "game".into(),
            genres: genres.iter().map(|(d, id)| Tag { id: *id, description: d.to_string() }).collect(),
            categories: categories.iter().map(|(d, id)| Tag { id: *id, description: d.to_string() }).collect(),
            storage_gb: None,
        }
    }

    #[test]
    fn tags_match_accepts_length_and_language_variants() {
        let keys: Vec<String> = ["牌组构建式类 Rogue", "卡牌游戏", "类 Rogue", "类银河战士恶魔城"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // 意图短词命中长标签（正向 contains）
        assert!(tags_match(&["牌组构建".to_string()], &keys));
        // 英文词命中中文混排标签（反向 contains）
        assert!(tags_match(&["Rogue".to_string()], &keys));
        // 同义组：俗名「肉鸽」命中官方标签「类 Rogue」
        assert!(tags_match(&["肉鸽".to_string()], &keys));
        // 同义组：口语「银河恶魔城」命中官方「类银河战士恶魔城」
        assert!(tags_match(&["银河恶魔城".to_string()], &keys));
        // 多标签任一命中即可
        assert!(tags_match(&["解谜".to_string(), "卡牌".to_string()], &keys));
        // 完全不沾边的意图不命中
        assert!(!tags_match(&["恐怖".to_string()], &keys));
    }

    #[test]
    fn tag_axes_match_genres_and_categories() {
        let d = detail(&[("冒险", 25), ("角色扮演", 3)], &[("在线对战", 36)]);
        let axes = tag_axes(Some(&d), &[]);
        // 冒险+RPG 偏探索，PvP 类目大幅拉高 killer
        assert!(axes[1] > 0.4, "explorer 应显著：{axes:?}");
        assert!(axes[2] > 0.25, "killer 应被 PvP 类目抬高：{axes:?}");
        let neutral = tag_axes(None, &[]);
        assert_eq!(neutral, NEUTRAL_AXES);
    }

    #[test]
    fn tag_axes_use_store_tags() {
        let d = detail(&[("动作", 1)], &[]);
        let tags = vec!["类魂".to_string(), "困难".to_string(), "开放世界".to_string()];
        let axes = tag_axes(Some(&d), &tags);
        assert!(axes[0] > 0.4, "类魂+困难应显著抬高 achiever：{axes:?}");
        assert!(axes[1] > 0.35, "开放世界应抬高 explorer：{axes:?}");
        // 具体规则先于泛化：大型多人在线不应被"多人"规则抢先
        let mmo = tag_axes(None, &["大型多人在线".to_string()]);
        assert!((mmo[0] - 0.45).abs() < 1e-9);
    }

    #[test]
    fn taste_keys_prefer_tags_and_filter_non_taste() {
        let d = detail(&[("动作", 1), ("独立", 23)], &[]);
        // 无标签：genres 兜底，独立被过滤
        let keys = taste_keys(Some(&d), &[]);
        assert_eq!(keys, vec!["动作".to_string()]);
        // 有标签：用标签，抢先体验被过滤
        let keys = taste_keys(Some(&d), &["开放世界".into(), "抢先体验".into()]);
        assert_eq!(keys, vec!["开放世界".to_string()]);
    }

    #[test]
    fn category_mix_shifts_axes() {
        let cats = vec![
            ("A".to_string(), AchievementCategory::Coop),
            ("B".to_string(), AchievementCategory::Other),
        ];
        let axes = category_mix_axes(&cats).unwrap();
        assert!(axes[3] > 0.4);
        assert_eq!(category_mix_axes(&[]), None);
    }

    #[test]
    fn motivation_match_is_zero_for_opposite_axes() {
        let p = BartleAxes { achiever: 0.0, explorer: 0.0, killer: 0.0, socializer: 0.0 };
        let g = [1.0, 1.0, 1.0, 1.0];
        assert!(motivation_match(&p, &g) < 0.01);
        let same = [0.5, 0.5, 0.5, 0.5];
        let p2 = BartleAxes { achiever: 0.5, explorer: 0.5, killer: 0.5, socializer: 0.5 };
        assert!((motivation_match(&p2, &same) - 1.0).abs() < 1e-9);
    }

    // ===== P1-d：探索感抽样与探索位 =====

    fn cand(id: u32, total: f64, tag: f64, motivation: f64) -> Candidate {
        cand_g(id, total, tag, motivation, vec!["动作".into()])
    }

    fn cand_g(id: u32, total: f64, tag: f64, motivation: f64, genres: Vec<String>) -> Candidate {
        Candidate {
            app_id: id,
            name: format!("G{id}"),
            depth: "未开封",
            last_played: None,
            badge: "立即可玩",
            genres,
            breakdown: ScoreBreakdown {
                motivation,
                tag,
                attainability: 0.5,
                session: 0.7,
                instant: 1.0,
                backlog: 0.75,
                fatigue: 0.0,
                total,
            },
            explore: false,
        }
    }

    #[test]
    fn temperature_maps_randomness() {
        assert_eq!(temperature(0.0, false), 0.0); // 0 = 恒定榜单
        let t_half = temperature(0.5, false);
        let t_full = temperature(1.0, false);
        assert!(t_half > 0.0 && t_full > t_half);
        // 探索模式提温
        assert!((temperature(0.5, true) - t_half * 1.6).abs() < 1e-12);
    }

    #[test]
    fn zero_temperature_is_plain_top_m() {
        let scored: Vec<Candidate> = (1..=10).map(|i| cand(i, 1.0 - i as f64 * 0.05, 0.3, 0.6)).collect();
        let out = rank_and_sample(scored, 3, 0.0, false, true, &[], None);
        assert_eq!(out.iter().map(|c| c.app_id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(out.iter().all(|c| !c.explore));
    }

    #[test]
    fn sampling_is_deterministic_with_fixed_seed_and_low_t_converges() {
        let mk = || (1..=10).map(|i| cand(i, 1.0 - i as f64 * 0.04, 0.3, 0.6)).collect();
        // 同一确定性配置（deterministic=true → 种子固定）：两次结果一致
        let a = rank_and_sample(mk(), 3, 0.10, false, true, &[], None);
        let b = rank_and_sample(mk(), 3, 0.10, false, true, &[], None);
        assert_eq!(a.iter().map(|c| c.app_id).collect::<Vec<_>>(), b.iter().map(|c| c.app_id).collect::<Vec<_>>());
        // 榜内始终按真实分数排序
        let totals: Vec<f64> = a.iter().map(|c| c.breakdown.total).collect();
        assert!(totals.windows(2).all(|w| w[0] >= w[1]));
        // 只从 top-1.5M 池抽样：第 6 名以后永不入榜（M=3 → 池 5）
        for _ in 0..20 {
            let out = rank_and_sample(mk(), 3, 0.5, false, true, &[], None);
            assert!(out.iter().all(|c| c.app_id <= 5), "超出池范围: {:?}", out.iter().map(|c| c.app_id).collect::<Vec<_>>());
        }
        // 低温趋近 top-M（权重比极大，抽样退化为贪心）
        let near = rank_and_sample(mk(), 3, 1e-4, false, true, &[], None);
        assert_eq!(near.iter().map(|c| c.app_id).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn exploration_slot_replaces_weakest_with_least_overlap() {
        // 1–4 高分且品味键与画像偏好重叠；50 分低但交集为零、动机相近
        let profile_keys = vec!["动作".to_string()];
        let mut scored: Vec<Candidate> = (1..=4).map(|i| cand(i, 0.9 - i as f64 * 0.05, 0.4, 0.7)).collect();
        scored.push(cand_g(50, 0.55, 0.0, 0.55, vec!["竞速".into()])); // 交集为零：探索位素材
        scored.push(cand_g(60, 0.30, 0.0, 0.2, vec!["竞速".into()])); // 动机太远：不合格
        scored.push(cand_g(70, 0.80, 0.0, 0.7, vec!["动作角色扮演".into()])); // 交集非零（"动作"contains 命中）
        let out = rank_and_sample(scored, 3, 0.0, true, true, &profile_keys, None);
        let slot = out.iter().find(|c| c.explore).expect("应有探索位");
        assert_eq!(slot.app_id, 50); // 交集为零优先于总分更高的 70（交集 1）
        assert_eq!(out.len(), 3); // 席位不变
        assert!(!out.iter().any(|c| c.app_id == 60));
        // 榜内仍按分数排序（探索位分数可能靠后，但真实）
        let totals: Vec<f64> = out.iter().map(|c| c.breakdown.total).collect();
        assert!(totals.windows(2).all(|w| w[0] >= w[1]));
    }
}
