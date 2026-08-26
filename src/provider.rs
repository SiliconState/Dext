use anyhow::{Context, Result};
use base64::Engine;
use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::session::{
    atomic_write_bytes, atomic_write_secret, dext_state_dir, unix_timestamp_secs,
};

/// `anthropic-version` request header value sent on all Anthropic Messages API calls.
pub(crate) const ANTHROPIC_API_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ApiProvider {
    #[default]
    Anthropic,
    OpenAi,
    ChatGpt,
}

impl serde::Serialize for ApiProvider {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ApiProvider {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "ollama" | "llama" | "llama.cpp" | "local"
            | "deepseek" => Self::OpenAi,
            "chatgpt" | "openai-codex" | "codex" | "codex-openai" => Self::ChatGpt,
            _ => Self::Anthropic,
        })
    }
}

impl ApiProvider {
    #[cfg(test)]
    pub(crate) fn from_env() -> Self {
        match std::env::var("DEXT_API_PROVIDER")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "openai" | "openai-compatible" | "ollama" | "llama" | "llama.cpp" | "local"
            | "deepseek" => Self::OpenAi,
            "chatgpt" | "openai-codex" | "codex" | "codex-openai" => Self::ChatGpt,
            "anthropic" | "claude" | "glm" | "zai" => Self::Anthropic,
            _ => {
                let base = std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if base.contains("openai")
                    || base.contains("ollama")
                    || base.contains("llama")
                    || base.contains("127.0.0.1")
                    || base.contains("localhost:11434")
                {
                    Self::OpenAi
                } else {
                    Self::Anthropic
                }
            }
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::ChatGpt => "chatgpt",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RequestContract {
    #[serde(rename = "anthropic-messages", alias = "anthropic")]
    AnthropicMessages,
    #[serde(
        rename = "openai-chat-completions",
        alias = "openai",
        alias = "openai-compatible"
    )]
    OpenAiChatCompletions,
    #[serde(rename = "openai-responses", alias = "responses")]
    OpenAiResponses,
    #[serde(rename = "chatgpt-responses", alias = "chatgpt", alias = "codex")]
    ChatGptResponses,
}

impl RequestContract {
    pub(crate) fn for_api_provider(api_provider: ApiProvider) -> Self {
        match api_provider {
            ApiProvider::Anthropic => Self::AnthropicMessages,
            ApiProvider::OpenAi => Self::OpenAiChatCompletions,
            ApiProvider::ChatGpt => Self::ChatGptResponses,
        }
    }

    pub(crate) fn api_provider(self) -> ApiProvider {
        match self {
            Self::AnthropicMessages => ApiProvider::Anthropic,
            Self::OpenAiChatCompletions | Self::OpenAiResponses => ApiProvider::OpenAi,
            Self::ChatGptResponses => ApiProvider::ChatGpt,
        }
    }

    pub(crate) fn is_responses(self) -> bool {
        matches!(self, Self::OpenAiResponses | Self::ChatGptResponses)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiChatCompletions => "openai-chat-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::ChatGptResponses => "chatgpt-responses",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ModelCapabilities {
    pub(crate) tools: Option<bool>,
    pub(crate) reasoning: Option<bool>,
    pub(crate) image_input: Option<bool>,
    pub(crate) prompt_cache: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ModelPricing {
    pub(crate) input_usd_per_mtok: f64,
    pub(crate) output_usd_per_mtok: f64,
    pub(crate) cache_read_usd_per_mtok: f64,
    pub(crate) cache_create_usd_per_mtok: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ModelSpec {
    pub(crate) context_window: Option<u64>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) effort_levels: Vec<String>,
    pub(crate) reasoning_modes: Vec<String>,
    pub(crate) capabilities: ModelCapabilities,
    pub(crate) pricing: Option<ModelPricing>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedModelSpec {
    pub(crate) context_window: Option<u64>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) effort_levels: Vec<String>,
    pub(crate) reasoning_modes: Vec<String>,
    pub(crate) tools: bool,
    pub(crate) reasoning: bool,
    pub(crate) image_input: bool,
    pub(crate) prompt_cache: bool,
    pub(crate) pricing: Option<ModelPricing>,
    pub(crate) source: &'static str,
}

const PROVIDER_CATALOG_VERSION: u32 = 2;
const AUTH_STORE_VERSION: u32 = 1;
const STATE_INSPECTION_MAX_BYTES: u64 = 1024 * 1024;
pub(crate) const DEFAULT_LOCAL_MODEL: &str = "qwen3.6-35b-a3b-mtp-ud-q5_k_m";
const LLAMA_CONTEXT_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(700);
const LLAMA_CONTEXT_DISCOVERY_PATHS: &[&str] = &["/props", "/slots", "/v1/models", "/models"];

const KIMI_CODE_BASE_URL: &str = "https://api.kimi.com/coding";
const KIMI_BUILTIN_PROFILE_MARKER: &str = "kimi-code";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_BUILTIN_PROFILE_MARKER: &str = "anthropic";
const OAUTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_CALLBACK_IO_TIMEOUT: Duration = Duration::from_secs(2);
const OAUTH_RESPONSE_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OAuthProtocol {
    #[default]
    Generic,
    AnthropicClaude,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OAuthFlow {
    pub(crate) authorize_url: String,
    pub(crate) token_url: String,
    pub(crate) client_id: String,
    pub(crate) scope: String,
    #[serde(default)]
    pub(crate) audience: String,
    #[serde(default)]
    pub(crate) redirect_uri: Option<String>,
    #[serde(default)]
    pub(crate) callback_host: Option<String>,
    #[serde(default)]
    pub(crate) protocol: OAuthProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProviderProfile {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) builtin: Option<String>,
    #[serde(default)]
    pub(crate) display_name: String,
    pub(crate) api_provider: ApiProvider,
    #[serde(default)]
    pub(crate) request_contract: Option<RequestContract>,
    pub(crate) base_url: String,
    pub(crate) default_model: String,
    #[serde(default)]
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) model_aliases: HashMap<String, String>,
    #[serde(default)]
    pub(crate) model_defaults: ModelSpec,
    #[serde(default)]
    pub(crate) model_specs: HashMap<String, ModelSpec>,
    #[serde(default)]
    pub(crate) env_vars: Vec<String>,
    #[serde(default = "default_provider_requires_api_key")]
    pub(crate) requires_api_key: bool,
    #[serde(default)]
    pub(crate) login_url: Option<String>,
    #[serde(default)]
    pub(crate) oauth_flow: Option<OAuthFlow>,
    #[serde(default)]
    pub(crate) notes: Option<String>,
    /// Default request context window (in tokens) for this provider's models.
    /// llama.cpp/local may override this at runtime. Env override: DEXT_CONTEXT_WINDOW[_TOKENS].
    #[serde(default)]
    pub(crate) context_window: Option<u64>,
    /// Optional per-model override of context_window. Map key = model id.
    #[serde(default)]
    pub(crate) model_context_windows: HashMap<String, u64>,
    /// Optional per-model provider-native reasoning effort levels.
    /// Map key = model id; values are strings such as "high"/"max".
    #[serde(default)]
    pub(crate) model_effort_levels: HashMap<String, Vec<String>>,
}

pub(crate) fn default_provider_requires_api_key() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProviderCatalog {
    #[serde(default = "default_provider_catalog_version")]
    pub(crate) version: u32,
    #[serde(default = "default_active_provider")]
    pub(crate) active_provider: String,
    pub(crate) providers: Vec<ProviderProfile>,
}

pub(crate) fn default_provider_catalog_version() -> u32 {
    PROVIDER_CATALOG_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderCatalogIntegrity {
    Missing,
    Valid {
        version: u32,
        legacy: bool,
    },
    InvalidSchema,
    UnsupportedVersion {
        version: u32,
    },
    Symlink,
    NonRegular,
    #[cfg(unix)]
    UnsafeOwner,
    #[cfg(unix)]
    UnsafeMode {
        mode: u32,
    },
    Unreadable,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderCatalogInspection {
    pub(crate) path: PathBuf,
    pub(crate) integrity: ProviderCatalogIntegrity,
    pub(crate) active_provider: Option<String>,
    pub(crate) provider_count: Option<usize>,
}

pub(crate) fn default_active_provider() -> String {
    "glm".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StoredCredential {
    ApiKey {
        key: String,
    },
    OAuth {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_at: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuthStore {
    #[serde(default = "default_auth_store_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) providers: HashMap<String, StoredCredential>,
}

impl Default for AuthStore {
    fn default() -> Self {
        Self {
            version: AUTH_STORE_VERSION,
            providers: HashMap::new(),
        }
    }
}

pub(crate) fn default_auth_store_version() -> u32 {
    AUTH_STORE_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthStoreFileSecurity {
    Missing,
    #[cfg(unix)]
    Secure {
        mode: u32,
    },
    #[cfg(unix)]
    UnsafeMode {
        mode: u32,
    },
    Symlink,
    NonRegular,
    #[cfg(unix)]
    UnsafeOwner,
    Unreadable,
    #[cfg(windows)]
    WindowsAclNotEvaluated,
    #[cfg(not(any(unix, windows)))]
    PermissionsNotEvaluated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthStoreIntegrity {
    NotChecked,
    Valid { version: u32, legacy: bool },
    InvalidSchema,
    UnsupportedVersion { version: u32 },
    Unreadable,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthStoreInspection {
    pub(crate) path: PathBuf,
    pub(crate) security: AuthStoreFileSecurity,
    pub(crate) integrity: AuthStoreIntegrity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RuntimeAuthKind {
    #[default]
    None,
    ApiKey,
    OAuth,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedProviderConfig {
    pub(crate) profile: ProviderProfile,
    pub(crate) api_key: String,
    pub(crate) auth_kind: RuntimeAuthKind,
    pub(crate) key_source: String,
    pub(crate) model: String,
    pub(crate) base_url: String,
    pub(crate) requires_api_key: bool,
}

pub(crate) fn provider_catalog_path() -> PathBuf {
    dext_state_dir().join("providers.json")
}

pub(crate) fn auth_store_path() -> PathBuf {
    dext_state_dir().join("auth.json")
}

pub(crate) fn canonical_provider_id(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "zai" | "bigmodel" | "bigmodel-cn" => "glm".to_string(),
        "openai-codex" | "codex-openai" | "codex" => "chatgpt".to_string(),
        "chatgpt-plus" | "chatgpt-pro" => "chatgpt".to_string(),
        "claude" => "anthropic".to_string(),
        "kimi-code" | "kimi-coding" | "kimi-membership" => "kimi".to_string(),
        "llama" | "llama.cpp" | "llamacpp" | "qwen" => "local".to_string(),
        other => other.to_string(),
    }
}

const RETIRED_BUNDLED_PROVIDER_IDS: &[&str] = &["openrouter", "ollama"];

fn model_pricing(input: f64, output: f64, cache_read: f64, cache_create: f64) -> ModelPricing {
    ModelPricing {
        input_usd_per_mtok: input,
        output_usd_per_mtok: output,
        cache_read_usd_per_mtok: cache_read,
        cache_create_usd_per_mtok: cache_create,
    }
}

fn gpt_5_6_model_specs(
    include_unsuffixed_alias: bool,
    include_max_effort_and_pro_mode: bool,
) -> HashMap<String, ModelSpec> {
    let spec = |pricing: ModelPricing| ModelSpec {
        context_window: Some(1_050_000),
        max_output_tokens: Some(128_000),
        effort_levels: if include_max_effort_and_pro_mode {
            ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_string)
                .collect()
        } else {
            ["none", "low", "medium", "high", "xhigh"]
                .into_iter()
                .map(str::to_string)
                .collect()
        },
        reasoning_modes: if include_max_effort_and_pro_mode {
            vec!["standard".to_string(), "pro".to_string()]
        } else {
            Vec::new()
        },
        capabilities: ModelCapabilities {
            tools: Some(true),
            reasoning: Some(true),
            image_input: Some(true),
            prompt_cache: Some(true),
        },
        pricing: Some(pricing),
    };
    let sol = spec(model_pricing(5.0, 30.0, 0.5, 6.25));
    let mut specs = HashMap::from([
        ("gpt-5.6-sol".to_string(), sol.clone()),
        (
            "gpt-5.6-terra".to_string(),
            spec(model_pricing(2.5, 15.0, 0.25, 3.125)),
        ),
        (
            "gpt-5.6-luna".to_string(),
            spec(model_pricing(1.0, 6.0, 0.1, 1.25)),
        ),
    ]);
    if include_unsuffixed_alias {
        specs.insert("gpt-5.6".to_string(), sol);
    }
    specs
}

fn builtin_model_pricing(provider_id: &str, model: &str) -> Option<ModelPricing> {
    let model = model.to_ascii_lowercase();
    match canonical_provider_id(provider_id).as_str() {
        "local" | "kimi" => Some(model_pricing(0.0, 0.0, 0.0, 0.0)),
        "glm" if model.trim_end_matches("[1m]") == "glm-5.3-flash" => {
            Some(model_pricing(0.15, 0.5, 0.03, 0.0))
        }
        "glm" => Some(model_pricing(1.0, 5.0, 0.1, 1.25)),
        "deepseek" if model.contains("reasoner") => Some(model_pricing(0.55, 2.19, 0.14, 0.55)),
        "deepseek" if model.contains("chat") => Some(model_pricing(0.27, 1.1, 0.07, 0.27)),
        "anthropic" if model.contains("fable") => Some(model_pricing(10.0, 50.0, 1.0, 12.5)),
        // Opus 4.5 through Opus 5 share one published rate; Opus 4.1-and-earlier
        // retain legacy pricing via the plain "opus" arm below.
        "anthropic"
            if [
                "opus-5", "opus5", "opus-4-5", "opus-4.5", "opus-4-6", "opus-4.6", "opus-4-7",
                "opus-4.7", "opus-4-8", "opus-4.8",
            ]
            .iter()
            .any(|generation| model.contains(generation)) =>
        {
            Some(model_pricing(5.0, 25.0, 0.5, 6.25))
        }
        "anthropic" if model.contains("opus") => Some(model_pricing(15.0, 75.0, 1.5, 18.75)),
        "anthropic" if model.contains("sonnet-5") || model.contains("sonnet5") => {
            Some(model_pricing(2.0, 10.0, 0.2, 2.5))
        }
        "anthropic" if model.contains("sonnet") => Some(model_pricing(3.0, 15.0, 0.3, 3.75)),
        "anthropic" if model.contains("haiku-4-5") || model.contains("haiku-4.5") => {
            Some(model_pricing(1.0, 5.0, 0.1, 1.25))
        }
        "anthropic" if model.contains("haiku") => Some(model_pricing(0.8, 4.0, 0.08, 1.0)),
        "openai" | "chatgpt" if model == "gpt-5.6-sol" => Some(model_pricing(5.0, 30.0, 0.5, 6.25)),
        "openai" | "chatgpt" if model == "gpt-5.6" => Some(model_pricing(5.0, 30.0, 0.5, 6.25)),
        "openai" | "chatgpt" if model == "gpt-5.6-terra" => {
            Some(model_pricing(2.5, 15.0, 0.25, 3.125))
        }
        "openai" | "chatgpt" if model == "gpt-5.6-luna" => Some(model_pricing(1.0, 6.0, 0.1, 1.25)),
        "openai" | "chatgpt" if model.starts_with("gpt-5.4-mini") => {
            Some(model_pricing(0.25, 2.0, 0.025, 0.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5.3-codex-spark") => {
            Some(model_pricing(0.25, 2.0, 0.025, 0.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5-mini") => {
            Some(model_pricing(0.25, 2.0, 0.025, 0.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5-nano") => {
            Some(model_pricing(0.05, 0.4, 0.005, 0.05))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-5") => {
            Some(model_pricing(1.25, 10.0, 0.125, 1.25))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4.1-mini") => {
            Some(model_pricing(0.4, 1.6, 0.1, 0.4))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4.1-nano") => {
            Some(model_pricing(0.1, 0.4, 0.025, 0.1))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4.1") => {
            Some(model_pricing(2.0, 8.0, 0.5, 2.0))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4o-mini") => {
            Some(model_pricing(0.15, 0.6, 0.075, 0.15))
        }
        "openai" | "chatgpt" if model.starts_with("gpt-4o") => {
            Some(model_pricing(2.5, 10.0, 1.25, 2.5))
        }
        "openai" | "chatgpt" if model.starts_with("o3-mini") || model.starts_with("o4-mini") => {
            Some(model_pricing(1.1, 4.4, 0.55, 1.1))
        }
        "openai" | "chatgpt" if model.starts_with("o3") => Some(model_pricing(2.0, 8.0, 0.5, 2.0)),
        _ => None,
    }
}

fn hydrate_builtin_model_specs(profiles: &mut [ProviderProfile]) {
    for profile in profiles {
        let provider_id = canonical_provider_id(&profile.id);
        let contract = request_contract_for_profile(profile);
        if profile.model_defaults.max_output_tokens.is_none() {
            profile.model_defaults.max_output_tokens = Some(8_192);
        }
        profile.model_defaults.capabilities = ModelCapabilities {
            tools: Some(true),
            reasoning: Some(true),
            image_input: Some(
                matches!(
                    contract,
                    RequestContract::AnthropicMessages
                        | RequestContract::OpenAiResponses
                        | RequestContract::ChatGptResponses
                ) || provider_id == "openai",
            ),
            prompt_cache: Some(matches!(
                provider_id.as_str(),
                "anthropic" | "openai" | "chatgpt" | "deepseek" | "local"
            )),
        };
        if provider_id == "glm" {
            profile.model_defaults.capabilities.image_input = Some(false);
        }
        for model in profile.models.clone() {
            let normalized = model.to_ascii_lowercase();
            let mut spec = profile.model_specs.remove(&normalized).unwrap_or_default();
            spec.pricing = builtin_model_pricing(&provider_id, &normalized);
            if provider_id == "glm" {
                spec.capabilities.image_input =
                    Some(normalized.trim_end_matches("[1m]") == "glm-5.3-flash");
            }
            if matches!(provider_id.as_str(), "openai" | "chatgpt")
                && (normalized.starts_with("gpt-4.1") || normalized.starts_with("gpt-4o"))
            {
                spec.capabilities.reasoning = Some(false);
            }
            if provider_id == "deepseek" && normalized.contains("chat") {
                spec.capabilities.reasoning = Some(false);
            }
            profile.model_specs.insert(normalized, spec);
        }
    }
}

pub(crate) fn built_in_provider_profiles() -> Vec<ProviderProfile> {
    let glm_flash_spec = ModelSpec {
        context_window: Some(1_000_000),
        max_output_tokens: Some(131_072),
        effort_levels: ["low", "high", "max"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        reasoning_modes: Vec::new(),
        capabilities: ModelCapabilities {
            tools: Some(true),
            reasoning: Some(true),
            image_input: Some(true),
            prompt_cache: None,
        },
        pricing: None,
    };
    let mut profiles = vec![
        ProviderProfile {
            id: "glm".to_string(),
            builtin: None,
            display_name: "ZAI GLM".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: "https://api.z.ai/api/anthropic".to_string(),
            default_model: "glm-5.2[1m]".to_string(),
            models: vec![
                "glm-5.3-flash".to_string(),
                "glm-5.3-flash[1m]".to_string(),
                "glm-5.2[1m]".to_string(),
                "glm-5.2".to_string(),
                "glm-5.1".to_string(),
                "glm-5.0".to_string(),
                "glm-4.6".to_string(),
            ],
            env_vars: vec!["ZAI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://open.bigmodel.cn/usercenter/apikeys".to_string()),
            oauth_flow: None,
            notes: Some(
                "Use your ZAI key. The catalog includes GLM-5.3-Flash with 1M context, 131,072-token output, and low/high/max effort; model-name context hints such as [1m] are honored, and other entitled models can be set directly with /model."
                    .to_string(),
            ),
            context_window: Some(200_000),
            model_context_windows: {
                let mut m = HashMap::new();
                m.insert("glm-5.3-flash".to_string(), 1_000_000);
                m.insert("glm-5.3-flash[1m]".to_string(), 1_000_000);
                m.insert("glm-5.2".to_string(), 1_000_000);
                m.insert("glm-5.2[1m]".to_string(), 1_000_000);
                m
            },
            model_effort_levels: {
                let mut m = HashMap::new();
                m.insert(
                    "glm-5.3-flash".to_string(),
                    vec!["low".to_string(), "high".to_string(), "max".to_string()],
                );
                m.insert(
                    "glm-5.3-flash[1m]".to_string(),
                    vec!["low".to_string(), "high".to_string(), "max".to_string()],
                );
                m.insert("glm-5.2".to_string(), vec!["high".to_string(), "max".to_string()]);
                m.insert(
                    "glm-5.2[1m]".to_string(),
                    vec!["high".to_string(), "max".to_string()],
                );
                m
            },
            request_contract: Some(RequestContract::AnthropicMessages),
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::from([
                ("glm-5.3-flash".to_string(), glm_flash_spec.clone()),
                ("glm-5.3-flash[1m]".to_string(), glm_flash_spec),
            ]),
        },
        ProviderProfile {
            id: "chatgpt".to_string(),
            builtin: None,
            display_name: "ChatGPT".to_string(),
            api_provider: ApiProvider::ChatGpt,
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            default_model: "gpt-5.4".to_string(),
            models: vec![
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-5.6-luna".to_string(),
                "gpt-5.4".to_string(),
                "gpt-5.4-mini".to_string(),
                "gpt-5.5".to_string(),
                "gpt-5.3-codex".to_string(),
                "gpt-5.3-codex-spark".to_string(),
                "gpt-5-codex".to_string(),
                "gpt-5".to_string(),
                "gpt-5-mini".to_string(),
                "gpt-4.1".to_string(),
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "o3".to_string(),
                "o3-mini".to_string(),
                "o4-mini".to_string(),
            ],
            env_vars: vec!["CHATGPT_ACCESS_TOKEN".to_string()],
            requires_api_key: true,
            login_url: Some("https://chatgpt.com".to_string()),
            oauth_flow: Some(OAuthFlow {
                authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
                token_url: "https://auth.openai.com/oauth/token".to_string(),
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
                scope: "openid profile email offline_access".to_string(),
                audience: String::new(),
                redirect_uri: Some("http://localhost:1455/auth/callback".to_string()),
                callback_host: Some("127.0.0.1".to_string()),
                protocol: OAuthProtocol::Generic,
            }),
            notes: Some(
                "ChatGPT/Codex login opens an OpenAI OAuth flow; paste the callback URL or authorization code if browser callback is unavailable."
                    .to_string(),
            ),
            context_window: Some(272_000),
            model_context_windows: {
                let mut m = HashMap::new();
                m.insert("gpt-4.1".to_string(), 1_000_000);
                m.insert("gpt-4o".to_string(), 128_000);
                m.insert("gpt-4o-mini".to_string(), 128_000);
                m
            },
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::ChatGptResponses),
            model_aliases: HashMap::from([(
                "gpt-5.6".to_string(),
                "gpt-5.6-sol".to_string(),
            )]),
            model_defaults: ModelSpec::default(),
            model_specs: gpt_5_6_model_specs(false, false),
        },
        ProviderProfile {
            id: "openai".to_string(),
            builtin: None,
            display_name: "OpenAI API".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "https://api.openai.com".to_string(),
            default_model: "gpt-5".to_string(),
            models: vec![
                "gpt-5.6".to_string(),
                "gpt-5.6-sol".to_string(),
                "gpt-5.6-terra".to_string(),
                "gpt-5.6-luna".to_string(),
                "gpt-5".to_string(),
                "gpt-5-mini".to_string(),
                "gpt-4.1".to_string(),
                "gpt-4.1-mini".to_string(),
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "o3".to_string(),
                "o3-mini".to_string(),
                "o4-mini".to_string(),
            ],
            env_vars: vec!["OPENAI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://platform.openai.com/api-keys".to_string()),
            oauth_flow: None,
            notes: Some(
                "Use an OpenAI Platform API key (not ChatGPT OAuth). GPT-5.6 is the official alias for GPT-5.6 Sol."
                    .to_string(),
            ),
            context_window: Some(400_000),
            model_context_windows: {
                let mut m = HashMap::new();
                m.insert("gpt-4.1".to_string(), 1_000_000);
                m.insert("gpt-4.1-mini".to_string(), 1_000_000);
                m.insert("gpt-4o".to_string(), 128_000);
                m.insert("gpt-4o-mini".to_string(), 128_000);
                m
            },
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::OpenAiChatCompletions),
            model_aliases: HashMap::from([
                ("gpt56".to_string(), "gpt-5.6".to_string()),
                ("gpt56sol".to_string(), "gpt-5.6-sol".to_string()),
                ("gpt56terra".to_string(), "gpt-5.6-terra".to_string()),
                ("gpt56luna".to_string(), "gpt-5.6-luna".to_string()),
            ]),
            model_defaults: ModelSpec::default(),
            model_specs: gpt_5_6_model_specs(true, true),
        },
        ProviderProfile {
            id: "anthropic".to_string(),
            builtin: Some(ANTHROPIC_BUILTIN_PROFILE_MARKER.to_string()),
            display_name: "Anthropic".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: ANTHROPIC_BASE_URL.to_string(),
            default_model: "claude-sonnet-4-6".to_string(),
            models: vec![
                "claude-sonnet-4-6".to_string(),
                "claude-sonnet-5".to_string(),
                "claude-opus-5".to_string(),
                "claude-opus-4-8".to_string(),
                "claude-opus-4-7".to_string(),
                "claude-opus-4-6".to_string(),
                "claude-fable-5".to_string(),
                "claude-sonnet-4-5".to_string(),
                "claude-opus-4-1".to_string(),
                "claude-opus-4-0".to_string(),
                "claude-haiku-4-5".to_string(),
                "claude-3-5-haiku-latest".to_string(),
            ],
            env_vars: vec!["ANTHROPIC_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://console.anthropic.com/settings/keys".to_string()),
            oauth_flow: Some(OAuthFlow {
                authorize_url: "https://claude.ai/oauth/authorize".to_string(),
                token_url: "https://platform.claude.com/v1/oauth/token".to_string(),
                client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string(),
                scope: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload".to_string(),
                audience: String::new(),
                redirect_uri: Some("http://localhost:53692/callback".to_string()),
                callback_host: Some("127.0.0.1".to_string()),
                protocol: OAuthProtocol::AnthropicClaude,
            }),
            notes: Some("Claude Pro/Max subscription OAuth is the default /login flow. ANTHROPIC_API_KEY continues to use standard API-key billing.".to_string()),
            context_window: Some(200_000),
            model_context_windows: HashMap::from([
                ("claude-sonnet-5".to_string(), 1_000_000),
                ("claude-opus-5".to_string(), 1_000_000),
                ("claude-fable-5".to_string(), 1_000_000),
            ]),
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::AnthropicMessages),
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::new(),
        },
        ProviderProfile {
            id: "kimi".to_string(),
            builtin: Some(KIMI_BUILTIN_PROFILE_MARKER.to_string()),
            display_name: "Kimi Code".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: KIMI_CODE_BASE_URL.to_string(),
            default_model: "k3".to_string(),
            models: vec![
                "k3".to_string(),
                "k2p7".to_string(),
                "kimi-for-coding".to_string(),
                "kimi-for-coding-highspeed".to_string(),
                "kimi-k2-thinking".to_string(),
            ],
            env_vars: vec!["KIMI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://www.kimi.com/code/console".to_string()),
            oauth_flow: None,
            notes: Some(
                "Kimi Code plan provider. /login kimi opens the Kimi Code console to create an API key; KIMI_API_KEY is not the separately billed MOONSHOT_API_KEY."
                    .to_string(),
            ),
            context_window: Some(262_144),
            model_context_windows: HashMap::from([("k3".to_string(), 1_048_576)]),
            model_effort_levels: HashMap::from([("k3".to_string(), vec!["max".to_string()])]),
            request_contract: Some(RequestContract::AnthropicMessages),
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec {
                context_window: Some(262_144),
                max_output_tokens: Some(32_768),
                effort_levels: Vec::new(),
                reasoning_modes: Vec::new(),
                capabilities: ModelCapabilities {
                    tools: Some(true),
                    reasoning: Some(true),
                    image_input: Some(true),
                    prompt_cache: None,
                },
                pricing: None,
            },
            model_specs: HashMap::from([(
                "k3".to_string(),
                ModelSpec {
                    context_window: Some(1_048_576),
                    max_output_tokens: Some(131_072),
                    effort_levels: vec!["max".to_string()],
                    reasoning_modes: Vec::new(),
                    capabilities: ModelCapabilities {
                        tools: Some(true),
                        reasoning: Some(true),
                        image_input: Some(true),
                        prompt_cache: None,
                    },
                    pricing: None,
                },
            )]),
        },
        ProviderProfile {
            id: "deepseek".to_string(),
            builtin: None,
            display_name: "DeepSeek".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "https://api.deepseek.com".to_string(),
            default_model: "deepseek-chat".to_string(),
            models: vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()],
            env_vars: vec!["DEEPSEEK_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://platform.deepseek.com/api_keys".to_string()),
            oauth_flow: None,
            notes: Some("Uses DeepSeek's OpenAI-compatible API.".to_string()),
            context_window: Some(128_000),
            model_context_windows: HashMap::new(),
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::OpenAiChatCompletions),
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::new(),
        },
        ProviderProfile {
            id: "local".to_string(),
            builtin: None,
            display_name: "Local llama.cpp".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "http://127.0.0.1:8080".to_string(),
            default_model: DEFAULT_LOCAL_MODEL.to_string(),
            models: vec![DEFAULT_LOCAL_MODEL.to_string()],
            env_vars: Vec::new(),
            requires_api_key: false,
            login_url: None,
            oauth_flow: None,
            notes: Some("Local OpenAI-compatible llama.cpp server. Start one server on 127.0.0.1:8080 and select its model alias; Qwen3.8 selects qwen3.8-27b-ud-q5_k_xl when that server id is configured. No cloud credentials are used. Dext probes llama.cpp for the live runtime context window.".to_string()),
            context_window: None,
            model_context_windows: HashMap::new(),
            model_effort_levels: HashMap::new(),
            request_contract: Some(RequestContract::OpenAiChatCompletions),
            model_aliases: HashMap::from([(
                "qwen3.8".to_string(),
                "qwen3.8-27b-ud-q5_k_xl".to_string(),
            )]),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::new(),
        },
    ];
    hydrate_builtin_model_specs(&mut profiles);
    profiles
}

fn local_llama_cache() -> &'static Mutex<HashMap<String, u64>> {
    static CACHE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn is_local_llama_provider(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
) -> bool {
    if api_provider != ApiProvider::OpenAi {
        return false;
    }
    let lower = base_url.trim().to_ascii_lowercase();
    canonical_provider_id(provider_id) == "local"
        || lower.contains("127.0.0.1")
        || lower.contains("localhost")
}

fn local_llama_cache_key(base_url: &str, model: &str) -> String {
    format!(
        "{}|{}",
        base_url.trim().trim_end_matches('/').to_ascii_lowercase(),
        model.trim().to_ascii_lowercase()
    )
}

#[cfg(test)]
pub(crate) fn clear_cached_local_llama_context_windows() {
    if let Ok(mut cache) = local_llama_cache().lock() {
        cache.clear();
    }
}

fn llama_endpoint_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/v1") && !path.starts_with("/v1/") {
        format!("{}{}", base.trim_end_matches("/v1"), path)
    } else {
        format!("{base}{path}")
    }
}

fn context_field_score(key: &str) -> Option<u8> {
    let normalized = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "nctx" | "ctxsize" | "contextsize" | "contextlength" | "contextwindow" => Some(0),
        "maxcontextlength" | "maxcontexttokens" | "contexttokens" => Some(1),
        "nctxtrain" => Some(3),
        _ => None,
    }
}

fn collect_llama_context_candidates(value: &Value, best: &mut Option<(u8, u64)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if let Some(score) = context_field_score(key)
                    && let Some(tokens) = child
                        .as_u64()
                        .or_else(|| child.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                        .filter(|tokens| (512..=10_000_000).contains(tokens))
                    && best.is_none_or(|(best_score, best_tokens)| {
                        score < best_score || (score == best_score && tokens > best_tokens)
                    })
                {
                    *best = Some((score, tokens));
                }
                collect_llama_context_candidates(child, best);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_llama_context_candidates(child, best);
            }
        }
        _ => {}
    }
}

pub(crate) fn parse_llama_context_window(value: &Value) -> Option<u64> {
    let mut best = None;
    collect_llama_context_candidates(value, &mut best);
    best.map(|(_, tokens)| tokens)
}

fn provider_blocking_http_client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .no_gzip()
        .no_brotli()
        .http1_only()
}

fn fetch_llama_context_window(client: &reqwest::blocking::Client, base_url: &str) -> Option<u64> {
    for path in LLAMA_CONTEXT_DISCOVERY_PATHS {
        let Ok(resp) = client.get(llama_endpoint_url(base_url, path)).send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(json) = resp.json::<Value>() else {
            continue;
        };
        if let Some(tokens) = parse_llama_context_window(&json) {
            return Some(tokens);
        }
    }
    None
}

pub(crate) fn refresh_local_llama_context_window(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
    model: &str,
) -> Option<u64> {
    if !is_local_llama_provider(provider_id, api_provider, base_url) {
        return None;
    }
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        return None;
    }
    let endpoint_key = local_llama_cache_key(base_url, model);
    if let Some(tokens) = local_llama_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&endpoint_key).copied())
        .filter(|tokens| *tokens > 0)
    {
        return Some(tokens);
    }

    let base_url = base_url.to_string();
    let tokens = std::thread::Builder::new()
        .name("dext-local-context-probe".to_string())
        .spawn(move || {
            let client = provider_blocking_http_client_builder()
                .timeout(LLAMA_CONTEXT_DISCOVERY_TIMEOUT)
                .build()
                .ok()?;
            fetch_llama_context_window(&client, &base_url)
        })
        .ok()?
        .join()
        .ok()??;
    if let Ok(mut cache) = local_llama_cache().lock() {
        cache.insert(endpoint_key, tokens);
    }
    Some(tokens)
}

pub(crate) fn normalize_provider_profile(mut profile: ProviderProfile) -> Option<ProviderProfile> {
    profile.id = canonical_provider_id(&profile.id);
    if profile.id.trim().is_empty() {
        return None;
    }
    profile.builtin = match (profile.id.as_str(), profile.builtin.as_deref()) {
        ("kimi", Some(KIMI_BUILTIN_PROFILE_MARKER)) => {
            Some(KIMI_BUILTIN_PROFILE_MARKER.to_string())
        }
        ("anthropic", Some(ANTHROPIC_BUILTIN_PROFILE_MARKER)) => {
            Some(ANTHROPIC_BUILTIN_PROFILE_MARKER.to_string())
        }
        _ => None,
    };

    let fallback_model = if profile.id == "local" {
        DEFAULT_LOCAL_MODEL
    } else {
        "glm-5.2[1m]"
    };
    profile.base_url = profile.base_url.trim().trim_end_matches('/').to_string();
    if profile.display_name.trim().is_empty() {
        profile.display_name = profile.id.clone();
    }
    profile.request_contract = Some(request_contract_for_profile(&profile));
    profile.api_provider = request_contract_for_profile(&profile).api_provider();
    let mut normalized_aliases = HashMap::new();
    for (alias, target) in std::mem::take(&mut profile.model_aliases) {
        let alias = normalize_model_alias_key(&alias);
        let target = if request_contract_for_profile(&profile) == RequestContract::ChatGptResponses
        {
            normalize_chatgpt_model_slug(&target)
        } else {
            target.trim().to_string()
        };
        if !alias.is_empty() && !target.is_empty() && alias != target.to_ascii_lowercase() {
            normalized_aliases.insert(alias, target);
        }
    }
    profile.model_aliases = normalized_aliases;
    if profile.default_model.trim().is_empty() {
        profile.default_model = fallback_model.to_string();
    }
    profile.default_model = normalize_provider_model_value(&profile, &profile.default_model);
    let mut seen_models = HashSet::new();
    let mut models = Vec::new();
    for model in std::mem::take(&mut profile.models) {
        let normalized = normalize_provider_model_value(&profile, &model);
        if normalized.is_empty() {
            continue;
        }
        if seen_models.insert(normalized.to_ascii_lowercase()) {
            models.push(normalized);
        }
    }
    if !models.iter().any(|m| m == &profile.default_model) {
        models.insert(0, profile.default_model.clone());
    }
    profile.models = models;

    let mut normalized_context_windows = HashMap::new();
    for (model, window) in std::mem::take(&mut profile.model_context_windows) {
        if window == 0 {
            continue;
        }
        let key = normalize_provider_model_value(&profile, &model).to_ascii_lowercase();
        if !key.is_empty() {
            normalized_context_windows.insert(key, window);
        }
    }
    profile.model_context_windows = normalized_context_windows;

    let mut normalized_effort_levels = HashMap::new();
    for (model, levels) in std::mem::take(&mut profile.model_effort_levels) {
        let key = normalize_provider_model_value(&profile, &model).to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let mut seen_levels = HashSet::new();
        let levels = levels
            .into_iter()
            .map(|level| level.trim().to_ascii_lowercase())
            .filter(|level| !level.is_empty())
            .filter(|level| seen_levels.insert(level.clone()))
            .collect::<Vec<_>>();
        if !levels.is_empty() {
            normalized_effort_levels.insert(key, levels);
        }
    }
    profile.model_effort_levels = normalized_effort_levels;

    profile.model_defaults = normalize_model_spec(profile.model_defaults);
    let mut normalized_model_specs = HashMap::new();
    for (model, spec) in std::mem::take(&mut profile.model_specs) {
        let key = normalize_provider_model_value(&profile, &model).to_ascii_lowercase();
        if !key.is_empty() {
            normalized_model_specs.insert(key, normalize_model_spec(spec));
        }
    }
    profile.model_specs = normalized_model_specs;

    let mut seen_env = HashSet::new();
    profile.env_vars = profile
        .env_vars
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .filter(|v| seen_env.insert(v.clone()))
        .collect();

    Some(profile)
}

pub(crate) fn merge_provider_profile(
    mut builtin: ProviderProfile,
    stored: ProviderProfile,
) -> ProviderProfile {
    let builtin_id = canonical_provider_id(&builtin.id);
    let stored_default = stored.default_model.trim();
    if !stored_default.is_empty() {
        builtin.default_model = normalize_provider_model_value(&builtin, stored_default);
    }
    if let Some(contract) = stored.request_contract {
        builtin.request_contract = Some(contract);
        builtin.api_provider = contract.api_provider();
    }
    for (alias, target) in stored.model_aliases {
        builtin.model_aliases.insert(alias, target);
    }
    merge_model_spec(&mut builtin.model_defaults, stored.model_defaults);
    for (model, spec) in stored.model_specs {
        merge_model_spec(builtin.model_specs.entry(model).or_default(), spec);
    }

    if let Some(window) = stored.context_window.filter(|window| *window > 0) {
        builtin.context_window = Some(window);
    }

    for (model, window) in stored.model_context_windows {
        if window == 0 {
            continue;
        }
        let key = normalize_provider_model_value(&builtin, &model).to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        builtin.model_context_windows.insert(key, window);
    }

    for (model, levels) in stored.model_effort_levels {
        let key = normalize_provider_model_value(&builtin, &model).to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let mut seen_levels = HashSet::new();
        let levels = levels
            .into_iter()
            .map(|level| level.trim().to_ascii_lowercase())
            .filter(|level| !level.is_empty())
            .filter(|level| seen_levels.insert(level.clone()))
            .collect::<Vec<_>>();
        if !levels.is_empty() {
            builtin.model_effort_levels.insert(key, levels);
        }
    }

    let builtin_owned: HashSet<String> = built_in_provider_profiles()
        .into_iter()
        .flat_map(|profile| {
            profile
                .models
                .into_iter()
                .chain(std::iter::once(profile.default_model))
        })
        .map(|model| model.to_ascii_lowercase())
        .collect();
    let mut seen_models: HashSet<String> = builtin
        .models
        .iter()
        .map(|model| model.to_ascii_lowercase())
        .collect();
    let chatgpt_route = request_contract_for_profile(&builtin) == RequestContract::ChatGptResponses;
    let extra_models = stored
        .models
        .into_iter()
        .map(|model| {
            if chatgpt_route {
                normalize_chatgpt_model_slug(&model)
            } else {
                model.trim().to_string()
            }
        })
        .filter(|model| !model.is_empty())
        .filter(|model| {
            builtin_id == "chatgpt"
                || model
                    .to_ascii_lowercase()
                    .starts_with(&format!("{builtin_id}-"))
                || !builtin_owned.contains(&model.to_ascii_lowercase())
        })
        .filter(|model| seen_models.insert(model.to_ascii_lowercase()));
    builtin.models.extend(extra_models);

    if !builtin
        .models
        .iter()
        .any(|model| model == &builtin.default_model)
    {
        builtin.models.insert(0, builtin.default_model.clone());
    }
    builtin
}

fn validate_kimi_profile_provenance(catalog: &ProviderCatalog) -> Result<()> {
    if let Some(profile) = catalog.providers.iter().find(|profile| {
        canonical_provider_id(&profile.id) == "kimi"
            && profile.builtin.as_deref() != Some(KIMI_BUILTIN_PROFILE_MARKER)
    }) {
        anyhow::bail!(
            "provider id '{}' conflicts with the built-in Kimi Code provider; rename the custom profile before upgrading (reserved ids: kimi, kimi-code, kimi-coding, kimi-membership)",
            profile.id
        );
    }
    Ok(())
}

pub(crate) fn normalize_provider_catalog(mut catalog: ProviderCatalog) -> Result<ProviderCatalog> {
    validate_kimi_profile_provenance(&catalog)?;
    let legacy_catalog = catalog.version < PROVIDER_CATALOG_VERSION;
    let mut stored_by_id: HashMap<String, ProviderProfile> = HashMap::new();
    let mut providers: Vec<ProviderProfile> = Vec::new();
    let builtin_ids: HashSet<String> = built_in_provider_profiles()
        .into_iter()
        .map(|profile| canonical_provider_id(&profile.id))
        .collect();

    for mut profile in catalog.providers.drain(..) {
        if legacy_catalog {
            profile.request_contract = None;
            profile.model_aliases.clear();
            profile.model_defaults = ModelSpec::default();
            profile.model_specs.clear();
        }
        if let Some(profile) = normalize_provider_profile(profile) {
            stored_by_id.insert(canonical_provider_id(&profile.id), profile);
        }
    }

    for builtin in built_in_provider_profiles() {
        if let Some(builtin) = normalize_provider_profile(builtin) {
            let id = canonical_provider_id(&builtin.id);
            let merged = match stored_by_id.remove(&id) {
                Some(stored) => merge_provider_profile(builtin, stored),
                None => builtin,
            };
            providers.push(merged);
        }
    }

    for (id, profile) in stored_by_id {
        if builtin_ids.contains(&id) {
            continue;
        }
        if RETIRED_BUNDLED_PROVIDER_IDS.contains(&id.as_str()) {
            continue;
        }
        providers.push(profile);
    }

    if providers.is_empty() {
        providers = built_in_provider_profiles()
            .into_iter()
            .filter_map(normalize_provider_profile)
            .collect();
    }

    let active = canonical_provider_id(&catalog.active_provider);
    let active_exists = providers
        .iter()
        .any(|p| canonical_provider_id(&p.id) == active);
    let active_provider = if active_exists {
        active
    } else if providers
        .iter()
        .any(|p| canonical_provider_id(&p.id) == "glm")
    {
        "glm".to_string()
    } else {
        providers
            .first()
            .map(|p| p.id.clone())
            .unwrap_or_else(default_active_provider)
    };

    Ok(ProviderCatalog {
        version: PROVIDER_CATALOG_VERSION,
        active_provider,
        providers,
    })
}

pub(crate) fn default_provider_catalog() -> ProviderCatalog {
    normalize_provider_catalog(ProviderCatalog {
        version: PROVIDER_CATALOG_VERSION,
        active_provider: default_active_provider(),
        providers: built_in_provider_profiles(),
    })
    .expect("built-in provider catalog must be valid")
}

fn read_state_inspection_bytes(
    path: &Path,
    expected: &std::fs::Metadata,
) -> std::result::Result<(Vec<u8>, std::fs::Metadata), bool> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(path).map_err(|_| false)?;
    let opened = file.metadata().map_err(|_| false)?;
    if !opened.is_file() {
        return Err(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != expected.dev() || opened.ino() != expected.ino() {
            return Err(false);
        }
    }
    #[cfg(not(unix))]
    let _ = expected;

    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(STATE_INSPECTION_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| false)?;
    if bytes.len() as u64 > STATE_INSPECTION_MAX_BYTES {
        Err(true)
    } else {
        Ok((bytes, opened))
    }
}

pub(crate) fn inspect_provider_catalog() -> ProviderCatalogInspection {
    let path = provider_catalog_path();
    let empty = |integrity| ProviderCatalogInspection {
        path: path.clone(),
        integrity,
        active_provider: None,
        provider_count: None,
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ProviderCatalogInspection {
                path,
                integrity: ProviderCatalogIntegrity::Missing,
                active_provider: Some(default_active_provider()),
                provider_count: Some(built_in_provider_profiles().len()),
            };
        }
        Err(_) => return empty(ProviderCatalogIntegrity::Unreadable),
    };
    if metadata.file_type().is_symlink() {
        return empty(ProviderCatalogIntegrity::Symlink);
    }
    if !metadata.is_file() {
        return empty(ProviderCatalogIntegrity::NonRegular);
    }
    if metadata.len() > STATE_INSPECTION_MAX_BYTES {
        return empty(ProviderCatalogIntegrity::TooLarge);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !runtime_state_owned_by_current_user(&metadata) {
            return empty(ProviderCatalogIntegrity::UnsafeOwner);
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return empty(ProviderCatalogIntegrity::UnsafeMode { mode });
        }
    }
    let (bytes, opened) = match read_state_inspection_bytes(&path, &metadata) {
        Ok(result) => result,
        Err(true) => return empty(ProviderCatalogIntegrity::TooLarge),
        Err(false) => return empty(ProviderCatalogIntegrity::Unreadable),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !runtime_state_owned_by_current_user(&opened) {
            return empty(ProviderCatalogIntegrity::UnsafeOwner);
        }
        let mode = opened.mode() & 0o777;
        if mode & 0o022 != 0 {
            return empty(ProviderCatalogIntegrity::UnsafeMode { mode });
        }
    }
    #[cfg(not(unix))]
    let _ = &opened;
    let raw = match serde_json::from_slice::<Value>(&bytes) {
        Ok(raw) => raw,
        Err(_) => return empty(ProviderCatalogIntegrity::InvalidSchema),
    };
    if raw.is_array() {
        return match serde_json::from_value::<Vec<ProviderProfile>>(raw) {
            Ok(providers) => {
                let normalized = match normalize_provider_catalog(ProviderCatalog {
                    version: 1,
                    active_provider: default_active_provider(),
                    providers,
                }) {
                    Ok(catalog) => catalog,
                    Err(_) => return empty(ProviderCatalogIntegrity::InvalidSchema),
                };
                ProviderCatalogInspection {
                    path,
                    integrity: ProviderCatalogIntegrity::Valid {
                        version: 1,
                        legacy: true,
                    },
                    active_provider: Some(normalized.active_provider),
                    provider_count: Some(normalized.providers.len()),
                }
            }
            Err(_) => empty(ProviderCatalogIntegrity::InvalidSchema),
        };
    }
    let version = match raw.get("version") {
        None => PROVIDER_CATALOG_VERSION,
        Some(value) => match value.as_u64().and_then(|v| u32::try_from(v).ok()) {
            Some(version) => version,
            None => return empty(ProviderCatalogIntegrity::InvalidSchema),
        },
    };
    if version == 0 || version > PROVIDER_CATALOG_VERSION {
        return empty(ProviderCatalogIntegrity::UnsupportedVersion { version });
    }
    match serde_json::from_value::<ProviderCatalog>(raw) {
        Ok(catalog) => {
            let normalized = match normalize_provider_catalog(catalog) {
                Ok(catalog) => catalog,
                Err(_) => return empty(ProviderCatalogIntegrity::InvalidSchema),
            };
            ProviderCatalogInspection {
                path,
                integrity: ProviderCatalogIntegrity::Valid {
                    version,
                    legacy: version < PROVIDER_CATALOG_VERSION,
                },
                active_provider: Some(normalized.active_provider),
                provider_count: Some(normalized.providers.len()),
            }
        }
        Err(_) => empty(ProviderCatalogIntegrity::InvalidSchema),
    }
}

#[cfg(unix)]
fn runtime_state_owned_by_current_user(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.uid() == unsafe { libc::geteuid() }
}

fn read_runtime_state_file(path: &Path, secret: bool) -> Result<Option<String>> {
    #[cfg(not(unix))]
    let _ = secret;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting state file {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "state file must be a regular non-symlink file: {}",
            path.display()
        );
    }
    if metadata.len() > STATE_INSPECTION_MAX_BYTES {
        anyhow::bail!(
            "state file exceeds the {} byte limit: {}",
            STATE_INSPECTION_MAX_BYTES,
            path.display()
        );
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening state file {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("validating open state file {}", path.display()))?;
    if !opened.is_file() {
        anyhow::bail!(
            "opened state path is not a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            anyhow::bail!("state file changed while opening: {}", path.display());
        }
        if !runtime_state_owned_by_current_user(&opened) {
            anyhow::bail!(
                "state file is not owned by the current user: {}",
                path.display()
            );
        }
        let mode = opened.mode() & 0o777;
        if secret && mode & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("repairing owner-only mode on {}", path.display()))?;
        } else if !secret && mode & 0o022 != 0 {
            anyhow::bail!(
                "provider state has unsafe writable mode {mode:04o}; remove group/world write bits: {}",
                path.display()
            );
        }
    }

    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(STATE_INSPECTION_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading state file {}", path.display()))?;
    if bytes.len() as u64 > STATE_INSPECTION_MAX_BYTES {
        anyhow::bail!(
            "state file exceeds the {} byte limit: {}",
            STATE_INSPECTION_MAX_BYTES,
            path.display()
        );
    }
    String::from_utf8(bytes)
        .with_context(|| format!("state file is not valid UTF-8: {}", path.display()))
        .map(Some)
}

pub(crate) fn load_provider_catalog() -> Result<ProviderCatalog> {
    let path = provider_catalog_path();
    let Some(text) = read_runtime_state_file(&path, false)? else {
        return Ok(default_provider_catalog());
    };
    let raw: Value = serde_json::from_str(&text).context("invalid provider catalog JSON")?;

    let catalog = if raw.is_array() {
        ProviderCatalog {
            version: 1,
            active_provider: default_active_provider(),
            providers: serde_json::from_value(raw).context("invalid provider catalog JSON")?,
        }
    } else {
        let version = raw
            .get("version")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|version| u32::try_from(version).ok())
                    .context("provider catalog version must be a positive integer")
            })
            .transpose()?
            .unwrap_or(PROVIDER_CATALOG_VERSION);
        if version == 0 || version > PROVIDER_CATALOG_VERSION {
            anyhow::bail!(
                "unsupported provider catalog version {version} (supported: 1-{PROVIDER_CATALOG_VERSION})"
            );
        }
        serde_json::from_value::<ProviderCatalog>(raw).context("invalid provider catalog JSON")?
    };
    normalize_provider_catalog(catalog)
}

pub(crate) fn save_provider_catalog(catalog: &ProviderCatalog) -> Result<()> {
    let path = provider_catalog_path();
    let normalized = normalize_provider_catalog(catalog.clone())?;
    let bytes = serde_json::to_vec_pretty(&normalized)?;
    atomic_write_bytes(&path, &bytes)?;
    Ok(())
}

fn normalize_auth_store(mut store: AuthStore) -> AuthStore {
    store.version = AUTH_STORE_VERSION;
    let mut entries = std::mem::take(&mut store.providers)
        .into_iter()
        .collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut providers = HashMap::new();
    for (provider, credential) in entries {
        let canonical = canonical_provider_id(&provider);
        if canonical.is_empty() {
            continue;
        }
        if provider.trim().eq_ignore_ascii_case(&canonical) || !providers.contains_key(&canonical) {
            providers.insert(canonical, credential);
        }
    }
    store.providers = providers;
    store
}

#[cfg(test)]
#[test]
fn auth_store_normalization_prefers_canonical_id_over_aliases() {
    let normalized = normalize_auth_store(AuthStore {
        version: AUTH_STORE_VERSION,
        providers: HashMap::from([
            (
                "codex".to_string(),
                StoredCredential::ApiKey {
                    key: "alias-key".to_string(),
                },
            ),
            (
                "ChatGPT".to_string(),
                StoredCredential::ApiKey {
                    key: "canonical-key".to_string(),
                },
            ),
            (
                "openai-codex".to_string(),
                StoredCredential::ApiKey {
                    key: "second-alias-key".to_string(),
                },
            ),
        ]),
    });

    assert_eq!(normalized.providers.len(), 1);
    assert!(matches!(
        normalized.providers.get("chatgpt"),
        Some(StoredCredential::ApiKey { key }) if key == "canonical-key"
    ));
}

fn auth_store_declared_version(raw: &Value) -> Result<Option<u32>> {
    if raw.get("providers").is_none() && raw.get("version").is_none() {
        return Ok(None);
    }
    let version = raw
        .get("version")
        .map(|value| {
            value
                .as_u64()
                .and_then(|version| u32::try_from(version).ok())
                .context("auth store version must be a positive integer")
        })
        .transpose()?
        .unwrap_or(AUTH_STORE_VERSION);
    if version == 0 || version > AUTH_STORE_VERSION {
        anyhow::bail!("unsupported auth store version {version} (supported: {AUTH_STORE_VERSION})");
    }
    Ok(Some(version))
}

pub(crate) fn load_auth_store() -> Result<AuthStore> {
    let path = auth_store_path();
    let Some(text) = read_runtime_state_file(&path, true)? else {
        return Ok(AuthStore::default());
    };

    let raw: Value = serde_json::from_str(&text).context("invalid auth store JSON")?;

    if auth_store_declared_version(&raw)?.is_some() {
        let store: AuthStore = serde_json::from_value(raw).context("invalid auth store JSON")?;
        return Ok(normalize_auth_store(store));
    }

    let mut store = AuthStore::default();
    let object = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("auth store must be a JSON object"))?;

    for (provider, value) in object {
        if canonical_provider_id(provider).is_empty() {
            continue;
        }

        if let Some(cred) = parse_external_auth_credential(value) {
            store.providers.insert(provider.clone(), cred);
            continue;
        }

        if let Some(api_key) = value.as_str() {
            let trimmed = api_key.trim();
            if !trimmed.is_empty() {
                store.providers.insert(
                    provider.clone(),
                    StoredCredential::ApiKey {
                        key: trimmed.to_string(),
                    },
                );
            }
        }
    }

    Ok(normalize_auth_store(store))
}

pub(crate) fn inspect_auth_store() -> AuthStoreInspection {
    let path = auth_store_path();
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return AuthStoreInspection {
                path,
                security: AuthStoreFileSecurity::Missing,
                integrity: AuthStoreIntegrity::NotChecked,
            };
        }
        Err(_) => {
            return AuthStoreInspection {
                path,
                security: AuthStoreFileSecurity::Unreadable,
                integrity: AuthStoreIntegrity::NotChecked,
            };
        }
    };

    if metadata.file_type().is_symlink() {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::Symlink,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }
    if !metadata.is_file() {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::NonRegular,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }

    #[cfg(unix)]
    if !runtime_state_owned_by_current_user(&metadata) {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::UnsafeOwner,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }

    #[cfg(unix)]
    let security = {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            AuthStoreFileSecurity::Secure { mode }
        } else {
            AuthStoreFileSecurity::UnsafeMode { mode }
        }
    };
    #[cfg(windows)]
    let security = AuthStoreFileSecurity::WindowsAclNotEvaluated;
    #[cfg(not(any(unix, windows)))]
    let security = AuthStoreFileSecurity::PermissionsNotEvaluated;

    if metadata.len() > STATE_INSPECTION_MAX_BYTES {
        return AuthStoreInspection {
            path,
            security,
            integrity: AuthStoreIntegrity::TooLarge,
        };
    }
    let (bytes, opened) = match read_state_inspection_bytes(&path, &metadata) {
        Ok(result) => result,
        Err(too_large) => {
            return AuthStoreInspection {
                path,
                security,
                integrity: if too_large {
                    AuthStoreIntegrity::TooLarge
                } else {
                    AuthStoreIntegrity::Unreadable
                },
            };
        }
    };
    #[cfg(unix)]
    if !runtime_state_owned_by_current_user(&opened) {
        return AuthStoreInspection {
            path,
            security: AuthStoreFileSecurity::UnsafeOwner,
            integrity: AuthStoreIntegrity::NotChecked,
        };
    }
    #[cfg(unix)]
    let security = {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = opened.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            AuthStoreFileSecurity::Secure { mode }
        } else {
            AuthStoreFileSecurity::UnsafeMode { mode }
        }
    };
    #[cfg(not(unix))]
    let _ = &opened;
    let raw = match serde_json::from_slice::<Value>(&bytes) {
        Ok(raw) => raw,
        Err(_) => {
            return AuthStoreInspection {
                path,
                security,
                integrity: AuthStoreIntegrity::InvalidSchema,
            };
        }
    };
    let declared = match auth_store_declared_version(&raw) {
        Ok(declared) => declared,
        Err(_) => {
            let integrity = raw
                .get("version")
                .and_then(Value::as_u64)
                .and_then(|version| u32::try_from(version).ok())
                .filter(|version| *version == 0 || *version > AUTH_STORE_VERSION)
                .map_or(AuthStoreIntegrity::InvalidSchema, |version| {
                    AuthStoreIntegrity::UnsupportedVersion { version }
                });
            return AuthStoreInspection {
                path,
                security,
                integrity,
            };
        }
    };
    let valid_schema = if declared.is_some() {
        serde_json::from_value::<AuthStore>(raw).is_ok()
    } else {
        raw.is_object()
    };
    let integrity = if valid_schema {
        AuthStoreIntegrity::Valid {
            version: declared.unwrap_or(AUTH_STORE_VERSION),
            legacy: declared.is_none(),
        }
    } else {
        AuthStoreIntegrity::InvalidSchema
    };

    AuthStoreInspection {
        path,
        security,
        integrity,
    }
}

fn verify_saved_auth_store(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("verifying saved auth store {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("saved auth store is not a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "saved auth store has unsafe mode {mode:04o}; run chmod 600 {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn save_auth_store(store: &AuthStore) -> Result<()> {
    let path = auth_store_path();
    let normalized = normalize_auth_store(store.clone());
    let bytes = serde_json::to_vec_pretty(&normalized)?;
    atomic_write_secret(&path, &bytes)?;
    verify_saved_auth_store(&path)
}

pub(crate) fn is_env_var_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

pub(crate) fn command_secret_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn resolve_secret_reference(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(cmd) = trimmed.strip_prefix('!') {
        let key = cmd.trim().to_string();
        if key.is_empty() {
            return None;
        }
        if let Some(cached) = command_secret_cache()
            .lock()
            .ok()
            .and_then(|m| m.get(&key).cloned())
        {
            // A cached None records a prior failure; don't re-run the command.
            return cached;
        }

        let secret = crate::run_internal_secret_command(&key);
        if let Ok(mut cache) = command_secret_cache().lock() {
            cache.insert(key, secret.clone());
        }
        return secret;
    }

    if is_env_var_token(trimmed)
        && let Ok(v) = std::env::var(trimmed)
    {
        let val = v.trim().to_string();
        if !val.is_empty() {
            return Some(val);
        }
    }

    Some(trimmed.to_string())
}

impl StoredCredential {
    pub(crate) fn resolve_secret(&self) -> Option<String> {
        match self {
            StoredCredential::ApiKey { key } => resolve_secret_reference(key),
            StoredCredential::OAuth {
                access_token,
                expires_at,
                ..
            } => {
                if (*expires_at).is_some_and(|ts| ts > 0 && unix_timestamp_secs() >= ts) {
                    return None;
                }
                let t = access_token.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }
        }
    }
}

pub(crate) fn resolve_active_provider_id(catalog: &ProviderCatalog) -> String {
    if let Ok(v) = std::env::var("DEXT_PROVIDER") {
        let c = canonical_provider_id(&v);
        if !c.is_empty() {
            return c;
        }
    }
    if let Ok(v) = std::env::var("DEXT_PROFILE") {
        let c = canonical_provider_id(&v);
        if !c.is_empty() {
            return c;
        }
    }
    if let Ok(v) = std::env::var("DEXT_API_PROVIDER") {
        let lower = v.trim().to_ascii_lowercase();
        let canonical = canonical_provider_id(&lower);
        if lower == "anthropic" && find_provider_profile(catalog, "anthropic").is_none() {
            return "glm".to_string();
        }
        if find_provider_profile(catalog, &canonical).is_some() {
            return canonical;
        }
    }
    canonical_provider_id(&catalog.active_provider)
}

pub(crate) fn find_provider_profile(
    catalog: &ProviderCatalog,
    id: &str,
) -> Option<ProviderProfile> {
    let wanted = canonical_provider_id(id);
    catalog
        .providers
        .iter()
        .find(|p| canonical_provider_id(&p.id) == wanted)
        .cloned()
}

fn dext_api_key_override() -> Option<(String, String)> {
    let key = std::env::var("DEXT_API_KEY").ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| (key.to_string(), "env:DEXT_API_KEY".to_string()))
}

fn resolve_provider_auth(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> Option<(String, String, RuntimeAuthKind)> {
    if let Some((secret, source)) = dext_api_key_override() {
        return Some((secret, source, RuntimeAuthKind::ApiKey));
    }

    let canonical_id = canonical_provider_id(&profile.id);
    if let Some(entry) = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical_id))
        && !(canonical_id == "kimi" && matches!(entry, StoredCredential::OAuth { .. }))
        && let Some(secret) = entry.resolve_secret()
    {
        let kind = match entry {
            StoredCredential::ApiKey { .. } => RuntimeAuthKind::ApiKey,
            StoredCredential::OAuth { .. } => RuntimeAuthKind::OAuth,
        };
        return Some((secret, format!("auth:{}", profile.id), kind));
    }

    for env in &profile.env_vars {
        if let Ok(v) = std::env::var(env) {
            let t = v.trim().to_string();
            if !t.is_empty() {
                return Some((t, format!("env:{env}"), RuntimeAuthKind::ApiKey));
            }
        }
    }
    None
}

fn resolve_provider_credential(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> Option<(String, String)> {
    resolve_provider_auth(profile, store).map(|(secret, source, _)| (secret, source))
}

pub(crate) fn resolve_provider_api_key(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> Option<(String, String)> {
    resolve_provider_credential(profile, store)
}

pub(crate) fn request_contract_for_profile(profile: &ProviderProfile) -> RequestContract {
    profile
        .request_contract
        .unwrap_or_else(|| RequestContract::for_api_provider(profile.api_provider))
}

pub(crate) fn is_gpt_5_6_model(model: &str) -> bool {
    matches!(
        model.trim().to_ascii_lowercase().as_str(),
        "gpt-5.6" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
    )
}

fn is_official_openai_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("api.openai.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.path().trim_end_matches('/'), "" | "/v1")
}

pub(crate) fn official_openai_gpt_5_6_responses(
    profile: &ProviderProfile,
    base_url: &str,
    model: &str,
) -> bool {
    canonical_provider_id(&profile.id) == "openai"
        && is_official_openai_base_url(base_url)
        && is_gpt_5_6_model(model)
}

pub(crate) fn effective_request_contract(
    profile: &ProviderProfile,
    base_url: &str,
    model: &str,
) -> RequestContract {
    let configured = request_contract_for_profile(profile);
    if configured == RequestContract::OpenAiChatCompletions
        && official_openai_gpt_5_6_responses(profile, base_url, model)
    {
        RequestContract::OpenAiResponses
    } else {
        configured
    }
}

fn normalize_model_alias_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace(['_', ' '], "-")
}

fn normalize_effort_levels(levels: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    levels
        .into_iter()
        .map(|level| level.trim().to_ascii_lowercase())
        .filter(|level| !level.is_empty())
        .filter(|level| seen.insert(level.clone()))
        .collect()
}

fn merge_model_spec(base: &mut ModelSpec, overlay: ModelSpec) {
    if overlay.context_window.is_some() {
        base.context_window = overlay.context_window;
    }
    if overlay.max_output_tokens.is_some() {
        base.max_output_tokens = overlay.max_output_tokens;
    }
    if !overlay.effort_levels.is_empty() {
        base.effort_levels = overlay.effort_levels;
    }
    if !overlay.reasoning_modes.is_empty() {
        base.reasoning_modes = overlay.reasoning_modes;
    }
    for (target, value) in [
        (&mut base.capabilities.tools, overlay.capabilities.tools),
        (
            &mut base.capabilities.reasoning,
            overlay.capabilities.reasoning,
        ),
        (
            &mut base.capabilities.image_input,
            overlay.capabilities.image_input,
        ),
        (
            &mut base.capabilities.prompt_cache,
            overlay.capabilities.prompt_cache,
        ),
    ] {
        if value.is_some() {
            *target = value;
        }
    }
    if overlay.pricing.is_some() {
        base.pricing = overlay.pricing;
    }
}

fn normalize_model_spec(mut spec: ModelSpec) -> ModelSpec {
    spec.context_window = spec.context_window.filter(|value| *value > 0);
    spec.max_output_tokens = spec.max_output_tokens.filter(|value| *value > 0);
    spec.effort_levels = normalize_effort_levels(spec.effort_levels);
    spec.reasoning_modes = normalize_effort_levels(spec.reasoning_modes);
    spec.pricing = spec.pricing.filter(|pricing| {
        [
            pricing.input_usd_per_mtok,
            pricing.output_usd_per_mtok,
            pricing.cache_read_usd_per_mtok,
            pricing.cache_create_usd_per_mtok,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    });
    spec
}

pub(crate) fn resolve_model_spec(profile: &ProviderProfile, model: &str) -> ResolvedModelSpec {
    let model = normalize_provider_model_value(profile, model).to_ascii_lowercase();
    let explicit = profile.model_specs.get(&model);
    let defaults = &profile.model_defaults;
    let capability = |model_value: Option<bool>, default_value: Option<bool>, fallback: bool| {
        model_value.or(default_value).unwrap_or(fallback)
    };
    let effort_levels = explicit
        .filter(|spec| !spec.effort_levels.is_empty())
        .map(|spec| spec.effort_levels.clone())
        .or_else(|| {
            profile
                .model_effort_levels
                .get(&model)
                .filter(|levels| !levels.is_empty())
                .cloned()
        })
        .unwrap_or_else(|| defaults.effort_levels.clone());
    let reasoning_modes = explicit
        .filter(|spec| !spec.reasoning_modes.is_empty())
        .map(|spec| spec.reasoning_modes.clone())
        .unwrap_or_else(|| defaults.reasoning_modes.clone());
    let contract = request_contract_for_profile(profile);
    ResolvedModelSpec {
        context_window: explicit
            .and_then(|spec| spec.context_window)
            .or_else(|| profile.model_context_windows.get(&model).copied())
            .or(defaults.context_window)
            .or(profile.context_window),
        max_output_tokens: explicit
            .and_then(|spec| spec.max_output_tokens)
            .or(defaults.max_output_tokens),
        effort_levels,
        reasoning_modes,
        tools: capability(
            explicit.and_then(|spec| spec.capabilities.tools),
            defaults.capabilities.tools,
            true,
        ),
        reasoning: capability(
            explicit.and_then(|spec| spec.capabilities.reasoning),
            defaults.capabilities.reasoning,
            true,
        ),
        image_input: capability(
            explicit.and_then(|spec| spec.capabilities.image_input),
            defaults.capabilities.image_input,
            false,
        ),
        prompt_cache: capability(
            explicit.and_then(|spec| spec.capabilities.prompt_cache),
            defaults.capabilities.prompt_cache,
            matches!(
                contract,
                RequestContract::OpenAiResponses | RequestContract::ChatGptResponses
            ),
        ),
        pricing: explicit
            .and_then(|spec| spec.pricing.clone())
            .or_else(|| defaults.pricing.clone()),
        source: if explicit.is_some() {
            "model"
        } else if profile.model_specs.is_empty() && profile.model_defaults == ModelSpec::default() {
            "legacy"
        } else {
            "provider"
        },
    }
}

pub(crate) fn normalize_provider_model_value(profile: &ProviderProfile, model: &str) -> String {
    let trimmed = model.trim();
    let chatgpt_contract =
        request_contract_for_profile(profile) == RequestContract::ChatGptResponses;
    let normalized_input = if chatgpt_contract {
        normalize_chatgpt_model_slug(trimmed)
    } else {
        trimmed.split_whitespace().collect::<Vec<_>>().join("-")
    };
    let alias_key = normalize_model_alias_key(&normalized_input);
    if let Some(target) = profile.model_aliases.get(&alias_key) {
        return if chatgpt_contract {
            normalize_chatgpt_model_slug(target)
        } else {
            target.trim().to_string()
        };
    }
    if chatgpt_contract {
        return normalized_input;
    }

    let find_curated = |needle: &str| {
        std::iter::once(profile.default_model.as_str())
            .chain(profile.models.iter().map(String::as_str))
            .find(|candidate| candidate.eq_ignore_ascii_case(needle))
            .map(str::to_string)
    };
    if let Some(found) = find_curated(trimmed) {
        return found;
    }

    let hyphenated = normalized_input;
    if hyphenated != trimmed
        && let Some(found) = find_curated(&hyphenated)
    {
        return found;
    }

    if canonical_provider_id(&profile.id) == "glm"
        && !hyphenated.to_ascii_lowercase().starts_with("glm-")
        && !hyphenated.is_empty()
    {
        let prefixed = format!("glm-{hyphenated}");
        if let Some(found) = find_curated(&prefixed) {
            return found;
        }
        return prefixed;
    }

    if hyphenated.is_empty() {
        trimmed.to_string()
    } else {
        hyphenated
    }
}

pub(crate) fn resolve_provider_model(profile: &ProviderProfile) -> String {
    let provider_env = format!(
        "DEXT_MODEL_{}",
        canonical_provider_id(&profile.id)
            .replace('-', "_")
            .to_ascii_uppercase()
    );
    if let Ok(v) = std::env::var(&provider_env) {
        let t = normalize_provider_model_value(profile, &v);
        if !t.is_empty() {
            return t;
        }
    }

    if let Ok(v) = std::env::var("DEXT_MODEL") {
        let t = normalize_provider_model_value(profile, &v);
        if !t.is_empty() {
            let force = std::env::var("DEXT_MODEL_FORCE").ok().is_some_and(|raw| {
                let low = raw.trim().to_ascii_lowercase();
                !(low.is_empty() || low == "0" || low == "false" || low == "off" || low == "no")
            });
            let compatible = profile.models.is_empty() || profile.models.iter().any(|m| m == &t);
            if force || compatible {
                return t;
            }
            if !force
                && !compatible
                && request_contract_for_profile(profile).api_provider() == ApiProvider::OpenAi
                && !profile.requires_api_key
            {
                let looks_local = canonical_provider_id(&profile.id) == "local"
                    || profile.base_url.contains("127.0.0.1")
                    || profile.base_url.contains("localhost");
                if looks_local {
                    return t;
                }
            }
        }
    }
    normalize_provider_model_value(profile, &profile.default_model)
}

pub(crate) fn resolve_provider_base_url(profile: &ProviderProfile) -> String {
    if let Ok(v) = std::env::var("DEXT_BASE_URL") {
        let t = v.trim().trim_end_matches('/').to_string();
        if !t.is_empty() {
            return t;
        }
    }

    match request_contract_for_profile(profile).api_provider() {
        ApiProvider::OpenAi => {
            if let Ok(v) = std::env::var("OPENAI_BASE_URL") {
                let t = v.trim().trim_end_matches('/').to_string();
                if !t.is_empty() {
                    return t;
                }
            }
        }
        ApiProvider::ChatGpt => {}
        ApiProvider::Anthropic => {
            if let Ok(v) = std::env::var("ANTHROPIC_BASE_URL") {
                let t = v.trim().trim_end_matches('/').to_string();
                if !t.is_empty() {
                    return t;
                }
            }
        }
    }

    profile.base_url.trim_end_matches('/').to_string()
}

pub(crate) fn provider_request_url(base_url: &str, contract: RequestContract) -> String {
    let base = base_url.trim_end_matches('/');
    match contract {
        RequestContract::OpenAiChatCompletions => {
            if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            }
        }
        RequestContract::OpenAiResponses => {
            if base.ends_with("/v1") {
                format!("{base}/responses")
            } else {
                format!("{base}/v1/responses")
            }
        }
        RequestContract::ChatGptResponses => {
            if base.ends_with("/codex/responses") {
                base.to_string()
            } else if base.ends_with("/codex") {
                format!("{base}/responses")
            } else {
                format!("{base}/codex/responses")
            }
        }
        RequestContract::AnthropicMessages => {
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        }
    }
}

pub(crate) fn normalize_chatgpt_model_slug(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return "gpt-5.4".to_string();
    }

    let canonical = trimmed.to_ascii_lowercase().replace(['_', ' '], "-");
    if canonical == "auto" {
        return "gpt-5.4".to_string();
    }

    let compact: String = canonical
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    match compact.as_str() {
        "gpt5" => "gpt-5".to_string(),
        "gpt5mini" => "gpt-5-mini".to_string(),
        "gpt5codex" => "gpt-5-codex".to_string(),
        "gpt54" => "gpt-5.4".to_string(),
        "gpt54mini" => "gpt-5.4-mini".to_string(),
        "gpt55" => "gpt-5.5".to_string(),
        "gpt56" | "gpt56sol" => "gpt-5.6-sol".to_string(),
        "gpt56terra" => "gpt-5.6-terra".to_string(),
        "gpt56luna" => "gpt-5.6-luna".to_string(),
        "gpt53codex" => "gpt-5.3-codex".to_string(),
        "gpt53codexspark" => "gpt-5.3-codex-spark".to_string(),
        _ => canonical,
    }
}

pub(crate) fn chatgpt_client_user_agent() -> String {
    format!(
        "dext ({}; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

pub(crate) fn chatgpt_reasoning_effort(
    model: &str,
    effort: crate::ThinkingEffort,
) -> Option<&'static str> {
    let raw = effort.as_str();
    if is_gpt_5_6_model(model) {
        return Some(match effort {
            crate::ThinkingEffort::Off => "none",
            crate::ThinkingEffort::Minimal => "low",
            crate::ThinkingEffort::Low => "low",
            crate::ThinkingEffort::Medium => "medium",
            crate::ThinkingEffort::High => "high",
            crate::ThinkingEffort::XHigh | crate::ThinkingEffort::Max => "xhigh",
        });
    }
    if effort == crate::ThinkingEffort::Off {
        return None;
    }
    if effort == crate::ThinkingEffort::Minimal {
        return Some("low");
    }
    if model.starts_with("gpt-5.2")
        || model.starts_with("gpt-5.3")
        || model.starts_with("gpt-5.4")
        || model.starts_with("gpt-5.5")
        || model.starts_with("gpt-5-codex")
    {
        return Some(match raw {
            "minimal" => "low",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "xhigh" | "max" => "xhigh",
            _ => "medium",
        });
    }
    if model == "gpt-5.1-codex-mini" {
        return Some(match raw {
            "high" | "xhigh" | "max" => "high",
            _ => "medium",
        });
    }
    if model.starts_with("gpt-5.1") {
        return Some(match raw {
            "xhigh" | "max" => "high",
            other => other,
        });
    }
    Some(raw)
}

pub(crate) fn build_chatgpt_request(
    model: &str,
    reasoning_effort: Option<&str>,
    system_text: &str,
    session_id: &str,
    input: Vec<Value>,
    tools: Vec<Value>,
) -> Value {
    let model = normalize_chatgpt_model_slug(model);
    // The ChatGPT codex backend rejects max_output_tokens with HTTP 400
    // ("Unsupported parameter"), so this request must never carry an output
    // cap — not even from DEXT_MAX_OUTPUT_TOKENS or catalog model specs.
    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": system_text,
        "input": input,
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "medium" },
        "prompt_cache_key": session_id,
    });
    if let Some(effort) = reasoning_effort {
        body.as_object_mut()
            .expect("chatgpt request body is always an object")
            .insert(
                "reasoning".to_string(),
                json!({ "effort": effort, "summary": "auto" }),
            );
    }
    if !tools.is_empty() {
        let object = body
            .as_object_mut()
            .expect("chatgpt request body is always an object");
        object.insert("tool_choice".to_string(), json!("auto"));
        object.insert("parallel_tool_calls".to_string(), json!(true));
        object.insert("tools".to_string(), json!(tools));
    }
    body
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenAiResponsesReasoning<'a> {
    pub(crate) effort: &'a str,
    pub(crate) mode: Option<&'a str>,
    pub(crate) include_encrypted_content: bool,
}

pub(crate) fn build_openai_responses_request(
    model: &str,
    reasoning: Option<OpenAiResponsesReasoning<'_>>,
    system_text: &str,
    prompt_cache_key: &str,
    input: Vec<Value>,
    tools: Vec<Value>,
    max_output_tokens: u32,
) -> Value {
    let mut body = json!({
        "model": model.trim(),
        "store": false,
        "stream": true,
        "instructions": system_text,
        "input": input,
        "max_output_tokens": max_output_tokens,
        "text": { "verbosity": "medium" },
        "prompt_cache_key": prompt_cache_key,
    });
    if let Some(reasoning) = reasoning {
        body["reasoning"] = json!({
            "effort": reasoning.effort,
            "summary": "auto",
        });
        if reasoning.include_encrypted_content {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        if let Some(reasoning_mode) = reasoning.mode {
            body["reasoning"]["mode"] = json!(reasoning_mode);
        }
    }
    if !tools.is_empty() {
        let object = body
            .as_object_mut()
            .expect("OpenAI Responses request body is always an object");
        object.insert("tool_choice".to_string(), json!("auto"));
        object.insert("parallel_tool_calls".to_string(), json!(true));
        object.insert("tools".to_string(), json!(tools));
    }
    body
}

pub(crate) fn build_chatgpt_summary_request(
    model: &str,
    compact_system: &str,
    user_text: &str,
    reasoning_effort: Option<&str>,
) -> Value {
    let mut body = json!({
        "model": normalize_chatgpt_model_slug(model),
        "store": false,
        "stream": true,
        "instructions": compact_system,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": user_text }],
        }],
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "low" },
        "tool_choice": "none",
        "parallel_tool_calls": false,
    });
    if let Some(reasoning_effort) = reasoning_effort {
        body.as_object_mut()
            .expect("chatgpt summary request body is always an object")
            .insert(
                "reasoning".to_string(),
                json!({ "effort": reasoning_effort, "summary": "auto" }),
            );
    }
    body
}

fn is_marked_builtin_anthropic(profile: &ProviderProfile) -> bool {
    profile.builtin.as_deref() == Some(ANTHROPIC_BUILTIN_PROFILE_MARKER)
        && canonical_provider_id(&profile.id) == "anthropic"
}

pub(crate) fn is_builtin_anthropic_profile(profile: &ProviderProfile) -> bool {
    is_marked_builtin_anthropic(profile)
        && request_contract_for_profile(profile) == RequestContract::AnthropicMessages
}

pub(crate) fn is_official_anthropic_profile(profile: &ProviderProfile, base_url: &str) -> bool {
    is_builtin_anthropic_profile(profile) && base_url.trim_end_matches('/') == ANTHROPIC_BASE_URL
}

pub(crate) fn is_official_kimi_profile(profile: &ProviderProfile, base_url: &str) -> bool {
    profile.builtin.as_deref() == Some(KIMI_BUILTIN_PROFILE_MARKER)
        && canonical_provider_id(&profile.id) == "kimi"
        && request_contract_for_profile(profile) == RequestContract::AnthropicMessages
        && base_url.trim_end_matches('/') == KIMI_CODE_BASE_URL
}

pub(crate) fn apply_provider_headers(
    req: RequestBuilder,
    contract: RequestContract,
    api_key: &str,
    official_kimi: bool,
    anthropic_subscription: bool,
    extended_anthropic_cache: bool,
    session_id: Option<&str>,
) -> Result<RequestBuilder> {
    Ok(match contract.api_provider() {
        ApiProvider::OpenAi => {
            if api_key.trim().is_empty() {
                req
            } else {
                req.header("authorization", format!("Bearer {}", api_key))
            }
        }
        ApiProvider::ChatGpt => {
            let account_id = chatgpt_account_id_from_token(api_key).ok_or_else(|| {
                anyhow::anyhow!(
                    "could not extract chatgpt account id from token. re-run /login chatgpt."
                )
            })?;
            let mut req = req
                .header("authorization", format!("Bearer {}", api_key))
                .header("chatgpt-account-id", account_id)
                .header("originator", oauth_originator())
                .header("user-agent", chatgpt_client_user_agent())
                .header("openai-beta", "responses=experimental");
            if let Some(id) = session_id {
                req = req
                    .header("session_id", id)
                    .header("x-client-request-id", id);
            }
            req
        }
        ApiProvider::Anthropic => {
            let mut req = req.header("anthropic-version", ANTHROPIC_API_VERSION);
            if anthropic_subscription {
                let beta = if extended_anthropic_cache {
                    "claude-code-20250219,oauth-2025-04-20,extended-cache-ttl-2025-04-11"
                } else {
                    "claude-code-20250219,oauth-2025-04-20"
                };
                req = req
                    .header("authorization", format!("Bearer {api_key}"))
                    .header("anthropic-beta", beta)
                    .header("anthropic-dangerous-direct-browser-access", "true")
                    .header("user-agent", crate::claude_subscription::user_agent())
                    .header("x-app", "cli")
                    .header(
                        "x-client-request-id",
                        crate::claude_subscription::random_uuid_v4()?,
                    );
                if let Some(id) = session_id {
                    req = req.header("x-claude-code-session-id", id);
                }
                return Ok(req);
            }
            if extended_anthropic_cache {
                req = req.header("anthropic-beta", "extended-cache-ttl-2025-04-11");
            }
            if official_kimi {
                let version = env!("CARGO_PKG_VERSION");
                req = req
                    .header("user-agent", format!("dext/{version}"))
                    .header("x-msh-platform", "kimi_code_cli")
                    .header("x-msh-version", version)
                    .header("x-msh-device-name", kimi_device_name())
                    .header(
                        "x-msh-device-model",
                        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                    )
                    .header("x-msh-os-version", std::env::consts::OS)
                    .header("x-msh-device-id", kimi_device_id()?);
            }
            if api_key.trim().is_empty() {
                req
            } else {
                req.header("x-api-key", api_key)
            }
        }
    })
}

pub(crate) fn base64_url_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

pub(crate) fn decode_base64_url(input: &str) -> Option<Vec<u8>> {
    let mut bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    while !bytes.len().is_multiple_of(4) {
        bytes.push(b'=');
    }

    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() != 4 {
            return None;
        }
        let mut vals = [0u8; 4];
        let mut pad = 0usize;
        for (i, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                pad += 1;
            } else {
                vals[i] = base64_url_value(*byte)?;
            }
        }
        let n = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | vals[3] as u32;
        out.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
    }
    Some(out)
}

pub(crate) fn chatgpt_account_id_from_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = decode_base64_url(payload)?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

pub(crate) fn resolve_runtime_provider(
    selected: Option<&str>,
    require_credentials: bool,
) -> Result<ResolvedProviderConfig> {
    let catalog = load_provider_catalog()?;
    let store = load_auth_store()?;

    let provider_id = selected
        .map(canonical_provider_id)
        .unwrap_or_else(|| resolve_active_provider_id(&catalog));

    let mut profile = find_provider_profile(&catalog, &provider_id).ok_or_else(|| {
        let available = catalog
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("unknown provider '{provider_id}'. available: {available}")
    })?;

    let mut model = resolve_provider_model(&profile);

    // Stale-default auto-reroute: older builds could persist a model as the
    // default for the wrong provider (e.g. `/model glm-5.1` saved glm-5.1 as
    // chatgpt's default_model). On startup without an explicit provider, if
    // the resolved model isn't owned by this provider per the built-in
    // catalog (which is authoritative — the merged profile's `models` can be
    // polluted by a stale default_model auto-injection), redirect to the
    // built-in owner if it's authenticated. Without this, ChatGPT's Codex
    // backend returns HTTP 400 "The '<model>' model is not supported when
    // using Codex with a ChatGPT account." on the first turn after the stale
    // save.
    if selected.is_none() {
        let builtins = built_in_provider_profiles();
        let active_canonical = canonical_provider_id(&profile.id);
        let owned_by_active = builtins
            .iter()
            .find(|b| canonical_provider_id(&b.id) == active_canonical)
            .map(|b| b.models.iter().any(|m| m.eq_ignore_ascii_case(&model)))
            .unwrap_or(true);
        if !owned_by_active {
            let mut candidate: Option<(ProviderProfile, String)> = None;
            let mut ambiguous = false;
            for builtin in &builtins {
                if canonical_provider_id(&builtin.id) == active_canonical {
                    continue;
                }
                if !builtin
                    .models
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(&model))
                {
                    continue;
                }
                let Some(stored_profile) = find_provider_profile(&catalog, &builtin.id) else {
                    continue;
                };
                if !provider_has_available_credentials(&stored_profile, &store) {
                    continue;
                }
                if candidate.is_some() {
                    ambiguous = true;
                    break;
                }
                let normalized = normalize_provider_model_value(&stored_profile, &model);
                candidate = Some((stored_profile, normalized));
            }
            if !ambiguous && let Some((new_profile, new_model)) = candidate {
                profile = new_profile;
                model = new_model;
            }
        }
    }

    let base_url = resolve_provider_base_url(&profile);
    let mut store = store;
    let explicit_api_key = dext_api_key_override();
    let stored_oauth = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical_provider_id(&profile.id)))
        .is_some_and(|credential| matches!(credential, StoredCredential::OAuth { .. }));
    if explicit_api_key.is_none()
        && stored_oauth
        && is_marked_builtin_anthropic(&profile)
        && !is_official_anthropic_profile(&profile, &base_url)
    {
        anyhow::bail!(
            "Anthropic subscription OAuth is restricted to {ANTHROPIC_BASE_URL}. Unset the Anthropic base-URL override or run `dext auth logout anthropic` and use an API key for the custom endpoint."
        );
    }
    let refreshed_token = if explicit_api_key.is_some() {
        None
    } else {
        refresh_oauth_credential_if_needed(&profile, &mut store)?
    };
    let resolved_auth = explicit_api_key
        .map(|(key, source)| (key, source, RuntimeAuthKind::ApiKey))
        .or_else(|| {
            refreshed_token.map(|token| {
                (
                    token,
                    format!("auth:{} (refreshed)", profile.id),
                    RuntimeAuthKind::OAuth,
                )
            })
        })
        .or_else(|| resolve_provider_auth(&profile, &store));

    let (api_key, key_source, auth_kind) = match (profile.requires_api_key, resolved_auth) {
        (_, Some((key, source, kind))) => (key, source, kind),
        (false, None) => (
            String::new(),
            "none (provider does not require key)".to_string(),
            RuntimeAuthKind::None,
        ),
        (true, None) if !require_credentials => (
            String::new(),
            "missing (login required)".to_string(),
            RuntimeAuthKind::None,
        ),
        (true, None) => {
            let env_hint = if profile.env_vars.is_empty() {
                "(no env vars configured)".to_string()
            } else {
                profile.env_vars.join(" or ")
            };
            anyhow::bail!(
                "missing credentials for provider '{}'. Run `dext auth login {}` or set {}.",
                profile.id,
                profile.id,
                env_hint
            );
        }
    };

    if auth_kind == RuntimeAuthKind::OAuth
        && is_marked_builtin_anthropic(&profile)
        && !is_official_anthropic_profile(&profile, &base_url)
    {
        anyhow::bail!(
            "Anthropic subscription OAuth is restricted to {ANTHROPIC_BASE_URL}. Unset the Anthropic base-URL override or run `dext auth logout anthropic` and use an API key for the custom endpoint."
        );
    }

    Ok(ResolvedProviderConfig {
        requires_api_key: profile.requires_api_key,
        profile,
        api_key,
        auth_kind,
        key_source,
        model,
        base_url,
    })
}

pub(crate) fn provider_auth_status(profile: &ProviderProfile, store: &AuthStore) -> String {
    let canonical = canonical_provider_id(&profile.id);
    if let Some(entry) = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical))
        && !(canonical == "kimi" && matches!(entry, StoredCredential::OAuth { .. }))
    {
        let state = if entry.resolve_secret().is_some() {
            "auth"
        } else {
            "auth(unresolved)"
        };
        return state.to_string();
    }
    for env in &profile.env_vars {
        if std::env::var(env)
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
        {
            return format!("env:{env}");
        }
    }
    if profile.requires_api_key {
        "missing".to_string()
    } else {
        "not-required".to_string()
    }
}

pub(crate) fn provider_has_available_credentials(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> bool {
    !profile.requires_api_key || resolve_provider_api_key(profile, store).is_some()
}

pub(crate) fn curated_provider_models(profile: &ProviderProfile) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for candidate in std::iter::once(profile.default_model.as_str())
        .chain(profile.models.iter().map(String::as_str))
    {
        let normalized = normalize_provider_model_value(profile, candidate);
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.to_ascii_lowercase()) {
            models.push(normalized);
        }
    }
    models
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderModelRef {
    pub(crate) provider_id: String,
    pub(crate) model: String,
}

fn pick_matching_model(
    matches: &[ProviderModelRef],
    active_provider: &str,
) -> Result<Option<ProviderModelRef>> {
    if matches.is_empty() {
        return Ok(None);
    }
    let active_provider = canonical_provider_id(active_provider);
    if let Some(active) = matches
        .iter()
        .find(|m| canonical_provider_id(&m.provider_id) == active_provider)
    {
        return Ok(Some(active.clone()));
    }
    if matches.len() == 1 {
        return Ok(Some(matches[0].clone()));
    }
    let rendered = matches
        .iter()
        .map(|m| format!("{}/{}", m.provider_id, m.model))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!("model matches multiple providers: {rendered}. use /model <provider>/<model>.");
}

pub(crate) fn resolve_provider_model_selection(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active_provider: &str,
    selector: &str,
) -> Result<ProviderModelRef> {
    let selector = selector.trim();
    if selector.is_empty() {
        anyhow::bail!("model selector cannot be empty");
    }

    for sep in ['/', ':'] {
        if let Some((provider_sel, model_sel)) = selector.split_once(sep) {
            if provider_sel.trim().is_empty() {
                continue;
            }
            if let Ok(provider_id) = provider_id_from_selector(catalog, provider_sel) {
                let profile = find_provider_profile(catalog, &provider_id)
                    .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
                let model = normalize_provider_model_value(&profile, model_sel);
                if model.is_empty() {
                    anyhow::bail!("model selector cannot be empty");
                }
                if profile.requires_api_key && !provider_has_available_credentials(&profile, store)
                {
                    anyhow::bail!(
                        "provider '{provider_id}' is not authenticated. Run `/login {provider_id}` first."
                    );
                }
                return Ok(ProviderModelRef { provider_id, model });
            }
        }
    }

    let mut auth_matches = Vec::new();
    let mut all_matches = Vec::new();
    for profile in &catalog.providers {
        let normalized = normalize_provider_model_value(profile, selector);
        if normalized.is_empty() {
            continue;
        }
        let alias_key = normalize_model_alias_key(selector);
        let matches_alias = profile.model_aliases.contains_key(&alias_key);
        let matches_curated = matches_alias
            || curated_provider_models(profile)
                .iter()
                .any(|model| model.eq_ignore_ascii_case(&normalized));
        if !matches_curated {
            continue;
        }
        let matched = ProviderModelRef {
            provider_id: canonical_provider_id(&profile.id),
            model: normalized,
        };
        if provider_has_available_credentials(profile, store) {
            auth_matches.push(matched.clone());
        }
        all_matches.push(matched);
    }

    if let Some(matched) = pick_matching_model(&auth_matches, active_provider)? {
        return Ok(matched);
    }
    if let Some(matched) = pick_matching_model(&all_matches, active_provider)? {
        return Ok(matched);
    }

    let active_provider = canonical_provider_id(active_provider);
    let profile = find_provider_profile(catalog, &active_provider)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{active_provider}'"))?;
    let model = normalize_provider_model_value(&profile, selector);
    if model.is_empty() {
        anyhow::bail!("model selector cannot be empty");
    }
    Ok(ProviderModelRef {
        provider_id: active_provider,
        model,
    })
}

pub(crate) fn render_provider_list(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active: &str,
) -> String {
    let mut lines = Vec::new();
    for profile in &catalog.providers {
        let marker = if canonical_provider_id(&profile.id) == canonical_provider_id(active) {
            "*"
        } else {
            " "
        };
        let name = if profile.display_name.trim().is_empty() {
            profile.id.clone()
        } else {
            profile.display_name.clone()
        };
        let status = provider_auth_status(profile, store);
        let contract = request_contract_for_profile(profile);
        let spec = resolve_model_spec(profile, &profile.default_model);
        lines.push(format!(
            "{marker} {:<12} {:<18} model={} contract={} api={} spec={} auth={} base={}",
            profile.id,
            name,
            profile.default_model,
            contract.as_str(),
            contract.api_provider().as_str(),
            spec.source,
            status,
            profile.base_url
        ));
    }
    lines.join("\n")
}

pub(crate) fn render_provider_picker(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active: &str,
) -> String {
    let mut lines = Vec::new();
    for (i, profile) in catalog.providers.iter().enumerate() {
        let marker = if canonical_provider_id(&profile.id) == canonical_provider_id(active) {
            "*"
        } else {
            " "
        };
        let status = provider_auth_status(profile, store);
        let contract = request_contract_for_profile(profile);
        let spec = resolve_model_spec(profile, &profile.default_model);
        lines.push(format!(
            "{:>2}) {} {:<10} model={:<16} contract={} spec={} auth={}",
            i + 1,
            marker,
            profile.id,
            profile.default_model,
            contract.as_str(),
            spec.source,
            status
        ));
    }
    lines.join("\n")
}

pub(crate) fn provider_id_from_selector(
    catalog: &ProviderCatalog,
    selector: &str,
) -> Result<String> {
    let token = selector.trim();
    if token.is_empty() {
        anyhow::bail!("provider selector cannot be empty");
    }

    if let Ok(index) = token.parse::<usize>() {
        if index == 0 || index > catalog.providers.len() {
            anyhow::bail!(
                "provider index {} out of range 1..={}",
                index,
                catalog.providers.len()
            );
        }
        return Ok(canonical_provider_id(&catalog.providers[index - 1].id));
    }

    let canonical = canonical_provider_id(token);
    if find_provider_profile(catalog, &canonical).is_none() {
        let available = catalog
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("unknown provider '{token}'. available: {available}");
    }
    Ok(canonical)
}

pub(crate) fn external_auth_path() -> PathBuf {
    if let Ok(path) = std::env::var("DEXT_EXTERNAL_AUTH_FILE") {
        return PathBuf::from(path);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dext").join("external-auth.json")
}

pub(crate) fn parse_expiry_epoch_seconds(v: &Value) -> Option<u64> {
    let mut epoch = if let Some(n) = v.as_u64() {
        n
    } else {
        v.as_str()?.trim().parse::<u64>().ok()?
    };
    if epoch > 1_000_000_000_000 {
        epoch /= 1000;
    }
    Some(epoch)
}

pub(crate) fn parse_external_auth_credential(value: &Value) -> Option<StoredCredential> {
    if let Ok(cred) = serde_json::from_value::<StoredCredential>(value.clone()) {
        return Some(cred);
    }

    if let Some(raw) = value.as_str() {
        let key = raw.trim();
        if !key.is_empty() {
            return Some(StoredCredential::ApiKey {
                key: key.to_string(),
            });
        }
    }

    let key = value
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    if let Some(key) = key {
        return Some(StoredCredential::ApiKey { key });
    }

    let access = value
        .get("access_token")
        .or_else(|| value.get("access"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let access_token = access?;

    let refresh_token = value
        .get("refresh_token")
        .or_else(|| value.get("refresh"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    let expires_at = value
        .get("expires_at")
        .or_else(|| value.get("expires"))
        .and_then(parse_expiry_epoch_seconds);

    Some(StoredCredential::OAuth {
        access_token,
        refresh_token,
        expires_at,
    })
}

pub(crate) fn external_auth_candidates(provider_id: &str) -> Vec<String> {
    let canonical = canonical_provider_id(provider_id);
    let mut candidates: Vec<String> = match canonical.as_str() {
        "chatgpt" => vec!["openai-codex", "codex", "chatgpt", "openai"],
        "openai" => vec!["openai"],
        "glm" => vec!["zai", "glm", "bigmodel", "anthropic"],
        "anthropic" => vec!["anthropic", "claude"],
        other => vec![other],
    }
    .into_iter()
    .map(str::to_string)
    .collect();

    if !candidates.iter().any(|c| c == &canonical) {
        candidates.push(canonical);
    }
    candidates
}

pub(crate) fn import_provider_credential_from_external(
    profile: &ProviderProfile,
    store: &mut AuthStore,
) -> Option<String> {
    let path = external_auth_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let raw: Value = serde_json::from_str(&text).ok()?;
    let object = raw.as_object()?;

    for candidate in external_auth_candidates(&profile.id) {
        let Some(value) = object.get(&candidate) else {
            continue;
        };
        let Some(credential) = parse_external_auth_credential(value) else {
            continue;
        };
        store
            .providers
            .insert(canonical_provider_id(&profile.id), credential);
        return Some(format!("external-auth:{candidate}"));
    }

    None
}

pub(crate) fn login_arg_requests_web_flow(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "web" | "--web" | "browser" | "--browser" | "reauth" | "--reauth"
    )
}

pub(crate) fn login_arg_requests_import(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "import" | "--import" | "reuse" | "--reuse"
    )
}

pub(crate) fn looks_like_login_secret_input(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    if extract_oauth_code_from_callback(trimmed).is_some() {
        return true;
    }
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return true;
    }
    if trimmed.contains("accessToken") || trimmed.contains("access_token") {
        return true;
    }
    normalize_login_secret(trimmed)
        .is_some_and(|secret| secret.len() >= 20 && !secret.contains(char::is_whitespace))
}

pub(crate) fn normalize_login_secret(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if login_arg_requests_web_flow(trimmed) {
        return None;
    }

    let bearer_prefix = "bearer ";
    if trimmed.len() > bearer_prefix.len()
        && trimmed
            .get(..bearer_prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(bearer_prefix))
    {
        return Some(trimmed[bearer_prefix.len()..].trim().to_string());
    }

    if trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && let Some(token) = v
            .get("accessToken")
            .or_else(|| v.get("access_token"))
            .or_else(|| v.get("access"))
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return Some(token.to_string());
    }

    Some(trimmed.to_string())
}

pub(crate) fn validate_login_secret_for_provider(
    profile: &ProviderProfile,
    secret: &str,
) -> Result<()> {
    if request_contract_for_profile(profile) == RequestContract::ChatGptResponses
        && chatgpt_account_id_from_token(secret).is_none()
    {
        anyhow::bail!(
            "invalid ChatGPT access token format. paste the 'accessToken' JWT from chatgpt.com/api/auth/session or the full JSON session blob."
        );
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn running_in_wsl() -> bool {
    if std::env::var("WSL_DISTRO_NAME")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return true;
    }
    if std::env::var("WSL_INTEROP")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .map(|s| s.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

fn browser_launcher_timeout() -> Duration {
    let seconds = std::env::var("DEXT_BROWSER_OPEN_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| (1..=30).contains(seconds))
        .unwrap_or(10);
    Duration::from_secs(seconds)
}

fn run_browser_launcher(mut command: Command, label: &str) -> Result<crate::InternalCommandOutput> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::run_internal_command_limited(command, label, browser_launcher_timeout())
        .map_err(anyhow::Error::msg)
}

pub(crate) fn open_url_in_browser(url: &str) -> Result<String> {
    if std::env::var("DEXT_SKIP_BROWSER_OPEN")
        .ok()
        .is_some_and(|raw| {
            let low = raw.trim().to_ascii_lowercase();
            !(low.is_empty() || low == "0" || low == "false" || low == "off" || low == "no")
        })
    {
        return Ok("disabled-by-env".to_string());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    if running_in_wsl()
        && std::env::var("DEXT_DISABLE_WSL_BROWSER_OPEN")
            .ok()
            .is_some_and(|raw| {
                let low = raw.trim().to_ascii_lowercase();
                !(low.is_empty() || low == "0" || low == "false" || low == "off" || low == "no")
            })
    {
        return Ok("disabled-by-wsl".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let powershell_cmd = format!("Start-Process '{}'", url.replace('\'', "''"));
        let mut command = Command::new("powershell");
        command
            .args(["-NoProfile", "-Command", &powershell_cmd])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let output = run_browser_launcher(command, "powershell browser launcher")
            .context("failed to launch browser via powershell Start-Process")?;
        if !output.success() {
            anyhow::bail!("browser launcher exited with status {}", output.code);
        }
        Ok("powershell Start-Process".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let output = run_browser_launcher(command, "macOS browser launcher")
            .context("failed to launch browser via open")?;
        if !output.success() {
            anyhow::bail!("browser launcher exited with status {}", output.code);
        }
        Ok("open".to_string())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut launchers: Vec<(String, Vec<String>)> = vec![
            ("xdg-open".to_string(), vec![url.to_string()]),
            ("gio".to_string(), vec!["open".to_string(), url.to_string()]),
            ("sensible-browser".to_string(), vec![url.to_string()]),
            ("x-www-browser".to_string(), vec![url.to_string()]),
        ];
        if running_in_wsl() {
            let powershell_cmd = format!("Start-Process '{}'", url.replace('\'', "''"));
            launchers.splice(
                0..0,
                [
                    ("wslview".to_string(), vec![url.to_string()]),
                    (
                        "powershell.exe".to_string(),
                        vec![
                            "-NoProfile".to_string(),
                            "-Command".to_string(),
                            powershell_cmd,
                        ],
                    ),
                ],
            );
        }

        let mut errors = Vec::new();
        for (bin, args) in launchers {
            let mut command = Command::new(&bin);
            command
                .args(&args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            match run_browser_launcher(command, &format!("{bin} browser launcher")) {
                Ok(output) if output.success() => return Ok(bin),
                Ok(output) => errors.push(format!("{bin} exited {}", output.code)),
                Err(error) => errors.push(format!("{bin}: {error}")),
            }
        }

        anyhow::bail!(
            "could not auto-open URL; tried launchers: {}",
            errors.join("; ")
        );
    }
}

pub(crate) fn prompt_input_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    if line.trim_start().starts_with('{') {
        loop {
            let trimmed = line.trim();
            if trimmed.ends_with('}') {
                break;
            }
            let mut extra = String::new();
            let read = io::stdin().read_line(&mut extra)?;
            if read == 0 {
                break;
            }
            line.push_str(&extra);
        }
    }

    Ok(line.trim().to_string())
}

pub(crate) fn set_active_provider_in_catalog(provider_id: &str) -> Result<()> {
    let mut catalog = load_provider_catalog()?;
    let canonical = canonical_provider_id(provider_id);
    let profile = find_provider_profile(&catalog, &canonical)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
    catalog.active_provider = canonical_provider_id(&profile.id);
    save_provider_catalog(&catalog)
}

pub(crate) fn set_provider_default_model_in_catalog(provider_id: &str, model: &str) -> Result<()> {
    let model = model.trim();
    if model.is_empty() {
        anyhow::bail!("model cannot be empty");
    }

    let mut catalog = load_provider_catalog()?;
    let canonical = canonical_provider_id(provider_id);
    let Some(profile) = catalog
        .providers
        .iter_mut()
        .find(|p| canonical_provider_id(&p.id) == canonical)
    else {
        anyhow::bail!("unknown provider '{provider_id}'");
    };

    let normalized = normalize_provider_model_value(profile, model);
    profile.default_model = normalized.clone();
    if !profile
        .models
        .iter()
        .any(|m| m.eq_ignore_ascii_case(&normalized))
    {
        profile.models.insert(0, normalized);
    }
    catalog.active_provider = canonical;

    save_provider_catalog(&catalog)
}

#[derive(Debug)]
pub(crate) struct LoginResult {
    pub(crate) message: String,
    pub(crate) provider_id: String,
    pub(crate) awaiting_credentials: bool,
}

fn generate_code_verifier() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).context("generate OAuth PKCE verifier")?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_code_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

fn generate_oauth_state() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("generate OAuth state")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn oauth_html_response(status_line: &str, body: &str) -> String {
    format!(
        "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn oauth_result_page(success: bool) -> String {
    let (eyebrow, title, detail, accent, mark) = if success {
        (
            "AUTHENTICATION COMPLETE",
            "You're signed in",
            "Dext received and stored your provider credentials. You can close this tab and return to your terminal.",
            "#72e0a8",
            "&#10003;",
        )
    } else {
        (
            "AUTHENTICATION INCOMPLETE",
            "Return to Dext",
            "The callback was received, but Dext could not finish the login. Check the terminal for details and retry there.",
            "#ff8f8f",
            "!",
        )
    };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Dext · {title}</title><style>:root{{color-scheme:dark}}*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:24px;background:#090b10;color:#edf2f7;font:16px/1.55 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}}main{{width:min(560px,100%);padding:38px;border:1px solid #28303d;border-radius:18px;background:linear-gradient(145deg,#121722,#0d1119);box-shadow:0 24px 80px #0009}}.brand{{margin-bottom:34px;color:#a9b4c5;letter-spacing:.16em}}.brand b{{color:#f5f7fa}}.mark{{display:grid;place-items:center;width:54px;height:54px;margin-bottom:22px;border:1px solid {accent};border-radius:14px;color:{accent};font-size:28px;font-weight:700;box-shadow:0 0 32px color-mix(in srgb,{accent} 24%,transparent)}}.eyebrow{{margin:0 0 8px;color:{accent};font-size:12px;font-weight:700;letter-spacing:.14em}}h1{{margin:0 0 14px;font:600 clamp(28px,7vw,42px)/1.15 ui-sans-serif,system-ui,sans-serif;letter-spacing:-.03em}}p{{margin:0;color:#aeb8c7}}.foot{{margin-top:30px;padding-top:20px;border-top:1px solid #252d39;color:#778397;font-size:13px}}</style></head><body><main><div class="brand"><b>DEXT</b> / LOGIN</div><div class="mark">{mark}</div><p class="eyebrow">{eyebrow}</p><h1>{title}</h1><p>{detail}</p><div class="foot">This window contains no credential data.</div></main></body></html>"#
    )
}

fn await_oauth_browser_result(
    receiver: &std::sync::mpsc::Receiver<OAuthBrowserResult>,
    cancelled: &std::sync::atomic::AtomicBool,
    timeout: Duration,
) -> Option<OAuthBrowserResult> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(result) => return Some(result),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum OAuthBrowserResult {
    Success,
    Failure,
}

enum OAuthCallbackKind {
    Code(String),
    Rejected,
}

struct ReceivedOAuthCallback {
    kind: OAuthCallbackKind,
    browser_result: std::sync::mpsc::Sender<OAuthBrowserResult>,
}

struct OAuthCallbackListener {
    receiver: std::sync::mpsc::Receiver<ReceivedOAuthCallback>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OAuthCallbackListener {
    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<ReceivedOAuthCallback, std::sync::mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl Drop for OAuthCallbackListener {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn spawn_oauth_code_listener(
    listener: std::net::TcpListener,
    expected_state: String,
    expected_path: String,
) -> Result<OAuthCallbackListener> {
    listener
        .set_nonblocking(true)
        .context("configure OAuth callback listener")?;
    let (tx, receiver) = std::sync::mpsc::channel::<ReceivedOAuthCallback>();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_cancelled = cancelled.clone();

    let thread = std::thread::spawn(move || {
        while !thread_cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(_) => break,
            };
            use std::io::{Read, Write};

            let request_started = std::time::Instant::now();
            if stream.set_nonblocking(false).is_err()
                || stream
                    .set_write_timeout(Some(OAUTH_CALLBACK_IO_TIMEOUT))
                    .is_err()
            {
                continue;
            }
            let mut request = Vec::with_capacity(1024);
            let mut buf = [0u8; 1024];
            let mut headers_complete = false;
            while request.len() < 8192 {
                let read_timeout =
                    OAUTH_CALLBACK_IO_TIMEOUT.saturating_sub(request_started.elapsed());
                if read_timeout.is_zero() {
                    break;
                }
                if stream.set_read_timeout(Some(read_timeout)).is_err() {
                    break;
                }
                let remaining = 8192 - request.len();
                let read_len = remaining.min(buf.len());
                match stream.read(&mut buf[..read_len]) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            headers_complete = true;
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&request);
            let path = headers_complete
                .then(|| req.lines().next())
                .flatten()
                .and_then(|line| {
                    let mut parts = line.split_whitespace();
                    let method = parts.next()?;
                    let path = parts.next()?;
                    let version = parts.next()?;
                    (method == "GET"
                        && matches!(version, "HTTP/1.0" | "HTTP/1.1")
                        && parts.next().is_none())
                    .then_some(path)
                });

            let mut accepted_callback = false;
            let response = if let Some(url) =
                path.and_then(|path| reqwest::Url::parse(&format!("http://localhost{path}")).ok())
            {
                if url.path() != expected_path {
                    oauth_html_response(
                        "HTTP/1.1 404 Not Found",
                        "<html><body><h2>Login failed</h2><p>Callback route not found.</p></body></html>",
                    )
                } else {
                    let returned_state = url
                        .query_pairs()
                        .find(|(key, _)| key == "state")
                        .map(|(_, value)| value.into_owned());
                    if returned_state.as_deref() != Some(expected_state.as_str()) {
                        oauth_html_response(
                            "HTTP/1.1 400 Bad Request",
                            "<html><body><h2>Login failed</h2><p>State mismatch.</p></body></html>",
                        )
                    } else if let Some(code) = url
                        .query_pairs()
                        .find(|(key, _)| key == "code")
                        .map(|(_, value)| value.into_owned())
                        .filter(|code| !code.trim().is_empty())
                        .filter(|_| !url.query_pairs().any(|(key, _)| key == "error"))
                    {
                        let (browser_result, result_receiver) = std::sync::mpsc::channel();
                        accepted_callback = true;
                        let delivered = tx
                            .send(ReceivedOAuthCallback {
                                kind: OAuthCallbackKind::Code(code),
                                browser_result,
                            })
                            .is_ok();
                        match delivered.then(|| {
                            await_oauth_browser_result(
                                &result_receiver,
                                &thread_cancelled,
                                OAUTH_REQUEST_TIMEOUT + Duration::from_secs(5),
                            )
                        }) {
                            Some(Some(OAuthBrowserResult::Success)) => {
                                oauth_html_response("HTTP/1.1 200 OK", &oauth_result_page(true))
                            }
                            Some(Some(OAuthBrowserResult::Failure)) | Some(None) | None => {
                                oauth_html_response(
                                    "HTTP/1.1 500 Internal Server Error",
                                    &oauth_result_page(false),
                                )
                            }
                        }
                    } else if url.query_pairs().any(|(key, _)| key == "error") {
                        let (browser_result, result_receiver) = std::sync::mpsc::channel();
                        accepted_callback = true;
                        let delivered = tx
                            .send(ReceivedOAuthCallback {
                                kind: OAuthCallbackKind::Rejected,
                                browser_result,
                            })
                            .is_ok();
                        if delivered {
                            let _ = await_oauth_browser_result(
                                &result_receiver,
                                &thread_cancelled,
                                Duration::from_secs(2),
                            );
                        }
                        oauth_html_response("HTTP/1.1 400 Bad Request", &oauth_result_page(false))
                    } else {
                        oauth_html_response(
                            "HTTP/1.1 400 Bad Request",
                            "<html><body><h2>Login failed</h2><p>Missing authorization code.</p></body></html>",
                        )
                    }
                }
            } else {
                oauth_html_response(
                    "HTTP/1.1 400 Bad Request",
                    "<html><body><h2>Login failed</h2><p>Invalid callback request.</p></body></html>",
                )
            };
            let _ = stream.write_all(response.as_bytes());
            if accepted_callback {
                break;
            }
        }
    });

    Ok(OAuthCallbackListener {
        receiver,
        cancelled,
        thread: Some(thread),
    })
}

#[cfg(test)]
fn receive_oauth_callback_for_test(
    expected_state: &str,
    expected_path: &str,
    requests: &[String],
) -> Result<String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let callback = spawn_oauth_code_listener(
        listener,
        expected_state.to_string(),
        expected_path.to_string(),
    )?;
    for request in requests {
        let request = request.clone();
        let client = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let mut stream = std::net::TcpStream::connect(address)?;
            stream.write_all(request.as_bytes())?;
            stream.flush()?;
            let mut response = Vec::new();
            if let Err(error) = stream.read_to_end(&mut response)
                && error.kind() != io::ErrorKind::ConnectionReset
            {
                return Err(error);
            }
            Ok(response)
        });
        match callback.recv_timeout(Duration::from_millis(500)) {
            Ok(received) => {
                let OAuthCallbackKind::Code(code) = received.kind else {
                    let _ = received.browser_result.send(OAuthBrowserResult::Failure);
                    let _ = client.join();
                    continue;
                };
                let _ = received.browser_result.send(OAuthBrowserResult::Success);
                let _ = client.join();
                return Ok(code);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                client
                    .join()
                    .map_err(|_| anyhow::anyhow!("OAuth test client panicked"))??;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("OAuth callback listener disconnected")
            }
        }
    }
    anyhow::bail!("receive OAuth callback in test")
}

fn oauth_callback_host(oauth: &OAuthFlow) -> Result<String> {
    let host = std::env::var("DEXT_OAUTH_CALLBACK_HOST")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| oauth.callback_host.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let address = host
        .parse::<std::net::IpAddr>()
        .with_context(|| format!("OAuth callback host must be a loopback address, got '{host}'"))?;
    if !address.is_loopback() {
        anyhow::bail!("OAuth callback host must be loopback-only, got '{host}'");
    }
    Ok(address.to_string())
}

fn oauth_bind_address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn oauth_callback_port(redirect_uri: &str) -> Result<u16> {
    let url = reqwest::Url::parse(redirect_uri)
        .with_context(|| format!("invalid OAuth redirect URI '{redirect_uri}'"))?;
    if url.host_str() != Some("localhost") {
        anyhow::bail!(
            "OAuth redirect URI must target localhost for CLI flow, got '{redirect_uri}'"
        );
    }
    url.port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("OAuth redirect URI has no port: {redirect_uri}"))
}

pub(crate) fn oauth_originator() -> String {
    std::env::var("DEXT_OAUTH_ORIGINATOR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dext".to_string())
}

fn build_oauth_authorize_url(
    oauth: &OAuthFlow,
    profile: &ProviderProfile,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> Result<String> {
    let mut url = reqwest::Url::parse(&oauth.authorize_url)
        .with_context(|| format!("invalid OAuth authorize URL '{}'", oauth.authorize_url))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &oauth.client_id);
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("scope", &oauth.scope);
        query.append_pair("code_challenge", code_challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("state", state);
        if !oauth.audience.trim().is_empty() {
            query.append_pair("audience", &oauth.audience);
        }
        if oauth.protocol == OAuthProtocol::AnthropicClaude {
            query.append_pair("code", "true");
        }
        if profile.id == "chatgpt" {
            query.append_pair("id_token_add_organizations", "true");
            query.append_pair("codex_cli_simplified_flow", "true");
            let originator = oauth_originator();
            query.append_pair("originator", &originator);
        }
    }
    Ok(url.to_string())
}

fn oauth_exchange_failure_result(
    profile_id: &str,
    manual_hint: &str,
    error: &anyhow::Error,
) -> LoginResult {
    LoginResult {
        message: format!(
            "OAuth token exchange failed: {error:#}\n\n{manual_hint}\nYou can paste the callback URL or authorization code into dext to retry without restarting /login."
        ),
        provider_id: profile_id.to_string(),
        awaiting_credentials: true,
    }
}

fn run_oauth_login(oauth: &OAuthFlow, profile: &ProviderProfile) -> Result<LoginResult> {
    let code_verifier = generate_code_verifier()?;
    let code_challenge = pkce_code_challenge(&code_verifier);
    let state = if oauth.protocol == OAuthProtocol::AnthropicClaude {
        code_verifier.clone()
    } else {
        generate_oauth_state()?
    };

    let redirect_uri = oauth
        .redirect_uri
        .as_deref()
        .unwrap_or("http://localhost:1455/auth/callback");
    let callback_host = oauth_callback_host(oauth)?;
    let callback_port = oauth_callback_port(redirect_uri)?;

    let callback_path = reqwest::Url::parse(redirect_uri)
        .context("parse OAuth callback URI")?
        .path()
        .to_string();
    let bind_addr = oauth_bind_address(&callback_host, callback_port);
    let (rx2, listener_warning) = match std::net::TcpListener::bind(&bind_addr) {
        Ok(listener) => (
            Some(spawn_oauth_code_listener(
                listener,
                state.clone(),
                callback_path.clone(),
            )?),
            None,
        ),
        Err(e) => (
            None,
            Some(format!(
                "could not bind OAuth callback listener on {bind_addr}: {e}. You must paste the callback URL or authorization code manually."
            )),
        ),
    };

    let authorize_url =
        build_oauth_authorize_url(oauth, profile, redirect_uri, &state, &code_challenge)?;

    save_pending_oauth(&profile.id, &code_verifier, &state, redirect_uri)?;

    let manual_code_description = if oauth.protocol == OAuthProtocol::AnthropicClaude {
        "the authorization code"
    } else {
        "the authorization code (starts with `ac_`)"
    };
    let manual_hint = format!(
        "If the browser callback doesn't auto-complete, paste the callback URL \
(http://localhost:{callback_port}{callback_path}?code=...) or just {manual_code_description} \
directly into dext. /login cancel aborts."
    );
    let listener_suffix = listener_warning
        .as_ref()
        .map(|w| format!("\n[warn] {w}"))
        .unwrap_or_default();

    match open_url_in_browser(&authorize_url) {
        Ok(msg) if msg.starts_with("disabled-by-") => {
            set_active_provider_in_catalog(&profile.id)?;
            let reason = if msg == "disabled-by-wsl" {
                "browser open disabled on WSL by default for reliability. "
            } else {
                "browser open disabled. "
            };
            return Ok(LoginResult {
                message: format!(
                    "{reason}Open this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
        Ok(_) => {}
        Err(e) => {
            set_active_provider_in_catalog(&profile.id)?;
            return Ok(LoginResult {
                message: format!(
                    "could not open browser for OAuth login: {e:#}\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
    }

    let Some(rx2) = rx2 else {
        set_active_provider_in_catalog(&profile.id)?;
        return Ok(LoginResult {
            message: format!(
                "browser opened for OAuth login, but callback listener is unavailable.\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
            ),
            provider_id: profile.id.clone(),
            awaiting_credentials: true,
        });
    };

    let received = match rx2.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(received) => received,
        Err(_) => {
            set_active_provider_in_catalog(&profile.id)?;
            return Ok(LoginResult {
                message: format!(
                    "OAuth login timed out (120s).\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
    };

    let kind = received.kind;
    let code = match kind {
        OAuthCallbackKind::Code(code) => code,
        OAuthCallbackKind::Rejected => {
            let _ = received.browser_result.send(OAuthBrowserResult::Failure);
            clear_pending_oauth_if_matches(&profile.id, &code_verifier, &state);
            anyhow::bail!(
                "OAuth authorization was declined for provider '{}'. Start a fresh login when ready.",
                profile.id
            );
        }
    };
    let token_response = exchange_oauth_code(oauth, &code, &code_verifier, &state, redirect_uri);

    match token_response {
        Ok(tokens) => {
            if !pending_oauth_matches(&profile.id, &code_verifier, &state) {
                let _ = received.browser_result.send(OAuthBrowserResult::Failure);
                anyhow::bail!(
                    "OAuth login was superseded by a newer login attempt. Complete the newest browser flow instead."
                );
            }
            let login = (|| -> Result<LoginResult> {
                set_active_provider_in_catalog(&profile.id)?;
                let mut current_store = load_auth_store()?;
                current_store.providers.insert(
                    canonical_provider_id(&profile.id),
                    StoredCredential::OAuth {
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                        expires_at: tokens.expires_at,
                    },
                );
                save_auth_store(&current_store)?;
                clear_pending_oauth_if_matches(&profile.id, &code_verifier, &state);
                Ok(LoginResult {
                    message: format!(
                        "OAuth login successful for provider '{}'. Credentials stored.",
                        profile.id
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                })
            })();
            let browser_result = if login.is_ok() {
                OAuthBrowserResult::Success
            } else {
                OAuthBrowserResult::Failure
            };
            let _ = received.browser_result.send(browser_result);
            login
        }
        Err(e) => {
            let _ = received.browser_result.send(OAuthBrowserResult::Failure);
            Ok(oauth_exchange_failure_result(&profile.id, &manual_hint, &e))
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingOAuthState {
    provider_id: String,
    code_verifier: String,
    state: String,
    redirect_uri: String,
    created_at: u64,
}

#[cfg(test)]
pub(crate) fn pending_oauth_path() -> PathBuf {
    dext_state_dir().join("pending_oauth.json")
}

#[cfg(not(test))]
fn pending_oauth_path() -> PathBuf {
    dext_state_dir().join("pending_oauth.json")
}

#[cfg(test)]
pub(crate) fn save_pending_oauth(
    provider_id: &str,
    code_verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<()> {
    let pending = PendingOAuthState {
        provider_id: provider_id.to_string(),
        code_verifier: code_verifier.to_string(),
        state: state.to_string(),
        redirect_uri: redirect_uri.to_string(),
        created_at: unix_timestamp_secs(),
    };
    let json = serde_json::to_string_pretty(&pending)?;
    Ok(atomic_write_secret(&pending_oauth_path(), json.as_bytes())?)
}

#[cfg(not(test))]
fn save_pending_oauth(
    provider_id: &str,
    code_verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<()> {
    let pending = PendingOAuthState {
        provider_id: provider_id.to_string(),
        code_verifier: code_verifier.to_string(),
        state: state.to_string(),
        redirect_uri: redirect_uri.to_string(),
        created_at: unix_timestamp_secs(),
    };
    let json = serde_json::to_string_pretty(&pending)?;
    Ok(atomic_write_secret(&pending_oauth_path(), json.as_bytes())?)
}

fn load_pending_oauth() -> Option<PendingOAuthState> {
    let path = pending_oauth_path();
    let data = match read_runtime_state_file(&path, true) {
        Ok(Some(data)) => data,
        Ok(None) => return None,
        Err(_) => {
            remove_pending_oauth_file(&path);
            return None;
        }
    };
    let state: PendingOAuthState = match serde_json::from_str(&data) {
        Ok(state) => state,
        Err(_) => {
            remove_pending_oauth_file(&path);
            return None;
        }
    };
    let age = unix_timestamp_secs().saturating_sub(state.created_at);
    if age > 600 {
        remove_pending_oauth_file(&path);
        return None;
    }
    Some(state)
}

fn remove_pending_oauth_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

fn clear_pending_oauth() {
    let path = pending_oauth_path();
    remove_pending_oauth_file(&path);
}

fn pending_oauth_matches(provider_id: &str, code_verifier: &str, state: &str) -> bool {
    load_pending_oauth().is_some_and(|pending| {
        canonical_provider_id(&pending.provider_id) == canonical_provider_id(provider_id)
            && pending.code_verifier == code_verifier
            && pending.state == state
    })
}

fn clear_pending_oauth_for_provider(provider_id: &str) {
    if load_pending_oauth().is_some_and(|pending| {
        canonical_provider_id(&pending.provider_id) == canonical_provider_id(provider_id)
    }) {
        clear_pending_oauth();
    }
}

fn clear_pending_oauth_if_matches(provider_id: &str, code_verifier: &str, state: &str) {
    if pending_oauth_matches(provider_id, code_verifier, state) {
        clear_pending_oauth();
    }
}

pub(crate) fn cancel_pending_oauth_login() -> bool {
    let had_callback_login = pending_oauth_path().exists();
    clear_pending_oauth();
    had_callback_login
}

#[derive(Debug, Clone)]
struct ParsedOAuthAuthorizationInput {
    code: String,
    state: Option<String>,
    state_required: bool,
}

fn parse_oauth_authorization_input(
    input: &str,
    allow_plain_code_fallback: bool,
    allow_secret_like_plain_code: bool,
) -> Option<ParsedOAuthAuthorizationInput> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let code = url
            .query_pairs()
            .find(|(k, value)| k == "code" && !value.trim().is_empty())
            .map(|(_, v)| v.into_owned());
        if let Some(code) = code {
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            return Some(ParsedOAuthAuthorizationInput {
                code,
                state,
                state_required: true,
            });
        }
    }

    if let Some((code, state)) = trimmed.split_once('#')
        && !trimmed.contains("://")
        && !trimmed.contains(char::is_whitespace)
    {
        let code = code.trim();
        let state = state.trim();
        if !code.is_empty() {
            return Some(ParsedOAuthAuthorizationInput {
                code: code.to_string(),
                state: if state.is_empty() {
                    None
                } else {
                    Some(state.to_string())
                },
                state_required: true,
            });
        }
    }

    let query_like = trimmed
        .strip_prefix('?')
        .or_else(|| trimmed.split_once('?').map(|(_, q)| q))
        .unwrap_or(trimmed);
    if query_like.contains("code=")
        && let Ok(url) =
            reqwest::Url::parse(&format!("http://localhost/auth/callback?{query_like}"))
    {
        let code = url
            .query_pairs()
            .find(|(k, value)| k == "code" && !value.trim().is_empty())
            .map(|(_, v)| v.into_owned());
        if let Some(code) = code {
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            return Some(ParsedOAuthAuthorizationInput {
                code,
                state,
                state_required: true,
            });
        }
    }

    let plain_code_allowed = if allow_plain_code_fallback {
        !trimmed.contains(char::is_whitespace)
            && !trimmed.contains("://")
            && !trimmed.starts_with('{')
            && !trimmed.starts_with("sk-ant-api")
            && (allow_secret_like_plain_code
                || (!trimmed.starts_with("sk-") && !trimmed.starts_with("eyJ")))
            && trimmed.len() >= 12
            && chatgpt_account_id_from_token(trimmed).is_none()
    } else {
        trimmed.starts_with("ac_")
            && !trimmed.contains(char::is_whitespace)
            && chatgpt_account_id_from_token(trimmed).is_none()
    };

    if plain_code_allowed {
        return Some(ParsedOAuthAuthorizationInput {
            code: trimmed.to_string(),
            state: None,
            state_required: false,
        });
    }

    None
}

pub(crate) fn extract_oauth_code_from_callback(input: &str) -> Option<String> {
    parse_oauth_authorization_input(input, false, false).map(|parsed| parsed.code)
}

fn validate_pending_oauth_callback(
    input: &str,
    expected_provider: Option<&str>,
) -> Result<Option<(ParsedOAuthAuthorizationInput, PendingOAuthState)>> {
    let structured = parse_oauth_authorization_input(input, false, false);
    let pending = load_pending_oauth();
    let allow_plain_code = pending.is_some()
        || expected_provider.is_some_and(|provider| canonical_provider_id(provider) != "anthropic");
    let allow_secret_like_plain_code = expected_provider
        .map(canonical_provider_id)
        .or_else(|| {
            pending
                .as_ref()
                .map(|pending| canonical_provider_id(&pending.provider_id))
        })
        .as_deref()
        == Some("anthropic");
    let parsed = structured.or_else(|| {
        allow_plain_code
            .then(|| parse_oauth_authorization_input(input, true, allow_secret_like_plain_code))
            .flatten()
    });
    let Some(parsed) = parsed else {
        return Ok(None);
    };
    let pending = match pending {
        Some(pending) => pending,
        None => anyhow::bail!(
            "received OAuth callback code but no pending OAuth session found. Start a fresh login with /login <provider> web"
        ),
    };

    if let Some(expected_provider) = expected_provider
        && canonical_provider_id(expected_provider) != canonical_provider_id(&pending.provider_id)
    {
        anyhow::bail!(
            "OAuth callback belongs to pending provider '{}', not '{}'. Complete or cancel the pending login first.",
            pending.provider_id,
            expected_provider
        );
    }

    if parsed.state_required && parsed.state.is_none() {
        anyhow::bail!(
            "OAuth callback is missing state. Paste the complete callback URL or just the authorization code, or start a fresh login with /login {} web",
            pending.provider_id
        );
    }
    if let Some(returned_state) = parsed.state.as_deref()
        && returned_state != pending.state
    {
        anyhow::bail!(
            "OAuth state mismatch. Start a fresh login with /login {} web",
            pending.provider_id
        );
    }
    Ok(Some((parsed, pending)))
}

pub(crate) fn try_complete_oauth_from_callback(
    input: &str,
    expected_provider: Option<&str>,
) -> Result<Option<String>> {
    let Some((parsed, pending)) = validate_pending_oauth_callback(input, expected_provider)? else {
        return Ok(None);
    };

    let catalog = load_provider_catalog()?;
    let profile = find_provider_profile(&catalog, &pending.provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{}' for pending OAuth",
            pending.provider_id
        )
    })?;

    let oauth = profile
        .oauth_flow
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("provider '{}' has no OAuth config", pending.provider_id))?;

    let tokens = exchange_oauth_code(
        oauth,
        &parsed.code,
        &pending.code_verifier,
        &pending.state,
        &pending.redirect_uri,
    )?;
    if !pending_oauth_matches(&pending.provider_id, &pending.code_verifier, &pending.state) {
        anyhow::bail!(
            "OAuth login was superseded by a newer login attempt. Complete the newest browser flow instead."
        );
    }

    set_active_provider_in_catalog(&pending.provider_id)?;

    let mut store = load_auth_store()?;
    store.providers.insert(
        canonical_provider_id(&pending.provider_id),
        StoredCredential::OAuth {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        },
    );
    save_auth_store(&store)?;
    clear_pending_oauth_if_matches(&pending.provider_id, &pending.code_verifier, &pending.state);

    Ok(Some(format!(
        "OAuth login successful for provider '{}'. Credentials stored.",
        pending.provider_id
    )))
}

#[derive(Debug)]
struct ExchangedTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
}

fn oauth_expires_at_from_response(body: &Value, refresh_early_secs: u64) -> Option<u64> {
    let expires_in = body.get("expires_in").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            .filter(|seconds| *seconds > 0)
    });
    expires_in.map(|seconds| {
        unix_timestamp_secs().saturating_add(seconds.saturating_sub(refresh_early_secs))
    })
}

fn kimi_device_id_path() -> PathBuf {
    dext_state_dir().join("kimi_device_id")
}

fn kimi_device_id_is_valid(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn kimi_device_id() -> Result<String> {
    let path = kimi_device_id_path();
    if let Ok(value) = std::fs::read_to_string(&path) {
        let value = value.trim();
        if kimi_device_id_is_valid(value) {
            return Ok(value.to_string());
        }
    }

    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("failed to generate Kimi device id")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let value = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    );
    let _ = atomic_write_secret(&path, value.as_bytes());
    Ok(value)
}

fn kimi_ascii_header(value: &str) -> String {
    let value = value
        .chars()
        .filter(|character| character.is_ascii() && !character.is_ascii_control())
        .collect::<String>();
    let value = value.trim();
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value.to_string()
    }
}

fn kimi_device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|value| kimi_ascii_header(&value))
        .unwrap_or_else(|| "unknown".to_string())
}

fn oauth_http_client() -> Result<reqwest::blocking::Client> {
    provider_blocking_http_client_builder()
        .connect_timeout(OAUTH_CONNECT_TIMEOUT)
        .timeout(OAUTH_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build OAuth HTTP client")
}

fn read_oauth_json_response(mut response: reqwest::blocking::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(OAUTH_RESPONSE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read OAuth response")?;
    if bytes.len() as u64 > OAUTH_RESPONSE_MAX_BYTES {
        anyhow::bail!("OAuth response exceeded the {OAUTH_RESPONSE_MAX_BYTES} byte limit");
    }
    serde_json::from_slice(&bytes).context("OAuth response was not valid JSON")
}

fn exchange_oauth_code(
    oauth: &OAuthFlow,
    code: &str,
    code_verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> Result<ExchangedTokens> {
    let client = oauth_http_client()?;
    let mut request = client.post(&oauth.token_url);
    if oauth.protocol == OAuthProtocol::AnthropicClaude {
        request = request.json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_id": oauth.client_id,
            "code_verifier": code_verifier,
            "state": state,
            "redirect_uri": redirect_uri,
        }));
    } else {
        request = request.form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", oauth.client_id.as_str()),
            ("code_verifier", code_verifier),
            ("redirect_uri", redirect_uri),
        ]);
    }
    let resp = request.send().context("token exchange request failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("token exchange failed ({status}): provider rejected the OAuth exchange");
    }
    let body = read_oauth_json_response(resp).context("token exchange response was invalid")?;

    let access_token = body
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no access_token in OAuth response"))?;
    let refresh_token = body
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let expires_at = oauth_expires_at_from_response(
        &body,
        if oauth.protocol == OAuthProtocol::AnthropicClaude {
            300
        } else {
            0
        },
    );
    if oauth.protocol == OAuthProtocol::AnthropicClaude {
        if refresh_token.is_none() {
            anyhow::bail!("no refresh_token in Anthropic OAuth response");
        }
        if expires_at.is_none() {
            anyhow::bail!("no valid expires_in in Anthropic OAuth response");
        }
    }

    Ok(ExchangedTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

fn exchange_oauth_refresh_token(oauth: &OAuthFlow, refresh_token: &str) -> Result<ExchangedTokens> {
    let client = oauth_http_client()?;
    let mut request = client.post(&oauth.token_url);
    if oauth.protocol == OAuthProtocol::AnthropicClaude {
        request = request.json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": oauth.client_id,
        }));
    } else {
        request = request.form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", oauth.client_id.as_str()),
        ]);
    }
    let resp = request.send().context("token refresh request failed")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("token refresh failed ({status}): provider rejected the OAuth refresh");
    }
    let body = read_oauth_json_response(resp).context("token refresh response was invalid")?;

    let access_token = body
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no access_token in OAuth refresh response"))?;
    let next_refresh = body
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .or_else(|| Some(refresh_token.to_string()));
    let expires_at = oauth_expires_at_from_response(
        &body,
        if oauth.protocol == OAuthProtocol::AnthropicClaude {
            300
        } else {
            0
        },
    );
    if oauth.protocol == OAuthProtocol::AnthropicClaude && expires_at.is_none() {
        anyhow::bail!("no valid expires_in in Anthropic OAuth refresh response");
    }

    Ok(ExchangedTokens {
        access_token,
        refresh_token: next_refresh,
        expires_at,
    })
}

fn refresh_oauth_credential_if_needed(
    profile: &ProviderProfile,
    store: &mut AuthStore,
) -> Result<Option<String>> {
    let canonical = canonical_provider_id(&profile.id);
    if canonical == "kimi" {
        return Ok(None);
    }
    let Some(StoredCredential::OAuth {
        access_token,
        refresh_token,
        expires_at,
    }) = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical))
        .cloned()
    else {
        return Ok(None);
    };

    let Some(expires_at) = expires_at else {
        return Ok(None);
    };
    let now = unix_timestamp_secs();
    if expires_at == 0 || now.saturating_add(60) < expires_at {
        return Ok(None);
    }

    let Some(refresh_token) = refresh_token else {
        if now < expires_at {
            return Ok(None);
        }
        anyhow::bail!(
            "OAuth access token for provider '{}' is expired and has no refresh token. Re-run /login {} web.",
            profile.id,
            profile.id
        );
    };
    let refreshed = {
        let oauth = profile.oauth_flow.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "provider '{}' has OAuth credential but no OAuth config",
                profile.id
            )
        })?;
        match exchange_oauth_refresh_token(oauth, &refresh_token) {
            Ok(refreshed) => refreshed,
            Err(error) => {
                let current_store = load_auth_store()?;
                let credential_changed = current_store
                    .providers
                    .get(&profile.id)
                    .or_else(|| current_store.providers.get(&canonical))
                    .is_none_or(|credential| {
                        !matches!(
                            credential,
                            StoredCredential::OAuth {
                                access_token: current_access,
                                refresh_token: Some(current_refresh),
                                expires_at: Some(current_expires),
                            } if current_access == &access_token
                                && current_refresh == &refresh_token
                                && *current_expires == expires_at
                        )
                    });
                if credential_changed {
                    *store = current_store;
                    return Ok(None);
                }
                return Err(error);
            }
        }
    };

    let mut current_store = load_auth_store()?;
    let credential_is_current = current_store
        .providers
        .get(&profile.id)
        .or_else(|| current_store.providers.get(&canonical))
        .is_some_and(|credential| {
            matches!(
                credential,
                StoredCredential::OAuth {
                    access_token: current_access,
                    refresh_token: Some(current_refresh),
                    expires_at: Some(current_expires),
                } if current_access == &access_token
                    && current_refresh == &refresh_token
                    && *current_expires == expires_at
            )
        });
    if !credential_is_current {
        *store = current_store;
        return Ok(None);
    }

    let token = refreshed.access_token.clone();
    current_store.providers.insert(
        canonical,
        StoredCredential::OAuth {
            access_token: refreshed.access_token,
            refresh_token: refreshed.refresh_token,
            expires_at: refreshed.expires_at,
        },
    );
    save_auth_store(&current_store)?;
    *store = current_store;
    Ok(Some(token))
}

pub(crate) fn login_provider(
    selected: Option<&str>,
    key_from_arg: Option<&str>,
    allow_prompt: bool,
) -> Result<LoginResult> {
    // A login is an explicit retry signal: drop cached `!command` secret
    // results (including cached failures) so a fixed external store —
    // e.g. an unlocked keychain — is picked up without restarting dext.
    if let Ok(mut cache) = command_secret_cache().lock() {
        cache.clear();
    }
    let mut catalog = load_provider_catalog()?;
    let provider_id = match selected {
        Some(sel) => provider_id_from_selector(&catalog, sel)?,
        None => resolve_active_provider_id(&catalog),
    };
    let profile = find_provider_profile(&catalog, &provider_id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;

    let mut store = load_auth_store()?;
    let explicit_web = key_from_arg.is_some_and(login_arg_requests_web_flow);
    let explicit_import = key_from_arg.is_some_and(login_arg_requests_import);
    let anthropic_subscription_login = profile
        .oauth_flow
        .as_ref()
        .is_some_and(|oauth| oauth.protocol == OAuthProtocol::AnthropicClaude);
    let mut key = if explicit_web || explicit_import {
        String::new()
    } else {
        key_from_arg
            .and_then(normalize_login_secret)
            .unwrap_or_default()
    };

    if key.trim().is_empty() {
        if !profile.requires_api_key {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(&catalog)?;
            return Ok(LoginResult {
                message: format!("provider '{}' selected. No API key required.", profile.id),
                provider_id: profile.id.clone(),
                awaiting_credentials: false,
            });
        }

        if explicit_web
            && !anthropic_subscription_login
            && store
                .providers
                .remove(&canonical_provider_id(&profile.id))
                .is_some()
        {
            save_auth_store(&store)?;
        }

        if !explicit_web && anthropic_subscription_login {
            let canonical = canonical_provider_id(&profile.id);
            if let Some(entry @ StoredCredential::OAuth { .. }) = store
                .providers
                .get(&profile.id)
                .or_else(|| store.providers.get(&canonical))
                && entry.resolve_secret().is_some()
            {
                clear_pending_oauth_for_provider(&profile.id);
                catalog.active_provider = profile.id.clone();
                save_provider_catalog(&catalog)?;
                return Ok(LoginResult {
                    message: format!(
                        "provider '{}' already authenticated via subscription OAuth and set active",
                        profile.id
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }
        }

        if !explicit_web
            && !anthropic_subscription_login
            && let Some((secret, source)) = resolve_provider_api_key(&profile, &store)
        {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(&catalog)?;
            if source.starts_with("auth:") {
                return Ok(LoginResult {
                    message: format!(
                        "provider '{}' already authenticated via {} and set active",
                        profile.id, source
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }

            if source.starts_with("env:") {
                store.providers.insert(
                    canonical_provider_id(&profile.id),
                    StoredCredential::ApiKey { key: secret },
                );
                save_auth_store(&store)?;
                clear_pending_oauth_for_provider(&profile.id);
                return Ok(LoginResult {
                    message: format!(
                        "imported credentials for provider '{}' from {} into {} and set it active",
                        profile.id,
                        source,
                        auth_store_path().display()
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }
        }

        if explicit_import {
            if let Some(source) = import_provider_credential_from_external(&profile, &mut store) {
                save_auth_store(&store)?;
                clear_pending_oauth_for_provider(&profile.id);
                catalog.active_provider = profile.id.clone();
                save_provider_catalog(&catalog)?;
                return Ok(LoginResult {
                    message: format!(
                        "imported credentials for provider '{}' from {} into {} and set it active",
                        profile.id,
                        source,
                        auth_store_path().display()
                    ),
                    provider_id: profile.id.clone(),
                    awaiting_credentials: false,
                });
            }
            anyhow::bail!(
                "no reusable external credential found for provider '{}'. Run `dext auth login {} web` or paste a token/key directly.",
                profile.id,
                profile.id
            );
        }

        if let Some(oauth) = &profile.oauth_flow {
            return run_oauth_login(oauth, &profile);
        }

        let mut login_urls: Vec<String> = Vec::new();
        if let Some(url) = &profile.login_url {
            login_urls.push(url.clone());
        }

        let mut opened_any_url = false;
        let mut launch_warnings: Vec<String> = Vec::new();
        for url in &login_urls {
            match open_url_in_browser(url) {
                Ok(_) => opened_any_url = true,
                Err(e) => launch_warnings.push(format!("could not auto-open {url}: {e:#}")),
            }
        }

        if allow_prompt {
            let prompt = if request_contract_for_profile(&profile)
                == RequestContract::ChatGptResponses
            {
                "paste ChatGPT access token (or full JSON session blob) now, or press Enter to skip: "
            } else {
                "paste API key/token now, or press Enter to skip: "
            };
            let entered = prompt_input_line(prompt)?;
            if let Some(secret) = normalize_login_secret(&entered) {
                key = secret;
            }
        }

        if key.trim().is_empty() {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(&catalog)?;

            let mut msg = if request_contract_for_profile(&profile)
                == RequestContract::ChatGptResponses
            {
                "opened ChatGPT in your browser. sign in if needed, then paste the access token or full session JSON directly into dext.".to_string()
            } else {
                format!(
                    "opened login page for provider '{}'. paste the credential directly into dext when ready.",
                    profile.id
                )
            };
            if !opened_any_url && !login_urls.is_empty() {
                msg.push_str("\nopen this URL:");
                for url in &login_urls {
                    msg.push_str(&format!("\n- {url}"));
                }
            }
            for warn in launch_warnings {
                msg.push_str(&format!("\n[warn] {warn}"));
            }
            if request_contract_for_profile(&profile) != RequestContract::ChatGptResponses
                && let Some(notes) = &profile.notes
            {
                msg.push_str(&format!("\n{notes}"));
            }
            return Ok(LoginResult {
                message: msg,
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
    }

    // Only treat pasted input as an OAuth artifact when this provider actually
    // uses OAuth, or the input is unambiguously a callback (URL / `ac_` code).
    // Plain API keys for non-OAuth providers (e.g. ZAI GLM `id.secret` keys)
    // would otherwise match the plain-code fallback and fail login with a
    // bogus "no pending OAuth session" error instead of being stored.
    if (profile.oauth_flow.is_some() || extract_oauth_code_from_callback(&key).is_some())
        && let Some(msg) = try_complete_oauth_from_callback(&key, Some(&profile.id))?
    {
        catalog.active_provider = profile.id.clone();
        save_provider_catalog(&catalog)?;
        return Ok(LoginResult {
            message: msg,
            provider_id: profile.id.clone(),
            awaiting_credentials: false,
        });
    }

    if profile.oauth_flow.is_some() && (key.contains("://") || key.contains("code=")) {
        anyhow::bail!(
            "malformed OAuth callback. Paste the complete callback URL, including its non-empty code and state, or paste just the authorization code."
        );
    }

    validate_login_secret_for_provider(&profile, &key)?;
    store.providers.insert(
        canonical_provider_id(&profile.id),
        StoredCredential::ApiKey { key },
    );
    save_auth_store(&store)?;
    clear_pending_oauth_for_provider(&profile.id);

    catalog.active_provider = profile.id.clone();
    save_provider_catalog(&catalog)?;

    Ok(LoginResult {
        message: format!(
            "stored credentials for provider '{}' in {} and set it active",
            profile.id,
            auth_store_path().display()
        ),
        provider_id: profile.id.clone(),
        awaiting_credentials: false,
    })
}

#[cfg(test)]
pub(crate) fn oauth_exchange_failure_result_message(profile_id: &str, manual_hint: &str) -> String {
    oauth_exchange_failure_result(
        profile_id,
        manual_hint,
        &anyhow::anyhow!("token exchange failed (400 Bad Request): unknown error"),
    )
    .message
}

#[cfg(test)]
pub(crate) fn login_provider_with_key(
    selected: Option<&str>,
    key_from_arg: Option<&str>,
    allow_prompt: bool,
) -> Result<String> {
    Ok(login_provider(selected, key_from_arg, allow_prompt)?.message)
}

pub(crate) fn logout_provider(selected: Option<&str>) -> Result<String> {
    let catalog = load_provider_catalog()?;
    let provider_id = match selected {
        Some(sel) => provider_id_from_selector(&catalog, sel)?,
        None => resolve_active_provider_id(&catalog),
    };
    let canonical = canonical_provider_id(&provider_id);

    let mut store = load_auth_store()?;
    let removed = store.providers.remove(&canonical).is_some();
    save_auth_store(&store)?;

    Ok(if removed {
        format!("removed stored credentials for provider '{canonical}'")
    } else {
        format!("no stored credentials for provider '{canonical}'")
    })
}

fn render_provider_models(profile: &ProviderProfile) -> String {
    let models = curated_provider_models(profile);
    let mut out = if models.is_empty() {
        format!(
            "provider '{}' default model: {} (no curated model list configured)",
            profile.id, profile.default_model
        )
    } else {
        format!(
            "provider '{}' models:\n{}",
            profile.id,
            models
                .iter()
                .map(|m| format!("- {m}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    if !profile.model_aliases.is_empty() {
        let mut aliases = profile.model_aliases.iter().collect::<Vec<_>>();
        aliases.sort_by_key(|(alias, _)| *alias);
        out.push_str("\naliases:\n");
        out.push_str(
            &aliases
                .into_iter()
                .map(|(alias, target)| format!("- {alias} -> {target}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !profile.requires_api_key {
        out.push_str(
            "\nnote: no credentials required; the local/server endpoint must already be running.",
        );
    }
    if let Some(notes) = &profile.notes {
        out.push('\n');
        out.push_str(notes);
    }
    if request_contract_for_profile(profile) == RequestContract::ChatGptResponses {
        out.push_str(
            "\nnote: this is a local curated list, not your full account entitlement list.\nset any model slug directly with /model <slug>.",
        );
    }
    out
}

fn prefix_first_line(prefix: &str, text: &str) -> String {
    if let Some((first, rest)) = text.split_once('\n') {
        format!("{prefix}{first}\n{rest}")
    } else {
        format!("{prefix}{text}")
    }
}

pub(crate) fn list_models_for_provider(
    catalog: &ProviderCatalog,
    provider_id: &str,
) -> Result<String> {
    let profile = find_provider_profile(catalog, provider_id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
    Ok(render_provider_models(&profile))
}

pub(crate) fn list_models_for_available_providers(
    catalog: &ProviderCatalog,
    store: &AuthStore,
    active_provider: &str,
) -> Result<String> {
    let active_provider = canonical_provider_id(active_provider);
    let mut sections = Vec::new();
    for profile in &catalog.providers {
        if !provider_has_available_credentials(profile, store) {
            continue;
        }
        let marker = if canonical_provider_id(&profile.id) == active_provider {
            "* "
        } else {
            "  "
        };
        sections.push(prefix_first_line(marker, &render_provider_models(profile)));
    }
    if sections.is_empty() {
        let profile = find_provider_profile(catalog, &active_provider)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{active_provider}'"))?;
        return Ok(format!(
            "no authenticated providers found; showing active provider.\n{}",
            prefix_first_line("* ", &render_provider_models(&profile))
        ));
    }
    Ok(sections.join("\n\n"))
}

const AUTH_LOGIN_ARGUMENTS: &str = "[credential|web|import]";

fn auth_retry_guidance(provider_id: &str) -> String {
    format!(
        "login for provider '{provider_id}' remains incomplete. retry `dext auth login {provider_id}`; to paste a credential or manual OAuth callback without putting it in shell arguments, open Dext and use `/login {provider_id}`. shell history and process listings may retain command-line secrets"
    )
}

pub(crate) fn handle_auth_cli(argv: &[String]) -> Result<Option<i32>> {
    if argv.first().map(String::as_str) != Some("auth") {
        return Ok(None);
    }

    let sub = argv.get(1).map(String::as_str).unwrap_or("status");
    let args: Vec<&str> = argv.iter().skip(2).map(String::as_str).collect();

    match sub {
        "help" | "-h" | "--help" => {
            println!("dext auth commands:");
            println!("  dext auth status");
            println!("  dext auth providers            list configured providers and auth status");
            println!("  dext auth provider [id|index]  show/set active provider");
            println!("  dext auth models [provider|index] list known models for provider");
            println!(
                "  dext auth login [provider|index] {AUTH_LOGIN_ARGUMENTS}   open login flow and store credentials"
            );
            println!(
                "      omit the credential to paste at Dext's prompt; shell arguments may be retained"
            );
            println!("  dext auth logout [provider|index] remove stored credential");
            Ok(Some(0))
        }
        "status" | "providers" | "list" => {
            let catalog = load_provider_catalog()?;
            let store = load_auth_store()?;
            let active = resolve_active_provider_id(&catalog);
            println!(
                "active provider: {}\n{}\n\nprovider catalog: {}\nauth store: {}",
                active,
                render_provider_list(&catalog, &store, &active),
                provider_catalog_path().display(),
                auth_store_path().display(),
            );
            Ok(Some(0))
        }
        "provider" | "use" => {
            let catalog = load_provider_catalog()?;
            if args.is_empty() {
                let active = resolve_active_provider_id(&catalog);
                println!("active provider: {active}");
                return Ok(Some(0));
            }
            let target = provider_id_from_selector(&catalog, args[0])?;
            set_active_provider_in_catalog(&target)?;
            println!("active provider -> {target}");
            Ok(Some(0))
        }
        "models" => {
            let catalog = load_provider_catalog()?;
            let store = load_auth_store()?;
            let active = resolve_active_provider_id(&catalog);
            let list = match args.first().copied() {
                None | Some("all") => {
                    list_models_for_available_providers(&catalog, &store, &active)
                }
                Some(sel) => provider_id_from_selector(&catalog, sel)
                    .and_then(|target| list_models_for_provider(&catalog, &target)),
            }?;
            println!("{list}");
            Ok(Some(0))
        }
        "login" => {
            let catalog = load_provider_catalog()?;
            let store = load_auth_store()?;
            let active = resolve_active_provider_id(&catalog);

            if args.is_empty() {
                println!("select provider for login (id or index):");
                println!("{}", render_provider_picker(&catalog, &store, &active));

                if io::stdin().is_terminal() {
                    let selection = prompt_input_line("provider (Enter for active): ")?;
                    let selected = if selection.trim().is_empty() {
                        None
                    } else {
                        Some(selection)
                    };
                    let login = login_provider(selected.as_deref(), None, true)?;
                    println!("{}", login.message);
                    if login.awaiting_credentials {
                        println!("{}", auth_retry_guidance(&login.provider_id));
                    }
                } else {
                    println!("usage: dext auth login <provider|index> {AUTH_LOGIN_ARGUMENTS}");
                }
                return Ok(Some(0));
            }

            let provider = args.first().copied();
            let key_buf = if args.len() > 1 {
                Some(args[1..].join(" "))
            } else {
                None
            };
            let allow_prompt = io::stdin().is_terminal();
            let login = login_provider(provider, key_buf.as_deref(), allow_prompt)?;
            println!("{}", login.message);
            if login.awaiting_credentials {
                println!("{}", auth_retry_guidance(&login.provider_id));
            }
            Ok(Some(0))
        }
        "logout" => {
            let provider = args.first().copied();
            let msg = logout_provider(provider)?;
            println!("{msg}");
            Ok(Some(0))
        }
        other => {
            eprintln!("unknown auth command: {other}. try `dext auth help`");
            Ok(Some(2))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dext-provider-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp home");
        path
    }

    #[test]
    fn oauth_callback_listener_ignores_invalid_requests_until_valid_callback() -> Result<()> {
        let code = receive_oauth_callback_for_test(
            "expected-state",
            "/callback",
            &[
                "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string(),
                "GET /callback?code=wrong&state=wrong-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
                "GET /callback?code=accepted-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
            ],
        )?;
        assert_eq!(code, "accepted-code");
        Ok(())
    }

    #[test]
    fn oauth_callback_listener_rejects_malformed_and_oversized_requests() -> Result<()> {
        let oversized = format!(
            "GET /callback?code=oversized&state=expected-state HTTP/1.1\r\nX-Fill: {}\r\n\r\n",
            "x".repeat(8192)
        );
        let code = receive_oauth_callback_for_test(
            "expected-state",
            "/callback",
            &[
                "POST /callback?code=post&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
                "GET /callback?code=incomplete&state=expected-state HTTP/1.1\r\nHost: localhost\r\n"
                    .to_string(),
                oversized,
                "GET /callback?code=accepted-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
                    .to_string(),
            ],
        )?;
        assert_eq!(code, "accepted-code");
        Ok(())
    }

    #[test]
    fn oauth_callback_listener_renders_failure_only_after_completion_fails() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let callback = spawn_oauth_code_listener(
            listener,
            "expected-state".to_string(),
            "/callback".to_string(),
        )?;
        let client = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let mut stream = std::net::TcpStream::connect(address)?;
            stream.write_all(
                b"GET /callback?code=accepted-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )?;
            stream.flush()?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response)?;
            Ok(response)
        });
        let received = callback.recv_timeout(Duration::from_secs(5))?;
        let OAuthCallbackKind::Code(code) = received.kind else {
            anyhow::bail!("expected OAuth code callback");
        };
        assert_eq!(code, "accepted-code");
        received
            .browser_result
            .send(OAuthBrowserResult::Failure)
            .map_err(|_| anyhow::anyhow!("send OAuth browser result"))?;
        let response = client
            .join()
            .map_err(|_| anyhow::anyhow!("OAuth test client panicked"))??;
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("500 Internal Server Error"));
        assert!(response.contains("AUTHENTICATION INCOMPLETE"));
        assert!(!response.contains("You're signed in"));
        Ok(())
    }

    #[test]
    fn oauth_callback_listener_renders_provider_denial() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let callback = spawn_oauth_code_listener(
            listener,
            "expected-state".to_string(),
            "/callback".to_string(),
        )?;
        let client = std::thread::spawn(move || -> io::Result<Vec<u8>> {
            let mut stream = std::net::TcpStream::connect(address)?;
            stream.write_all(
                b"GET /callback?error=access_denied&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )?;
            stream.flush()?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response)?;
            Ok(response)
        });
        let received = callback.recv_timeout(Duration::from_secs(2))?;
        assert!(matches!(received.kind, OAuthCallbackKind::Rejected));
        received
            .browser_result
            .send(OAuthBrowserResult::Failure)
            .map_err(|_| anyhow::anyhow!("send OAuth browser result"))?;
        let response = client
            .join()
            .map_err(|_| anyhow::anyhow!("OAuth test client panicked"))??;
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("400 Bad Request"));
        assert!(response.contains("AUTHENTICATION INCOMPLETE"));
        assert!(!response.contains("access_denied"));
        Ok(())
    }

    #[test]
    fn oauth_refresh_rotates_and_persists_tokens() -> Result<()> {
        let _guard = crate::test_env_lock();
        let home = temp_home("oauth-refresh-rotation");
        let old_home = std::env::var_os("DEXT_HOME");
        unsafe { std::env::set_var("DEXT_HOME", &home) };

        let result = (|| -> Result<()> {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let server = std::thread::spawn(move || -> io::Result<()> {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut request = [0u8; 4096];
                let size = stream.read(&mut request)?;
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(request.contains("POST /token HTTP/1.1"));
                assert!(request.contains("old-refresh"));
                let body = r#"{"access_token":" new-access ","refresh_token":" new-refresh ","expires_in":3600}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )?;
                Ok(())
            });

            let mut profile = built_in_provider_profiles()
                .into_iter()
                .find(|profile| profile.id == "anthropic")
                .context("Anthropic profile")?;
            profile
                .oauth_flow
                .as_mut()
                .context("Anthropic OAuth")?
                .token_url = format!("http://{address}/token");
            let mut store = AuthStore::default();
            store.providers.insert(
                "anthropic".to_string(),
                StoredCredential::OAuth {
                    access_token: "old-access".to_string(),
                    refresh_token: Some("old-refresh".to_string()),
                    expires_at: Some(unix_timestamp_secs()),
                },
            );
            save_auth_store(&store)?;

            assert_eq!(
                refresh_oauth_credential_if_needed(&profile, &mut store)?.as_deref(),
                Some("new-access")
            );
            server
                .join()
                .map_err(|_| anyhow::anyhow!("OAuth refresh server panicked"))??;
            let persisted = load_auth_store()?;
            assert!(matches!(
                persisted.providers.get("anthropic"),
                Some(StoredCredential::OAuth {
                    access_token,
                    refresh_token: Some(refresh_token),
                    expires_at: Some(expires_at),
                }) if access_token == "new-access"
                    && refresh_token == "new-refresh"
                    && *expires_at > unix_timestamp_secs()
            ));
            Ok(())
        })();

        if let Some(value) = old_home {
            unsafe { std::env::set_var("DEXT_HOME", value) };
        } else {
            unsafe { std::env::remove_var("DEXT_HOME") };
        }
        let _ = std::fs::remove_dir_all(home);
        result
    }

    #[test]
    fn oauth_callback_listener_accepts_fragmented_request_headers() -> Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let callback = spawn_oauth_code_listener(
            listener,
            "expected-state".to_string(),
            "/callback".to_string(),
        )?;
        let mut stream = std::net::TcpStream::connect(address)?;
        stream.write_all(b"GET /callback?code=fragmented")?;
        stream.flush()?;
        std::thread::sleep(Duration::from_millis(350));
        stream.write_all(b"-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\n\r\n")?;
        stream.flush()?;
        let mut response = Vec::new();
        let received = callback.recv_timeout(Duration::from_secs(5))?;
        let OAuthCallbackKind::Code(code) = received.kind else {
            anyhow::bail!("expected OAuth code callback");
        };
        received
            .browser_result
            .send(OAuthBrowserResult::Success)
            .map_err(|_| anyhow::anyhow!("send OAuth browser result"))?;
        stream.read_to_end(&mut response)?;
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("200 OK"));
        assert!(response.contains("DEXT</b> / LOGIN"));
        assert!(response.contains("You're signed in"));
        assert!(response.contains("Cache-Control: no-store"));
        assert_eq!(code, "fragmented-code");
        Ok(())
    }

    #[test]
    fn auth_login_guidance_avoids_shell_secret_arguments() {
        assert_eq!(AUTH_LOGIN_ARGUMENTS, "[credential|web|import]");
        let guidance = auth_retry_guidance("anthropic");
        assert!(guidance.contains("remains incomplete"), "{guidance}");
        assert!(guidance.contains("dext auth login anthropic"), "{guidance}");
        assert!(guidance.contains("/login anthropic"), "{guidance}");
        assert!(guidance.contains("manual OAuth callback"), "{guidance}");
        assert!(guidance.contains("shell history"), "{guidance}");
        assert!(!guidance.contains("<api-key"), "{guidance}");
    }

    #[test]
    fn kimi_device_id_is_stable_uuid_and_ascii_headers_are_safe() -> Result<()> {
        let _guard = crate::test_env_lock();
        let home = temp_home("device-id");
        let old_home = std::env::var_os("DEXT_HOME");
        unsafe { std::env::set_var("DEXT_HOME", &home) };

        let result = (|| -> Result<()> {
            let first = kimi_device_id()?;
            let second = kimi_device_id()?;
            assert_eq!(first, second);
            assert_eq!(first.len(), 36);
            assert_eq!(first.bytes().filter(|byte| *byte == b'-').count(), 4);
            assert!(kimi_device_id_is_valid(&first));
            assert_eq!(kimi_ascii_header(" h\u{00f4}st\n"), "hst");
            assert_eq!(kimi_ascii_header("\n\t"), "unknown");
            Ok(())
        })();

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("DEXT_HOME", value),
                None => std::env::remove_var("DEXT_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
        result
    }
}
