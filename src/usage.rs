//! Token usage accounting: normalizing per-provider usage into disjoint input
//! buckets, per-model pricing, and the spend/token budget cap.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::{ApiProvider, ModelPricing};
use crate::{anthropic_prompt_cache_supported, canonical_provider_id, is_gpt_5_6_model};

pub(crate) const DEFAULT_INPUT_USD_PER_MTOK: f64 = 1.0;
pub(crate) const DEFAULT_OUTPUT_USD_PER_MTOK: f64 = 5.0;
pub(crate) const DEFAULT_CACHE_READ_USD_PER_MTOK: f64 = 0.1;
pub(crate) const DEFAULT_CACHE_CREATE_USD_PER_MTOK: f64 = 1.25;

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Usage {
    // Provider usage is normalized into disjoint input buckets:
    // - Anthropic: input_tokens plus cache_creation/read_input_tokens.
    // - OpenAI/ChatGPT: prompt/input tokens minus cached_tokens, with cached_tokens as cache_read.
    // - Z.ai/GLM: Anthropic-compatible fields when present; otherwise no cache buckets.
    // - local llama.cpp: timings.prompt_n as new prompt input and timings.cache_n as cache_read.
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_create: u64,
    pub(crate) cache_read: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cost_usd: Option<f64>,
}

impl Usage {
    pub(crate) fn add(&mut self, o: Usage) {
        let lhs_cost = self.cost_usd;
        let rhs_cost = o.cost_usd;
        let lhs_tokens = self.total_tokens();
        let rhs_tokens = o.total_tokens();
        self.input = self.input.saturating_add(o.input);
        self.output = self.output.saturating_add(o.output);
        self.cache_create = self.cache_create.saturating_add(o.cache_create);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.cost_usd = match (lhs_cost, rhs_cost) {
            (Some(a), Some(b)) => Some(a + b).filter(|cost| cost.is_finite()),
            (Some(a), None) if rhs_tokens == 0 => Some(a),
            (None, Some(b)) if lhs_tokens == 0 => Some(b),
            (None, None) if lhs_tokens == 0 && rhs_tokens == 0 => None,
            _ => None,
        };
    }

    pub(crate) fn actual_input_tokens(&self) -> u64 {
        self.input
    }

    pub(crate) fn cached_input_tokens(&self) -> u64 {
        self.cache_create.saturating_add(self.cache_read)
    }

    pub(crate) fn total_input_tokens(&self) -> u64 {
        self.input.saturating_add(self.cached_input_tokens())
    }

    pub(crate) fn billed_tokens(&self) -> u64 {
        self.total_input_tokens().saturating_add(self.output)
    }

    pub(crate) fn context_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    pub(crate) fn total_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.cost_usd
            .is_none_or(|cost| cost.is_finite() && cost >= 0.0)
    }

    pub(crate) fn estimated_cost_usd(&self) -> f64 {
        if let Some(cost) = self.cost_usd {
            return cost;
        }
        let per_mtok = 1_000_000.0;
        (self.input as f64 / per_mtok) * DEFAULT_INPUT_USD_PER_MTOK
            + (self.output as f64 / per_mtok) * DEFAULT_OUTPUT_USD_PER_MTOK
            + (self.cache_read as f64 / per_mtok) * DEFAULT_CACHE_READ_USD_PER_MTOK
            + (self.cache_create as f64 / per_mtok) * DEFAULT_CACHE_CREATE_USD_PER_MTOK
    }

    pub(crate) fn parse(v: &Value) -> Self {
        let cache_create = v["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cache_read = v["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let input = if let Some(input) = v["input_tokens"].as_u64() {
            input
        } else {
            v["prompt_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(cache_create)
                .saturating_sub(cache_read)
        };
        let output = v["output_tokens"]
            .as_u64()
            .or_else(|| v["completion_tokens"].as_u64())
            .unwrap_or(0);
        Self {
            input,
            output,
            cache_create,
            cache_read,
            cost_usd: parse_usage_cost(v),
        }
    }

    pub(crate) fn parse_openai(v: &Value) -> Self {
        let prompt_cache_hit = v["prompt_cache_hit_tokens"].as_u64().unwrap_or(0);
        let prompt_cache_miss = v["prompt_cache_miss_tokens"].as_u64();
        let total_input = v["prompt_tokens"]
            .as_u64()
            .or_else(|| v["input_tokens"].as_u64())
            .or_else(|| prompt_cache_miss.map(|miss| miss.saturating_add(prompt_cache_hit)))
            .unwrap_or(0);
        let output = v["completion_tokens"]
            .as_u64()
            .or_else(|| v["output_tokens"].as_u64())
            .or_else(|| v["completion_tokens_details"]["accepted_prediction_tokens"].as_u64())
            .unwrap_or(0);
        let cache_read = v
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .or_else(|| {
                v.get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
            })
            .or_else(|| v["cache_read_input_tokens"].as_u64())
            .or_else(|| v["cached_tokens"].as_u64())
            .or(Some(prompt_cache_hit).filter(|tokens| *tokens > 0))
            .unwrap_or(0);
        let cache_create = v["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cost_usd = parse_usage_cost(v);
        Self {
            input: total_input
                .saturating_sub(cache_read)
                .saturating_sub(cache_create),
            output,
            cache_create,
            cache_read,
            cost_usd,
        }
    }

    pub(crate) fn parse_openai_timings(v: &Value) -> Option<Self> {
        let cache_read = v["cache_n"].as_u64().unwrap_or(0);
        let input = v["prompt_n"].as_u64().unwrap_or(0);
        let output = v["predicted_n"].as_u64().unwrap_or(0);
        (cache_read > 0 || input > 0 || output > 0).then_some(Self {
            input,
            output,
            cache_create: 0,
            cache_read,
            cost_usd: None,
        })
    }

    pub(crate) fn line(&self) -> String {
        let mut input = format!("input={}", self.total_input_tokens());
        if self.cached_input_tokens() > 0 {
            input.push_str(&format!(
                " new_in={} cache_r={} cache_w={}",
                self.actual_input_tokens(),
                self.cache_read,
                self.cache_create
            ));
        }
        format!(
            "{} out={} total={} est=${:.4}",
            input,
            self.output,
            self.total_tokens(),
            self.estimated_cost_usd()
        )
    }
}

pub(crate) fn parse_usage_cost(v: &Value) -> Option<f64> {
    v["cost"]
        .as_f64()
        .or_else(|| v["cost_usd"].as_f64())
        .or_else(|| v["total_cost"].as_f64())
        .or_else(|| v["total_cost_usd"].as_f64())
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

#[derive(Clone, Copy)]
pub(crate) struct UsagePricing {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_create: f64,
}

impl UsagePricing {
    const fn new(input: f64, output: f64, cache_read: f64, cache_create: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_create,
        }
    }

    const fn zero() -> Self {
        Self::new(0.0, 0.0, 0.0, 0.0)
    }

    pub(crate) fn scaled(self, input: f64, output: f64) -> Self {
        Self::new(
            self.input * input,
            self.output * output,
            self.cache_read * input,
            self.cache_create * input,
        )
    }

    pub(crate) fn estimate(self, usage: Usage) -> f64 {
        let per_mtok = 1_000_000.0;
        (usage.input as f64 / per_mtok) * self.input
            + (usage.output as f64 / per_mtok) * self.output
            + (usage.cache_read as f64 / per_mtok) * self.cache_read
            + (usage.cache_create as f64 / per_mtok) * self.cache_create
    }
}

impl From<&ModelPricing> for UsagePricing {
    fn from(pricing: &ModelPricing) -> Self {
        Self::new(
            pricing.input_usd_per_mtok,
            pricing.output_usd_per_mtok,
            pricing.cache_read_usd_per_mtok,
            pricing.cache_create_usd_per_mtok,
        )
    }
}

impl Default for UsagePricing {
    fn default() -> Self {
        Self::new(
            DEFAULT_INPUT_USD_PER_MTOK,
            DEFAULT_OUTPUT_USD_PER_MTOK,
            DEFAULT_CACHE_READ_USD_PER_MTOK,
            DEFAULT_CACHE_CREATE_USD_PER_MTOK,
        )
    }
}

pub(crate) fn provider_cost_estimate_overrides_wire_cost(
    provider_id: &str,
    api_provider: ApiProvider,
    model: &str,
) -> bool {
    api_provider == ApiProvider::Anthropic && anthropic_prompt_cache_supported(provider_id, model)
}

pub(crate) fn usage_pricing_for(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
    model: &str,
) -> UsagePricing {
    usage_pricing_from_env(usage_pricing_default_for(
        provider_id,
        api_provider,
        base_url,
        model,
    ))
}

pub(crate) fn usage_pricing_default_for(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
    model: &str,
) -> UsagePricing {
    if crate::provider::is_local_llama_provider(provider_id, api_provider, base_url) {
        UsagePricing::zero()
    } else {
        let provider = canonical_provider_id(provider_id);
        let model = normalize_price_model(model);
        match provider.as_str() {
            "openai" | "chatgpt" => openai_pricing(&model).unwrap_or_default(),
            "anthropic" | "glm" => anthropic_pricing(&model).unwrap_or_default(),
            "deepseek" => deepseek_pricing(&model).unwrap_or_default(),
            _ if api_provider == ApiProvider::Anthropic => {
                anthropic_pricing(&model).unwrap_or_default()
            }
            _ => UsagePricing::default(),
        }
    }
}

pub(crate) fn pricing_env_override_is_set() -> bool {
    [
        "DEXT_INPUT_USD_PER_MTOK",
        "DEXT_OUTPUT_USD_PER_MTOK",
        "DEXT_CACHE_READ_USD_PER_MTOK",
        "DEXT_CACHE_CREATE_USD_PER_MTOK",
    ]
    .into_iter()
    .any(|name| env_f64(name).is_some())
}

pub(crate) fn gpt_5_6_long_context_pricing(
    provider_id: &str,
    model: &str,
    usage: Usage,
    pricing: UsagePricing,
) -> UsagePricing {
    gpt_5_6_long_context_pricing_with_override_state(
        provider_id,
        model,
        usage,
        pricing,
        pricing_env_override_is_set(),
    )
}

pub(crate) fn gpt_5_6_long_context_pricing_with_override_state(
    provider_id: &str,
    model: &str,
    usage: Usage,
    pricing: UsagePricing,
    pricing_override_is_set: bool,
) -> UsagePricing {
    if matches!(
        canonical_provider_id(provider_id).as_str(),
        "openai" | "chatgpt"
    ) && is_gpt_5_6_model(model)
        && usage.total_input_tokens() > 272_000
        && !pricing_override_is_set
        && openai_pricing(&normalize_price_model(model)).is_some_and(|official| {
            pricing.input == official.input
                && pricing.output == official.output
                && pricing.cache_read == official.cache_read
                && pricing.cache_create == official.cache_create
        })
    {
        pricing.scaled(2.0, 1.5)
    } else {
        pricing
    }
}

pub(crate) fn usage_with_current_pricing(
    mut usage: Usage,
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
    model: &str,
    model_pricing: Option<&ModelPricing>,
) -> Usage {
    if usage.total_tokens() > 0
        && (provider_cost_estimate_overrides_wire_cost(provider_id, api_provider, model)
            || usage.cost_usd.is_none())
    {
        let pricing = model_pricing.map_or_else(
            || usage_pricing_for(provider_id, api_provider, base_url, model),
            |pricing| usage_pricing_from_env(UsagePricing::from(pricing)),
        );
        let pricing = gpt_5_6_long_context_pricing(provider_id, model, usage, pricing);
        usage.cost_usd = Some(pricing.estimate(usage));
    }
    usage
}

pub(crate) fn normalize_price_model(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

pub(crate) fn openai_pricing(model: &str) -> Option<UsagePricing> {
    if matches!(model, "gpt-5.6" | "gpt-5.6-sol") {
        Some(UsagePricing::new(5.0, 30.0, 0.5, 6.25))
    } else if model == "gpt-5.6-terra" {
        Some(UsagePricing::new(2.5, 15.0, 0.25, 3.125))
    } else if model == "gpt-5.6-luna" {
        Some(UsagePricing::new(1.0, 6.0, 0.1, 1.25))
    } else if model.starts_with("gpt-5.4-mini") {
        Some(UsagePricing::new(0.25, 2.0, 0.025, 0.25))
    } else if model.starts_with("gpt-5.4") {
        Some(UsagePricing::new(1.25, 10.0, 0.125, 1.25))
    } else if model.starts_with("gpt-5.3-codex-spark") {
        Some(UsagePricing::new(0.25, 2.0, 0.025, 0.25))
    } else if model.starts_with("gpt-5.3-codex") {
        Some(UsagePricing::new(1.25, 10.0, 0.125, 1.25))
    } else if model.starts_with("gpt-5-mini") {
        Some(UsagePricing::new(0.25, 2.0, 0.025, 0.25))
    } else if model.starts_with("gpt-5-nano") {
        Some(UsagePricing::new(0.05, 0.4, 0.005, 0.05))
    } else if model.starts_with("gpt-5") {
        Some(UsagePricing::new(1.25, 10.0, 0.125, 1.25))
    } else if model.starts_with("gpt-4.1-mini") {
        Some(UsagePricing::new(0.4, 1.6, 0.1, 0.4))
    } else if model.starts_with("gpt-4.1-nano") {
        Some(UsagePricing::new(0.1, 0.4, 0.025, 0.1))
    } else if model.starts_with("gpt-4.1") {
        Some(UsagePricing::new(2.0, 8.0, 0.5, 2.0))
    } else if model.starts_with("gpt-4o-mini") {
        Some(UsagePricing::new(0.15, 0.6, 0.075, 0.15))
    } else if model.starts_with("gpt-4o") {
        Some(UsagePricing::new(2.5, 10.0, 1.25, 2.5))
    } else if model.starts_with("o3-mini") || model.starts_with("o4-mini") {
        Some(UsagePricing::new(1.1, 4.4, 0.55, 1.1))
    } else if model.starts_with("o3") {
        Some(UsagePricing::new(2.0, 8.0, 0.5, 2.0))
    } else {
        None
    }
}

pub(crate) fn anthropic_pricing(model: &str) -> Option<UsagePricing> {
    if model.starts_with("glm-") {
        return Some(UsagePricing::default());
    }
    if model.contains("fable") {
        Some(UsagePricing::new(10.0, 50.0, 1.0, 12.5))
    } else if [
        "opus-5", "opus5", "opus-4-5", "opus-4.5", "opus-4-6", "opus-4.6", "opus-4-7", "opus-4.7",
        "opus-4-8", "opus-4.8",
    ]
    .iter()
    .any(|generation| model.contains(generation))
    {
        Some(UsagePricing::new(5.0, 25.0, 0.5, 6.25))
    } else if model.contains("opus") {
        Some(UsagePricing::new(15.0, 75.0, 1.5, 18.75))
    } else if model.contains("sonnet-5") || model.contains("sonnet5") {
        Some(UsagePricing::new(2.0, 10.0, 0.2, 2.5))
    } else if model.contains("sonnet") {
        Some(UsagePricing::new(3.0, 15.0, 0.3, 3.75))
    } else if model.contains("haiku-4-5") || model.contains("haiku-4.5") {
        Some(UsagePricing::new(1.0, 5.0, 0.1, 1.25))
    } else if model.contains("haiku") {
        Some(UsagePricing::new(0.8, 4.0, 0.08, 1.0))
    } else {
        None
    }
}

pub(crate) fn deepseek_pricing(model: &str) -> Option<UsagePricing> {
    if model.contains("reasoner") {
        Some(UsagePricing::new(0.55, 2.19, 0.14, 0.55))
    } else if model.contains("chat") {
        Some(UsagePricing::new(0.27, 1.1, 0.07, 0.27))
    } else {
        None
    }
}

pub(crate) fn usage_pricing_from_env(default: UsagePricing) -> UsagePricing {
    usage_pricing_with_overrides(
        default,
        env_f64("DEXT_INPUT_USD_PER_MTOK"),
        env_f64("DEXT_OUTPUT_USD_PER_MTOK"),
        env_f64("DEXT_CACHE_READ_USD_PER_MTOK"),
        env_f64("DEXT_CACHE_CREATE_USD_PER_MTOK"),
    )
}

pub(crate) fn usage_pricing_with_overrides(
    default: UsagePricing,
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_create: Option<f64>,
) -> UsagePricing {
    UsagePricing {
        input: input.unwrap_or(default.input),
        output: output.unwrap_or(default.output),
        cache_read: cache_read.unwrap_or(default.cache_read),
        cache_create: cache_create.unwrap_or(default.cache_create),
    }
}

pub(crate) fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct BudgetCap {
    pub(crate) usd: Option<f64>,
    pub(crate) tokens: Option<u64>,
}

impl BudgetCap {
    pub(crate) fn is_valid(&self) -> bool {
        (self.usd.is_some() || self.tokens.is_some())
            && self.usd.is_none_or(|usd| usd.is_finite() && usd > 0.0)
            && self.tokens.is_none_or(|tokens| tokens > 0)
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("off")
            || raw.eq_ignore_ascii_case("none")
            || raw.eq_ignore_ascii_case("disabled")
            || raw == "0"
        {
            return None;
        }
        let parts: Vec<&str> = raw.split([',', '+']).map(str::trim).collect();
        if parts.len() > 1 {
            if parts.iter().any(|part| part.is_empty()) {
                return None;
            }
            let mut cap = Self {
                usd: None,
                tokens: None,
            };
            for part in parts {
                let parsed = Self::parse_one(part)?;
                if let Some(usd) = parsed.usd
                    && cap.usd.replace(usd).is_some()
                {
                    return None;
                }
                if let Some(tokens) = parsed.tokens
                    && cap.tokens.replace(tokens).is_some()
                {
                    return None;
                }
            }
            return (cap.usd.is_some() || cap.tokens.is_some()).then_some(cap);
        }
        Self::parse_one(raw)
    }

    pub(crate) fn parse_one(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("$") {
            return parse_positive_f64(rest).map(|usd| Self {
                usd: Some(usd),
                tokens: None,
            });
        }
        if let Some(rest) = lower
            .strip_suffix("usd")
            .or_else(|| lower.strip_suffix("dollars"))
            .or_else(|| lower.strip_suffix("dollar"))
        {
            return parse_positive_f64(rest.trim()).map(|usd| Self {
                usd: Some(usd),
                tokens: None,
            });
        }
        if let Some(rest) = lower
            .strip_suffix("tok")
            .or_else(|| lower.strip_suffix("tokens"))
            .or_else(|| lower.strip_suffix("token"))
            .or_else(|| lower.strip_suffix('t'))
        {
            return parse_token_count(rest.trim()).map(|tokens| Self {
                usd: None,
                tokens: Some(tokens),
            });
        }
        parse_positive_f64(&lower).map(|usd| Self {
            usd: Some(usd),
            tokens: None,
        })
    }

    pub(crate) fn from_env() -> Result<Option<Self>, String> {
        let raw = match std::env::var("DEXT_BUDGET_CAP") {
            Ok(raw) => raw,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err("DEXT_BUDGET_CAP must be valid UTF-8".to_string());
            }
        };
        if raw.trim().eq_ignore_ascii_case("off")
            || raw.trim().eq_ignore_ascii_case("none")
            || raw.trim().eq_ignore_ascii_case("disabled")
            || raw.trim() == "0"
        {
            return Ok(None);
        }
        Self::parse(&raw).map(Some).ok_or_else(|| {
            "invalid DEXT_BUDGET_CAP (expected positive dollars or tokens, optionally one of each)"
                .to_string()
        })
    }

    pub(crate) fn exceeded(&self, usage: Usage) -> Option<String> {
        if let Some(tokens) = self.tokens {
            let used = usage.total_tokens();
            if used >= tokens {
                return Some(format!("token budget cap reached: {used}/{tokens} tokens"));
            }
        }
        if let Some(usd) = self.usd {
            let used = usage.estimated_cost_usd();
            if used >= usd {
                return Some(format!("budget cap reached: ${used:.4}/${usd:.4}"));
            }
        }
        None
    }

    pub(crate) fn line(self) -> String {
        match (self.usd, self.tokens) {
            (Some(usd), Some(tokens)) => format!("${usd:.4} or {tokens} tokens"),
            (Some(usd), None) => format!("${usd:.4}"),
            (None, Some(tokens)) => format!("{tokens} tokens"),
            (None, None) => "off".to_string(),
        }
    }
}

pub(crate) fn parse_positive_f64(raw: &str) -> Option<f64> {
    let value = raw.trim().parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

pub(crate) fn parse_token_count(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (number, mult) = if let Some(n) = trimmed.strip_suffix('k') {
        (n, 1_000.0)
    } else if let Some(n) = trimmed.strip_suffix('m') {
        (n, 1_000_000.0)
    } else {
        (trimmed, 1.0)
    };
    let value = parse_positive_f64(number)?;
    let tokens = (value * mult).round();
    (tokens.is_finite() && tokens >= 1.0 && tokens < u64::MAX as f64).then_some(tokens as u64)
}
