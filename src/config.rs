//! 配置（design.md §7.8 / R3）：config.toml 存参数，.env 存密钥（不进仓库）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_KEY_ENV: &str = "STEAM_WEB_API_KEY";

const DEFAULT_CONFIG_TOML: &str = r#"# 「今晚玩什么」配置文件
# 密钥不要写在这里：放 .env（参考 .env.example），此处只写环境变量名。

[steam]
# Steam 安装目录；留空则自动检测（Windows 注册表 → 常见路径）
install_dir = ""
# Steam Web API Key 从哪个环境变量读取
api_key_env = "STEAM_WEB_API_KEY"

[network]
# 访问 Steam API 的代理，如 "http://127.0.0.1:7897"；
# 留空 = 自动（环境变量 HTTPS_PROXY → Windows 系统代理）
proxy = ""

[agent]
active_llm = "deepseek"
thinking_mode = false
daily_budget_cny = 2.0
# LLM 游戏定位增强（可选）：生成画像/推荐时是否融合 LLM 对游戏定位的判断（缓存复用）
llm_positioning = false

[llm.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-chat"
context_tokens = 65536
# 价格不在此配置：DeepSeek 系模型走内置价格表（官方定价快照 + 高峰/空闲时段自动判断，
# 见 llm.rs builtin_price / beijing_peak）。需要自定义价格时在设置页填，或在此写
# price_input_per_m / price_cache_per_m / price_output_per_m 覆盖（输入+输出齐填生效）。

[profile]
idle_min_minutes = 300
idle_easy_rate = 0.20
# 剧情覆盖率判通关（v0.46）：拿到 ≥80% 的剧情/通关类成就（合作/竞技不计入分母）
# 即视为已完成主线——非成就党通关路径；成就数 <10 的游戏信号弱，要求全拿
story_finish_rate = 0.80

[recommender]
top_m = 10
w_motiv = 0.30
w_tag = 0.20
w_attain = 0.15
w_session = 0.15
w_ready = 0.10
w_backlog = 0.10
# 探索感（0–1）：top-1.5M 池 softmax 抽样的温度来源；0 = 恒定榜单（设置页滑条覆盖此项）
randomness = 0.2
# 固定抽样种子（测试/演示复现；true 时同一库同一评分恒定同榜）
deterministic = false
"#;

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub steam: SteamCfg,
    pub network: NetworkCfg,
    pub llm: BTreeMap<String, LlmProfile>,
    pub agent: AgentCfg,
    pub profile: ProfileCfg,
    pub recommender: RecommenderCfg,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct NetworkCfg {
    pub proxy: Option<String>,
    /// 下载带宽估算（Mbps），用于预约下载的时长提示
    pub download_speed_mbps: f64,
}

impl Default for NetworkCfg {
    fn default() -> Self {
        NetworkCfg { proxy: None, download_speed_mbps: 100.0 }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SteamCfg {
    pub install_dir: Option<String>,
    pub api_key_env: String,
}

impl Default for SteamCfg {
    fn default() -> Self {
        SteamCfg { install_dir: None, api_key_env: DEFAULT_KEY_ENV.to_string() }
    }
}

/// 任意 OpenAI 兼容端点的模型配置（R3：用户可自由切换）。
#[allow(dead_code)] // LLM 里程碑（9.04–9.05）使用
#[derive(Debug, Deserialize, Clone)]
pub struct LlmProfile {
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
    pub context_tokens: Option<u64>,
    pub price_input_per_m: Option<f64>,
    /// 缓存命中输入价（CNY/1M；DeepSeek 约为未命中的 1/30。缺省用内置表，无缓存计费端点可不填）
    pub price_cache_per_m: Option<f64>,
    pub price_output_per_m: Option<f64>,
    pub thinking: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct AgentCfg {
    pub active_llm: Option<String>,
    pub thinking_mode: bool,
    pub daily_budget_cny: f64,
    /// LLM 游戏定位增强（可选，默认关）：用户在生成画像时决定是否启用（CLI --llm-positioning 可临时开启）
    pub llm_positioning: bool,
}

/// 注水提议与通关判定参数（design.md §7.4，待真实数据校准）。
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ProfileCfg {
    pub idle_min_minutes: u32,
    pub idle_easy_rate: f64,
    /// 里程碑通关：玩家获得「通关标记类（CompletionMark）成就」即视为触及游戏终点
    /// （成就类型来自 C4 缓存）；此阈值为该成就的全球完成度百分比上限。
    /// v2 分类收紧后从 35 放宽到 55：v1 时代 35% 是防 story 类混入的补丁；
    /// 现在分类保守（拿不准不给）+ 2 小时 playtime 门槛 + 手动标注兜底，
    /// 可以容纳热门/老游戏的高完成度终点成就（Portal 初代 BEAT_GAME 全球 52% 曾被挡）
    pub finished_mark_max_pct: f64,
    /// 剧情覆盖率判通关（v0.46）：拿到 ≥ 此比例的剧情/通关类成就即视为完成主线。
    /// 非成就党通关路径：通关者会拿到几乎全部主线推进成就，但总完成度可能远低于 80%
    /// （传送门2 真机：35% 完成度、结局成就被分到 story、全库无 completion 项）。
    /// 仅对剧情类成就 ≥10 项的游戏生效；<10 项信号弱，要求全拿。
    pub story_finish_rate: f64,
}

impl Default for ProfileCfg {
    fn default() -> Self {
        ProfileCfg {
            idle_min_minutes: 300,
            idle_easy_rate: 0.20,
            finished_mark_max_pct: 55.0,
            story_finish_rate: 0.80,
        }
    }
}

/// 评分权重（design.md §7.5）。
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RecommenderCfg {
    pub top_m: u32,
    pub w_motiv: f64,
    pub w_tag: f64,
    pub w_attain: f64,
    pub w_session: f64,
    pub w_ready: f64,
    pub w_backlog: f64,
    /// 探索感 0–1：softmax 抽样温度来源（设置页滑条存 meta 覆盖）
    pub randomness: f64,
    /// 固定种子（测试/演示复现）
    pub deterministic: bool,
}

impl Default for RecommenderCfg {
    fn default() -> Self {
        RecommenderCfg {
            top_m: 10,
            w_motiv: 0.30,
            w_tag: 0.20,
            w_attain: 0.15,
            w_session: 0.15,
            w_ready: 0.10,
            w_backlog: 0.10,
            randomness: 0.2,
            deterministic: false,
        }
    }
}

impl Config {
    /// 配置里的 Steam 安装目录（空串视为未设置，走自动检测）。
    pub fn steam_install_dir(&self) -> Option<&str> {
        self.steam
            .install_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// 显式代理配置（空串视为未设置，走自动解析）。
    pub fn proxy(&self) -> Option<&str> {
        self.network
            .proxy
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        std::fs::write(path, DEFAULT_CONFIG_TOML)
            .with_context(|| format!("写入默认配置失败: {}", path.display()))?;
        println!("已生成默认配置 {}（密钥请写入 .env，参考 .env.example）", path.display());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置失败: {}", path.display()))?;
    let mut cfg: Config =
        toml::from_str(&text).with_context(|| format!("解析配置失败: {}", path.display()))?;
    // 迁移：旧版默认配置把 V3 时代的 deepseek 价格（1.0/2.0）直接写进了 [llm.deepseek]，
    // 而 Config 层优先于内置价格表——旧文件不清理的话，切任何 DeepSeek 模型都按旧价计费
    // （真机反馈"v4-pro 计费没同步"的根因）。只清与旧默认完全一致的值（用户自定义不动），
    // 不改写用户文件，每次加载幂等生效。
    if let Some(p) = cfg.llm.get_mut("deepseek") {
        if p.price_input_per_m == Some(1.0)
            && p.price_output_per_m == Some(2.0)
            && p.price_cache_per_m.is_none()
        {
            p.price_input_per_m = None;
            p.price_output_per_m = None;
        }
    }
    Ok(cfg)
}
