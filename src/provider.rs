use anyhow::{Context, Result};
use base64::Engine;
use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::session::{atomic_write_bytes, dext_state_dir, unix_timestamp_secs};

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

const PROVIDER_CATALOG_VERSION: u32 = 1;
const AUTH_STORE_VERSION: u32 = 1;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProviderProfile {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: String,
    pub(crate) api_provider: ApiProvider,
    pub(crate) base_url: String,
    pub(crate) default_model: String,
    #[serde(default)]
    pub(crate) models: Vec<String>,
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
    /// Editable in ~/.dext/providers.json. Env override: DEXT_CONTEXT_WINDOW[_TOKENS].
    #[serde(default)]
    pub(crate) context_window: Option<u64>,
    /// Optional per-model override of context_window. Map key = model id.
    #[serde(default)]
    pub(crate) model_context_windows: HashMap<String, u64>,
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

#[derive(Debug, Clone)]
pub(crate) struct ResolvedProviderConfig {
    pub(crate) profile: ProviderProfile,
    pub(crate) api_key: String,
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
        "llama" | "llama.cpp" | "llamacpp" | "qwen" => "local".to_string(),
        other => other.to_string(),
    }
}

const RETIRED_BUNDLED_PROVIDER_IDS: &[&str] = &["openrouter", "ollama"];

pub(crate) fn built_in_provider_profiles() -> Vec<ProviderProfile> {
    vec![
        ProviderProfile {
            id: "glm".to_string(),
            display_name: "ZAI GLM".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: "https://api.z.ai/api/anthropic".to_string(),
            default_model: "glm-4.6".to_string(),
            models: vec![
                "glm-4.6".to_string(),
                "glm-5.0".to_string(),
                "glm-5.1".to_string(),
            ],
            env_vars: vec!["ZAI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://open.bigmodel.cn/usercenter/apikeys".to_string()),
            oauth_flow: None,
            notes: Some(
                "Use your ZAI key. If your key unlocks newer GLM models, set /model directly."
                    .to_string(),
            ),
            context_window: Some(200_000),
            model_context_windows: HashMap::new(),
        },
        ProviderProfile {
            id: "chatgpt".to_string(),
            display_name: "ChatGPT".to_string(),
            api_provider: ApiProvider::ChatGpt,
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            default_model: "gpt-5.4".to_string(),
            models: vec![
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
        },
        ProviderProfile {
            id: "openai".to_string(),
            display_name: "OpenAI API".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "https://api.openai.com".to_string(),
            default_model: "gpt-5".to_string(),
            models: vec![
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
            notes: Some("Use an OpenAI Platform API key (not ChatGPT OAuth).".to_string()),
            context_window: Some(400_000),
            model_context_windows: {
                let mut m = HashMap::new();
                m.insert("gpt-4.1".to_string(), 1_000_000);
                m.insert("gpt-4.1-mini".to_string(), 1_000_000);
                m.insert("gpt-4o".to_string(), 128_000);
                m.insert("gpt-4o-mini".to_string(), 128_000);
                m
            },
        },
        ProviderProfile {
            id: "anthropic".to_string(),
            display_name: "Anthropic".to_string(),
            api_provider: ApiProvider::Anthropic,
            base_url: "https://api.anthropic.com".to_string(),
            default_model: "claude-sonnet-4-5".to_string(),
            models: vec![
                "claude-sonnet-4-5".to_string(),
                "claude-opus-4-1".to_string(),
                "claude-opus-4-0".to_string(),
                "claude-haiku-4-5".to_string(),
                "claude-3-5-haiku-latest".to_string(),
            ],
            env_vars: vec!["ANTHROPIC_API_KEY".to_string()],
            requires_api_key: true,
            login_url: Some("https://console.anthropic.com/settings/keys".to_string()),
            oauth_flow: None,
            notes: Some("Use an Anthropic Console API key.".to_string()),
            context_window: Some(200_000),
            model_context_windows: HashMap::new(),
        },
        ProviderProfile {
            id: "deepseek".to_string(),
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
        },
        ProviderProfile {
            id: "local".to_string(),
            display_name: "Local llama.cpp".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "http://127.0.0.1:8080".to_string(),
            default_model: "qwen-local".to_string(),
            models: vec![
                "qwen-local".to_string(),
                "Qwen3.6-35B-A3B-Q4_K_M.gguf".to_string(),
            ],
            env_vars: Vec::new(),
            requires_api_key: false,
            login_url: None,
            oauth_flow: None,
            notes: Some("Local OpenAI-compatible llama.cpp server. Start llama-server on 127.0.0.1:8080 first; no cloud credentials are used.".to_string()),
            context_window: Some(4_096),
            model_context_windows: HashMap::new(),
        },
    ]
}

pub(crate) fn normalize_provider_profile(mut profile: ProviderProfile) -> Option<ProviderProfile> {
    profile.id = canonical_provider_id(&profile.id);
    if profile.id.trim().is_empty() {
        return None;
    }

    let fallback_model = if profile.id == "local" {
        "qwen-local"
    } else {
        "glm-4.6"
    };
    profile.base_url = profile.base_url.trim().trim_end_matches('/').to_string();
    if profile.display_name.trim().is_empty() {
        profile.display_name = profile.id.clone();
    }
    if profile.default_model.trim().is_empty() {
        profile.default_model = fallback_model.to_string();
    }
    if profile.api_provider == ApiProvider::ChatGpt {
        profile.default_model = normalize_chatgpt_model_slug(&profile.default_model);
    }
    let mut seen_models = HashSet::new();
    let mut models = Vec::new();
    for m in profile.models {
        let trimmed = if profile.api_provider == ApiProvider::ChatGpt {
            normalize_chatgpt_model_slug(&m)
        } else {
            m.trim().to_string()
        };
        if trimmed.is_empty() {
            continue;
        }
        if seen_models.insert(trimmed.to_ascii_lowercase()) {
            models.push(trimmed.to_string());
        }
    }
    if !models.iter().any(|m| m == &profile.default_model) {
        models.insert(0, profile.default_model.clone());
    }
    profile.models = models;

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
    let stored_default = stored.default_model.trim();
    if !stored_default.is_empty() {
        builtin.default_model = if builtin.api_provider == ApiProvider::ChatGpt {
            normalize_chatgpt_model_slug(stored_default)
        } else {
            stored_default.to_string()
        };
    }

    if let Some(window) = stored.context_window.filter(|window| *window > 0) {
        builtin.context_window = Some(window);
    }

    for (model, window) in stored.model_context_windows {
        if window == 0 {
            continue;
        }
        let key = if builtin.api_provider == ApiProvider::ChatGpt {
            normalize_chatgpt_model_slug(&model)
        } else {
            model.trim().to_string()
        };
        if key.is_empty() {
            continue;
        }
        builtin.model_context_windows.insert(key, window);
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
    let builtin_id = canonical_provider_id(&builtin.id);
    let extra_models = stored
        .models
        .into_iter()
        .map(|model| {
            if builtin.api_provider == ApiProvider::ChatGpt {
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

pub(crate) fn normalize_provider_catalog(mut catalog: ProviderCatalog) -> ProviderCatalog {
    let mut stored_by_id: HashMap<String, ProviderProfile> = HashMap::new();
    let mut providers: Vec<ProviderProfile> = Vec::new();
    let builtin_ids: HashSet<String> = built_in_provider_profiles()
        .into_iter()
        .map(|profile| canonical_provider_id(&profile.id))
        .collect();

    for profile in catalog.providers.drain(..) {
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

    ProviderCatalog {
        version: PROVIDER_CATALOG_VERSION,
        active_provider,
        providers,
    }
}

pub(crate) fn default_provider_catalog() -> ProviderCatalog {
    normalize_provider_catalog(ProviderCatalog {
        version: PROVIDER_CATALOG_VERSION,
        active_provider: default_active_provider(),
        providers: built_in_provider_profiles(),
    })
}

pub(crate) fn load_provider_catalog() -> Result<ProviderCatalog> {
    let path = provider_catalog_path();
    if !path.exists() {
        return Ok(default_provider_catalog());
    }

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading provider catalog {}", path.display()))?;

    let parsed = serde_json::from_str::<ProviderCatalog>(&text).or_else(|_| {
        let providers = serde_json::from_str::<Vec<ProviderProfile>>(&text)?;
        Ok::<ProviderCatalog, serde_json::Error>(ProviderCatalog {
            version: PROVIDER_CATALOG_VERSION,
            active_provider: default_active_provider(),
            providers,
        })
    });

    let catalog = parsed.context("invalid provider catalog JSON")?;
    Ok(normalize_provider_catalog(catalog))
}

pub(crate) fn save_provider_catalog(catalog: &ProviderCatalog) -> Result<()> {
    let path = provider_catalog_path();
    let normalized = normalize_provider_catalog(catalog.clone());
    let bytes = serde_json::to_vec_pretty(&normalized)?;
    atomic_write_bytes(&path, &bytes)?;
    Ok(())
}

fn normalize_auth_store(mut store: AuthStore) -> AuthStore {
    store.version = AUTH_STORE_VERSION;
    let mut providers = HashMap::new();
    for (provider, credential) in store.providers {
        let canonical = canonical_provider_id(&provider);
        if canonical.is_empty() {
            continue;
        }
        providers.insert(canonical, credential);
    }
    store.providers = providers;
    store
}

pub(crate) fn load_auth_store() -> Result<AuthStore> {
    let path = auth_store_path();
    if !path.exists() {
        return Ok(AuthStore::default());
    }

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading auth store {}", path.display()))?;

    let raw: Value = serde_json::from_str(&text).context("invalid auth store JSON")?;

    if raw.get("providers").is_some() {
        let store: AuthStore = serde_json::from_value(raw).context("invalid auth store JSON")?;
        return Ok(normalize_auth_store(store));
    }

    let mut store = AuthStore::default();
    let object = raw
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("auth store must be a JSON object"))?;

    for (provider, value) in object {
        let key = canonical_provider_id(provider);
        if key.is_empty() {
            continue;
        }

        if let Some(cred) = parse_external_auth_credential(value) {
            store.providers.insert(key, cred);
            continue;
        }

        if let Some(api_key) = value.as_str() {
            let trimmed = api_key.trim();
            if !trimmed.is_empty() {
                store.providers.insert(
                    key,
                    StoredCredential::ApiKey {
                        key: trimmed.to_string(),
                    },
                );
            }
        }
    }

    Ok(normalize_auth_store(store))
}

pub(crate) fn save_auth_store(store: &AuthStore) -> Result<()> {
    let path = auth_store_path();
    let normalized = normalize_auth_store(store.clone());
    let bytes = serde_json::to_vec_pretty(&normalized)?;
    atomic_write_bytes(&path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
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
            .flatten()
        {
            return Some(cached);
        }

        let output = Command::new("bash").arg("-lc").arg(&key).output().ok()?;
        if !output.status.success() {
            if let Ok(mut m) = command_secret_cache().lock() {
                m.insert(key, None);
            }
            return None;
        }
        let secret = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if secret.is_empty() {
            if let Ok(mut m) = command_secret_cache().lock() {
                m.insert(key, None);
            }
            return None;
        }
        if let Ok(mut m) = command_secret_cache().lock() {
            m.insert(key, Some(secret.clone()));
        }
        return Some(secret);
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

pub(crate) fn resolve_provider_api_key(
    profile: &ProviderProfile,
    store: &AuthStore,
) -> Option<(String, String)> {
    if let Ok(v) = std::env::var("DEXT_API_KEY") {
        let t = v.trim().to_string();
        if !t.is_empty() {
            return Some((t, "env:DEXT_API_KEY".to_string()));
        }
    }

    let canonical_id = canonical_provider_id(&profile.id);
    if let Some(entry) = store
        .providers
        .get(&profile.id)
        .or_else(|| store.providers.get(&canonical_id))
        && let Some(secret) = entry.resolve_secret()
    {
        return Some((secret, format!("auth:{}", profile.id)));
    }

    for env in &profile.env_vars {
        if let Ok(v) = std::env::var(env) {
            let t = v.trim().to_string();
            if !t.is_empty() {
                return Some((t, format!("env:{env}")));
            }
        }
    }
    None
}

pub(crate) fn normalize_provider_model_value(profile: &ProviderProfile, model: &str) -> String {
    let trimmed = model.trim();
    if profile.api_provider == ApiProvider::ChatGpt {
        return normalize_chatgpt_model_slug(trimmed);
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

    let hyphenated = trimmed.split_whitespace().collect::<Vec<_>>().join("-");
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
                && profile.api_provider == ApiProvider::OpenAi
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

    match profile.api_provider {
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

pub(crate) fn provider_request_url(base_url: &str, api_provider: ApiProvider) -> String {
    let base = base_url.trim_end_matches('/');
    match api_provider {
        ApiProvider::OpenAi => {
            if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            }
        }
        ApiProvider::ChatGpt => {
            if base.ends_with("/codex/responses") {
                base.to_string()
            } else if base.ends_with("/codex") {
                format!("{base}/responses")
            } else {
                format!("{base}/codex/responses")
            }
        }
        ApiProvider::Anthropic => {
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
    if effort == crate::ThinkingEffort::Off {
        return None;
    }
    let raw = effort.as_str();
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
            "xhigh" => "xhigh",
            _ => "medium",
        });
    }
    if model == "gpt-5.1-codex-mini" {
        return Some(match raw {
            "high" | "xhigh" => "high",
            _ => "medium",
        });
    }
    if model.starts_with("gpt-5.1") {
        return Some(match raw {
            "xhigh" => "high",
            other => other,
        });
    }
    Some(raw)
}

pub(crate) fn build_chatgpt_request(
    model: &str,
    thinking_effort: crate::ThinkingEffort,
    system_text: &str,
    session_id: &str,
    input: Vec<Value>,
    tools: Vec<Value>,
) -> Value {
    let model = normalize_chatgpt_model_slug(model);
    let effort = chatgpt_reasoning_effort(&model, thinking_effort);
    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": system_text,
        "input": input,
        "include": ["reasoning.encrypted_content"],
        "text": { "verbosity": "medium" },
        "prompt_cache_key": session_id,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });
    if let Some(effort) = effort {
        body.as_object_mut()
            .expect("chatgpt request body is always an object")
            .insert(
                "reasoning".to_string(),
                json!({ "effort": effort, "summary": "auto" }),
            );
    }
    if !tools.is_empty() {
        body.as_object_mut()
            .expect("chatgpt request body is always an object")
            .insert("tools".to_string(), json!(tools));
    }
    body
}

pub(crate) fn build_chatgpt_summary_request(
    model: &str,
    compact_system: &str,
    user_text: &str,
) -> Value {
    json!({
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
        "reasoning": { "effort": "low", "summary": "auto" },
    })
}

pub(crate) fn apply_provider_headers(
    req: RequestBuilder,
    api_provider: ApiProvider,
    api_key: &str,
    session_id: Option<&str>,
) -> Result<RequestBuilder> {
    Ok(match api_provider {
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
            let req = req.header("anthropic-version", "2023-06-01");
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
    let refreshed_token = refresh_oauth_credential_if_needed(&profile, &mut store)?;
    let resolved_key = refreshed_token
        .map(|token| (token, format!("auth:{} (refreshed)", profile.id)))
        .or_else(|| resolve_provider_api_key(&profile, &store));

    let (api_key, key_source) = match (profile.requires_api_key, resolved_key) {
        (_, Some((key, source))) => (key, source),
        (false, None) => (
            String::new(),
            "none (provider does not require key)".to_string(),
        ),
        (true, None) if !require_credentials => {
            (String::new(), "missing (login required)".to_string())
        }
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

    Ok(ResolvedProviderConfig {
        requires_api_key: profile.requires_api_key,
        profile,
        api_key,
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
        let matches_curated = curated_provider_models(profile)
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
        lines.push(format!(
            "{marker} {:<12} {:<18} model={} auth={} base={}",
            profile.id, name, profile.default_model, status, profile.base_url
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
        lines.push(format!(
            "{:>2}) {} {:<10} model={:<16} auth={}",
            i + 1,
            marker,
            profile.id,
            profile.default_model,
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
    } else if let Some(s) = v.as_str() {
        s.trim().parse::<u64>().ok()?
    } else {
        return None;
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
    if profile.api_provider == ApiProvider::ChatGpt
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
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", &powershell_cmd])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to launch browser via powershell Start-Process")?;
        if !status.success() {
            anyhow::bail!(
                "browser launcher exited with status {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "<signal>".to_string())
            );
        }
        return Ok("powershell Start-Process".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to launch browser via open")?;
        if !status.success() {
            anyhow::bail!(
                "browser launcher exited with status {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "<signal>".to_string())
            );
        }
        return Ok("open".to_string());
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
            match Command::new(&bin)
                .args(&args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(status) if status.success() => return Ok(bin),
                Ok(status) => errors.push(format!(
                    "{bin} exited {}",
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "<signal>".to_string())
                )),
                Err(e) => errors.push(format!("{bin}: {e}")),
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

fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("failed to generate random bytes for PKCE");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_code_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

fn generate_oauth_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("failed to generate OAuth state bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

fn oauth_html_response(status_line: &str, body: &str) -> String {
    format!(
        "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}"
    )
}

fn spawn_oauth_code_listener(
    listener: std::net::TcpListener,
    expected_state: String,
) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            use std::io::{Read, Write};

            let mut buf = [0u8; 8192];
            if let Ok(n) = stream.try_clone().and_then(|mut s| s.read(&mut buf)) {
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let mut response = oauth_html_response(
                    "HTTP/1.1 400 Bad Request",
                    "<html><body><h2>Login failed</h2><p>Missing authorization code.</p></body></html>",
                );

                if let Ok(url) = reqwest::Url::parse(&format!("http://localhost{path}")) {
                    if url.path() != "/auth/callback" {
                        response = oauth_html_response(
                            "HTTP/1.1 404 Not Found",
                            "<html><body><h2>Login failed</h2><p>Callback route not found.</p></body></html>",
                        );
                    } else {
                        let returned_state = url
                            .query_pairs()
                            .find(|(k, _)| k == "state")
                            .map(|(_, v)| v.into_owned());
                        if returned_state.as_deref() != Some(expected_state.as_str()) {
                            response = oauth_html_response(
                                "HTTP/1.1 400 Bad Request",
                                "<html><body><h2>Login failed</h2><p>State mismatch.</p></body></html>",
                            );
                        } else if let Some(code) = url
                            .query_pairs()
                            .find(|(k, _)| k == "code")
                            .map(|(_, v)| v.into_owned())
                        {
                            response = oauth_html_response(
                                "HTTP/1.1 200 OK",
                                "<html><body><h2>Login successful!</h2><p>You can close this tab.</p></body></html>",
                            );
                            let _ = tx.send(code);
                        }
                    }
                }

                let _ = stream
                    .try_clone()
                    .and_then(|mut s| s.write_all(response.as_bytes()));
            }
        }
    });

    rx
}

fn oauth_callback_host(oauth: &OAuthFlow) -> String {
    if let Ok(raw) = std::env::var("DEXT_OAUTH_CALLBACK_HOST") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    oauth
        .callback_host
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_string())
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

fn run_oauth_login(
    oauth: &OAuthFlow,
    profile: &ProviderProfile,
    catalog: &mut ProviderCatalog,
    store: &mut AuthStore,
) -> Result<LoginResult> {
    let code_verifier = generate_code_verifier();
    let code_challenge = pkce_code_challenge(&code_verifier);
    let state = generate_oauth_state();

    let redirect_uri = oauth
        .redirect_uri
        .as_deref()
        .unwrap_or("http://localhost:1455/auth/callback");
    let callback_host = oauth_callback_host(oauth);
    let callback_port = oauth_callback_port(redirect_uri)?;

    let bind_addr = format!("{callback_host}:{callback_port}");
    let (rx2, listener_warning) = match std::net::TcpListener::bind(&bind_addr) {
        Ok(listener) => (
            Some(spawn_oauth_code_listener(listener, state.clone())),
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

    let manual_hint = format!(
        "If the browser callback doesn't auto-complete, paste the callback URL \
(http://localhost:{callback_port}/auth/callback?code=...) or just the authorization code \
(starts with `ac_`) directly into dext. /login cancel aborts."
    );
    let listener_suffix = listener_warning
        .as_ref()
        .map(|w| format!("\n[warn] {w}"))
        .unwrap_or_default();

    match open_url_in_browser(&authorize_url) {
        Ok(msg) if msg.starts_with("disabled-by-") => {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(catalog)?;
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
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(catalog)?;
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
        catalog.active_provider = profile.id.clone();
        save_provider_catalog(catalog)?;
        return Ok(LoginResult {
            message: format!(
                "browser opened for OAuth login, but callback listener is unavailable.\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
            ),
            provider_id: profile.id.clone(),
            awaiting_credentials: true,
        });
    };

    let code = match rx2.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(code) => code,
        Err(_) => {
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(catalog)?;
            return Ok(LoginResult {
                message: format!(
                    "OAuth login timed out (120s).\n\nOpen this URL manually:\n{authorize_url}\n\n{manual_hint}{listener_suffix}"
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: true,
            });
        }
    };

    let token_response = exchange_oauth_code(
        &oauth.token_url,
        &oauth.client_id,
        &code,
        &code_verifier,
        redirect_uri,
    );

    match token_response {
        Ok(tokens) => {
            store.providers.insert(
                canonical_provider_id(&profile.id),
                StoredCredential::OAuth {
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                    expires_at: tokens.expires_at,
                },
            );
            save_auth_store(store)?;
            catalog.active_provider = profile.id.clone();
            save_provider_catalog(catalog)?;
            clear_pending_oauth();
            Ok(LoginResult {
                message: format!(
                    "OAuth login successful for provider '{}'. Credentials stored.",
                    profile.id
                ),
                provider_id: profile.id.clone(),
                awaiting_credentials: false,
            })
        }
        Err(e) => Ok(oauth_exchange_failure_result(&profile.id, &manual_hint, &e)),
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
    eprintln!(
        "[oauth] saving pending state to {} (state={})",
        pending_oauth_path().display(),
        state
    );
    Ok(atomic_write_bytes(&pending_oauth_path(), json.as_bytes())?)
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
    eprintln!(
        "[oauth] saving pending state to {} (state={})",
        pending_oauth_path().display(),
        state
    );
    Ok(atomic_write_bytes(&pending_oauth_path(), json.as_bytes())?)
}

fn load_pending_oauth() -> Option<PendingOAuthState> {
    let path = pending_oauth_path();
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[oauth] no pending state at {}: {e}", path.display());
            return None;
        }
    };
    let state: PendingOAuthState = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[oauth] failed to parse pending state: {e}");
            return None;
        }
    };
    let age = unix_timestamp_secs().saturating_sub(state.created_at);
    if age > 600 {
        eprintln!("[oauth] pending state expired ({age}s old)");
        let _ = std::fs::remove_file(&path);
        return None;
    }
    eprintln!(
        "[oauth] loaded pending state for provider={} state={} age={}s",
        state.provider_id, state.state, age
    );
    Some(state)
}

fn clear_pending_oauth() {
    let _ = std::fs::remove_file(pending_oauth_path());
}

pub(crate) fn cancel_pending_oauth_login() {
    clear_pending_oauth();
}

#[derive(Debug, Clone)]
struct ParsedOAuthAuthorizationInput {
    code: String,
    state: Option<String>,
}

fn parse_oauth_authorization_input(
    input: &str,
    allow_plain_code_fallback: bool,
) -> Option<ParsedOAuthAuthorizationInput> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned());
        if let Some(code) = code {
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            return Some(ParsedOAuthAuthorizationInput { code, state });
        }
    }

    if let Some((code, state)) = trimmed.split_once('#') {
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
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned());
        if let Some(code) = code {
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            return Some(ParsedOAuthAuthorizationInput { code, state });
        }
    }

    let plain_code_allowed = if allow_plain_code_fallback {
        !trimmed.contains(char::is_whitespace)
            && !trimmed.contains("://")
            && !trimmed.starts_with('{')
            && !trimmed.starts_with("sk-")
            && !trimmed.starts_with("eyJ")
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
        });
    }

    None
}

pub(crate) fn extract_oauth_code_from_callback(input: &str) -> Option<String> {
    parse_oauth_authorization_input(input, false).map(|parsed| parsed.code)
}

pub(crate) fn try_complete_oauth_from_callback(input: &str) -> Result<Option<String>> {
    let parsed = match parse_oauth_authorization_input(input, true) {
        Some(parsed) => parsed,
        None => return Ok(None),
    };

    eprintln!(
        "[oauth] extracted code from callback input: {}...{}",
        &parsed.code[..parsed.code.len().min(12)],
        &parsed.code[parsed.code.len().saturating_sub(8)..]
    );

    let pending = match load_pending_oauth() {
        Some(p) => p,
        None => anyhow::bail!(
            "received OAuth callback code but no pending OAuth session found. Start a fresh login with /login chatgpt web"
        ),
    };

    if let Some(returned_state) = parsed.state.as_deref()
        && returned_state != pending.state
    {
        anyhow::bail!(
            "OAuth state mismatch (expected {}, got {}). Start a fresh login with /login chatgpt web",
            pending.state,
            returned_state
        );
    }

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
        &oauth.token_url,
        &oauth.client_id,
        &parsed.code,
        &pending.code_verifier,
        &pending.redirect_uri,
    )?;

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

    let mut catalog = catalog;
    catalog.active_provider = pending.provider_id.clone();
    save_provider_catalog(&catalog)?;
    clear_pending_oauth();

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

fn oauth_expires_at_from_response(body: &Value) -> Option<u64> {
    let expires_in = body.get("expires_in").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
    });
    expires_in.map(|seconds| unix_timestamp_secs().saturating_add(seconds))
}

fn exchange_oauth_code(
    token_url: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<ExchangedTokens> {
    let client = reqwest::blocking::Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
        ("redirect_uri", redirect_uri),
    ];
    let resp = client
        .post(token_url)
        .form(&params)
        .send()
        .context("token exchange request failed")?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .context("token exchange response was not valid JSON")?;
    if !status.is_success() {
        let error_desc = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("token exchange failed ({status}): {error_desc}");
    }

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no access_token in OAuth response"))?;
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let expires_at = oauth_expires_at_from_response(&body);

    Ok(ExchangedTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

fn exchange_oauth_refresh_token(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<ExchangedTokens> {
    let client = reqwest::blocking::Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let resp = client
        .post(token_url)
        .form(&params)
        .send()
        .context("token refresh request failed")?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .context("token refresh response was not valid JSON")?;
    if !status.is_success() {
        let error_desc = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("token refresh failed ({status}): {error_desc}");
    }

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no access_token in OAuth refresh response"))?;
    let next_refresh = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| Some(refresh_token.to_string()));
    let expires_at = oauth_expires_at_from_response(&body);

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
    let Some(StoredCredential::OAuth {
        access_token: _access_token,
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
    if expires_at == 0 || unix_timestamp_secs().saturating_add(60) < expires_at {
        return Ok(None);
    }

    let Some(refresh_token) = refresh_token else {
        return Ok(None);
    };
    let oauth = profile.oauth_flow.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "provider '{}' has OAuth credential but no OAuth config",
            profile.id
        )
    })?;
    let refreshed =
        exchange_oauth_refresh_token(&oauth.token_url, &oauth.client_id, &refresh_token)?;
    let token = refreshed.access_token;
    store.providers.insert(
        canonical,
        StoredCredential::OAuth {
            access_token: token.clone(),
            refresh_token: refreshed.refresh_token,
            expires_at: refreshed.expires_at,
        },
    );
    save_auth_store(store)?;
    Ok(Some(token))
}

pub(crate) fn login_provider(
    selected: Option<&str>,
    key_from_arg: Option<&str>,
    allow_prompt: bool,
) -> Result<LoginResult> {
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
            && store
                .providers
                .remove(&canonical_provider_id(&profile.id))
                .is_some()
        {
            save_auth_store(&store)?;
        }

        if !explicit_web && let Some((secret, source)) = resolve_provider_api_key(&profile, &store)
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
            return run_oauth_login(oauth, &profile, &mut catalog, &mut store);
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
            let prompt = if profile.api_provider == ApiProvider::ChatGpt {
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

            let mut msg = if profile.api_provider == ApiProvider::ChatGpt {
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
            if profile.api_provider != ApiProvider::ChatGpt
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

    if let Some(msg) = try_complete_oauth_from_callback(&key)? {
        catalog.active_provider = profile.id.clone();
        save_provider_catalog(&catalog)?;
        return Ok(LoginResult {
            message: msg,
            provider_id: profile.id.clone(),
            awaiting_credentials: false,
        });
    }

    validate_login_secret_for_provider(&profile, &key)?;
    store.providers.insert(
        canonical_provider_id(&profile.id),
        StoredCredential::ApiKey { key },
    );
    save_auth_store(&store)?;

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
    if !profile.requires_api_key {
        out.push_str(
            "\nnote: no credentials required; the local/server endpoint must already be running.",
        );
    }
    if let Some(notes) = &profile.notes {
        out.push_str("\n");
        out.push_str(notes);
    }
    if profile.api_provider == ApiProvider::ChatGpt {
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
                "  dext auth login [provider|index] [token|web|import]   web login + store credential"
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
                        println!(
                            "then run: dext auth login {} <code|callback-url|token|json>",
                            login.provider_id
                        );
                    }
                } else {
                    println!("usage: dext auth login <provider|index> [token|web|import]");
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
                println!(
                    "then run: dext auth login {} <code|callback-url|token|json>",
                    login.provider_id
                );
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
