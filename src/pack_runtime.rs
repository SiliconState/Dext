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

#[derive(Debug, Clone)]
pub(crate) struct ActiveRuntime {
    pub(crate) pack_name: String,
    pub(crate) pack_source: String,
    pub(crate) executable: PathBuf,
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
    let object = schema
        .as_object()
        .context("pack runtime tool input_schema must be a JSON object")?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        bail!("pack runtime tool input_schema must declare type=object");
    }
    if serde_json::to_vec(schema)?.len() > RUNTIME_SCHEMA_CAP {
        bail!("pack runtime tool input_schema exceeds {RUNTIME_SCHEMA_CAP} bytes");
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .context("pack runtime tool schema required must be an array")?;
        if !required.iter().all(Value::is_string) {
            bail!("pack runtime tool schema required entries must be strings");
        }
    }
    if let Some(properties) = object.get("properties")
        && !properties.is_object()
    {
        bail!("pack runtime tool schema properties must be an object");
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
    let timeout = runtime_timeout(manifest.timeout_seconds)?;
    Ok(Some(ActiveRuntime {
        pack_name: pack.name.clone(),
        pack_source: pack.source.clone(),
        executable,
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
    pub(crate) fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            pack_name: self.pack_name.clone(),
            pack_source: self.pack_source.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            state: self.state.clone(),
            continuations_used: self.continuations_used,
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
        validate_state(&snapshot.state)?;
        self.state = snapshot.state.clone();
        self.continuations_used = snapshot.continuations_used.min(self.max_continuations);
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

fn validate_state(state: &Value) -> Result<()> {
    if serde_json::to_vec(state)?.len() > RUNTIME_STATE_CAP {
        bail!("pack runtime state exceeds {RUNTIME_STATE_CAP} bytes");
    }
    Ok(())
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
    if response.effects.len() > RUNTIME_EFFECT_LIMIT {
        bail!("pack runtime returned more than {RUNTIME_EFFECT_LIMIT} effects");
    }
    if let Some(state) = &response.state {
        validate_state(state)?;
    }
    for effect in &response.effects {
        match effect {
            RuntimeEffect::Steer { text }
                if text.is_empty() || text.len() > RUNTIME_CONTENT_CAP =>
            {
                bail!("pack runtime steer effect exceeds its size limit");
            }
            RuntimeEffect::Continue { prompt, delay_ms }
                if prompt.is_empty()
                    || prompt.len() > RUNTIME_CONTENT_CAP
                    || *delay_ms > RUNTIME_MAX_DELAY_MS =>
            {
                bail!("pack runtime continue effect exceeds its size or delay limit");
            }
            RuntimeEffect::View { title, markdown }
                if title.trim().is_empty()
                    || title.len() > 256
                    || markdown.len() > RUNTIME_VIEW_CAP =>
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
    let seconds = std::env::var("DEXT_PACK_RUNTIME_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .or(configured)
        .unwrap_or(RUNTIME_DEFAULT_TIMEOUT_SECS);
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
    validate_state(&runtime.state)?;
    let (event_name, tool, input) = match event {
        RuntimeEvent::Activate => ("activate", None, None),
        RuntimeEvent::Tool { name, input } => ("tool", Some(name), Some(input)),
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
    }

    #[test]
    fn runtime_tool_names_are_provider_safe() {
        assert!(valid_tool_name("init_experiment"));
        assert!(!valid_tool_name("runtime.tool"));
        assert!(!valid_tool_name("_private"));
        assert!(!valid_tool_name(""));
    }

    #[test]
    fn runtime_timeout_manifest_bound_is_fail_closed() {
        assert!(runtime_timeout(Some(0)).is_err());
        assert!(runtime_timeout(Some(RUNTIME_MAX_TIMEOUT_SECS + 1)).is_err());
    }

    #[test]
    fn runtime_response_effects_are_bounded() {
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
        std::fs::write(
            &helper,
            "#!/bin/sh\nset -eu\nrequest=$(cat)\nprintf '%s' \"$request\" | grep -q '\"event\":\"activate\"'\nprintf '%s\\n' '{\"version\":1,\"content\":\"ready\",\"state\":{\"runs\":1},\"effects\":[{\"type\":\"view\",\"title\":\"Demo\",\"markdown\":\"# Ready\"}]}'\n",
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
                markdown: "# Ready".to_string(),
            }]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
