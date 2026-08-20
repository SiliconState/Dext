use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::packs::PackInfo;
use crate::{ExternalExecutionPolicy, SandboxProfile, execute_external_async_status};

pub(crate) const RUNTIME_MANIFEST_NAME: &str = "runtime.json";
const RUNTIME_PROTOCOL_VERSION: u32 = 1;
const RUNTIME_MANIFEST_CAP: u64 = 256 * 1024;
const RUNTIME_REQUEST_CAP: usize = 256 * 1024;
const RUNTIME_RESPONSE_CAP: usize = 256 * 1024;
const RUNTIME_STATE_CAP: usize = 64 * 1024;
const RUNTIME_CONTENT_CAP: usize = 128 * 1024;
const RUNTIME_VIEW_CAP: usize = 128 * 1024;
const RUNTIME_EFFECT_LIMIT: usize = 16;
const RUNTIME_TOOL_LIMIT: usize = 32;
const RUNTIME_SCHEMA_CAP: usize = 32 * 1024;
const RUNTIME_EXECUTABLE_CAP: u64 = 256 * 1024 * 1024;
const RUNTIME_MAX_SCHEMA_DEPTH: usize = 16;
const RUNTIME_MAX_SCHEMA_PROPERTIES: usize = 256;
const RUNTIME_MAX_ENUM_VALUES: usize = 256;
const RUNTIME_PENDING_CONTINUATION_LIMIT: usize = 32;
const RUNTIME_MAX_DELAY_MS: u64 = 30_000;
const RUNTIME_MAX_CONTINUATIONS: u32 = 1_000;
const RUNTIME_DEFAULT_TIMEOUT_SECS: u64 = 120;
const RUNTIME_MAX_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeRisk {
    Read,
    #[default]
    Write,
    Danger,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
    pub(crate) risk: RuntimeRisk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeApprovalIdentity {
    pack_name: String,
    pack_source: String,
    manifest_sha256: String,
    executable_sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveRuntime {
    pub(crate) pack_name: String,
    pub(crate) pack_source: String,
    pub(crate) executable: PathBuf,
    pub(crate) executable_sha256: String,
    pub(crate) args: Vec<String>,
    pub(crate) timeout: Duration,
    pub(crate) tools: Vec<RuntimeTool>,
    pub(crate) manifest_sha256: String,
    pub(crate) state: Value,
    pub(crate) max_continuations: u32,
    pub(crate) continuations_used: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct RuntimeSnapshot {
    pub(crate) pack_name: String,
    pub(crate) pack_source: String,
    pub(crate) manifest_sha256: String,
    #[serde(default)]
    pub(crate) state: Value,
    #[serde(default)]
    pub(crate) continuations_used: u32,
    #[serde(default)]
    pub(crate) pending_continuations: Vec<(String, u64)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RuntimeEvent<'a> {
    Activate,
    Tool { name: &'a str, input: &'a Value },
    Idle,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimeContext<'a> {
    pub(crate) turn_id: &'a str,
    pub(crate) iteration: u32,
    pub(crate) history_messages: usize,
    pub(crate) compacted: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum RuntimeEffect {
    Steer {
        text: String,
    },
    Continue {
        prompt: String,
        #[serde(default)]
        delay_ms: u64,
    },
    View {
        title: String,
        markdown: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeInvocation {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) state: Option<Value>,
    pub(crate) effects: Vec<RuntimeEffect>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeManifest {
    version: u32,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    tools: Vec<RuntimeToolManifest>,
    #[serde(default = "default_max_continuations")]
    max_continuations: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeToolManifest {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(default)]
    risk: RuntimeRisk,
}

#[derive(Serialize)]
struct RuntimeRequest<'a> {
    version: u32,
    event: &'static str,
    pack: &'a str,
    session_id: &'a str,
    cwd: String,
    state: &'a Value,
    context: RuntimeContext<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<&'a Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeResponse {
    version: u32,
    #[serde(default)]
    content: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    state: Option<Value>,
    #[serde(default)]
    effects: Vec<RuntimeEffect>,
}

fn default_max_continuations() -> u32 {
    100
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_regular_bounded(path: &Path, cap: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{label} is not a regular non-symlink file: {}",
            path.display()
        );
    }
    if metadata.len() > cap {
        bail!("{label} exceeds the {cap} byte limit: {}", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {label} {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspecting open {label} {}", path.display()))?;
    if !opened.is_file() || opened.len() > cap {
        bail!(
            "{label} changed or exceeds its byte limit: {}",
            path.display()
        );
    }
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    if bytes.len() as u64 > cap {
        bail!("{label} exceeds the {cap} byte limit: {}", path.display());
    }
    Ok(bytes)
}

fn sha256_regular_bounded(path: &Path, cap: u64, label: &str) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{label} is not a regular non-symlink file: {}",
            path.display()
        );
    }
    if metadata.len() > cap {
        bail!("{label} exceeds the {cap} byte limit: {}", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening {label} {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspecting open {label} {}", path.display()))?;
    if !opened.is_file() || opened.len() > cap {
        bail!(
            "{label} changed or exceeds its byte limit: {}",
            path.display()
        );
    }
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {label} {}", path.display()))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > cap {
            bail!("{label} exceeds the {cap} byte limit: {}", path.display());
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn valid_tool_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn validate_relative_command(command: &str) -> Result<()> {
    let path = Path::new(command);
    if command.trim().is_empty()
        || command.contains('\0')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!("pack runtime command must be a safe relative path inside the pack");
    }
    Ok(())
}

fn resolve_executable(pack: &PackInfo, command: &str) -> Result<PathBuf> {
    validate_relative_command(command)?;
    let pack_root = std::fs::canonicalize(&pack.path)
        .with_context(|| format!("canonicalizing pack root {}", pack.path.display()))?;
    let candidate = pack.path.join(command);
    let metadata = std::fs::symlink_metadata(&candidate)
        .with_context(|| format!("inspecting pack runtime {}", candidate.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "pack runtime command is not a regular non-symlink file: {}",
            candidate.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!(
                "pack runtime command is not executable: {}",
                candidate.display()
            );
        }
    }
    let executable = std::fs::canonicalize(&candidate)
        .with_context(|| format!("canonicalizing pack runtime {}", candidate.display()))?;
    if !executable.starts_with(&pack_root) {
        bail!("pack runtime command escapes the pack root");
    }
    Ok(executable)
}

fn validate_schema(schema: &Value) -> Result<()> {
    if serde_json::to_vec(schema)?.len() > RUNTIME_SCHEMA_CAP {
        bail!("pack runtime tool input_schema exceeds {RUNTIME_SCHEMA_CAP} bytes");
    }
    validate_schema_node(schema, "$", 0)?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!("pack runtime tool input_schema must declare type=object");
    }
    Ok(())
}

fn validate_schema_node(schema: &Value, path: &str, depth: usize) -> Result<()> {
    if depth > RUNTIME_MAX_SCHEMA_DEPTH {
        bail!("{path} schema exceeds the nesting limit");
    }
    let object = schema
        .as_object()
        .with_context(|| format!("{path} schema must be an object"))?;
    const SUPPORTED: &[&str] = &[
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "description",
    ];
    if let Some(keyword) = object.keys().find(|key| !SUPPORTED.contains(&key.as_str())) {
        bail!("{path} schema uses unsupported keyword {keyword}");
    }
    let kind = match object.get("type") {
        Some(Value::String(kind))
            if matches!(
                kind.as_str(),
                "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
            ) =>
        {
            kind.as_str()
        }
        Some(Value::String(kind)) => bail!("{path} schema has unsupported type {kind}"),
        Some(_) => bail!("{path} schema type must be a string"),
        None => bail!("{path} schema must declare a supported type"),
    };
    if let Some(description) = object.get("description") {
        let description = description
            .as_str()
            .with_context(|| format!("{path} schema description must be a string"))?;
        if description.len() > 2_000 {
            bail!("{path} schema description exceeds 2000 bytes");
        }
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .with_context(|| format!("{path} schema enum must be an array"))?;
        if values.is_empty() || values.len() > RUNTIME_MAX_ENUM_VALUES {
            bail!("{path} schema enum must contain 1-{RUNTIME_MAX_ENUM_VALUES} values");
        }
        if let Some(value) = values.iter().find(|value| !value_matches_type(value, kind)) {
            bail!("{path} schema enum value {value} does not match type {kind}");
        }
    }
    let properties = match object.get("properties") {
        None => None,
        Some(Value::Object(properties)) => Some(properties),
        Some(_) => bail!("{path} schema properties must be an object"),
    };
    if (properties.is_some()
        || object.contains_key("required")
        || object.contains_key("additionalProperties"))
        && kind != "object"
    {
        bail!("{path} schema object keywords require type=object");
    }
    if let Some(properties) = properties {
        if properties.len() > RUNTIME_MAX_SCHEMA_PROPERTIES {
            bail!("{path} schema declares more than {RUNTIME_MAX_SCHEMA_PROPERTIES} properties");
        }
        for (name, child) in properties {
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                bail!("{path} schema contains an invalid property name");
            }
            validate_schema_node(child, &format!("{path}.{name}"), depth + 1)?;
        }
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .with_context(|| format!("{path} schema required must be an array"))?;
        if required.len() > RUNTIME_MAX_SCHEMA_PROPERTIES {
            bail!("{path} schema required list is too large");
        }
        let mut names = HashSet::new();
        for name in required {
            let name = name
                .as_str()
                .with_context(|| format!("{path} schema required entries must be strings"))?;
            if !names.insert(name) {
                bail!("{path} schema required contains duplicate {name}");
            }
            if properties.is_none_or(|properties| !properties.contains_key(name)) {
                bail!("{path} schema required entry {name} has no property schema");
            }
        }
    }
    if let Some(additional) = object.get("additionalProperties")
        && !additional.is_boolean()
    {
        bail!("{path} schema additionalProperties must be a boolean");
    }
    if let Some(items) = object.get("items") {
        if kind != "array" {
            bail!("{path} schema items requires type=array");
        }
        validate_schema_node(items, &format!("{path}[]"), depth + 1)?;
    } else if kind == "array" {
        bail!("{path} array schema must declare items");
    }
    Ok(())
}

fn validate_manifest(manifest: &RuntimeManifest, builtin_names: &HashSet<String>) -> Result<()> {
    if manifest.version != RUNTIME_PROTOCOL_VERSION {
        bail!(
            "unsupported pack runtime protocol version {}; expected {}",
            manifest.version,
            RUNTIME_PROTOCOL_VERSION
        );
    }
    if manifest.tools.len() > RUNTIME_TOOL_LIMIT {
        bail!("pack runtime declares more than {RUNTIME_TOOL_LIMIT} tools");
    }
    if manifest.max_continuations > RUNTIME_MAX_CONTINUATIONS {
        bail!("pack runtime max_continuations exceeds {RUNTIME_MAX_CONTINUATIONS}");
    }
    if manifest.args.len() > 32
        || manifest
            .args
            .iter()
            .any(|arg| arg.len() > 4_096 || arg.contains('\0'))
    {
        bail!("pack runtime args exceed their count or size limit");
    }
    let mut names = HashSet::new();
    for tool in &manifest.tools {
        if !valid_tool_name(&tool.name) {
            bail!(
                "pack runtime tool names must be 1-64 ASCII letters, digits, or underscores and start with a letter: {}",
                tool.name
            );
        }
        if builtin_names.contains(&tool.name) || !names.insert(tool.name.clone()) {
            bail!(
                "pack runtime tool name collides with an exposed tool: {}",
                tool.name
            );
        }
        if tool.description.trim().is_empty() || tool.description.len() > 2_000 {
            bail!(
                "pack runtime tool description must be 1-2000 bytes: {}",
                tool.name
            );
        }
        validate_schema(&tool.input_schema)
            .with_context(|| format!("validating pack runtime tool {}", tool.name))?;
    }
    Ok(())
}

pub(crate) fn load(
    pack: &PackInfo,
    occupied_names: &HashSet<String>,
) -> Result<Option<ActiveRuntime>> {
    let Some(path) = pack.runtime_path.as_deref() else {
        return Ok(None);
    };
    let bytes = read_regular_bounded(path, RUNTIME_MANIFEST_CAP, "pack runtime manifest")?;
    let manifest: RuntimeManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing pack runtime manifest {}", path.display()))?;
    validate_manifest(&manifest, occupied_names)?;
    let executable = resolve_executable(pack, &manifest.command)?;
    let executable_sha256 = sha256_regular_bounded(
        &executable,
        RUNTIME_EXECUTABLE_CAP,
        "pack runtime executable",
    )?;
    let timeout = runtime_timeout(manifest.timeout_seconds)?;
    Ok(Some(ActiveRuntime {
        pack_name: pack.name.clone(),
        pack_source: pack.source_identity(),
        executable,
        executable_sha256,
        args: manifest.args,
        timeout,
        tools: manifest
            .tools
            .into_iter()
            .map(|tool| RuntimeTool {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
                risk: tool.risk,
            })
            .collect(),
        manifest_sha256: sha256_hex(&bytes),
        state: Value::Null,
        max_continuations: manifest.max_continuations,
        continuations_used: 0,
    }))
}

impl ActiveRuntime {
    pub(crate) fn approval_identity(&self) -> RuntimeApprovalIdentity {
        RuntimeApprovalIdentity {
            pack_name: self.pack_name.clone(),
            pack_source: self.pack_source.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            executable_sha256: self.executable_sha256.clone(),
        }
    }

    pub(crate) fn snapshot(&self, pending_continuations: &[(String, u64)]) -> RuntimeSnapshot {
        RuntimeSnapshot {
            pack_name: self.pack_name.clone(),
            pack_source: self.pack_source.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            state: self.state.clone(),
            continuations_used: self.continuations_used,
            pending_continuations: pending_continuations.to_vec(),
        }
    }

    pub(crate) fn tool(&self, name: &str) -> Option<&RuntimeTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    pub(crate) fn restore_state(&mut self, snapshot: &RuntimeSnapshot) -> Result<()> {
        if self.pack_name != snapshot.pack_name
            || self.pack_source != snapshot.pack_source
            || self.manifest_sha256 != snapshot.manifest_sha256
        {
            bail!("pack runtime identity or manifest changed since the session was saved");
        }
        validate_runtime_state(&snapshot.state)?;
        validate_pending_continuations(&snapshot.pending_continuations)?;
        if snapshot.continuations_used > self.max_continuations
            || snapshot.pending_continuations.len() as u32 > snapshot.continuations_used
        {
            bail!("pack runtime continuation snapshot exceeds its declared budget");
        }
        self.state = snapshot.state.clone();
        self.continuations_used = snapshot.continuations_used;
        Ok(())
    }
}

pub(crate) fn validate_tool_input(tool: &RuntimeTool, input: &Value) -> Result<()> {
    validate_schema_value(&tool.input_schema, input, "$", 0)
        .with_context(|| format!("invalid tool args for {}", tool.name))
}

fn value_matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn validate_schema_value(schema: &Value, value: &Value, path: &str, depth: usize) -> Result<()> {
    if depth > 16 {
        bail!("{path} exceeds the schema nesting limit");
    }
    let schema = schema
        .as_object()
        .with_context(|| format!("{path} schema must be an object"))?;
    if let Some(kind) = schema.get("type").and_then(Value::as_str)
        && !value_matches_type(value, kind)
    {
        bail!("{path} must be {kind}");
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        bail!("{path} is not one of the allowed values");
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    bail!("{path}.{name} is required");
                }
            }
        }
        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
            for name in object.keys() {
                if properties.is_none_or(|properties| !properties.contains_key(name)) {
                    bail!("{path}.{name} is not allowed");
                }
            }
        }
        if let Some(properties) = properties {
            for (name, child_schema) in properties {
                if let Some(child) = object.get(name) {
                    validate_schema_value(
                        child_schema,
                        child,
                        &format!("{path}.{name}"),
                        depth + 1,
                    )?;
                }
            }
        }
    }
    if let Some(array) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for (index, child) in array.iter().enumerate() {
            validate_schema_value(items, child, &format!("{path}[{index}]"), depth + 1)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_runtime_state(state: &Value) -> Result<()> {
    if serde_json::to_vec(state)?.len() > RUNTIME_STATE_CAP {
        bail!("pack runtime state exceeds {RUNTIME_STATE_CAP} bytes");
    }
    Ok(())
}

pub(crate) fn validate_pending_continuations(prompts: &[(String, u64)]) -> Result<()> {
    if prompts.len() > RUNTIME_PENDING_CONTINUATION_LIMIT {
        bail!(
            "pack runtime pending continuation count exceeds {RUNTIME_PENDING_CONTINUATION_LIMIT}"
        );
    }
    let bytes = prompts
        .iter()
        .try_fold(0usize, |total, (prompt, delay_ms)| {
            if prompt.trim().is_empty()
                || prompt.len() > RUNTIME_CONTENT_CAP
                || *delay_ms > RUNTIME_MAX_DELAY_MS
                || contains_unsafe_runtime_control(prompt, true)
            {
                bail!("pack runtime pending continuation exceeds its size or delay limit");
            }
            total
                .checked_add(prompt.len())
                .context("pack runtime pending continuation size overflow")
        })?;
    if bytes > RUNTIME_STATE_CAP {
        bail!("pack runtime pending continuations exceed {RUNTIME_STATE_CAP} bytes");
    }
    Ok(())
}

fn contains_unsafe_runtime_control(text: &str, multiline: bool) -> bool {
    text.chars()
        .any(|ch| ch.is_control() && !(multiline && matches!(ch, '\n' | '\t')))
}

fn validate_response(response: &RuntimeResponse) -> Result<()> {
    if response.version != RUNTIME_PROTOCOL_VERSION {
        bail!(
            "pack runtime response version {} does not match protocol {}",
            response.version,
            RUNTIME_PROTOCOL_VERSION
        );
    }
    if response.content.len() > RUNTIME_CONTENT_CAP {
        bail!("pack runtime content exceeds {RUNTIME_CONTENT_CAP} bytes");
    }
    if contains_unsafe_runtime_control(&response.content, true) {
        bail!("pack runtime content contains unsafe terminal control characters");
    }
    if response.effects.len() > RUNTIME_EFFECT_LIMIT {
        bail!("pack runtime returned more than {RUNTIME_EFFECT_LIMIT} effects");
    }
    if let Some(state) = &response.state {
        validate_runtime_state(state)?;
    }
    for effect in &response.effects {
        match effect {
            RuntimeEffect::Steer { text }
                if text.trim().is_empty()
                    || text.len() > RUNTIME_CONTENT_CAP
                    || contains_unsafe_runtime_control(text, true) =>
            {
                bail!("pack runtime steer effect exceeds its size limit");
            }
            RuntimeEffect::Continue { prompt, delay_ms }
                if prompt.trim().is_empty()
                    || prompt.len() > RUNTIME_CONTENT_CAP
                    || *delay_ms > RUNTIME_MAX_DELAY_MS
                    || contains_unsafe_runtime_control(prompt, true) =>
            {
                bail!("pack runtime continue effect exceeds its size or delay limit");
            }
            RuntimeEffect::View { title, markdown }
                if title.trim().is_empty()
                    || title.len() > 256
                    || markdown.len() > RUNTIME_VIEW_CAP
                    || contains_unsafe_runtime_control(title, false)
                    || contains_unsafe_runtime_control(markdown, true) =>
            {
                bail!("pack runtime view effect exceeds its size limit");
            }
            _ => {}
        }
    }
    Ok(())
}

fn runtime_timeout(configured: Option<u64>) -> Result<Duration> {
    if configured.is_some_and(|seconds| !(1..=RUNTIME_MAX_TIMEOUT_SECS).contains(&seconds)) {
        bail!("pack runtime timeout_seconds must be between 1 and {RUNTIME_MAX_TIMEOUT_SECS}");
    }
    let seconds = match std::env::var("DEXT_PACK_RUNTIME_TIMEOUT_SECS") {
        Ok(value) => value.parse::<u64>().with_context(
            || "DEXT_PACK_RUNTIME_TIMEOUT_SECS must be an integer between 1 and 604800",
        )?,
        Err(std::env::VarError::NotPresent) => configured.unwrap_or(RUNTIME_DEFAULT_TIMEOUT_SECS),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("DEXT_PACK_RUNTIME_TIMEOUT_SECS must be valid Unicode")
        }
    };
    if !(1..=RUNTIME_MAX_TIMEOUT_SECS).contains(&seconds) {
        bail!("pack runtime timeout_seconds must be between 1 and {RUNTIME_MAX_TIMEOUT_SECS}");
    }
    Ok(Duration::from_secs(seconds))
}

pub(crate) async fn invoke(
    runtime: &ActiveRuntime,
    event: RuntimeEvent<'_>,
    root: &Path,
    session_id: &str,
    context: RuntimeContext<'_>,
    interrupt: Arc<AtomicBool>,
    sandbox_profile: SandboxProfile,
) -> Result<RuntimeInvocation> {
    validate_runtime_state(&runtime.state)?;
    let executable_sha256 = sha256_regular_bounded(
        &runtime.executable,
        RUNTIME_EXECUTABLE_CAP,
        "pack runtime executable",
    )?;
    if executable_sha256 != runtime.executable_sha256 {
        bail!("pack runtime executable changed after activation; reactivate the pack to review it");
    }
    let (event_name, tool, input) = match event {
        RuntimeEvent::Activate => ("activate", None, None),
        RuntimeEvent::Tool { name, input } => {
            let tool = runtime
                .tool(name)
                .with_context(|| format!("pack runtime does not declare tool {name}"))?;
            validate_tool_input(tool, input)?;
            ("tool", Some(name), Some(input))
        }
        RuntimeEvent::Idle => ("idle", None, None),
    };
    let request = RuntimeRequest {
        version: RUNTIME_PROTOCOL_VERSION,
        event: event_name,
        pack: &runtime.pack_name,
        session_id,
        cwd: root.display().to_string(),
        state: &runtime.state,
        context,
        tool,
        input,
    };
    let request = serde_json::to_string(&request)?;
    if request.len() > RUNTIME_REQUEST_CAP {
        bail!("pack runtime request exceeds {RUNTIME_REQUEST_CAP} bytes");
    }
    let executable = runtime.executable.to_string_lossy().into_owned();
    let (stdout, stderr, status) = execute_external_async_status(
        &executable,
        &runtime.args,
        Some(&request),
        root,
        interrupt,
        ExternalExecutionPolicy {
            timeout: runtime.timeout,
            sandbox_profile,
            allow_tool_credentials: false,
            stdout_cap: RUNTIME_RESPONSE_CAP + 1,
            stderr_cap: 16 * 1024 + 1,
        },
    )
    .await
    .map_err(anyhow::Error::msg)?;
    if status != 0 {
        let stderr =
            crate::cap_bytes_with_hint(stderr, 16 * 1024, "pack runtime stderr truncated.");
        bail!(
            "pack runtime exited with status {status}: {}",
            stderr.trim()
        );
    }
    if stdout.len() > RUNTIME_RESPONSE_CAP {
        bail!("pack runtime response exceeds {RUNTIME_RESPONSE_CAP} bytes");
    }
    let response: RuntimeResponse = serde_json::from_str(stdout.trim())
        .context("pack runtime stdout must contain one JSON response object")?;
    validate_response(&response)?;
    Ok(RuntimeInvocation {
        content: response.content,
        is_error: response.is_error,
        state: response.state,
        effects: response.effects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> RuntimeTool {
        RuntimeTool {
            name: "demo_tool".to_string(),
            description: "demo".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "count": {"type": "integer"}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            risk: RuntimeRisk::Read,
        }
    }

    #[test]
    fn runtime_schema_validation_is_fail_closed() {
        assert!(validate_tool_input(&tool(), &json!({"name": "ok", "count": 2})).is_ok());
        assert!(validate_tool_input(&tool(), &json!({"count": 2})).is_err());
        assert!(validate_tool_input(&tool(), &json!({"name": "ok", "extra": true})).is_err());
        assert!(validate_tool_input(&tool(), &json!({"name": "ok", "count": 2.5})).is_err());

        for schema in [
            json!({"type": "object", "properties": {"x": {"type": "mystery"}}}),
            json!({"type": "object", "properties": {"x": {"type": "string", "minLength": 1}}}),
            json!({"type": "object", "properties": {"x": {"type": "array"}}}),
            json!({"type": "object", "properties": {}, "required": ["missing"]}),
            json!({"type": "object", "additionalProperties": {"type": "string"}}),
            json!({"type": "object", "properties": {"x": {"type": "string", "enum": [1]}}}),
        ] {
            assert!(validate_schema(&schema).is_err(), "{schema}");
        }
    }

    #[test]
    fn runtime_tool_names_are_provider_safe() {
        assert!(valid_tool_name("init_experiment"));
        assert!(!valid_tool_name("runtime.tool"));
        assert!(!valid_tool_name("_private"));
        assert!(!valid_tool_name(""));
    }

    #[test]
    fn runtime_timeout_manifest_and_override_bounds_are_fail_closed() {
        let _guard = crate::test_env_lock();
        let old = std::env::var_os("DEXT_PACK_RUNTIME_TIMEOUT_SECS");
        unsafe {
            std::env::remove_var("DEXT_PACK_RUNTIME_TIMEOUT_SECS");
        }
        assert!(runtime_timeout(Some(0)).is_err());
        assert!(runtime_timeout(Some(RUNTIME_MAX_TIMEOUT_SECS + 1)).is_err());
        unsafe {
            std::env::set_var("DEXT_PACK_RUNTIME_TIMEOUT_SECS", "not-a-number");
        }
        let error = runtime_timeout(Some(10)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("DEXT_PACK_RUNTIME_TIMEOUT_SECS must be an integer"),
            "{error:#}"
        );
        unsafe {
            match old {
                Some(value) => std::env::set_var("DEXT_PACK_RUNTIME_TIMEOUT_SECS", value),
                None => std::env::remove_var("DEXT_PACK_RUNTIME_TIMEOUT_SECS"),
            }
        }
    }

    #[test]
    fn runtime_response_effects_are_bounded_and_terminal_safe() {
        let response = RuntimeResponse {
            version: 1,
            content: String::new(),
            is_error: false,
            state: Some(json!({"active": true})),
            effects: vec![RuntimeEffect::Continue {
                prompt: "continue".to_string(),
                delay_ms: 1,
            }],
        };
        assert!(validate_response(&response).is_ok());

        for response in [
            RuntimeResponse {
                version: 1,
                content: "unsafe\u{1b}[2J".to_string(),
                is_error: false,
                state: None,
                effects: Vec::new(),
            },
            RuntimeResponse {
                version: 1,
                content: String::new(),
                is_error: false,
                state: None,
                effects: vec![RuntimeEffect::Steer {
                    text: "unsafe\u{7}".to_string(),
                }],
            },
            RuntimeResponse {
                version: 1,
                content: String::new(),
                is_error: false,
                state: None,
                effects: vec![RuntimeEffect::View {
                    title: "unsafe\nview".to_string(),
                    markdown: "safe markdown".to_string(),
                }],
            },
        ] {
            assert!(validate_response(&response).is_err(), "{response:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_manifest_and_one_shot_protocol_round_trip() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "dext-pack-runtime-test-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        let pack_path = root.join("shelf/packs/demo");
        std::fs::create_dir_all(&pack_path).unwrap();
        std::fs::write(pack_path.join("PACK.md"), "# Demo\n").unwrap();
        let helper = pack_path.join("runtime.sh");
        let large_markdown = "x".repeat(10_000);
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nset -eu\nrequest=$(cat)\nprintf '%s' \"$request\" | grep -q '\"event\":\"activate\"'\nprintf '%s\\n' '{{\"version\":1,\"content\":\"ready\",\"state\":{{\"runs\":1}},\"effects\":[{{\"type\":\"view\",\"title\":\"Demo\",\"markdown\":\"{large_markdown}\"}}]}}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let manifest_path = pack_path.join(RUNTIME_MANIFEST_NAME);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&json!({
                "version": 1,
                "command": "runtime.sh",
                "tools": [{
                    "name": "demo_status",
                    "description": "Read demo status.",
                    "risk": "read",
                    "input_schema": {"type": "object", "additionalProperties": false}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let pack = PackInfo {
            name: "demo".to_string(),
            description: "demo runtime".to_string(),
            path: pack_path.clone(),
            pack_md_path: pack_path.join("PACK.md"),
            phooks_path: None,
            runtime_path: Some(manifest_path),
            credential_env: Vec::new(),
            credential_env_ignored: false,
            source: "test".to_string(),
            shelf: Some("shelf".to_string()),
        };
        let runtime = load(&pack, &HashSet::new()).unwrap().unwrap();
        assert_eq!(runtime.tools[0].risk, RuntimeRisk::Read);
        let result = invoke(
            &runtime,
            RuntimeEvent::Activate,
            &root,
            "session",
            RuntimeContext {
                turn_id: "turn",
                iteration: 0,
                history_messages: 0,
                compacted: false,
            },
            Arc::new(AtomicBool::new(false)),
            SandboxProfile::ReadOnly,
        )
        .await
        .unwrap();
        assert_eq!(result.content, "ready");
        assert_eq!(result.state, Some(json!({"runs": 1})));
        assert_eq!(
            result.effects,
            vec![RuntimeEffect::View {
                title: "Demo".to_string(),
                markdown: large_markdown,
            }]
        );
        std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
        let error = invoke(
            &runtime,
            RuntimeEvent::Activate,
            &root,
            "session",
            RuntimeContext {
                turn_id: "turn-2",
                iteration: 1,
                history_messages: 0,
                compacted: false,
            },
            Arc::new(AtomicBool::new(false)),
            SandboxProfile::ReadOnly,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("executable changed"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
