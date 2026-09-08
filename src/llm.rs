//! LLM 客户端（design.md C3/C4、R3/R6）。
//! 任意 OpenAI 兼容端点 + usage 捕获 + 三层定价：配置覆盖 > 内置价格表（快照）> 未知仅计 token。

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;

/// CNY / 1M tokens
#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub input_per_m: f64,
    /// 缓存命中输入价（DeepSeek 约为未命中的 1/30）；None = 未知，按全价保守计
    pub cache_input_per_m: Option<f64>,
    pub output_per_m: f64,
}

/// 内置价格表快照日期
pub const PRICE_SNAPSHOT_DATE: &str = "2026-09-06";

/// 内置价格表：DeepSeek 官方定价页「高峰时段」口径（2026-09-06 核准；
/// 空闲时段全场半价，chat() 按北京时间自动判断，见 beijing_peak）。
/// 缓存命中价约为未命中的 1/30——DeepSeek 的 system prompt 前缀缓存命中率很高，
/// 按全价计会显著虚高（真机反馈的"计费有误"主因）。
/// v4-flash-vision-exp 与 v4-flash 同价（官方页明列）；表外模型经 config [llm.*]
/// 或设置页覆盖。本地端点（localhost/127.0.0.1）默认免费。
fn builtin_price(model: &str) -> Option<Price> {
    let m = model.trim().to_ascii_lowercase();
    // (关键字, 输入未命中, 输入命中, 输出) 元 / 百万 tokens，高峰时段
    const TABLE: &[(&str, f64, f64, f64)] = &[
        ("deepseek-chat", 3.0, 0.10, 9.0),
        ("deepseek-v4-flash-vision-exp", 3.0, 0.10, 9.0),
        ("deepseek-v4-flash", 3.0, 0.10, 9.0),
        ("deepseek-reasoner", 9.0, 0.30, 27.0),
        ("deepseek-v4-pro", 9.0, 0.30, 27.0),
    ];
    TABLE
        .iter()
        .find(|(k, _, _, _)| m == *k || m.starts_with(&format!("{k}-")) || m.starts_with(&format!("{k}_")))
        .map(|(_, i, c, o)| Price {
            input_per_m: *i,
            cache_input_per_m: Some(*c),
            output_per_m: *o,
        })
}

/// DeepSeek 高峰时段判断（北京时间 = UTC+8 固定偏移，无夏令时）：
/// 周一至周五 9:00–12:00 与 14:00–18:00 为高峰，其余（含周末、午休）空闲半价。
/// 只对内置表（DeepSeek 系）生效；手动配置的价格按用户填写值原样计。
fn beijing_peak(unix_secs: u64) -> bool {
    let bj = unix_secs + 8 * 3600; // 北京时间当日内秒数
    let day = bj / 86400;
    let hour = (bj % 86400) / 3600;
    // 1970-01-01 是周四；折算成周一=0 的星期序号
    let weekday_mon0 = (day + 3) % 7;
    weekday_mon0 < 5 && ((9..12).contains(&hour) || (14..18).contains(&hour))
}

fn local_free(base_url: &str) -> Option<Price> {
    let b = base_url.to_ascii_lowercase();
    if b.contains("localhost") || b.contains("127.0.0.1") {
        Some(Price { input_per_m: 0.0, cache_input_per_m: Some(0.0), output_per_m: 0.0 })
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: &'static str,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        ChatMessage { role: "system", content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage { role: "user", content: content.into() }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// 缓存命中的输入 tokens（DeepSeek: usage.prompt_cache_hit_tokens；
    /// OpenAI 系: usage.prompt_tokens_details.cached_tokens；缺失 = 0，按全价保守计）
    pub cached_prompt_tokens: u64,
}

pub struct ChatOutput {
    pub content: String,
    pub usage: Usage,
    pub model: String,
    pub cost_cny: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("LLM 未配置：config.toml 需要设置 [agent] active_llm 并提供对应 [llm.*] 段")]
    NotConfigured,
    #[error("LLM 缺少 API Key：请设置环境变量 {0}（可写入 .env）")]
    NoKey(String),
    #[error("LLM HTTP {status}: {msg}")]
    Http { status: u16, msg: String },
    #[error("LLM 网络错误: {0}")]
    Network(String),
    #[error("LLM 响应无内容（可能被内容策略拦截，或模型仅输出了思考过程）")]
    EmptyContent,
}

/// 敏感信息脱敏（错误信息/日志共用，v0.48）：掩码四类形态——
/// ① URL 查询串（部分用户把 key 拼在 base_url 的 `?` 之后，reqwest 错误会带完整 URL）；
/// ② `Bearer <token>`；③ `sk-` 前缀凭证；④ `key=<长凭证>`（Steam 风格）。
/// 只在 ASCII 边界切分，中文正文不受影响。
pub fn redact(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        // ① URL 查询串：当前"词"（自上一个空白起）已含 ://，且此处是 ?
        if b[i] == b'?' {
            let word_start = out.rfind(char::is_whitespace).map(|p| p + 1).unwrap_or(0);
            if out[word_start..].contains("://") {
                let mut j = i + 1;
                while j < b.len()
                    && !matches!(b[j], b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b')')
                    && (b[j] as char) != '）'
                {
                    j += 1;
                }
                out.push_str("?***");
                i = j;
                continue;
            }
        }
        let masked_to = if s[i..].starts_with("Bearer ") || s[i..].starts_with("bearer ") {
            // ② Authorization 头形态
            let mut j = i + 7;
            while j < b.len() && !matches!(b[j], b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b')') {
                j += 1;
            }
            (j >= i + 10).then_some(j) // 至少几位 token 才值得掩
        } else if s[i..].starts_with("sk-") {
            // ③ OpenAI 风格凭证
            let mut j = i + 3;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-' || b[j] == b'_') {
                j += 1;
            }
            (j - i >= 16).then_some(j)
        } else if s[i..].starts_with("key=") {
            // ④ Steam 风格 key=<32位凭证>，泛化为 ≥16 位
            let mut j = i + 4;
            while j < b.len() && b[j].is_ascii_alphanumeric() {
                j += 1;
            }
            (j - i >= 20).then_some(j)
        } else {
            None
        };
        if let Some(j) = masked_to {
            // 保留前缀（Bearer / sk- / key=），其余掩码
            let prefix_end = match () {
                _ if s[i..].starts_with("Bearer ") => 7,
                _ if s[i..].starts_with("bearer ") => 7,
                _ if s[i..].starts_with("sk-") => 3,
                _ => 4, // key=
            };
            out.push_str(&s[i..i + prefix_end]);
            out.push_str("***");
            i = j;
            continue;
        }
        // 普通字符：整字符拷贝（i 只会停在 ASCII 或字符边界）
        let ch = s[i..].chars().next().expect("非空");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

impl LlmError {
    /// 按错误形态给中文排查指引（None = 无特定建议，展示原始错误即可）。
    /// 与 server 侧 friendly_sync_error 对称——LLM 侧此前只有裸技术错误。
    pub fn friendly_hint(&self) -> Option<String> {
        match self {
            LlmError::Http { status: 401, .. } => {
                Some("LLM 返回 401：API Key 无效或未授权，请到服务商控制台核对 Key".into())
            }
            LlmError::Http { status: 402, .. } => {
                Some("LLM 返回 402：账户余额不足，请到服务商控制台充值后重试".into())
            }
            LlmError::Http { status: 403, .. } => {
                Some("LLM 返回 403：Key 无权访问该模型或端点，核对 Key 权限或换模型".into())
            }
            LlmError::Http { status: 404, .. } => {
                Some("LLM 返回 404：多半是 base_url 缺 /v1 或路径多写，请检查服务地址".into())
            }
            LlmError::Http { status: 429, .. } => {
                Some("LLM 返回 429：触发服务商限流（已自动退避重试仍失败），稍等片刻再试".into())
            }
            LlmError::Network(e) if e.contains("timed out") || e.contains("timeout") => {
                Some("LLM 请求超时：检查网络或代理设置后重试".into())
            }
            LlmError::Network(_) => {
                Some("LLM 网络错误：检查网络连通性；大陆网络访问境外端点通常需要代理".into())
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    Builtin,
    Config,
    LocalFree,
    Unknown,
}

/// 价格解析（三层：config 覆盖 > 内置表 > 本地免费；设置页展示用，不依赖 key 是否已配置）
pub fn resolve_price(p: &crate::config::LlmProfile) -> (Option<Price>, PriceSource) {
    match (p.price_input_per_m, p.price_output_per_m) {
        (Some(i), Some(o)) => (
            Some(Price { input_per_m: i, cache_input_per_m: p.price_cache_per_m, output_per_m: o }),
            PriceSource::Config,
        ),
        _ => match builtin_or_local(&p.base_url, &p.model) {
            Some((pr, src)) => (Some(pr), src),
            None => (None, PriceSource::Unknown),
        },
    }
}

/// 内置表 > 本地端点免费（带来源；resolve_price 与界面预填共用同一优先级）
fn builtin_or_local(base_url: &str, model: &str) -> Option<(Price, PriceSource)> {
    builtin_price(model)
        .map(|p| (p, PriceSource::Builtin))
        .or_else(|| local_free(base_url).map(|p| (p, PriceSource::LocalFree)))
}

/// 界面预填用：给定 base_url/model 解析「将生效」的内置价格（不含 meta 覆盖——
/// 覆盖是否适用由 server 侧按模型绑定判断后优先返回）
pub fn peek_builtin_price(base_url: &str, model: &str) -> Option<Price> {
    builtin_or_local(base_url, model).map(|(p, _)| p)
}

/// 拉取服务端可用模型列表（OpenAI 兼容 GET /models；设置页/引导页「获取模型」按钮用）。
/// 短超时独立请求，不复用 chat 的 180s client；错误原样透传服务商 message
/// （如 base_url 缺 /v1 时多数端点 404，前端据此提示）。
pub async fn fetch_models(base_url: &str, api_key: &str) -> Result<Vec<String>, LlmError> {
    let url = format!("{}/models", base_url.trim().trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("tonight/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| LlmError::Network(e.to_string()))?;
    let resp = http
        .get(&url)
        .bearer_auth(api_key.trim())
        .send()
        .await
        .map_err(|e| LlmError::Network(redact(&e.to_string())))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| LlmError::Network(redact(&e.to_string())))?;
    if !(200..300).contains(&status) {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| crate::steam_client::truncate(&text, 200));
        return Err(LlmError::Http { status, msg: redact(&msg) });
    }
    Ok(parse_models_response(&text))
}

/// 解析 OpenAI 兼容 /models 响应 {"data":[{"id":..},..]}；排序去空，缺 data/非 JSON 返回空表
fn parse_models_response(text: &str) -> Vec<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["data"].as_array().cloned())
        .map(|arr| {
            let mut ids: Vec<String> = arr
                .iter()
                .filter_map(|m| m["id"].as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            ids.sort();
            ids.dedup();
            ids
        })
        .unwrap_or_default()
}

/// 解析流式 SSE 的一条 data 载荷（纯函数，便于单测）：
/// 返回本条携带的增量文本（无则为 None）；usage chunk（include_usage 的流末帧）写入 usage。
fn parse_stream_data(data: &str, usage: &mut Option<Usage>) -> Option<String> {
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    if v["usage"].is_object() && v["usage"]["prompt_tokens"].as_u64().is_some() {
        *usage = Some(Usage {
            prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            cached_prompt_tokens: v["usage"]["prompt_cache_hit_tokens"]
                .as_u64()
                .or_else(|| v["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64())
                .unwrap_or(0),
        });
    }
    v["choices"][0]["delta"]["content"]
        .as_str()
        .map(str::to_string)
}

/// 服务商/模型参数约束（快照 2026-09-06，依各家官方文档与社区实测核准）：
/// - Moonshot/Kimi（api.moonshot.cn/.ai）：K2.5+/K3 等新模型显式传 temperature/top_p 会
///   被 400 拒绝（官方文档建议不传）——一律省略，用服务端默认（对旧模型也无害）。
/// - OpenAI 推理系（o1/o3/o4/gpt-5 前缀，不限 host）：temperature 返回 400
///   unsupported_parameter，且 max_tokens 需改用 max_completion_tokens（Azure OpenAI 文档）。
/// - 思考模式全局关闭：本应用所有 LLM 任务（意图解析/卡片/定位）都是结构化短输出，
///   不需要思考。一律发送 thinking:{type:disabled}——DeepSeek V4 等默认思考模型的
///   reasoning_content 计入 max_tokens，会把小预算调用的回答挤成空响应（真机实测：
///   意图解析 512 预算 100% 被思考吃光，content 恒空）；顺带省掉思考 token 费用。
///   服务商不识别该字段而 400 时按错误文本剥离重试；忽略该字段的表外思考模型由
///   chat() 的"空响应加倍预算重试"兜底。
/// - DeepSeek/GLM/MiniMax/Qwen/豆包等其余取值范围覆盖本项目用量（0.1–0.7），无需干预。
/// 其余表外模型由 chat() 的 400 降级重试兜底：错误点名哪个参数就剥离哪个。
#[derive(Debug, Clone, Copy)]
pub struct ParamRule {
    /// 省略 temperature 字段（用服务端默认）
    pub omit_temperature: bool,
    /// max_tokens 语义的实际字段名
    pub max_tokens_field: &'static str,
    /// 界面提示（设置页/引导页展示；None = 无需提示）
    pub notice: Option<&'static str>,
}

const PARAM_DEFAULT: ParamRule =
    ParamRule { omit_temperature: false, max_tokens_field: "max_tokens", notice: None };

pub fn builtin_param_rule(base_url: &str, model: &str) -> ParamRule {
    let host = base_url.to_ascii_lowercase();
    if host.contains("moonshot") || host.contains("kimi") {
        return ParamRule {
            omit_temperature: true,
            max_tokens_field: "max_tokens",
            notice: Some(
                "检测到 Kimi（Moonshot）：其新模型不允许显式传 temperature 等采样参数，已自动适配（省略该参数，用服务端默认）。如遇异常推荐使用 DeepSeek。",
            ),
        };
    }
    let m = model.trim().to_ascii_lowercase();
    let is_openai_reasoning = ["o1", "o3", "o4", "gpt-5"].iter().any(|k| {
        m == *k || m.starts_with(&format!("{k}-")) || m.starts_with(&format!("{k}.")) || m.starts_with(&format!("{k}_"))
    });
    if is_openai_reasoning {
        return ParamRule {
            omit_temperature: true,
            max_tokens_field: "max_completion_tokens",
            notice: Some(
                "检测到 OpenAI 推理系模型：不支持 temperature 等采样参数，已自动适配（省略该参数，输出上限改用 max_completion_tokens 字段）。如遇异常推荐使用 DeepSeek。",
            ),
        };
    }
    PARAM_DEFAULT
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    price: Option<Price>,
    price_source: PriceSource,
}

impl LlmClient {
    pub fn from_config(cfg: &Config) -> Result<Self, LlmError> {
        let name = cfg
            .agent
            .active_llm
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(LlmError::NotConfigured)?;
        let p = cfg.llm.get(name).ok_or(LlmError::NotConfigured)?;
        let api_key = std::env::var(&p.api_key_env)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LlmError::NoKey(p.api_key_env.clone()))?;
        let (price, price_source) = resolve_price(p);
        Ok(LlmClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .user_agent(concat!("tonight/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| LlmError::Network(e.to_string()))?,
            base_url: p.base_url.trim_end_matches('/').to_string(),
            api_key,
            model: p.model.clone(),
            price,
            price_source,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// 生效价格三档（输入未命中 / 输入命中 / 输出），未计价模型返回 None（设置页展示与编辑预填用）
    pub fn price_values(&self) -> Option<(f64, Option<f64>, f64)> {
        self.price.map(|p| (p.input_per_m, p.cache_input_per_m, p.output_per_m))
    }

    /// 定价来源说明（R6 界面展示用）。
    pub fn price_note(&self) -> String {
        match self.price_source {
            PriceSource::Builtin => {
                let cache = self
                    .price
                    .and_then(|p| p.cache_input_per_m)
                    .map(|c| format!("，缓存命中 ¥{c}/M"))
                    .unwrap_or_default();
                format!(
                    "内置价格表（快照 {PRICE_SNAPSHOT_DATE}，按北京时间自动区分高峰/空闲：工作日 9–12、14–18 全价，其余时段半价{cache}；缓存命中部分已按命中价计）"
                )
            }
            PriceSource::Config => "配置覆盖（config.toml / 设置页）".into(),
            PriceSource::LocalFree => "本地端点（免费）".into(),
            PriceSource::Unknown => "未配置价格：仅统计 token，费用显示为空（可在设置页填入价格）".into(),
        }
    }

    /// 构造请求体（chat / chat_stream 共用）：全局 thinking 关闭、参数约束适配、
    /// json_mode；stream / include_usage 仅流式变体置 true。
    fn compose_body(
        &self,
        messages: &[ChatMessage],
        temperature: f32,
        json_out: bool,
        with_temp: bool,
        budget: u32,
        with_thinking: bool,
        stream: bool,
        include_usage: bool,
    ) -> Value {
        let rule = builtin_param_rule(&self.base_url, &self.model);
        let mut body = json!({
            "model": self.model,
            "messages": messages
                .iter()
                .map(|m| json!({"role": m.role, "content": m.content}))
                .collect::<Vec<_>>(),
            "stream": stream,
        });
        if with_temp {
            body["temperature"] = json!(temperature);
        }
        body[rule.max_tokens_field] = json!(budget);
        if with_thinking {
            body["thinking"] = json!({"type": "disabled"});
        }
        if json_out {
            body["response_format"] = json!({"type": "json_object"});
        }
        if stream && include_usage {
            // OpenAI 兼容端点需要显式声明才在流末返回 usage（DeepSeek 同样支持）
            body["stream_options"] = json!({"include_usage": true});
        }
        body
    }

    /// 按当前价格与用量计费（chat / chat_stream 共用）。
    /// DeepSeek 内置表按官方规则区分时段：高峰全价、空闲半价（北京时间自动判断）；
    /// 手动配置的价格（Config/LocalFree）按用户填写值原样计，不做时段折算。
    /// 缓存命中的输入按命中价计（DeepSeek 约为全价的 1/30），缺失缓存价时按全价保守。
    fn cost_of(&self, usage: &Usage) -> Option<f64> {
        let time_mult = if self.price_source == PriceSource::Builtin
            && !beijing_peak(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            ) {
            0.5
        } else {
            1.0
        };
        self.price.map(|p| {
            let cached = usage.cached_prompt_tokens.min(usage.prompt_tokens) as f64;
            let fresh = usage.prompt_tokens as f64 - cached;
            let cache_price = p.cache_input_per_m.unwrap_or(p.input_per_m);
            (fresh / 1e6 * p.input_per_m
                + cached / 1e6 * cache_price
                + usage.completion_tokens as f64 / 1e6 * p.output_per_m)
                * time_mult
        })
    }

    /// chat/completions。json_mode=true 时请求 JSON 输出；端点不支持 response_format 时自动降级重试一次。
    /// 所有调用一律带 thinking:{type:disabled}（本应用的任务都不需要思考；DeepSeek V4 等
    /// 默认思考模型会把小预算的回答挤成空响应）。表外服务商双重兜底：400 按错误文本降级
    /// 重试一次（点名 temperature/thinking/response_format 就剥离对应字段）；200 但 content
    /// 空且有思考痕迹（reasoning_content 或 finish=length，字段被忽略的思考模型）则加倍
    /// max_tokens 重试一次。
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        temperature: f32,
        json_mode: bool,
        max_tokens: u32,
    ) -> Result<ChatOutput, LlmError> {
        let rule = builtin_param_rule(&self.base_url, &self.model);
        let url = format!("{}/chat/completions", self.base_url);
        let build = |json_out: bool, with_temp: bool, budget: u32, with_thinking: bool| {
            self.compose_body(messages, temperature, json_out, with_temp, budget, with_thinking, false, false)
        };
        let (mut status, mut text) =
            self.post(&url, build(json_mode, !rule.omit_temperature, max_tokens, true)).await?;
        if status == 400 {
            let lower = text.to_ascii_lowercase();
            let drop_temp = !rule.omit_temperature && lower.contains("temperature");
            let drop_thinking = lower.contains("thinking");
            // json_mode 下任何 400 都值得去掉 response_format 试一次（原降级行为保留）
            if drop_temp || drop_thinking || json_mode {
                let r = self
                    .post(
                        &url,
                        build(
                            !drop_thinking && json_mode,
                            !drop_temp && !rule.omit_temperature,
                            max_tokens,
                            !drop_thinking,
                        ),
                    )
                    .await?;
                status = r.0;
                text = r.1;
            }
        }
        if !(200..300).contains(&status) {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| crate::steam_client::truncate(&text, 200));
            return Err(LlmError::Http { status, msg: redact(&msg) });
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Network(format!("响应解析失败: {e}")))?;
        let mut content = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        // 空响应自救：思考模型（忽略 thinking 字段的）把 max_tokens 花光在 reasoning 上
        if content.is_empty() {
            let ch = &v["choices"][0];
            let thought = !ch["message"]["reasoning_content"].as_str().unwrap_or_default().is_empty()
                || ch["finish_reason"].as_str() == Some("length");
            if thought && max_tokens < 8192 {
                let budget = (max_tokens.saturating_mul(2)).min(8192);
                let (s2, t2) = self
                    .post(&url, build(json_mode, !rule.omit_temperature, budget, true))
                    .await?;
                if (200..300).contains(&s2) {
                    if let Ok(v2) = serde_json::from_str::<Value>(&t2) {
                        if let Some(c2) = v2["choices"][0]["message"]["content"].as_str() {
                            if !c2.trim().is_empty() {
                                content = c2.trim().to_string();
                            }
                        }
                    }
                }
            }
        }
        if content.is_empty() {
            return Err(LlmError::EmptyContent);
        }
        let usage = Usage {
            prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            cached_prompt_tokens: v["usage"]["prompt_cache_hit_tokens"]
                .as_u64()
                .or_else(|| v["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64())
                .unwrap_or(0),
        };
        let cost = self.cost_of(&usage);
        Ok(ChatOutput { content, usage, model: self.model.clone(), cost_cny: cost })
    }

    /// 流式 chat（推荐卡生成专用，v0.49）：增量文本经 on_delta 实时回调——前端据此
    /// 展示"推荐语 x/3"逐张进度。与 chat() 共用 compose_body（thinking 关闭/参数适配/
    /// json_mode 全沿用）；400 按错误文本剥离重试一次（多一个 stream_options 分支）；
    /// usage 取流末 chunk（stream_options.include_usage），服务商不给则计 0 并告警。
    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        temperature: f32,
        json_mode: bool,
        max_tokens: u32,
        on_delta: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<ChatOutput, LlmError> {
        let rule = builtin_param_rule(&self.base_url, &self.model);
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.compose_body(messages, temperature, json_mode, !rule.omit_temperature, max_tokens, true, true, true);
        let mut resp = self.send_stream(&url, &body).await?;
        let mut status = resp.status().as_u16();
        if status == 400 {
            let text = resp.text().await.unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            let drop_temp = !rule.omit_temperature && lower.contains("temperature");
            let drop_thinking = lower.contains("thinking");
            let drop_so = lower.contains("stream_options");
            if drop_temp || drop_thinking || drop_so || json_mode {
                let retry = self.compose_body(
                    messages,
                    temperature,
                    !drop_thinking && json_mode,
                    !drop_temp && !rule.omit_temperature,
                    max_tokens,
                    !drop_thinking,
                    true,
                    !drop_so,
                );
                resp = self.send_stream(&url, &retry).await?;
                status = resp.status().as_u16();
            } else {
                // 没有可剥离的字段：直接按 400 报错（resp 已被读空）
                let msg = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or_else(|| crate::steam_client::truncate(&text, 200));
                return Err(LlmError::Http { status, msg: redact(&msg) });
            }
        }
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| crate::steam_client::truncate(&text, 200));
            return Err(LlmError::Http { status, msg: redact(&msg) });
        }
        use tokio_stream::StreamExt;
        let mut content = String::new();
        let mut usage: Option<Usage> = None;
        let mut buf = String::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| LlmError::Network(redact(&e.to_string())))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim_end_matches(['\n', '\r']);
                let Some(data) = line.strip_prefix("data:") else { continue };
                if let Some(delta) = parse_stream_data(data.trim(), &mut usage) {
                    content.push_str(&delta);
                    on_delta(&delta);
                }
            }
        }
        if content.trim().is_empty() {
            return Err(LlmError::EmptyContent);
        }
        let usage = usage.unwrap_or_else(|| {
            tracing::warn!("流式响应未返回 usage（服务商可能不支持 stream_options），本次按 0 计");
            Usage { prompt_tokens: 0, completion_tokens: 0, cached_prompt_tokens: 0 }
        });
        let cost = self.cost_of(&usage);
        Ok(ChatOutput { content, usage, model: self.model.clone(), cost_cny: cost })
    }

    /// 流式请求发送（429/5xx 退避重试与 post() 同口径，但成功时把 Response 留给调用方流式消费）
    async fn send_stream(&self, url: &str, body: &Value) -> Result<reqwest::Response, LlmError> {
        let mut attempt = 0u32;
        loop {
            let resp = self
                .http
                .post(url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await
                .map_err(|e| LlmError::Network(redact(&e.to_string())))?;
            let status = resp.status().as_u16();
            if status == 429 || (500..600).contains(&status) {
                attempt += 1;
                let text = resp.text().await.unwrap_or_default();
                if attempt >= 3 {
                    return Err(LlmError::Http {
                        status,
                        msg: redact(&crate::steam_client::truncate(&text, 200)),
                    });
                }
                let wait = Duration::from_millis(1_000u64 << attempt);
                tracing::warn!("LLM HTTP {status}，退避 {:?} 后重试", wait);
                tokio::time::sleep(wait).await;
                continue;
            }
            return Ok(resp);
        }
    }

    async fn post(&self, url: &str, body: Value) -> Result<(u16, String), LlmError> {
        let mut attempt = 0u32;
        loop {
            let resp = self
                .http
                .post(url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| LlmError::Network(redact(&e.to_string())))?;
            let status = resp.status().as_u16();
            let text = resp
                .text()
                .await
                .map_err(|e| LlmError::Network(redact(&e.to_string())))?;
            if status == 429 || (500..600).contains(&status) {
                attempt += 1;
                if attempt >= 3 {
                    return Err(LlmError::Http { status, msg: redact(&crate::steam_client::truncate(&text, 200)) });
                }
                let wait = Duration::from_millis(1_000u64 << attempt);
                tracing::warn!("LLM HTTP {status}，退避 {:?} 后重试", wait);
                tokio::time::sleep(wait).await;
                continue;
            }
            return Ok((status, text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stream_data_extracts_deltas_and_usage() {
        let mut usage = None;
        // 普通增量
        let d = parse_stream_data(r#"{"choices":[{"delta":{"content":"{\"cards\":["}}]}"#, &mut usage).unwrap();
        assert_eq!(d, "{\"cards\":[");
        assert!(usage.is_none());
        // usage 帧（include_usage 的流末 chunk）：choices 为空数组，只带 usage
        let d = parse_stream_data(
            r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":35,"prompt_cache_hit_tokens":80}}"#,
            &mut usage,
        );
        assert!(d.is_none());
        let u = usage.unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens, u.cached_prompt_tokens), (120, 35, 80));
        // OpenAI 风格缓存字段
        let mut usage2 = None;
        parse_stream_data(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}"#,
            &mut usage2,
        );
        assert_eq!(usage2.unwrap().cached_prompt_tokens, 4);
        // DONE / 空行 / 非法 JSON / 无 delta 的帧
        let mut u3 = None;
        assert!(parse_stream_data("[DONE]", &mut u3).is_none());
        assert!(parse_stream_data("", &mut u3).is_none());
        assert!(parse_stream_data("not json", &mut u3).is_none());
        assert!(parse_stream_data(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#, &mut u3).is_none());
        assert!(u3.is_none());
    }

    #[test]
    fn redact_masks_urls_bearers_and_token_forms() {
        // ① base_url 拼 key 的 URL 查询串（reqwest Network 错误的常见形态）
        let s = "error sending request for url (https://api.x.com/v1/chat/completions?key=abcdefghij1234567890)";
        let r = redact(s);
        assert!(r.contains("?***") && r.ends_with(')'), "{r}");
        assert!(!r.contains("abcdefghij"));
        // ② Bearer 头
        let r = redact("header 'Bearer sk-1234567890abcdef1234' invalid");
        assert!(r.contains("Bearer ***"), "{r}");
        assert!(!r.contains("sk-1234567890"));
        // ③ sk- 凭证（≥16 位才掩，短词不误伤）
        let r = redact("key sk-abcdefghijklmnop123456 rejected");
        assert!(r.contains("sk-***"), "{r}");
        assert_eq!(redact("prefix sk-short ok"), "prefix sk-short ok");
        // ④ Steam 风格 key=
        let r = redact("url?key=0123456789ABCDEF0123456789ABCDEF&x=1");
        assert!(r.contains("key=***"), "{r}");
        assert!(!r.contains("0123456789ABCDEF"));
        // 中文正文与非 URL 问号不受影响
        assert_eq!(redact("网络错误：连接被重置？请检查代理"), "网络错误：连接被重置？请检查代理");
        // 无查询串的 URL 原样保留
        assert_eq!(redact("https://api.deepseek.com/v1"), "https://api.deepseek.com/v1");
    }

    #[test]
    fn friendly_hint_maps_common_llm_failures() {
        assert!(LlmError::Http { status: 401, msg: String::new() }.friendly_hint().unwrap().contains("401"));
        assert!(LlmError::Http { status: 402, msg: String::new() }.friendly_hint().unwrap().contains("余额"));
        assert!(LlmError::Http { status: 429, msg: String::new() }.friendly_hint().unwrap().contains("限流"));
        assert!(LlmError::Http { status: 404, msg: String::new() }.friendly_hint().unwrap().contains("/v1"));
        assert!(LlmError::Network("operation timed out".into()).friendly_hint().unwrap().contains("超时"));
        assert!(LlmError::Network("connection reset".into()).friendly_hint().unwrap().contains("网络"));
        assert_eq!(LlmError::EmptyContent.friendly_hint(), None);
    }

    #[test]
    fn builtin_price_matches_models_and_suffixes() {
        let p = builtin_price("deepseek-chat").unwrap();
        assert_eq!(p.input_per_m, 3.0);
        assert_eq!(p.output_per_m, 9.0);
        // 缓存命中价（官方 2026-09-06：命中约为未命中的 1/30）
        assert_eq!(p.cache_input_per_m, Some(0.10));
        let p = builtin_price("DeepSeek-V4-Flash-0731").unwrap();
        assert_eq!(p.output_per_m, 9.0);
        assert_eq!(p.cache_input_per_m, Some(0.10));
        assert_eq!(builtin_price("deepseek-reasoner").unwrap().input_per_m, 9.0);
        assert!(builtin_price("gpt-unknown").is_none());
    }

    #[test]
    fn local_endpoints_are_free() {
        let p = local_free("http://localhost:11434/v1").unwrap();
        assert_eq!(p.input_per_m, 0.0);
        assert_eq!(p.cache_input_per_m, Some(0.0));
        assert!(local_free("https://api.deepseek.com/v1").is_none());
    }

    /// 缓存计费公式：fresh×全价 + cached×命中价 + out×输出价；cached 缺失/超界回退全价
    #[test]
    fn cost_formula_uses_cache_hit_price() {
        let price = Some(Price {
            input_per_m: 3.0,
            cache_input_per_m: Some(0.10),
            output_per_m: 9.0,
        });
        let cost = |usage: Usage| {
            price.map(|p| {
                let cached = usage.cached_prompt_tokens.min(usage.prompt_tokens) as f64;
                let fresh = usage.prompt_tokens as f64 - cached;
                let cache_price = p.cache_input_per_m.unwrap_or(p.input_per_m);
                fresh / 1e6 * p.input_per_m
                    + cached / 1e6 * cache_price
                    + usage.completion_tokens as f64 / 1e6 * p.output_per_m
            })
        };
        // 1M 输入（0.9M 命中）+ 0.5M 输出：0.1×3 + 0.9×0.1 + 0.5×9 = 4.89
        let c = cost(Usage { prompt_tokens: 1_000_000, completion_tokens: 500_000, cached_prompt_tokens: 900_000 }).unwrap();
        assert!((c - 4.89).abs() < 1e-9, "{c}");
        // 全命中 vs 全未命中差 30 倍
        let full_hit = cost(Usage { prompt_tokens: 1_000_000, completion_tokens: 0, cached_prompt_tokens: 1_000_000 }).unwrap();
        let no_hit = cost(Usage { prompt_tokens: 1_000_000, completion_tokens: 0, cached_prompt_tokens: 0 }).unwrap();
        assert!((no_hit / full_hit - 30.0).abs() < 1e-6);
        // cached 超界（脏数据）被 clamp，不产生负数
        let clamped = cost(Usage { prompt_tokens: 1000, completion_tokens: 0, cached_prompt_tokens: 5000 }).unwrap();
        assert!((clamped - 1000.0 / 1e6 * 0.10).abs() < 1e-12);
    }

    #[test]
    fn builtin_price_covers_v4_family_with_official_snapshot() {
        // 2026-09-06 官方定价页核准（高峰口径）：vision-exp 与 v4-flash 同价、pro 与 reasoner 同档
        let flash = builtin_price("deepseek-v4-flash").unwrap();
        let vision = builtin_price("deepseek-v4-flash-vision-exp").unwrap();
        assert_eq!((vision.input_per_m, vision.cache_input_per_m, vision.output_per_m), (3.0, Some(0.10), 9.0));
        assert_eq!(vision.input_per_m, flash.input_per_m);
        let pro = builtin_price("deepseek-v4-pro").unwrap();
        assert_eq!((pro.input_per_m, pro.cache_input_per_m, pro.output_per_m), (9.0, Some(0.30), 27.0));
    }

    #[test]
    fn beijing_peak_follows_deepseek_official_windows() {
        // 硬时间戳经 Python datetime 对照核准（北京时间）：
        // 高峰 = 工作日 9:00–12:00 与 14:00–18:00，其余（午休/早晚/周末）空闲半价
        assert!(beijing_peak(1788746400), "周一 10:00 高峰");
        assert!(!beijing_peak(1788753600), "周一 12:00 午休");
        assert!(!beijing_peak(1788760740), "周一 13:59 午休");
        assert!(beijing_peak(1788760800), "周一 14:00 高峰");
        assert!(!beijing_peak(1788775200), "周一 18:00 峰结束");
        assert!(!beijing_peak(1788660000), "周日 10:00 空闲");
        assert!(!beijing_peak(1788591600), "周六 15:00 空闲");
        assert!(!beijing_peak(1788742740), "周一 08:59 早于峰");
    }

    #[test]
    fn parse_models_response_sorts_and_tolerates_garbage() {
        let std = r#"{"object":"list","data":[{"id":"deepseek-reasoner"},{"id":"deepseek-chat"},{"id":"deepseek-chat"}]}"#;
        assert_eq!(parse_models_response(std), vec!["deepseek-chat", "deepseek-reasoner"]);
        // 缺 data / 非 JSON / 空串 → 空表（前端表现为"0 个模型"而非报错）
        assert!(parse_models_response(r#"{"error":{"message":"nope"}}"#).is_empty());
        assert!(parse_models_response("not json").is_empty());
        assert!(parse_models_response("").is_empty());
        // 空白 id 被过滤
        let ws = r#"{"data":[{"id":"  "},{"id":"m1"}]}"#;
        assert_eq!(parse_models_response(ws), vec!["m1"]);
    }

    #[test]
    fn builtin_param_rule_matches_host_and_model_prefix() {
        // Kimi/Moonshot host：省略 temperature，max_tokens 字段不变
        let kimi = builtin_param_rule("https://api.moonshot.cn/v1", "kimi-k3");
        assert!(kimi.omit_temperature);
        assert_eq!(kimi.max_tokens_field, "max_tokens");
        assert!(kimi.notice.is_some());
        assert!(builtin_param_rule("https://api.moonshot.ai/v1", "kimi-k2.6").omit_temperature);
        // OpenAI 推理系（不限 host）：省略 temperature + 换 max_completion_tokens
        for model in ["o4-mini", "gpt-5.1", "o3", "gpt-5"] {
            let r = builtin_param_rule("https://some-proxy.example.com/v1", model);
            assert!(r.omit_temperature, "{model}");
            assert_eq!(r.max_tokens_field, "max_completion_tokens", "{model}");
        }
        // 无约束：DeepSeek / GLM / 前缀近似的无辜模型（如 "o2" 不存在但防御性区分）
        for (url, model) in [
            ("https://api.deepseek.com/v1", "deepseek-chat"),
            ("https://open.bigmodel.cn/api/paas/v4", "glm-5.3"),
            ("https://x.example.com/v1", "k2-mini"), // 不含 kimi/moonshot host
        ] {
            let r = builtin_param_rule(url, model);
            assert!(!r.omit_temperature, "{model}");
            assert_eq!(r.max_tokens_field, "max_tokens");
            assert!(r.notice.is_none(), "{model}");
        }
    }

    /// 本地 mock chat/completions：请求体带 temperature 即 400（模拟 Kimi/OpenAI 推理系），
    /// 否则 200 返回最小补全。用于验证规则适配与 400 降级重试。
    async fn spawn_mock() -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::response::IntoResponse;
        use axum::routing::post as axum_post;
        use axum::Json as AxJson;
        use axum::Router;

        let bodies: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            axum_post(move |body: String| {
                let seen = seen.clone();
                async move {
                    let v: Value = serde_json::from_str(&body).unwrap();
                    let reject_temp = v.get("temperature").is_some();
                    seen.lock().unwrap().push(v);
                    if reject_temp {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            AxJson(json!({"error": {"message": "Unsupported parameter: 'temperature' is not supported with this model."}})),
                        )
                            .into_response();
                    }
                    (
                        axum::http::StatusCode::OK,
                        AxJson(json!({
                            "choices": [{"message": {"content": " ok "}}],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                        })),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, bodies)
    }

    fn client_for(addr: std::net::SocketAddr, model: &str) -> LlmClient {
        LlmClient {
            http: reqwest::Client::new(),
            base_url: format!("http://{addr}/v1"),
            api_key: "test".into(),
            model: model.into(),
            price: None,
            price_source: PriceSource::Unknown,
        }
    }

    #[tokio::test]
    async fn chat_adapts_param_rules_and_degrades_on_400() {
        let (addr, bodies) = spawn_mock().await;
        let msgs = [ChatMessage::user("hi")];

        // 表内规则（模型前缀匹配，与 host 无关）：gpt-5 用 max_completion_tokens 且不带 temperature
        client_for(addr, "gpt-5.1").chat(&msgs, 0.4, false, 100).await.unwrap();
        // 表外模型：首发带 temperature 被 400，按错误文本剥离后重试成功
        client_for(addr, "future-model").chat(&msgs, 0.4, false, 100).await.unwrap();

        // 注：Kimi 的 host 规则无法对 mock host 生效（rule 按 base_url host 匹配），
        // 其判定已由 builtin_param_rule_matches_host_and_model_prefix 单测覆盖。
        let all = bodies.lock().unwrap();
        assert_eq!(all.len(), 3, "gpt-5×1 + 表外降级×2");
        // 全局关思考：所有请求都带 thinking:disabled
        for (i, b) in all.iter().enumerate() {
            assert_eq!(b["thinking"]["type"], "disabled", "req {i} 应带关思考字段");
        }
        let gpt5 = all[0].as_object().unwrap();
        assert!(!gpt5.contains_key("temperature"));
        assert!(gpt5.contains_key("max_completion_tokens"));
        assert!(!gpt5.contains_key("max_tokens"));
        assert!(all[1].as_object().unwrap().contains_key("temperature"), "表外首发带 temperature");
        assert!(!all[2].as_object().unwrap().contains_key("temperature"), "降级重试已剥离");
        assert_eq!(all[2]["model"], "future-model");
    }

    /// mock：请求体带 thinking 字段即 400（模拟不认识该字段的服务商）
    async fn spawn_mock_rejects_thinking() -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::response::IntoResponse;
        use axum::routing::post as axum_post;
        use axum::Json as AxJson;
        use axum::Router;

        let bodies: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            axum_post(move |body: String| {
                let seen = seen.clone();
                async move {
                    let v: Value = serde_json::from_str(&body).unwrap();
                    let reject_thinking = v.get("thinking").is_some();
                    seen.lock().unwrap().push(v);
                    if reject_thinking {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            AxJson(json!({"error": {"message": "Unrecognized request argument: thinking."}})),
                        )
                            .into_response();
                    }
                    (
                        axum::http::StatusCode::OK,
                        AxJson(json!({
                            "choices": [{"message": {"content": " ok "}}],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                        })),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, bodies)
    }

    #[tokio::test]
    async fn chat_strips_thinking_on_400() {
        let (addr, bodies) = spawn_mock_rejects_thinking().await;
        let msgs = [ChatMessage::user("hi")];
        client_for(addr, "future-model").chat(&msgs, 0.4, false, 100).await.unwrap();
        let all = bodies.lock().unwrap();
        assert_eq!(all.len(), 2, "首发 400 + 剥离 thinking 重试");
        assert!(all[0].as_object().unwrap().contains_key("thinking"), "首发带 thinking");
        assert!(!all[1].as_object().unwrap().contains_key("thinking"), "重试已剥离 thinking");
    }

    /// mock：首次 200 但只有 reasoning_content（思考耗尽预算的思考模型形态），
    /// 再次请求（更大预算）返回正常 content。
    async fn spawn_mock_thinking_eats_budget() -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::response::IntoResponse;
        use axum::routing::post as axum_post;
        use axum::Json as AxJson;
        use axum::Router;

        let bodies: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            axum_post(move |body: String| {
                let seen = seen.clone();
                async move {
                    let v: Value = serde_json::from_str(&body).unwrap();
                    let first = seen.lock().unwrap().is_empty();
                    seen.lock().unwrap().push(v);
                    if first {
                        // 忽略 thinking 字段的思考模型：预算全花在 reasoning 上
                        return (
                            axum::http::StatusCode::OK,
                            AxJson(json!({
                                "choices": [{
                                    "message": {"role": "assistant", "content": "", "reasoning_content": "让我想一想……"},
                                    "finish_reason": "length"
                                }],
                                "usage": {"prompt_tokens": 10, "completion_tokens": 512}
                            })),
                        )
                            .into_response();
                    }
                    (
                        axum::http::StatusCode::OK,
                        AxJson(json!({
                            "choices": [{"message": {"content": " ok "}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                        })),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, bodies)
    }

    #[tokio::test]
    async fn chat_retries_with_doubled_budget_on_empty_content() {
        let (addr, bodies) = spawn_mock_thinking_eats_budget().await;
        let msgs = [ChatMessage::user("hi")];
        let out = client_for(addr, "future-model").chat(&msgs, 0.4, false, 512).await.unwrap();
        assert_eq!(out.content, "ok");
        let all = bodies.lock().unwrap();
        assert_eq!(all.len(), 2, "空响应自救：加倍预算重试一次");
        assert_eq!(all[0]["max_tokens"], 512);
        assert_eq!(all[1]["max_tokens"], 1024, "重试预算翻倍");
    }
}
