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

/// 内置价格表：按「高峰时段」的保守口径（空闲时段减半，见 price_note）。
/// 快照来源：DeepSeek 官方定价页（2026-09-06 核准，api-docs.deepseek.com/zh-cn/quick_start/pricing）。
/// 缓存命中价约为未命中的 1/30——DeepSeek 的 system prompt 前缀缓存命中率很高，
/// 按全价计会显著虚高（真机反馈的"计费有误"主因）。
/// deepseek-chat 现行价格与 V4-Flash 同档；表外模型经 config [llm.*] 或设置页覆盖。
/// 本地端点（localhost/127.0.0.1）默认免费。
fn builtin_price(model: &str) -> Option<Price> {
    let m = model.trim().to_ascii_lowercase();
    // (关键字, 输入未命中, 输入命中, 输出) 元 / 百万 tokens，高峰时段
    const TABLE: &[(&str, f64, f64, f64)] = &[
        ("deepseek-chat", 3.0, 0.10, 9.0),
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
        _ => {
            if let Some(pr) = builtin_price(&p.model) {
                (Some(pr), PriceSource::Builtin)
            } else if let Some(pr) = local_free(&p.base_url) {
                (Some(pr), PriceSource::LocalFree)
            } else {
                (None, PriceSource::Unknown)
            }
        }
    }
}

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
                    "内置价格表（快照 {PRICE_SNAPSHOT_DATE}，高峰口径，空闲减半{cache}；缓存命中部分已按命中价计）"
                )
            }
            PriceSource::Config => "配置覆盖（config.toml / 设置页）".into(),
            PriceSource::LocalFree => "本地端点（免费）".into(),
            PriceSource::Unknown => "未配置价格：仅统计 token，费用显示为空（可在设置页填入价格）".into(),
        }
    }

    /// chat/completions。json_mode=true 时请求 JSON 输出；端点不支持 response_format 时自动降级重试一次。
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        temperature: f32,
        json_mode: bool,
        max_tokens: u32,
    ) -> Result<ChatOutput, LlmError> {
        let url = format!("{}/chat/completions", self.base_url);
        let build = |json_out: bool| {
            let mut body = json!({
                "model": self.model,
                "messages": messages
                    .iter()
                    .map(|m| json!({"role": m.role, "content": m.content}))
                    .collect::<Vec<_>>(),
                "temperature": temperature,
                "max_tokens": max_tokens,
                "stream": false,
            });
            if json_out {
                body["response_format"] = json!({"type": "json_object"});
            }
            body
        };
        let (mut status, mut text) = self.post(&url, build(json_mode)).await?;
        if status == 400 && json_mode {
            let r = self.post(&url, build(false)).await?;
            status = r.0;
            text = r.1;
        }
        if !(200..300).contains(&status) {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| crate::steam_client::truncate(&text, 200));
            return Err(LlmError::Http { status, msg });
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::Network(format!("响应解析失败: {e}")))?;
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .ok_or(LlmError::EmptyContent)?
            .trim()
            .to_string();
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
        // 缓存命中的输入按命中价计（DeepSeek 约为全价的 1/30），缺失缓存价时按全价保守
        let cost = self.price.map(|p| {
            let cached = usage.cached_prompt_tokens.min(usage.prompt_tokens) as f64;
            let fresh = usage.prompt_tokens as f64 - cached;
            let cache_price = p.cache_input_per_m.unwrap_or(p.input_per_m);
            fresh / 1e6 * p.input_per_m
                + cached / 1e6 * cache_price
                + usage.completion_tokens as f64 / 1e6 * p.output_per_m
        });
        Ok(ChatOutput { content, usage, model: self.model.clone(), cost_cny: cost })
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
                .map_err(|e| LlmError::Network(e.to_string()))?;
            let status = resp.status().as_u16();
            let text = resp
                .text()
                .await
                .map_err(|e| LlmError::Network(e.to_string()))?;
            if status == 429 || (500..600).contains(&status) {
                attempt += 1;
                if attempt >= 3 {
                    return Err(LlmError::Http { status, msg: crate::steam_client::truncate(&text, 200) });
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
}
