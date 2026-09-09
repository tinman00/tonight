//! 核心数据结构（对应 design.md §7.3，随里程碑逐步补齐）。

/// Tier A：库清单条目（GetOwnedGames）
#[derive(Debug, Clone)]
pub struct OwnedGame {
    pub app_id: u32,
    pub name: String,
    pub playtime_min: u32,        // playtime_forever（分钟）
    pub playtime_2weeks_min: u32, // 活跃度信号；近期未玩时接口不返回该字段，记 0
    pub last_played: Option<u64>, // rtime_last_played（unix 秒）；从未玩过为 None
}

/// Tier B：玩家成就条目（GetPlayerAchievements）
#[derive(Debug, Clone)]
pub struct Achievement {
    pub api_name: String,
    pub achieved: bool,
    pub unlock_time: Option<u64>,
}

/// 成就类型（C4：在线 LLM 分析结果，SQLite 永久缓存；一游戏一次，新游戏增量）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AchievementCategory {
    Story,          // 剧情推进
    Challenge,      // 挑战（无伤/速通/高难）
    Collect,        // 收集
    Explore,        // 探索发现
    Competitive,    // 多人竞技
    Coop,           // 合作
    Grind,          // 刷量（重复劳动）
    CompletionMark, // 通关/章节标记
    Other,          // 未分类 / 命名不可表意
}

impl AchievementCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            AchievementCategory::Story => "story",
            AchievementCategory::Challenge => "challenge",
            AchievementCategory::Collect => "collect",
            AchievementCategory::Explore => "explore",
            AchievementCategory::Competitive => "competitive",
            AchievementCategory::Coop => "coop",
            AchievementCategory::Grind => "grind",
            AchievementCategory::CompletionMark => "completion",
            AchievementCategory::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "story" => AchievementCategory::Story,
            "challenge" => AchievementCategory::Challenge,
            "collect" => AchievementCategory::Collect,
            "explore" => AchievementCategory::Explore,
            "competitive" => AchievementCategory::Competitive,
            "coop" => AchievementCategory::Coop,
            "grind" => AchievementCategory::Grind,
            "completion" | "completion_mark" | "completionmark" => AchievementCategory::CompletionMark,
            "other" => AchievementCategory::Other,
            _ => return None,
        })
    }
}

/// 商店侧标签（appdetails 的 genres / categories 条目；id 稳定，description 为本地化文本）
#[derive(Debug, Clone)]
pub struct Tag {
    pub id: i64,
    pub description: String,
}

/// Tier D：应用详情（appdetails，一次性获取长期缓存）
#[derive(Debug, Clone)]
pub struct AppDetail {
    pub app_type: String, // game / dlc / application / hardware / ...
    pub genres: Vec<Tag>,
    pub categories: Vec<Tag>,
    /// 存储空间需求 GB（商店页系统需求区解析；None = 未知）
    pub storage_gb: Option<f64>,
    /// 支持平台（appdetails basic.platforms：windows/mac/linux）；None = 未知（旧缓存，不过滤）
    pub platforms: Option<Vec<String>>,
}

/// 软件类目（Steam 商店的非游戏分类，中/英双保险）。
/// 实测发现 Wallpaper Engine、PrprLive 等工具型应用的 type 字段也是 "game"，
/// 不可靠；但它们的 genres 全部落在软件类目——以此兜底判定非游戏。
pub const SOFTWARE_GENRES: &[&str] = &[
    "实用工具",
    "动画制作和建模",
    "设计和插画",
    "游戏开发",
    "音频制作",
    "视频制作",
    "照片编辑",
    "网络出版",
    "教育",
    "软件培训",
    "Utilities",
    "Animation & Modeling",
    "Design & Illustration",
    "Game Development",
    "Audio Production",
    "Video Production",
    "Photo Editing",
    "Software Training",
    "Education",
    "Web Publishing",
];

/// 商业模式/发行方式/平台功能类目：不是口味信号，不参与品类偏好与匹配。
/// "独立"是制作与发行规模的概念，不构成玩法动机（2026-08-31 与开发者确认）。
/// （引入商店用户标签后，此表同时适用于 genres 与 tags 的过滤。）
pub const NON_TASTE_GENRES: &[&str] = &[
    "免费开玩",
    "抢先体验",
    "独立",
    "单人玩家",
    "单人",
    "好评原声",
    "原声音轨",
    "Steam 成就",
    "Steam 创意工坊",
    "Steam 交易卡",
    "集换式卡牌",
    "支持控制器",
    "控制器支持",
    "Windows",
    "Mac",
    "Linux",
    "Free to Play",
    "Early Access",
    "Indie",
    "Single-player",
    "Great Soundtrack",
    "Steam Achievements",
];

/// genres 非空且全部属于软件类目 → 视为非游戏应用。
pub fn is_software_only(genres: &[Tag]) -> bool {
    !genres.is_empty()
        && genres
            .iter()
            .all(|g| SOFTWARE_GENRES.iter().any(|s| g.description.contains(s)))
}

/// 明确非游戏的 app_type 值（白名单口径）：只有命中这些才按 type 剔除。
/// "unknown" 是 appdetails 拉取失败（403 风控/下架/sub 合集包）的哨兵值，不是"非游戏"证据——
/// 真机反馈曾因此误剔雀魂/潜龙谍影合集。"mod"（tModLoader 类加载器）无独立游戏内容，归非游戏。
/// 库来自 owned_games，误剔真游戏的代价远大于漏过一个工具软件（后者还有 genres/标签两层兜底，
/// 画像页也可手动"这是游戏"）。
pub const NON_GAME_APP_TYPES: &[&str] =
    &["application", "hardware", "music", "series", "video", "episode", "dlc", "mod"];

pub fn is_non_game_type(app_type: &str) -> bool {
    NON_GAME_APP_TYPES.iter().any(|t| app_type.eq_ignore_ascii_case(t))
}

/// 用户标签兜底：前 3 个标签里 ≥2 个属于软件类目 → 视为非游戏。
/// 针对 PrprLive 这类混合类目应用：官方 genres 混有游戏类目，但用户标签头部全是软件标签
/// （2026-08-31 真机发现）。
pub fn is_software_by_tags(tags: &[String]) -> bool {
    if tags.len() < 2 {
        return false;
    }
    let is_sw = |t: &str| SOFTWARE_GENRES.iter().any(|s| t.contains(s)) || t.contains("软件");
    tags.iter().take(3).filter(|t| is_sw(t)).count() >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_only_detection() {
        let software = vec![
            Tag { id: 57, description: "实用工具".into() },
            Tag { id: 51, description: "动画制作和建模".into() },
        ];
        assert!(is_software_only(&software));
        // 混有游戏类目 → 是游戏
        let mixed = vec![
            Tag { id: 57, description: "实用工具".into() },
            Tag { id: 1, description: "动作".into() },
        ];
        assert!(!is_software_only(&mixed));
        // 无类目信息 → 不判软件（交给 type 字段）
        assert!(!is_software_only(&[]));
    }

    #[test]
    fn software_by_tags_detection() {
        // 头部两个软件标签 → 判非游戏（PrprLive 场景）
        let tags = vec!["视频制作".to_string(), "动画制作和建模".to_string(), "动漫".to_string()];
        assert!(is_software_by_tags(&tags));
        // 头部只有一个软件标签 → 游戏含软件功能（正常）
        let tags = vec!["开放世界".to_string(), "实用工具".to_string(), "生存".to_string()];
        assert!(!is_software_by_tags(&tags));
        assert!(!is_software_by_tags(&["实用工具".to_string()]));
    }
}
