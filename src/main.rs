mod orchestrator;
mod provider;
mod session;
mod tool_policy;
mod tools;
mod tui;

#[cfg(test)]
mod main_tests;

use anyhow::{Context, Result};
use provider::{
    ApiProvider, ProviderProfile, ResolvedProviderConfig, apply_provider_headers, auth_store_path,
    build_chatgpt_request, build_chatgpt_summary_request, built_in_provider_profiles,
    cancel_pending_oauth_login, canonical_provider_id, extract_oauth_code_from_callback,
    handle_auth_cli, list_models_for_available_providers, list_models_for_provider,
    load_auth_store, load_provider_catalog, login_provider, logout_provider,
    looks_like_login_secret_input, provider_auth_status, provider_catalog_path,
    provider_id_from_selector, provider_request_url, render_provider_list, render_provider_picker,
    resolve_active_provider_id, resolve_provider_model_selection, resolve_runtime_provider,
    set_active_provider_in_catalog, set_provider_default_model_in_catalog,
    try_complete_oauth_from_callback,
};
use session::{
    ProjectStateLock, atomic_write_bytes, latest_log_path, latest_session_path,
    list_session_records_for_root, named_session_path_for_root, named_sessions_dir_for_root,
    parse_session_header, project_key, project_state_dir, project_state_lock_path,
    release_registered_locks, render_limited_csv, restore_terminal_if_tui, unix_timestamp_secs,
    wolf_state_dir,
};
use tools::{
    Tool, ToolProfile, is_external_process_tool, needs_permission,
    should_parallelize_builtin_tools, tool_definitions,
};

#[cfg(test)]
use provider::{
    ProviderCatalog, StoredCredential, find_provider_profile, login_provider_with_key,
    normalize_login_secret, oauth_exchange_failure_result_message, resolve_provider_api_key,
    save_auth_store, save_provider_catalog,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const TOOL_RESULT_CAP: usize = 12_000;
const FRUGAL_TOOL_RESULT_CAP: usize = 6_000;
const TEXT_TOOL_CAPTURE_CAP: usize = 10_000;
const FRUGAL_TEXT_TOOL_CAPTURE_CAP: usize = 6_000;
const READ_FILE_EXPLICIT_CAPTURE_CAP: usize = 16_000;
const FRUGAL_READ_FILE_EXPLICIT_CAPTURE_CAP: usize = 10_000;
const PROCESS_STREAM_CAPTURE_CAP: usize = 6_000;
const HTTP_EXTRACT_INPUT_CAP: usize = 128_000;
const HTTP_EXTRACT_OUTPUT_CAP: usize = 24_000;
const HOOK_OUTPUT_CAPTURE_CAP: usize = 4_000;
const HTTP_ERROR_BODY_CAP: usize = 4_000;
const PROJECT_CONTEXT_CAP: usize = 12_000;
const FRUGAL_PROJECT_CONTEXT_CAP: usize = 6_000;
const SUMMARY_TRANSCRIPT_CAP: usize = 24_000;
const FRUGAL_SUMMARY_TRANSCRIPT_CAP: usize = 10_000;
const LOG_DETAIL_CAP: usize = 2_000;
const LATEST_LOG_CAP: usize = 64_000;
const LATEST_LOG_ARCHIVE_MAX: u32 = 16;
const SLASH_LIST_LIMIT: usize = 50;
const SLASH_TEXT_CAP: usize = 8_000;
const PROJECT_STATE_LOCK_NAME: &str = "owner.lock.json";
const PROJECT_STATE_LOCK_STALE_SECS: u64 = 86_400;
const STREAM_EVENT_BUFFER_CAP: usize = 256_000;
const TOOL_SUMMARY_CHAR_CAP: usize = 180;
const TOOL_UI_CONTENT_CAP: usize = 8_000;
const VERIFICATION_ARTIFACT_TAIL_CAP: usize = 2_000;
pub(crate) const BASH_UNSAFE_FLAG_OVERRIDE_ENV: &str = "WOLF_ALLOW_BREAK_SYSTEM_PACKAGES";
const AUTH_CIRCUIT_BREAKER_THRESHOLD: usize = 2;
const TOOL_CATALOG_VERSION: u32 = 2;
const MAX_STREAM_ATTEMPTS: u32 = 4;
const DEFAULT_INPUT_USD_PER_MTOK: f64 = 1.0;
const DEFAULT_OUTPUT_USD_PER_MTOK: f64 = 5.0;
const DEFAULT_CACHE_READ_USD_PER_MTOK: f64 = 0.1;
const DEFAULT_CACHE_CREATE_USD_PER_MTOK: f64 = 1.25;
const SESSION_HTML_STYLE: &str = r#"body{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;max-width:980px;margin:2rem auto;padding:0 1rem;background:#0f1115;color:#e6edf3}a{color:#8ab4ff}.meta{color:#9aa4b2;margin-bottom:1.5rem}.msg{border:1px solid #283241;border-radius:12px;margin:1rem 0;padding:1rem;background:#151922}.role{font-weight:700;text-transform:uppercase;font-size:.8rem;letter-spacing:.08em;margin-bottom:.6rem;color:#9aa4b2}.user{border-left:4px solid #7dd3fc}.assistant{border-left:4px solid #a78bfa}.tool{border-left:4px solid #f59e0b}.thinking{color:#9aa4b2}.block{white-space:pre-wrap;line-height:1.45}.tool-name{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;color:#fbbf24}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#0b0d12;border:1px solid #283241;border-radius:8px;padding:.8rem}code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}.err{color:#fca5a5}.ok{color:#86efac}summary{cursor:pointer}.footer{margin:2rem 0;color:#687385;font-size:.85rem}"#;

fn api_family_label(provider: ApiProvider) -> &'static str {
    match provider {
        ApiProvider::Anthropic => "anthropic-messages",
        ApiProvider::OpenAi => "openai-chat-completions",
        ApiProvider::ChatGpt => "chatgpt-responses",
    }
}

#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn millis_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThinkingEffort {
    Low,
    #[default]
    Medium,
    High,
    XHigh,
}

impl ThinkingEffort {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    fn guidance(self) -> &'static str {
        match self {
            Self::Low => "Keep reasoning concise and deliver a direct answer quickly.",
            Self::Medium => "Use balanced reasoning depth and concise explanations.",
            Self::High => "Reason carefully through edge cases before answering.",
            Self::XHigh => {
                "Use deep, methodical reasoning with explicit verification before answering."
            }
        }
    }

    fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "low" | "l" => Some(Self::Low),
            "medium" | "med" | "m" | "default" => Some(Self::Medium),
            "high" | "h" => Some(Self::High),
            "xhigh" | "x-high" | "veryhigh" | "very-high" | "xh" => Some(Self::XHigh),
            _ => None,
        }
    }

    fn cycle(self, step: i8) -> Self {
        let levels = [Self::Low, Self::Medium, Self::High, Self::XHigh];
        let idx = levels.iter().position(|v| *v == self).unwrap_or(1) as i32;
        let len = levels.len() as i32;
        let next = (idx + i32::from(step)).rem_euclid(len) as usize;
        levels[next]
    }
}

fn dim(s: &str, pretty: bool) -> String {
    if pretty {
        format!("\x1b[90m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn accent(s: &str, pretty: bool) -> String {
    if pretty {
        format!("\x1b[36m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn stream_error_body(err: &anyhow::Error) -> String {
    let body = format!("{err:#}");
    let lower = body.to_ascii_lowercase();
    if lower.contains("error decoding response body")
        && lower.contains("unexpected eof during chunk size line")
    {
        "transient stream transport error: provider closed the chunked response early (unexpected EOF during chunk size line)".to_string()
    } else {
        body
    }
}

fn stream_chunk_err(e: reqwest::Error) -> anyhow::Error {
    let raw = e.to_string();
    let lower = raw.to_ascii_lowercase();
    if lower.contains("error decoding response body")
        && lower.contains("unexpected eof during chunk size line")
    {
        anyhow::anyhow!(
            "transient stream transport error: provider closed the chunked response early (unexpected EOF during chunk size line)"
        )
    } else {
        anyhow::Error::new(e)
    }
}

async fn read_stream_next_chunk(
    stream: &mut (
             impl futures_util::Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Unpin
         ),
    interrupt: &AtomicBool,
    interrupted_msg: &str,
) -> Result<Option<bytes::Bytes>> {
    use futures_util::StreamExt;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(25));
    loop {
        if interrupt.load(Ordering::SeqCst) {
            anyhow::bail!("{interrupted_msg}");
        }
        tokio::select! {
            chunk = stream.next() => {
                return match chunk {
                    Some(Ok(chunk)) => Ok(Some(chunk)),
                    Some(Err(e)) => Err(stream_chunk_err(e)),
                    None => Ok(None),
                };
            }
            _ = ticker.tick() => {}
        }
    }
}

fn maybe_preserve_partial_stream(blocks: &[Block], history: &mut Vec<Message>) -> bool {
    if blocks.is_empty()
        || !blocks
            .iter()
            .any(|b| matches!(b, Block::Text { .. } | Block::PartialStream { .. }))
    {
        return false;
    }
    if let Some(last) = history.last()
        && last.role == "assistant"
        && last.content == blocks
    {
        return false;
    }
    history.push(Message {
        role: "assistant".to_string(),
        content: blocks.to_vec(),
    });
    true
}

fn panic_payload_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
}

fn panic_info_is_broken_pipe(info: &std::panic::PanicHookInfo<'_>) -> bool {
    panic_payload_text(info.payload()).is_some_and(|msg| msg.contains("Broken pipe"))
}

#[derive(Default)]
struct CrashRuntimeState {
    current_session_id: Option<String>,
    last_event_ids: Vec<String>,
}

fn crash_runtime_state() -> &'static Mutex<CrashRuntimeState> {
    static STATE: OnceLock<Mutex<CrashRuntimeState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(CrashRuntimeState::default()))
}

fn record_crash_session_id(path: &Path) {
    if let Ok(mut state) = crash_runtime_state().lock() {
        state.current_session_id = Some(path.display().to_string());
    }
}

pub(crate) fn record_crash_event(event: &AgentEvent) {
    let label = match event {
        AgentEvent::TurnStart => Some("turn_start".to_string()),
        AgentEvent::ToolCallPreview { call_id, name, .. } => {
            Some(format!("tool_preview:{name}:{call_id}"))
        }
        AgentEvent::ToolCallStart { call_id, name, .. } => {
            Some(format!("tool_start:{name}:{call_id}"))
        }
        AgentEvent::ToolCallResult {
            call_id, name, ok, ..
        } => Some(format!("tool_result:{name}:{call_id}:ok={ok}")),
        AgentEvent::ToolBatchStart { batch_id, .. } => Some(format!("tool_batch_start:{batch_id}")),
        AgentEvent::ToolBatchEnd { batch_id, .. } => Some(format!("tool_batch_end:{batch_id}")),
        AgentEvent::HttpRetry {
            attempt, reason, ..
        } => Some(format!(
            "http_retry:{attempt}:{}",
            summarize_inline(reason, 80)
        )),
        AgentEvent::CompactStart => Some("compact_start".to_string()),
        AgentEvent::CompactEnd { before, after } => Some(format!("compact_end:{before}->{after}")),
        AgentEvent::CompactFailed { message } => {
            Some(format!("compact_failed:{}", summarize_inline(message, 80)))
        }
        AgentEvent::Interrupted => Some("interrupted".to_string()),
        AgentEvent::SteeringReceived { messages, preview } => Some(format!(
            "steering:{messages}:{}",
            summarize_inline(preview, 80)
        )),
        AgentEvent::TurnEnd { .. } => Some("turn_end".to_string()),
        _ => None,
    };
    let Some(label) = label else {
        return;
    };
    if let Ok(mut state) = crash_runtime_state().lock() {
        state.last_event_ids.push(label);
        if state.last_event_ids.len() > 24 {
            let excess = state.last_event_ids.len() - 24;
            state.last_event_ids.drain(0..excess);
        }
    }
}

fn write_crash_snapshot(info: &std::panic::PanicHookInfo<'_>) -> Option<PathBuf> {
    let id = format!("crash-{}-{}", unix_timestamp_secs(), std::process::id());
    let path = wolf_state_dir().join("crashes").join(format!("{id}.json"));
    let location = info
        .location()
        .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()));
    let runtime = crash_runtime_state().lock().ok().map(|state| {
        json!({
            "current_session_id": state.current_session_id,
            "last_event_ids": state.last_event_ids,
            "input_buffer_state": null,
            "active_modal": null,
        })
    });
    let body = json!({
        "id": id,
        "panic": panic_payload_text(info.payload()).unwrap_or("unknown panic"),
        "location": location,
        "terminal": {
            "columns": std::env::var("COLUMNS").ok(),
            "lines": std::env::var("LINES").ok(),
        },
        "pid": std::process::id(),
        "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
        "runtime": runtime,
        "backtrace": std::env::var("RUST_BACKTRACE").unwrap_or_default(),
    });
    let bytes = serde_json::to_vec_pretty(&body).ok()?;
    atomic_write_bytes(&path, &bytes).ok()?;
    Some(path)
}

fn byte_prefix_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn byte_suffix_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn normalize_tool_call_id(raw: &str, turn: u32, ordinal: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        format!("call-{turn}-{}", ordinal + 1)
    } else {
        trimmed.to_string()
    }
}

fn next_subagent_run_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn namespace_subagent_call_id(run_id: u64, call_id: &str) -> String {
    format!("sub.{run_id}.{}", call_id)
}

fn namespace_subagent_batch_id(run_id: u64, batch_id: &str) -> String {
    format!("sub.{run_id}.{}", batch_id)
}

#[derive(Debug, Default)]
struct LimitedTextCapture {
    cap: usize,
    kept: String,
    observed_bytes: usize,
    truncated: bool,
}

impl LimitedTextCapture {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Default::default()
        }
    }

    fn try_push_unit(&mut self, unit: &str) -> bool {
        self.observed_bytes += unit.len();
        if self.kept.len() + unit.len() <= self.cap {
            self.kept.push_str(unit);
            return true;
        }
        if self.kept.is_empty() {
            self.kept
                .push_str(byte_prefix_at_char_boundary(unit, self.cap));
        }
        self.truncated = true;
        false
    }

    fn finish(mut self, hint: &str) -> String {
        if self.truncated {
            if !self.kept.is_empty() && !self.kept.ends_with('\n') {
                self.kept.push('\n');
            }
            let suffix = if hint.is_empty() {
                String::new()
            } else {
                format!(" {hint}")
            };
            self.kept.push_str(&format!(
                "\n…[output capped after {} bytes observed; kept first {}.{}]\n",
                self.observed_bytes, self.cap, suffix
            ));
        }
        self.kept
    }
}

#[derive(Debug, Default)]
struct LimitedByteCapture {
    cap: usize,
    kept: Vec<u8>,
    observed_bytes: usize,
    truncated: bool,
}

impl LimitedByteCapture {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Default::default()
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.observed_bytes += chunk.len();
        if self.kept.len() < self.cap {
            let remaining = self.cap - self.kept.len();
            let take = remaining.min(chunk.len());
            self.kept.extend_from_slice(&chunk[..take]);
        }
        self.truncated = self.observed_bytes > self.kept.len();
    }

    fn render(&self, label: &str) -> String {
        let mut out = String::from_utf8_lossy(&self.kept).to_string();
        if self.truncated {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!(
                "\n…[{label} capped after {} bytes observed; kept first {}]\n",
                self.observed_bytes, self.cap
            ));
        }
        out
    }
}

fn cap_bytes_with_hint(s: String, cap: usize, hint: &str) -> String {
    if s.len() <= cap {
        return s;
    }
    let kept = byte_prefix_at_char_boundary(&s, cap).to_string();
    let suffix = if hint.is_empty() {
        String::new()
    } else {
        format!(" {hint}")
    };
    format!(
        "{kept}\n\n…[truncated after {} bytes; kept first {}.{}]",
        s.len(),
        cap,
        suffix
    )
}

fn cap_tool_output_with_cap(s: String, cap: usize) -> String {
    cap_bytes_with_hint(
        s,
        cap,
        "Narrow/paginate; prefer symbol-scoped reads or targeted diff/stat before retrying.",
    )
}

fn cap_tool_output(s: String) -> String {
    cap_tool_output_with_cap(s, TOOL_RESULT_CAP)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    len: u64,
    modified_ns: Option<u128>,
}

fn file_signature_from_metadata(metadata: &std::fs::Metadata) -> FileSignature {
    FileSignature {
        len: metadata.len(),
        modified_ns: metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos()),
    }
}

#[derive(Debug, Default)]
struct CachedReadFile {
    signature: Option<FileSignature>,
    lines: BTreeMap<usize, String>,
    eof_at: Option<usize>,
}

#[derive(Debug, Default)]
struct ReadFileCache {
    files: HashMap<PathBuf, CachedReadFile>,
}

impl ReadFileCache {
    fn get_window(
        &self,
        path: &Path,
        signature: FileSignature,
        offset: usize,
        limit: usize,
        cap: usize,
    ) -> Option<String> {
        let cached = self.files.get(path)?;
        if cached.signature != Some(signature) {
            return None;
        }

        let mut capture = LimitedTextCapture::new(cap);
        for line_no in offset..offset.saturating_add(limit) {
            if let Some(line) = cached.lines.get(&line_no) {
                let rendered = format!("{line_no}\t{line}\n");
                if !capture.try_push_unit(&rendered) {
                    return None;
                }
            } else if cached.eof_at.is_some_and(|last| line_no > last) {
                break;
            } else {
                return None;
            }
        }
        Some(capture.finish(""))
    }

    fn record_window(
        &mut self,
        path: PathBuf,
        signature: FileSignature,
        lines: Vec<(usize, String)>,
        eof_at: Option<usize>,
    ) {
        let cached = self.files.entry(path).or_default();
        if cached.signature != Some(signature) {
            *cached = CachedReadFile {
                signature: Some(signature),
                ..Default::default()
            };
        }
        for (line_no, line) in lines {
            cached.lines.insert(line_no, line);
        }
        if let Some(eof_at) = eof_at {
            cached.eof_at = Some(cached.eof_at.map_or(eof_at, |old| old.max(eof_at)));
        }
    }
}

fn canonical_within(root: &Path, user_path: &str) -> std::result::Result<PathBuf, String> {
    let candidate = if Path::new(user_path).is_absolute() {
        PathBuf::from(user_path)
    } else {
        root.join(user_path)
    };
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(_) => {
            // Non-existent target (e.g., write_file creating a new file) — verify parent.
            let parent = candidate.parent().ok_or("path has no parent")?;
            let name = candidate.file_name().ok_or("path has no filename")?;
            let parent_canon = std::fs::canonicalize(parent)
                .map_err(|e| format!("parent dir does not exist or not accessible: {e}"))?;
            parent_canon.join(name)
        }
    };
    if !canonical.starts_with(root) {
        return Err(format!(
            "path outside sandbox ({}): {}",
            root.display(),
            canonical.display()
        ));
    }
    Ok(canonical)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OutputMode {
    #[default]
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ApprovalProfile {
    #[default]
    Ask,
    AutoRead,
    AutoWrite,
    Never,
    Always,
}

impl ApprovalProfile {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ask" | "on-request" | "request" | "prompt" | "default" | "guarded" => Some(Self::Ask),
            "auto-read" | "read" | "read-only" => Some(Self::AutoRead),
            "auto-write" | "trusted" | "on-failure" => Some(Self::AutoWrite),
            "never" | "deny" | "no-ask" => Some(Self::Never),
            "always" | "auto" | "fangs-out" | "danger" => Some(Self::Always),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoRead => "auto-read",
            Self::AutoWrite => "auto-write",
            Self::Never => "never",
            Self::Always => "always",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SandboxProfile {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxProfile {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "read-only" | "readonly" | "ro" => Some(Self::ReadOnly),
            "workspace-write" | "workspace" | "write" | "default" | "guarded" => {
                Some(Self::WorkspaceWrite)
            }
            "danger-full-access" | "full-access" | "danger" | "unrestricted" => {
                Some(Self::DangerFullAccess)
            }
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BrowserRecipe {
    #[default]
    Disabled,
    AgentBrowser,
}

impl BrowserRecipe {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "false" | "off" | "none" | "disabled" => Some(Self::Disabled),
            "1" | "true" | "on" | "agent-browser" | "agent_browser" | "browser" => {
                Some(Self::AgentBrowser)
            }
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "off",
            Self::AgentBrowser => "agent-browser",
        }
    }
}

impl OutputMode {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "stream-json" | "stream_json" | "jsonl" => Some(Self::StreamJson),
            _ => None,
        }
    }

    fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::StreamJson)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashUiUpdate {
    None,
    ModelProvider,
    ThinkingEffort,
    ApprovalProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactSlash {
    RunNow,
    Status,
    Auto,
    SetPercent(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveInputRoute {
    Submitted,
    SteeringQueued,
    Dropped,
}

fn route_interactive_input_line(
    line: String,
    agent_busy: &AtomicBool,
    input_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    steering_tx: &tokio::sync::mpsc::UnboundedSender<String>,
) -> InteractiveInputRoute {
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        return InteractiveInputRoute::Dropped;
    }
    if agent_busy.load(Ordering::SeqCst) {
        if steering_tx.send(trimmed).is_ok() {
            InteractiveInputRoute::SteeringQueued
        } else {
            InteractiveInputRoute::Dropped
        }
    } else if input_tx.send(trimmed).is_ok() {
        InteractiveInputRoute::Submitted
    } else {
        InteractiveInputRoute::Dropped
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkMapEventKind {
    Map,
    Packet,
    Focus,
    Tracks,
}

#[derive(Serialize, Clone)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
enum AgentEvent {
    TurnStart,
    HistoryContextUpdated {
        chars: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<u64>,
    },
    TextDelta(String),
    TextBlockComplete(String),
    ThinkingDelta(String),
    ThinkingBlockComplete(String),
    ToolCallPreview {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolCallStart {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolCallResult {
        call_id: String,
        name: String,
        ok: bool,
        preview: String,
        content: String,
    },
    ToolBatchStart {
        batch_id: String,
        call_ids: Vec<String>,
        labels: Vec<String>,
    },
    ToolBatchEnd {
        batch_id: String,
        call_ids: Vec<String>,
        labels: Vec<String>,
        failed: usize,
    },
    UsageUpdate {
        turn: Usage,
        session: Usage,
    },
    HttpRetry {
        attempt: u32,
        wait_secs: u64,
        reason: String,
    },
    ExternalTelemetry {
        telemetry: orchestrator::ExternalTelemetry,
    },
    TurnDiagnostics {
        provider: String,
        api_family: String,
        auth_source: String,
        model: String,
        last_retry_reason: Option<String>,
        workaround_fired: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_mode: Option<ContextMode>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_profile: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        compacted: Option<bool>,
    },
    ThinkingEffortChanged {
        effort: ThinkingEffort,
    },
    ApprovalProfileChanged {
        profile: ApprovalProfile,
    },
    Info(String),
    Warn(String),
    Error(String),
    Slash(String),
    WorkMap {
        kind: WorkMapEventKind,
        text: String,
        waypoint_ids: Vec<String>,
        selector: Option<String>,
    },
    TurnEnd {
        usage: Usage,
    },
    CompactStart,
    CompactEnd {
        before: usize,
        after: usize,
    },
    CompactFailed {
        message: String,
    },
    Interrupted,
    SteeringReceived {
        messages: usize,
        preview: String,
    },
}

trait EventSink: Send + Sync {
    fn emit(&mut self, event: AgentEvent);
    fn request_permission(&mut self, name: &str, input: &Value) -> Choice;
}

struct ConsoleSink {
    pretty: bool,
    silent: bool,
    printed_any_text_this_block: bool,
    printed_prefix: bool,
    text_accum: String,
}

impl ConsoleSink {
    fn new(pretty: bool, silent: bool) -> Self {
        Self {
            pretty,
            silent,
            printed_any_text_this_block: false,
            printed_prefix: false,
            text_accum: String::new(),
        }
    }
}

impl EventSink for ConsoleSink {
    fn emit(&mut self, event: AgentEvent) {
        record_crash_event(&event);
        if self.silent {
            return;
        }
        match event {
            AgentEvent::TurnStart => {}
            AgentEvent::HistoryContextUpdated { .. } => {}
            AgentEvent::TextDelta(t) => {
                if self.pretty {
                    if !self.printed_prefix {
                        print!("wolf> …");
                        let _ = io::stdout().flush();
                        self.printed_prefix = true;
                    }
                    self.text_accum.push_str(&t);
                } else {
                    if !self.printed_prefix {
                        print!("wolf> ");
                        self.printed_prefix = true;
                    }
                    print!("{t}");
                    let _ = io::stdout().flush();
                    self.printed_any_text_this_block = true;
                }
            }
            AgentEvent::TextBlockComplete(full) => {
                if self.pretty && !full.is_empty() {
                    print!("\r\x1b[2K");
                    let _ = io::stdout().flush();
                    print!("{full}");
                    if !full.ends_with('\n') {
                        println!();
                    }
                    self.printed_prefix = false;
                    self.printed_any_text_this_block = false;
                    self.text_accum.clear();
                } else if self.printed_any_text_this_block {
                    println!();
                    self.printed_any_text_this_block = false;
                    self.printed_prefix = false;
                }
            }
            AgentEvent::ToolCallPreview { summary, .. } => {
                if self.printed_any_text_this_block {
                    println!();
                    self.printed_any_text_this_block = false;
                    self.printed_prefix = false;
                }
                let marker = accent("▶", self.pretty);
                let line = dim(&summary, self.pretty);
                println!("{marker} {line}");
            }
            AgentEvent::ToolCallStart { .. } => {}
            AgentEvent::ToolCallResult { .. } => {}
            AgentEvent::ToolBatchStart { .. } => {}
            AgentEvent::ToolBatchEnd { .. } => {}
            AgentEvent::UsageUpdate { .. } => {}
            AgentEvent::HttpRetry {
                attempt,
                wait_secs,
                reason,
            } => {
                eprintln!("[retry {attempt}/4: {reason}, waiting {wait_secs}s]");
            }
            AgentEvent::ExternalTelemetry { .. } => {}
            AgentEvent::TurnDiagnostics { .. } => {}
            AgentEvent::ThinkingEffortChanged { .. } => {}
            AgentEvent::ApprovalProfileChanged { .. } => {}
            AgentEvent::Info(s) => println!("{s}"),
            AgentEvent::Warn(s) => eprintln!("{s}"),
            AgentEvent::Error(s) => eprintln!("{s}"),
            AgentEvent::Slash(s) => println!("{s}"),
            AgentEvent::WorkMap { text, .. } => println!("{text}"),
            AgentEvent::TurnEnd { usage } => {
                println!(
                    "{}",
                    dim(&format!("[usage: {}]", usage.line()), self.pretty)
                );
            }
            AgentEvent::CompactStart => {}
            AgentEvent::CompactEnd { before, after } => {
                println!("[compacted {before} → {after} messages]");
            }
            AgentEvent::CompactFailed { message } => {
                eprintln!("[compact failed: {message}]");
            }
            AgentEvent::Interrupted => eprintln!("[interrupted]"),
            AgentEvent::ThinkingDelta(_) => {}
            AgentEvent::ThinkingBlockComplete(_) => {}
            AgentEvent::SteeringReceived { messages, preview } => {
                let noun = if messages == 1 { "update" } else { "updates" };
                eprintln!(
                    "[queued {messages} {noun}; folding into next response — {}]",
                    summarize_inline(&preview, 120)
                );
            }
        }
    }

    fn request_permission(&mut self, name: &str, input: &Value) -> Choice {
        prompt_permission(name, input, self.pretty)
    }
}

#[derive(Serialize, Default)]
struct OutputStreamState {
    text: String,
}

struct JsonSink {
    mode: OutputMode,
    inner: ConsoleSink,
    stream: OutputStreamState,
}

impl JsonSink {
    fn new(mode: OutputMode, pretty: bool, silent: bool) -> Self {
        Self {
            mode,
            inner: ConsoleSink::new(pretty, silent),
            stream: OutputStreamState::default(),
        }
    }

    fn emit_json_line(value: &Value) {
        if let Ok(line) = serde_json::to_string(value) {
            println!("{line}");
        }
    }
}

impl EventSink for JsonSink {
    fn emit(&mut self, event: AgentEvent) {
        record_crash_event(&event);
        match self.mode {
            OutputMode::Text => self.inner.emit(event),
            OutputMode::StreamJson => {
                match &event {
                    AgentEvent::TextDelta(delta) => self.stream.text.push_str(delta),
                    AgentEvent::TextBlockComplete(full) => self.stream.text = full.clone(),
                    _ => {}
                }
                if let Ok(value) = serde_json::to_value(&event) {
                    Self::emit_json_line(&value);
                }
            }
            OutputMode::Json => match event {
                AgentEvent::WorkMap { text, .. } => {
                    Self::emit_json_line(&json!({
                        "event": "work_map",
                        "data": {"text": text}
                    }));
                }
                AgentEvent::TextDelta(delta) => self.stream.text.push_str(&delta),
                AgentEvent::TextBlockComplete(full) => self.stream.text = full,
                AgentEvent::TurnEnd { usage } => {
                    Self::emit_json_line(&json!({
                        "event": "final",
                        "data": {
                            "text": self.stream.text,
                            "usage": usage,
                        }
                    }));
                }
                AgentEvent::Error(message) => {
                    Self::emit_json_line(&json!({
                        "event": "error",
                        "data": {"message": message}
                    }));
                }
                _ => {}
            },
        }
    }

    fn request_permission(&mut self, name: &str, input: &Value) -> Choice {
        self.inner.request_permission(name, input)
    }
}

struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: AgentEvent) {
        record_crash_event(&event);
        let _ = self.tx.send(event);
    }
    fn request_permission(&mut self, _name: &str, _input: &Value) -> Choice {
        Choice::Deny
    }
}

fn emit_external_telemetry(sink: &mut dyn EventSink, state: &orchestrator::TurnRuntimeState) {
    sink.emit(AgentEvent::ExternalTelemetry {
        telemetry: state.telemetry(),
    });
}

fn emit_work_map_event(
    sink: &mut dyn EventSink,
    kind: WorkMapEventKind,
    text: String,
    waypoint_ids: Vec<String>,
    selector: Option<String>,
) {
    sink.emit(AgentEvent::WorkMap {
        kind,
        text,
        waypoint_ids,
        selector,
    });
}

#[cfg(test)]
struct NullSink;

#[cfg(test)]
impl EventSink for NullSink {
    fn emit(&mut self, event: AgentEvent) {
        record_crash_event(&event);
    }
    fn request_permission(&mut self, _name: &str, _input: &Value) -> Choice {
        Choice::Deny
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentMode {
    Inline,
    Detached,
}

impl SubagentMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "inline" | "foreground" | "fg" => Some(Self::Inline),
            "detached" | "background" | "bg" | "window" | "tmux" | "terminal" => {
                Some(Self::Detached)
            }
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
struct SubagentRequest {
    task: String,
    system: Option<String>,
    allowed_tools: Option<Vec<String>>,
    max_iterations: Option<u64>,
    approval_profile: ApprovalProfile,
    sandbox_profile: SandboxProfile,
    browser_recipe: BrowserRecipe,
    thinking_effort: ThinkingEffort,
    context_mode: ContextMode,
    tool_profile: ToolProfile,
    budget_cap: Option<BudgetCap>,
    privacy_enabled: bool,
}

impl Default for SubagentRequest {
    fn default() -> Self {
        Self {
            task: String::new(),
            system: None,
            allowed_tools: None,
            max_iterations: None,
            approval_profile: ApprovalProfile::default(),
            sandbox_profile: SandboxProfile::default(),
            browser_recipe: BrowserRecipe::default(),
            thinking_effort: ThinkingEffort::default(),
            context_mode: ContextMode::default(),
            tool_profile: ToolProfile::default(),
            budget_cap: None,
            privacy_enabled: true,
        }
    }
}

impl SubagentRequest {
    fn from_input(agent: &Agent, input: &Value) -> std::result::Result<Self, String> {
        let task = input["task"].as_str().ok_or("missing task")?.to_string();
        let system = input["system"].as_str().map(ToString::to_string);
        let allowed_tools = input["allowed_tools"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
        Ok(Self {
            task,
            system,
            allowed_tools,
            max_iterations: input["max_iterations"].as_u64(),
            approval_profile: agent.approval_profile,
            sandbox_profile: agent.sandbox_profile,
            browser_recipe: agent.browser_recipe,
            thinking_effort: agent.thinking_effort,
            context_mode: agent.context_mode,
            tool_profile: agent.tool_profile,
            budget_cap: agent.budget_cap,
            privacy_enabled: agent.privacy.enabled,
        })
    }

    fn to_tool_input(&self) -> Value {
        let mut input = json!({
            "task": self.task,
        });
        if let Some(max_iterations) = self.max_iterations {
            input["max_iterations"] = json!(max_iterations);
        }
        if let Some(system) = &self.system {
            input["system"] = json!(system);
        }
        if let Some(tools) = &self.allowed_tools {
            input["allowed_tools"] = json!(tools);
        }
        input
    }
}

#[derive(Debug)]
struct DetachedSubagentLaunch {
    label: String,
    report_path: PathBuf,
    stdout_path: PathBuf,
    command: String,
    monitor_command: String,
}

#[cfg(unix)]
fn shell_single_quote(raw: &str) -> String {
    if raw.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn subagent_requests_dir(root: &Path) -> PathBuf {
    project_state_dir(root).join("subagents")
}

fn write_subagent_request(root: &Path, request: &SubagentRequest) -> Result<(PathBuf, PathBuf)> {
    let dir = subagent_requests_dir(root);
    std::fs::create_dir_all(&dir)?;
    let run_id = next_subagent_run_id();
    let request_path = dir.join(format!("subagent-{run_id}.request.json"));
    let report_path = dir.join(format!("subagent-{run_id}.report.md"));
    let bytes = serde_json::to_vec_pretty(request)?;
    if let Some(parent) = request_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_bytes(&request_path, &bytes)?;
    Ok((request_path, report_path))
}

fn spawn_detached_subagent_process(
    root: &Path,
    request_path: &Path,
    report_path: &Path,
) -> Result<DetachedSubagentLaunch> {
    let exe = std::env::current_exe().context("resolve current wolf executable")?;
    let stdout_path = report_path.with_extension("stdout.log");
    let task = format!(
        "wolf subagent worker {} {}",
        request_path.display(),
        report_path.display()
    );
    #[cfg(unix)]
    let monitor_command = format!(
        "tail -f {}",
        shell_single_quote(&stdout_path.display().to_string())
    );
    #[cfg(windows)]
    let monitor_command = format!(
        "powershell -NoProfile -Command Get-Content -Wait {}",
        stdout_path.display()
    );
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut args = vec![
        "subagent-worker".to_string(),
        request_path.display().to_string(),
        report_path.display().to_string(),
    ];
    let mut label;

    #[cfg(unix)]
    {
        if binary_on_path("tmux") {
            label = "tmux".to_string();
            let quoted: Vec<String> = std::iter::once(exe.display().to_string())
                .chain(args.iter().cloned())
                .map(|s| shell_single_quote(&s))
                .collect();
            let command = format!(
                "{}; printf '\\n[subagent complete — report: {}]\\n'; printf 'press Enter to close '; read _",
                quoted.join(" "),
                shell_single_quote(&report_path.display().to_string())
            );
            let status = Command::new("tmux")
                .args(["new-window", "-n", "wolf-subagent", &command])
                .current_dir(root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("launch tmux subagent window")?;
            if status.success() {
                return Ok(DetachedSubagentLaunch {
                    label,
                    report_path: report_path.to_path_buf(),
                    stdout_path,
                    command: task,
                    monitor_command,
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if binary_on_path("osascript") {
            label = "Terminal.app".to_string();
            let quoted: Vec<String> = std::iter::once(exe.display().to_string())
                .chain(args.iter().cloned())
                .map(|s| shell_single_quote(&s))
                .collect();
            let command = format!(
                "cd {}; {}; printf '\\n[subagent complete — report: {}]\\n'",
                shell_single_quote(&root.display().to_string()),
                quoted.join(" "),
                shell_single_quote(&report_path.display().to_string())
            );
            let script = format!(
                "tell application \"Terminal\" to do script {}",
                serde_json::to_string(&command)?
            );
            Command::new("osascript")
                .args(["-e", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("launch Terminal.app subagent window")?;
            return Ok(DetachedSubagentLaunch {
                label,
                report_path: report_path.to_path_buf(),
                stdout_path,
                command: task,
                monitor_command,
            });
        }
    }

    #[cfg(windows)]
    {
        label = "new console".to_string();
        let exe_arg = exe.display().to_string();
        let mut cmd_args = vec![
            "/C".to_string(),
            "start".to_string(),
            "wolf-subagent".to_string(),
            exe_arg,
        ];
        cmd_args.append(&mut args);
        Command::new("cmd")
            .args(cmd_args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("launch Windows subagent console")?;
        return Ok(DetachedSubagentLaunch {
            label,
            report_path: report_path.to_path_buf(),
            stdout_path,
            command: task,
            monitor_command,
        });
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for terminal in ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"] {
            if !binary_on_path(terminal) {
                continue;
            }
            let launched = match terminal {
                "gnome-terminal" => Command::new(terminal)
                    .arg("--working-directory")
                    .arg(root)
                    .arg("--")
                    .arg(&exe)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn(),
                "konsole" => Command::new(terminal)
                    .arg("--workdir")
                    .arg(root)
                    .arg("-e")
                    .arg(&exe)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn(),
                _ => Command::new(terminal)
                    .arg("-e")
                    .arg(&exe)
                    .args(&args)
                    .current_dir(root)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn(),
            };
            if launched.is_ok() {
                return Ok(DetachedSubagentLaunch {
                    label: terminal.to_string(),
                    report_path: report_path.to_path_buf(),
                    stdout_path,
                    command: task,
                    monitor_command,
                });
            }
        }
    }

    label = "background process".to_string();
    let stdout = std::fs::File::create(&stdout_path)?;
    let stderr = stdout.try_clone()?;
    Command::new(exe)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("launch background subagent process")?;
    Ok(DetachedSubagentLaunch {
        label,
        report_path: report_path.to_path_buf(),
        stdout_path,
        command: task,
        monitor_command,
    })
}

#[derive(Clone)]
struct SubagentToolTrace {
    call_id: String,
    name: String,
    summary: String,
    content: String,
    ok: bool,
}

struct SubagentRunReport {
    task: String,
    max_iterations: Option<u32>,
    iterations: u32,
    calls: usize,
    failed_calls: usize,
    elapsed: std::time::Duration,
    halted_reason: Option<String>,
    traces: Vec<SubagentToolTrace>,
    final_text: String,
}

fn compact_call_id(call_id: &str) -> String {
    let tail = call_id.rsplit('.').next().unwrap_or(call_id);
    let compact = if tail.len() > 12 { &tail[..12] } else { tail };
    format!("#{compact}")
}

fn tool_target_key_for_summary(name: &str, summary: &str) -> String {
    let prefix = format!("{name}: ");
    let body = summary.strip_prefix(&prefix).unwrap_or(summary);
    let head = match body.split_once(" (") {
        Some((h, _)) => h,
        None => body,
    };
    let trimmed = head.trim();
    if trimmed.is_empty() {
        "?".to_string()
    } else {
        trimmed.to_string()
    }
}

fn subagent_quality_gate_missing(final_text: &str) -> Vec<&'static str> {
    fn heading_present(lower: &str, aliases: &[&str]) -> bool {
        lower.lines().any(|line| {
            let trimmed = line.trim_start_matches(['#', '-', '*', ' ', '\t']);
            let heading = trimmed.split(':').next().unwrap_or(trimmed).trim();
            aliases.contains(&heading)
        })
    }

    const REQUIRED: &[(&str, &[&str])] = &[
        ("source inspected", &["source inspected"]),
        ("verification run", &["verification run"]),
        ("files touched", &["files touched"]),
        (
            "uncertainty/open questions",
            &[
                "uncertainty/open questions",
                "uncertainty",
                "open questions",
            ],
        ),
        ("confidence", &["confidence"]),
        (
            "exact recommended edits",
            &["exact recommended edits", "recommended edits"],
        ),
        ("remaining risks", &["remaining risks"]),
    ];
    let lower = final_text.to_ascii_lowercase();
    REQUIRED
        .iter()
        .filter_map(|(label, aliases)| (!heading_present(&lower, aliases)).then_some(*label))
        .collect()
}

fn subagent_iteration_limit_label(max_iterations: Option<u32>) -> String {
    max_iterations
        .map(|max| max.to_string())
        .unwrap_or_else(|| "unlimited".to_string())
}

fn render_subagent_report(report: &SubagentRunReport) -> String {
    let mut summary = format!(
        "=== SUBAGENT RESULT ===\n\
         task: {}\n\
         iterations: {}/{}, tool calls: {} ({} failed)\n\
         elapsed: {:.1}s\n\
         {}\n\
         === OUTPUT ===\n{}\n=== END ===",
        report.task,
        report.iterations,
        subagent_iteration_limit_label(report.max_iterations),
        report.calls,
        report.failed_calls,
        report.elapsed.as_secs_f64(),
        report
            .halted_reason
            .as_ref()
            .map(|r| format!("HALTED: {r}\nParent should treat this as incomplete and verify/redo with higher --max-iter or narrower scope."))
            .unwrap_or_default(),
        report.final_text,
    );

    let trace_summary = render_subagent_ui_content(report);
    if !trace_summary.is_empty() {
        summary.push_str(&format!("\n--- tool trace ---\n{trace_summary}"));
    }
    let missing_quality_fields = subagent_quality_gate_missing(&report.final_text);
    if !missing_quality_fields.is_empty() {
        summary.push_str(&format!(
            "\n--- quality gate ---\nmissing required handoff field(s): {}\nParent must verify before using this output.\n",
            missing_quality_fields.join(", ")
        ));
    }
    summary
}

async fn run_subagent_worker(request_path: &Path, report_path: &Path) -> Result<()> {
    let request: SubagentRequest = serde_json::from_slice(
        &std::fs::read(request_path)
            .with_context(|| format!("reading subagent request {}", request_path.display()))?,
    )?;
    let root = std::env::current_dir()?;
    let mut agent = Agent::new_with_sandbox(Some(root), false)?;
    agent.silent = false;
    agent.pretty = io::stdout().is_terminal();
    agent.sink = Box::new(ConsoleSink::new(agent.pretty, false));
    agent.set_approval_profile(request.approval_profile);
    agent.set_sandbox_profile(request.sandbox_profile);
    agent.set_browser_recipe(request.browser_recipe);
    agent.set_thinking_effort(request.thinking_effort);
    agent.context_mode = request.context_mode;
    agent.tool_profile = request.tool_profile;
    agent.set_budget_cap(request.budget_cap);
    if !request.privacy_enabled {
        agent.privacy.enabled = false;
    }
    agent.refresh_tools_for_context();
    let report = agent
        .run_subagent(&request.to_tool_input())
        .await
        .map_err(anyhow::Error::msg)?;
    let rendered = render_subagent_report(&report);
    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_bytes(report_path, rendered.as_bytes())?;
    println!("\n[subagent report written: {}]", report_path.display());
    Ok(())
}

fn render_subagent_ui_content(report: &SubagentRunReport) -> String {
    let mut out = String::new();
    out.push_str("subagent grouped activity\n");

    if report.traces.is_empty() {
        out.push_str("(no tool calls)\n");
    } else {
        let mut i = 0usize;
        while i < report.traces.len() {
            let first = &report.traces[i];
            let first_target = tool_target_key_for_summary(&first.name, &first.summary);
            let mut j = i + 1;
            let mut total_lines = first.content.lines().count();
            while j < report.traces.len() {
                let next = &report.traces[j];
                if next.name != first.name
                    || next.ok != first.ok
                    || tool_target_key_for_summary(&next.name, &next.summary) != first_target
                {
                    break;
                }
                total_lines += next.content.lines().count();
                j += 1;
            }
            let count = j - i;
            if count > 1 {
                if first.ok {
                    out.push_str(&format!(
                        "- {}: {} · {} chunks · {} lines\n",
                        first.name, first_target, count, total_lines
                    ));
                } else {
                    out.push_str(&format!(
                        "- {}: {} · {} failed attempts\n",
                        first.name, first_target, count
                    ));
                }
            } else {
                out.push_str(&format!("- {}: {}\n", first.name, first_target));
            }
            i = j;
        }
    }

    out.push_str("\noutput preview\n");
    let mut preview_lines = 0usize;
    for trace in &report.traces {
        if trace.content.trim().is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{} {}\n",
            compact_call_id(&trace.call_id),
            trace.summary
        ));
        let mut shown_for_trace = 0usize;
        for line in trace.content.lines() {
            out.push_str(line);
            out.push('\n');
            preview_lines += 1;
            shown_for_trace += 1;
            if preview_lines >= 14 || shown_for_trace >= 6 {
                break;
            }
        }
        if trace.content.lines().count() > shown_for_trace {
            out.push_str("…\n");
        }
        if preview_lines >= 14 {
            break;
        }
    }

    if let Some(halt) = &report.halted_reason {
        out.push_str(&format!("\n⚠ {halt}\n"));
    }

    out.push_str("\nfinal subagent response\n");
    out.push_str(report.final_text.trim());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(
            default = "empty_tool_result_metadata",
            skip_serializing_if = "ToolResultMetadata::is_empty"
        )]
        metadata: ToolResultMetadata,
    },
    PartialStream {
        text: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
struct ToolResultMetadata {
    status: Option<String>,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    artifact: Option<String>,
}

impl ToolResultMetadata {
    fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.exit_code.is_none()
            && self.duration_ms.is_none()
            && self.artifact.is_none()
    }
}

fn empty_tool_result_metadata() -> ToolResultMetadata {
    ToolResultMetadata::default()
}

fn strip_tool_result_metadata(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|message| Message {
            role: message.role.clone(),
            content: message
                .content
                .iter()
                .map(|block| match block {
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => Block::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                        metadata: ToolResultMetadata::default(),
                    },
                    other => other.clone(),
                })
                .collect(),
        })
        .collect()
}

fn message_approx_tokens(message: &Message) -> u64 {
    let chars = message
        .content
        .iter()
        .map(|block| match block {
            Block::Text { text } | Block::PartialStream { text } => text.len(),
            Block::Thinking { text } => text.len(),
            Block::ToolUse { input, .. } => json_byte_len(input),
            Block::ToolResult { content, .. } => content.len(),
        })
        .sum::<usize>() as u64;
    ((chars.saturating_add(3)) / 4).max(1)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
struct TrackOrigin {
    source_session: String,
    source_waypoint: String,
    mode: String,
    packet_hash: String,
    created_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkMapKind {
    Intent,
    Evidence,
    Change,
    Failure,
    Verify,
    Decision,
    Compact,
    Result,
}

impl WorkMapKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Evidence => "evidence",
            Self::Change => "change",
            Self::Failure => "failure",
            Self::Verify => "verify",
            Self::Decision => "decision",
            Self::Compact => "compact",
            Self::Result => "result",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkMapWaypoint {
    id: String,
    kind: WorkMapKind,
    message_start: usize,
    message_end: usize,
    summary: String,
    files: Vec<String>,
    commands: Vec<String>,
    status: Option<String>,
}

impl WorkMapWaypoint {
    fn display_range(&self) -> String {
        match (self.message_start, self.message_end) {
            (0, 0) => "ledger".to_string(),
            (start, end) if start == end => format!("#{start}"),
            (start, end) => format!("#{start}..#{end}"),
        }
    }
}

#[derive(Clone)]
struct WorkMap {
    source: String,
    header: SessionHeader,
    messages: usize,
    waypoints: Vec<WorkMapWaypoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkMapSelection {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FocusMode {
    Carry(Vec<String>),
    Exact,
}

impl FocusMode {
    fn label(&self) -> &'static str {
        match self {
            Self::Carry(_) => "carry",
            Self::Exact => "exact",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
struct PrivacyPolicy {
    enabled: bool,
    strict_paths: bool,
    findings: PrivacyFindingCounts,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
struct PrivacyFindingCounts {
    ssn: u64,
    credit_card: u64,
    api_key: u64,
    private_key: u64,
    account_number: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PrivacyRedaction {
    text: String,
    counts: PrivacyFindingCounts,
}

impl Default for PrivacyPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            strict_paths: true,
            findings: PrivacyFindingCounts::default(),
        }
    }
}

impl PrivacyPolicy {
    fn from_env() -> Self {
        let mut policy = Self::default();
        if let Ok(v) = std::env::var("WOLF_PRIVACY") {
            policy.enabled = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on" | "redact" | "strict"
            );
            if v.trim().eq_ignore_ascii_case("strict") {
                policy.strict_paths = true;
            }
        }
        policy
    }

    fn mode_label(&self) -> &'static str {
        if self.enabled { "redact" } else { "off" }
    }

    fn prompt_status_line(&self) -> String {
        if self.enabled {
            "privacy=redact (tool outputs/session logs locally redact SSN, card, API key, private key, account-like numbers before model context)".to_string()
        } else {
            "privacy=off".to_string()
        }
    }

    fn status_text(&self) -> String {
        let mut out = format!(
            "privacy: {}\nstrict path guard: {}\nredacts: ssn, credit-card, api-key/token, private-key, account-like long numbers",
            self.mode_label(),
            if self.strict_paths { "on" } else { "off" }
        );
        if self.findings.total() > 0 {
            out.push_str(&format!(
                "\nredacted this session: {}",
                self.findings.summary()
            ));
        }
        out
    }

    fn redact_text(&self, text: &str) -> PrivacyRedaction {
        if !self.enabled || text.is_empty() {
            return PrivacyRedaction {
                text: text.to_string(),
                counts: PrivacyFindingCounts::default(),
            };
        }
        redact_sensitive_text(text)
    }

    fn redact_log_detail(&self, text: &str) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        redact_sensitive_text(text).text
    }

    fn apply_tool_output(
        &mut self,
        tool_name: &str,
        _input: &Value,
        content: String,
    ) -> PrivacyRedaction {
        let mut redacted = self.redact_text(&content);
        if self.enabled && redacted.counts.total() > 0 {
            let summary = redacted.counts.summary();
            self.findings.add(&redacted.counts);
            redacted.text.push_str(&format!(
                "\n\n[privacy] Redacted {summary} from {tool_name} output before model context/session logging. Raw values withheld."
            ));
        }
        redacted
    }

    fn path_denial(&mut self, tool_name: &str, input: &Value) -> Option<String> {
        if !(self.enabled && self.strict_paths && matches!(tool_name, "read_file" | "read_symbol"))
        {
            return None;
        }
        let path = input["path"].as_str()?;
        if !privacy_sensitive_path(path) {
            return None;
        }
        self.findings.private_key = self.findings.private_key.saturating_add(1);
        Some(format!(
            "[privacy] blocked {tool_name} for sensitive-looking path `{path}`. Raw file content withheld. Ask the user to disable `/privacy` or provide a sanitized excerpt if this read is necessary."
        ))
    }
}

impl PrivacyFindingCounts {
    fn add(&mut self, other: &Self) {
        self.ssn = self.ssn.saturating_add(other.ssn);
        self.credit_card = self.credit_card.saturating_add(other.credit_card);
        self.api_key = self.api_key.saturating_add(other.api_key);
        self.private_key = self.private_key.saturating_add(other.private_key);
        self.account_number = self.account_number.saturating_add(other.account_number);
    }

    fn total(&self) -> u64 {
        self.ssn
            .saturating_add(self.credit_card)
            .saturating_add(self.api_key)
            .saturating_add(self.private_key)
            .saturating_add(self.account_number)
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.ssn > 0 {
            parts.push(format!("{} SSN", self.ssn));
        }
        if self.credit_card > 0 {
            parts.push(format!("{} card", self.credit_card));
        }
        if self.api_key > 0 {
            parts.push(format!("{} API/token", self.api_key));
        }
        if self.private_key > 0 {
            parts.push(format!("{} private-key/path", self.private_key));
        }
        if self.account_number > 0 {
            parts.push(format!("{} account-like", self.account_number));
        }
        if parts.is_empty() {
            "0 items".to_string()
        } else {
            parts.join(", ")
        }
    }
}

fn redact_sensitive_text(text: &str) -> PrivacyRedaction {
    let mut counts = PrivacyFindingCounts::default();
    let mut out = redact_private_key_blocks(text, &mut counts);
    out = redact_secret_assignments(&out, &mut counts);
    out = redact_ssns(&out, &mut counts);
    out = redact_digit_sequences(&out, &mut counts);
    PrivacyRedaction { text: out, counts }
}

fn redact_private_key_blocks(text: &str, counts: &mut PrivacyFindingCounts) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_key = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_key && trimmed.starts_with("-----BEGIN ") && trimmed.contains("PRIVATE KEY-----") {
            counts.private_key = counts.private_key.saturating_add(1);
            out.push_str("[REDACTED_PRIVATE_KEY]\n");
            in_key = true;
            continue;
        }
        if in_key {
            if trimmed.starts_with("-----END ") && trimmed.contains("PRIVATE KEY-----") {
                in_key = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn redact_secret_assignments(text: &str, counts: &mut PrivacyFindingCounts) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let secretish = [
            "api_key",
            "apikey",
            "access_token",
            "auth_token",
            "bearer ",
            "client_secret",
            "secret_key",
            "private_key",
            "password",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        if secretish {
            if let Some(pos) = line.find('=') {
                counts.api_key = counts.api_key.saturating_add(1);
                out.push_str(line[..=pos].trim_end());
                out.push_str(" [REDACTED_SECRET]\n");
                continue;
            }
            if let Some(pos) = line.find(':') {
                counts.api_key = counts.api_key.saturating_add(1);
                out.push_str(line[..=pos].trim_end());
                out.push_str(" [REDACTED_SECRET]\n");
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn redact_ssns(text: &str, counts: &mut PrivacyFindingCounts) -> String {
    redact_by_byte_spans(text, find_ssn_spans(text, counts), "[REDACTED_SSN]")
}

fn redact_digit_sequences(text: &str, counts: &mut PrivacyFindingCounts) -> String {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut digits = String::new();
    let mut digit_count = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch.is_ascii_digit() {
            if start.is_none() {
                start = Some(idx);
                digits.clear();
                digit_count = 0;
            }
            digits.push(ch);
            digit_count += 1;
        } else if start.is_some() && matches!(ch, ' ' | '-' | '_' | '.') {
            digits.push(ch);
        } else if let Some(s) = start.take() {
            classify_digit_span(text, s, idx, &digits, digit_count, &mut spans, counts);
            digits.clear();
            digit_count = 0;
        }
    }
    if let Some(s) = start {
        classify_digit_span(
            text,
            s,
            text.len(),
            &digits,
            digit_count,
            &mut spans,
            counts,
        );
    }
    redact_by_labeled_spans(text, spans)
}

fn classify_digit_span(
    text: &str,
    start: usize,
    end: usize,
    raw_digits: &str,
    digit_count: usize,
    spans: &mut Vec<(usize, usize, &'static str)>,
    counts: &mut PrivacyFindingCounts,
) {
    if digit_count < 9 {
        return;
    }
    let digits: String = raw_digits.chars().filter(|c| c.is_ascii_digit()).collect();
    if digit_count == 9 && looks_like_ssn_context(text, start) {
        counts.ssn = counts.ssn.saturating_add(1);
        spans.push((start, end, "[REDACTED_SSN]"));
    } else if (13..=19).contains(&digit_count) && luhn_valid(&digits) {
        counts.credit_card = counts.credit_card.saturating_add(1);
        spans.push((start, end, "[REDACTED_CARD]"));
    } else if (10..=17).contains(&digit_count) && looks_like_account_context(text, start) {
        counts.account_number = counts.account_number.saturating_add(1);
        spans.push((start, end, "[REDACTED_ACCOUNT]"));
    }
}

fn find_ssn_spans(text: &str, counts: &mut PrivacyFindingCounts) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i + 11 <= bytes.len() {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3] == b'-'
            && bytes[i + 4].is_ascii_digit()
            && bytes[i + 5].is_ascii_digit()
            && bytes[i + 6] == b'-'
            && bytes[i + 7].is_ascii_digit()
            && bytes[i + 8].is_ascii_digit()
            && bytes[i + 9].is_ascii_digit()
            && bytes[i + 10].is_ascii_digit()
            && byte_boundary_ok(bytes, i, i + 11)
        {
            counts.ssn = counts.ssn.saturating_add(1);
            spans.push((i, i + 11));
            i += 11;
        } else {
            i += 1;
        }
    }
    spans
}

fn byte_boundary_ok(bytes: &[u8], start: usize, end: usize) -> bool {
    let before = start.checked_sub(1).and_then(|i| bytes.get(i)).copied();
    let after = bytes.get(end).copied();
    !before.is_some_and(|b| b.is_ascii_alphanumeric())
        && !after.is_some_and(|b| b.is_ascii_alphanumeric())
}

fn redact_by_byte_spans(text: &str, spans: Vec<(usize, usize)>, replacement: &str) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for (start, end) in spans {
        if start < last {
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(replacement);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

fn redact_by_labeled_spans(text: &str, mut spans: Vec<(usize, usize, &'static str)>) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    spans.sort_by_key(|(s, _, _)| *s);
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for (start, end, replacement) in spans {
        if start < last {
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(replacement);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

fn looks_like_ssn_context(text: &str, start: usize) -> bool {
    let prefix = byte_suffix_at_char_boundary(&text[..start], 32).to_ascii_lowercase();
    prefix.contains("ssn") || prefix.contains("social security")
}

fn looks_like_account_context(text: &str, start: usize) -> bool {
    let context = byte_suffix_at_char_boundary(&text[..start], 40).to_ascii_lowercase();
    [
        "account",
        "acct",
        "routing",
        "iban",
        "member id",
        "customer id",
    ]
    .iter()
    .any(|needle| context.contains(needle))
}

fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for ch in digits.chars().rev() {
        let Some(mut n) = ch.to_digit(10) else {
            return false;
        };
        if double {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        double = !double;
    }
    sum > 0 && sum % 10 == 0
}

fn privacy_sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.contains("secret")
        || lower.contains("credential")
        || lower.contains("private")
        || lower.contains("id_rsa")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
    {
        return true;
    }
    Path::new(path)
        .components()
        .any(|component| match component {
            Component::Normal(name) => {
                let n = name.to_string_lossy().to_ascii_lowercase();
                matches!(n.as_str(), ".env" | ".netrc" | "credentials" | "secrets")
            }
            _ => false,
        })
}

#[derive(Serialize, Deserialize, Clone)]
struct Message {
    role: String,
    content: Vec<Block>,
}

#[derive(Serialize, Clone, Copy)]
pub(crate) struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

impl CacheControl {
    pub(crate) const EPHEMERAL: Self = Self { kind: "ephemeral" };
}

#[derive(Serialize)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
pub(crate) struct WireTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

fn openai_reasoning_effort(effort: ThinkingEffort) -> Option<&'static str> {
    Some(match effort {
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::High | ThinkingEffort::XHigh => "high",
    })
}

fn anthropic_thinking_budget_tokens(effort: ThinkingEffort) -> Option<u32> {
    Some(match effort {
        ThinkingEffort::Low => 1_024,
        ThinkingEffort::Medium => 2_048,
        ThinkingEffort::High => 4_096,
        ThinkingEffort::XHigh => 8_192,
    })
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a [SystemBlock<'a>],
    messages: &'a [Message],
    tools: &'a [WireTool],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
}

#[derive(Serialize, Clone, Copy)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

#[derive(Serialize)]
struct OaiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<OaiMessage>,
    tools: Vec<OaiTool>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
struct OaiMessage {
    role: String,
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct OaiToolCall {
    id: String,
    r#type: String,
    function: OaiFunction,
}

#[derive(Serialize)]
struct OaiFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
pub(crate) struct OaiTool {
    r#type: String,
    function: OaiFunctionDef,
}

#[derive(Serialize)]
pub(crate) struct OaiFunctionDef {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize)]
struct Usage {
    // Billable/non-cached input tokens. Providers like ChatGPT report total
    // input plus cached_tokens; normalize by subtracting cached_tokens here.
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
}

impl Usage {
    fn add(&mut self, o: Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_create += o.cache_create;
        self.cache_read += o.cache_read;
    }

    fn actual_input_tokens(&self) -> u64 {
        self.input
    }

    fn cached_input_tokens(&self) -> u64 {
        self.cache_create.saturating_add(self.cache_read)
    }

    fn billed_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_create)
            .saturating_add(self.cache_read)
    }

    fn context_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    fn total_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    fn estimated_cost_usd(&self) -> f64 {
        let per_mtok = 1_000_000.0;
        (self.input as f64 / per_mtok) * DEFAULT_INPUT_USD_PER_MTOK
            + (self.output as f64 / per_mtok) * DEFAULT_OUTPUT_USD_PER_MTOK
            + (self.cache_read as f64 / per_mtok) * DEFAULT_CACHE_READ_USD_PER_MTOK
            + (self.cache_create as f64 / per_mtok) * DEFAULT_CACHE_CREATE_USD_PER_MTOK
    }

    fn parse(v: &Value) -> Self {
        let cache_create = v["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cache_read = v["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let input = v["input_tokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(cache_create)
            .saturating_sub(cache_read);
        Self {
            input,
            output: v["output_tokens"].as_u64().unwrap_or(0),
            cache_create,
            cache_read,
        }
    }

    fn parse_openai(v: &Value) -> Self {
        let total_input = v["prompt_tokens"]
            .as_u64()
            .or_else(|| v["input_tokens"].as_u64())
            .unwrap_or(0);
        let output = v["completion_tokens"]
            .as_u64()
            .or_else(|| v["output_tokens"].as_u64())
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
            .unwrap_or(0);
        Self {
            input: total_input.saturating_sub(cache_read),
            output,
            cache_create: 0,
            cache_read,
        }
    }

    fn line(&self) -> String {
        format!(
            "in={} out={} cache_r={} cache_w={} cached={} total={} est=${:.4}",
            self.actual_input_tokens(),
            self.output,
            self.cache_read,
            self.cache_create,
            self.cached_input_tokens(),
            self.total_tokens(),
            self.estimated_cost_usd()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct BudgetCap {
    usd: Option<f64>,
    tokens: Option<u64>,
}

impl BudgetCap {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.eq_ignore_ascii_case("off")
            || raw.eq_ignore_ascii_case("none")
            || raw.eq_ignore_ascii_case("disabled")
            || raw == "0"
        {
            return None;
        }
        let parts: Vec<&str> = raw
            .split([',', '+'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() > 1 {
            let mut cap = Self {
                usd: None,
                tokens: None,
            };
            for part in parts {
                let parsed = Self::parse_one(part)?;
                cap.usd = cap.usd.or(parsed.usd);
                cap.tokens = cap.tokens.or(parsed.tokens);
            }
            return (cap.usd.is_some() || cap.tokens.is_some()).then_some(cap);
        }
        Self::parse_one(raw)
    }

    fn parse_one(raw: &str) -> Option<Self> {
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

    fn from_env() -> Option<Self> {
        std::env::var("WOLF_BUDGET_CAP")
            .ok()
            .and_then(|v| Self::parse(&v))
    }

    fn exceeded(&self, usage: Usage) -> Option<String> {
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

    fn line(self) -> String {
        match (self.usd, self.tokens) {
            (Some(usd), Some(tokens)) => format!("${usd:.4} or {tokens} tokens"),
            (Some(usd), None) => format!("${usd:.4}"),
            (None, Some(tokens)) => format!("{tokens} tokens"),
            (None, None) => "off".to_string(),
        }
    }
}

fn parse_positive_f64(raw: &str) -> Option<f64> {
    let value = raw.trim().parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

fn parse_token_count(raw: &str) -> Option<u64> {
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
    (tokens.is_finite() && tokens >= 1.0 && tokens <= u64::MAX as f64).then_some(tokens as u64)
}

#[derive(Default)]
struct PartialBlock {
    kind: String,
    text: String,
    id: String,
    name: String,
    input_json: String,
    thinking_signature: Option<String>,
}

impl PartialBlock {
    fn finalize(self) -> Option<Block> {
        match self.kind.as_str() {
            "text" => Some(Block::Text { text: self.text }),
            "thinking" => {
                let text = if self.text.is_empty() {
                    self.thinking_signature
                        .map(|sig| format!("[encrypted reasoning signature: {sig}]"))
                        .unwrap_or_default()
                } else {
                    self.text
                };
                (!text.is_empty()).then_some(Block::Thinking { text })
            }
            "tool_use" => Some(Block::ToolUse {
                id: self.id,
                name: self.name,
                input: parse_tool_input_json(&self.input_json),
            }),
            _ => None,
        }
    }
}

fn parse_tool_input_json(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    let mut input: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    if let Value::String(s) = &input
        && let Ok(decoded) = serde_json::from_str::<Value>(s)
    {
        input = decoded;
    }
    if input.is_null() {
        Value::Object(Default::default())
    } else {
        input
    }
}

fn is_meaningful_tool_input(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::String(s) => !s.trim().is_empty(),
        _ => true,
    }
}

fn set_tool_input_json_if_meaningful(dst: &mut String, input: &Value) {
    if !is_meaningful_tool_input(input) {
        return;
    }
    *dst = match input {
        Value::String(s) => s.clone(),
        _ => input.to_string(),
    };
}

fn append_tool_input_json_fragment(dst: &mut String, fragment: &str) {
    let trimmed = dst.trim();
    if trimmed == "{}" || trimmed == "[]" {
        dst.clear();
    }
    dst.push_str(fragment);
}

// Finds the earliest SSE event delimiter in `buf` starting the scan at
// `from.saturating_sub(3)` so a CRLF boundary straddling prior scan state is
// still discovered. Returns (delimiter_start_index, delimiter_len) where
// delimiter_len is 2 for LF/LF and 4 for CRLF/CRLF.
fn find_sse_delimiter(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let start = from.saturating_sub(3);
    let slice = &buf[start..];
    let mut i = 0;
    while i + 1 < slice.len() {
        if slice[i] == b'\n' && slice[i + 1] == b'\n' {
            return Some((start + i, 2));
        }
        if i + 3 < slice.len()
            && slice[i] == b'\r'
            && slice[i + 1] == b'\n'
            && slice[i + 2] == b'\r'
            && slice[i + 3] == b'\n'
        {
            return Some((start + i, 4));
        }
        i += 1;
    }
    None
}

// Convenience wrapper used by tests: returns (event_text, bytes_consumed)
// where bytes_consumed includes the delimiter. Picks the earliest delimiter
// rather than preferring LF over CRLF by code-order.
#[cfg(test)]
fn next_sse_event(buf: &[u8]) -> Option<(String, usize)> {
    let (end, sep_len) = find_sse_delimiter(buf, 0)?;
    let text = String::from_utf8_lossy(&buf[..end]).into_owned();
    Some((text, end + sep_len))
}

const EXTERNAL_TOOL_TIMEOUT_SECS: u64 = 60;

fn output_suspicious_stderr_note(status: i32, stderr: &str) -> Option<String> {
    if status != 0 || stderr.trim().is_empty() {
        return None;
    }
    let lower = stderr.to_ascii_lowercase();
    let suspicious = [
        "command not found",
        "no such file or directory",
        "permission denied",
        "traceback",
        "error:",
        "failed",
        "panic",
    ];
    if suspicious.iter().any(|needle| lower.contains(needle)) {
        Some(
            "[wolf note] command exited 0 but stderr contains failure-looking text; verify before trusting this result.\n".to_string(),
        )
    } else {
        None
    }
}

fn merge_process_output_with_status(stdout: String, stderr: String, status: i32) -> String {
    let mut out = stdout;
    if let Some(note) = output_suspicious_stderr_note(status, &stderr) {
        out.push_str(&note);
    }
    if !stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(&stderr);
    }
    out
}

fn format_process_output(
    stdout: String,
    stderr: String,
    status: i32,
) -> std::result::Result<String, String> {
    // jq/rg/fd return non-zero on "no matches" — still informative, don't treat as hard error
    if status != 0 && stdout.is_empty() && !stderr.is_empty() {
        Err(format!("exit {status}: {stderr}"))
    } else {
        Ok(merge_process_output_with_status(stdout, stderr, status))
    }
}

fn collect_sync_limited<R: Read>(
    mut reader: R,
    cap: usize,
) -> std::result::Result<LimitedByteCapture, String> {
    let mut capture = LimitedByteCapture::new(cap);
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => capture.push(&buf[..n]),
            Err(e) => return Err(format!("{e}")),
        }
    }
    Ok(capture)
}

enum ProcWaitOutcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    Timeout,
    Interrupt,
}

async fn collect_async_limited<R>(mut reader: R, cap: usize) -> LimitedByteCapture
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut capture = LimitedByteCapture::new(cap);
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => capture.push(&buf[..n]),
            Err(_) => break,
        }
    }
    capture
}

#[cfg(unix)]
fn configure_std_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn configure_std_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn configure_tokio_process_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn configure_tokio_process_group(_cmd: &mut tokio::process::Command) {}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    let pgid = -(pid as libc::pid_t);
    unsafe {
        let _ = libc::kill(pgid, signal);
    }
}

fn terminate_process_group_after_exit(pid: u32) {
    #[cfg(unix)]
    {
        signal_process_group(pid, libc::SIGTERM);
        signal_process_group(pid, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn terminate_std_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        signal_process_group(pid, libc::SIGTERM);
        std::thread::sleep(std::time::Duration::from_millis(50));
        signal_process_group(pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

async fn terminate_tokio_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        signal_process_group(pid, libc::SIGTERM);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        signal_process_group(pid, libc::SIGKILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn run_sync_command_limited(
    mut cmd: Command,
    stdin_data: Option<&str>,
    capture_cap: usize,
    spawn_label: &str,
    timeout: std::time::Duration,
) -> std::result::Result<(LimitedByteCapture, LimitedByteCapture, i32), String> {
    use std::io::Write as _;
    use std::process::Stdio;

    if stdin_data.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_std_process_group(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {spawn_label}: {e}"))?;
    let child_pid = child.id();

    let stdout = child.stdout.take().ok_or("stdout not piped")?;
    let stderr = child.stderr.take().ok_or("stderr not piped")?;
    let out_handle = std::thread::spawn(move || collect_sync_limited(stdout, capture_cap));
    let err_handle = std::thread::spawn(move || collect_sync_limited(stderr, capture_cap));

    if let Some(data) = stdin_data
        && let Some(mut si) = child.stdin.take()
    {
        si.write_all(data.as_bytes()).map_err(|e| format!("{e}"))?;
    }

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                terminate_process_group_after_exit(child_pid);
                break status;
            }
            Ok(None) => {}
            Err(e) => return Err(format!("wait failed: {e}")),
        }
        if started.elapsed() >= timeout {
            terminate_std_child(&mut child);
            let out = out_handle
                .join()
                .map_err(|_| "stdout reader panicked".to_string())??;
            let err = err_handle
                .join()
                .map_err(|_| "stderr reader panicked".to_string())??;
            return Err(format!(
                "timed out after {}s running {spawn_label}\n--- stdout ---\n{}--- stderr ---\n{}",
                timeout.as_secs(),
                out.render("stdout"),
                err.render("stderr"),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    let out = out_handle
        .join()
        .map_err(|_| "stdout reader panicked".to_string())??;
    let err = err_handle
        .join()
        .map_err(|_| "stderr reader panicked".to_string())??;
    Ok((out, err, status.code().unwrap_or(-1)))
}

fn run_external(
    bin: &str,
    args: &[String],
    stdin_data: Option<&str>,
    cwd: &Path,
) -> std::result::Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.current_dir(cwd);
    let (out, err, status) = run_sync_command_limited(
        cmd,
        stdin_data,
        PROCESS_STREAM_CAPTURE_CAP,
        bin,
        external_tool_timeout(),
    )?;
    format_process_output(out.render("stdout"), err.render("stderr"), status)
}

fn timeout_from_env(var: &str, fallback_secs: u64) -> std::time::Duration {
    let secs = std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .or_else(|| {
            std::env::var("WOLF_EXTERNAL_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|secs| *secs > 0)
        })
        .unwrap_or(fallback_secs);
    std::time::Duration::from_secs(secs)
}

fn external_tool_timeout() -> std::time::Duration {
    timeout_from_env("WOLF_EXTERNAL_TIMEOUT_SECS", EXTERNAL_TOOL_TIMEOUT_SECS)
}

fn bash_tool_timeout() -> std::time::Duration {
    timeout_from_env("WOLF_BASH_TIMEOUT_SECS", EXTERNAL_TOOL_TIMEOUT_SECS)
}

fn timeout_from_tool_input(input: &Value, fallback: std::time::Duration) -> std::time::Duration {
    input["timeout"]
        .as_u64()
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(fallback)
}

fn hook_timeout() -> std::time::Duration {
    timeout_from_env("WOLF_HOOK_TIMEOUT_SECS", EXTERNAL_TOOL_TIMEOUT_SECS)
}

const CHECKPOINT_DEBOUNCE_MS_DEFAULT: u64 = 500;
const MAX_CONCURRENT_BUILTINS_DEFAULT: usize = 8;

fn max_concurrent_builtins() -> usize {
    std::env::var("WOLF_MAX_CONCURRENT_BUILTINS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_CONCURRENT_BUILTINS_DEFAULT)
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "wolf-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

fn git_unified_diff(before: &str, after: &str, path: &Path, root: &Path) -> Option<String> {
    if before == after {
        return None;
    }

    let relative = path.strip_prefix(root).ok()?;
    let temp_dir = unique_temp_dir("edit-diff");
    let before_path = temp_dir.join("before").join(relative);
    let after_path = temp_dir.join("after").join(relative);

    let write_result = (|| -> std::result::Result<(), String> {
        if let Some(parent) = before_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
        }
        if let Some(parent) = after_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
        }
        std::fs::write(&before_path, before).map_err(|e| format!("{e}"))?;
        std::fs::write(&after_path, after).map_err(|e| format!("{e}"))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return None;
    }

    let args = vec![
        "diff".to_string(),
        "--no-index".to_string(),
        "--no-color".to_string(),
        "--text".to_string(),
        "--unified=3".to_string(),
        before_path.to_string_lossy().into_owned(),
        after_path.to_string_lossy().into_owned(),
    ];
    let out = run_external("git", &args, None, root).ok();
    let _ = std::fs::remove_dir_all(&temp_dir);
    let out = out?;

    let mut body = String::new();
    for line in out.lines() {
        if line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            continue;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    if !body.is_empty() && out.ends_with('\n') {
        body.push('\n');
    }
    Some(body)
}

fn edit_result_with_diff(
    count: usize,
    path: &Path,
    root: &Path,
    before: &str,
    after: &str,
) -> String {
    let summary = if count <= 1 {
        format!("edited {}", path.display())
    } else {
        format!("applied {count} edits to {}", path.display())
    };
    match git_unified_diff(before, after, path, root) {
        Some(diff) if !diff.is_empty() => format!("{diff}{summary}"),
        _ => summary,
    }
}

// "Full jitter" backoff: returns a wait in [base/2, base). Prevents thundering-herd when multiple
// wolf processes see the same 429/503 at the same second and would otherwise retry in lockstep.
// Doesn't touch retry-after header waits — those come from the server and are respected exactly.
fn jittered_backoff_secs(base: u64) -> u64 {
    let base = base.max(1);
    let half = (base / 2).max(1);
    let span = (base - half).max(1);
    let noise = jitter_next() % span;
    half + noise
}

fn jitter_next() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    (nanos ^ c)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}

fn checkpoint_debounce() -> std::time::Duration {
    let ms = std::env::var("WOLF_CHECKPOINT_DEBOUNCE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(CHECKPOINT_DEBOUNCE_MS_DEFAULT);
    std::time::Duration::from_millis(ms)
}

fn binary_on_path(name: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    for dir in path_var.split(if cfg!(windows) { ';' } else { ':' }) {
        let p = Path::new(dir).join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if p.is_file()
                && p.metadata()
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            {
                return true;
            }
        }
        #[cfg(windows)]
        {
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

fn fd_exclude_path_patterns(glob: &str) -> Vec<String> {
    let trimmed = glob.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Some(dir) = trimmed
        .strip_prefix("**/")
        .and_then(|rest| rest.strip_suffix("/**"))
        .or_else(|| trimmed.strip_suffix("/**"))
        .filter(|dir| !dir.is_empty())
    {
        return vec![format!("*/{dir}/*"), format!("*/{dir}")];
    }
    if trimmed.contains('/') {
        vec![format!("*/{trimmed}"), format!("*/{trimmed}/*")]
    } else {
        vec![format!("*/{trimmed}/*"), format!("*/{trimmed}")]
    }
}

fn build_fd_find_fallback_args(search_root: &Path, pattern: &str, extra: &[String]) -> Vec<String> {
    let mut find_type = "f".to_string();
    let mut name_globs: Vec<String> = Vec::new();
    let mut exclude_globs: Vec<String> = Vec::new();
    let mut idx = 0usize;
    while idx < extra.len() {
        match extra[idx].as_str() {
            "-H" | "--hidden" | "-a" | "--absolute-path" | "-c" | "--color" | "--follow" | "-L"
            | "--no-ignore" | "--no-ignore-vcs" | "--strip-cwd-prefix" => {
                idx += 1;
            }
            "--extension" | "-e" => {
                if let Some(ext) = extra.get(idx + 1) {
                    let ext = ext.trim().trim_start_matches('.');
                    if !ext.is_empty() {
                        name_globs.push(format!("*.{ext}"));
                    }
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "--exclude" => {
                if let Some(glob) = extra.get(idx + 1) {
                    exclude_globs.extend(fd_exclude_path_patterns(glob));
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "--glob" | "-g" => {
                if let Some(glob) = extra.get(idx + 1) {
                    let glob = glob.trim();
                    if let Some(exclude) = glob.strip_prefix('!') {
                        exclude_globs.extend(fd_exclude_path_patterns(exclude));
                    } else if !glob.is_empty() {
                        name_globs.push(glob.to_string());
                    }
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "-t" | "--type" => {
                if let Some(kind) = extra.get(idx + 1) {
                    match kind.as_str() {
                        "f" | "file" => find_type = "f".to_string(),
                        "d" | "directory" | "dir" => find_type = "d".to_string(),
                        _ => {}
                    }
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            other => {
                if let Some(glob) = other.strip_prefix("--glob=") {
                    let glob = glob.trim();
                    if let Some(exclude) = glob.strip_prefix('!') {
                        exclude_globs.extend(fd_exclude_path_patterns(exclude));
                    } else if !glob.is_empty() {
                        name_globs.push(glob.to_string());
                    }
                } else if let Some(exclude) = other.strip_prefix("--exclude=") {
                    exclude_globs.extend(fd_exclude_path_patterns(exclude));
                } else if let Some(ext) = other.strip_prefix("--extension=") {
                    let ext = ext.trim().trim_start_matches('.');
                    if !ext.is_empty() {
                        name_globs.push(format!("*.{ext}"));
                    }
                }
                idx += 1;
            }
        }
    }

    let mut args: Vec<String> = vec![
        search_root.to_string_lossy().to_string(),
        "-type".into(),
        find_type,
        "-regextype".into(),
        "posix-extended".into(),
        "-regex".into(),
        format!(".*{}.*", pattern),
    ];
    for exclude in exclude_globs {
        args.push("!".into());
        args.push("-path".into());
        args.push(exclude);
    }
    for glob in name_globs {
        args.push("-name".into());
        args.push(glob);
    }
    args
}

fn rg_negated_glob_is_recursive(glob: &str) -> bool {
    if !glob.starts_with('!') || glob.starts_with("!**/") || glob.ends_with("/**") {
        return false;
    }
    let body = glob.trim_start_matches('!').trim_end_matches('/');
    !body.is_empty()
        && !body.contains('/')
        && !body.contains('*')
        && !body.contains('?')
        && !body.contains('[')
        && !body.contains('{')
}

fn translate_exclude_globs_for_rg(extra: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(extra.len());
    let mut idx = 0usize;
    while idx < extra.len() {
        let arg = &extra[idx];
        if arg == "--glob" || arg == "-g" {
            if let Some(glob) = extra.get(idx + 1) {
                out.push(arg.clone());
                if rg_negated_glob_is_recursive(glob) {
                    out.push(format!("!**/{}/**", glob.trim_start_matches('!')));
                } else {
                    out.push(glob.clone());
                }
                idx += 2;
            } else {
                out.push(arg.clone());
                idx += 1;
            }
        } else if let Some(glob) = arg.strip_prefix("--glob=") {
            if rg_negated_glob_is_recursive(glob) {
                out.push(format!("--glob=!**/{}/**", glob.trim_start_matches('!')));
            } else {
                out.push(arg.clone());
            }
            idx += 1;
        } else {
            out.push(arg.clone());
            idx += 1;
        }
    }
    out
}

fn translate_grep_glob_arg(glob: &str, out: &mut Vec<String>) {
    if let Some(exclude) = glob.strip_prefix('!') {
        let trimmed = exclude.trim().trim_end_matches('/');
        if let Some(dir) = trimmed
            .strip_prefix("**/")
            .and_then(|rest| rest.strip_suffix("/**"))
            .or_else(|| trimmed.strip_suffix("/**"))
            .filter(|dir| !dir.is_empty() && !dir.contains('*'))
        {
            out.push(format!("--exclude-dir={dir}"));
        } else if !trimmed.is_empty()
            && !trimmed.contains('/')
            && !trimmed.contains('*')
            && !trimmed.contains('?')
            && !trimmed.contains('[')
            && !trimmed.contains('{')
        {
            out.push(format!("--exclude-dir={trimmed}"));
        } else if !trimmed.is_empty() {
            out.push(format!("--exclude={trimmed}"));
        }
    } else if !glob.trim().is_empty() {
        out.push(format!("--include={glob}"));
    }
}

fn translate_extra_args_for_grep(extra: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    let mut idx = 0usize;
    while idx < extra.len() {
        match extra[idx].as_str() {
            "-i" | "--ignore-case" => {
                args.push("-i".into());
                idx += 1;
            }
            "--glob" | "-g" => {
                if let Some(glob) = extra.get(idx + 1) {
                    translate_grep_glob_arg(glob, &mut args);
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            other => {
                if let Some(glob) = other.strip_prefix("--glob=") {
                    translate_grep_glob_arg(glob, &mut args);
                }
                idx += 1;
            }
        }
    }
    args
}

fn prepare_external_tool(
    name: &str,
    input: &Value,
    root: &Path,
) -> std::result::Result<(String, Vec<String>, Option<String>), String> {
    match name {
        "fd" => {
            let pattern = input["pattern"].as_str().ok_or("missing pattern")?;
            if pattern.trim().is_empty() {
                return Err(
                    "fd pattern cannot be empty (would match every file and flood output). Provide a regex or glob."
                        .to_string(),
                );
            }
            let user_path = input["path"].as_str().unwrap_or(".");
            let search_root = canonical_within(root, user_path)?;
            let extra = str_array(&input["extra_args"]);
            if binary_on_path("fd") {
                let mut args: Vec<String> = extra;
                args.push(pattern.to_string());
                args.push(search_root.to_string_lossy().to_string());
                Ok(("fd".to_string(), args, None))
            } else {
                let args = build_fd_find_fallback_args(&search_root, pattern, &extra);
                Ok(("find".to_string(), args, None))
            }
        }
        "rg" => {
            let pattern = input["pattern"].as_str().ok_or("missing pattern")?;
            let user_path = input["path"].as_str().unwrap_or(".");
            let search_root = canonical_within(root, user_path)?;
            let extra = str_array(&input["extra_args"]);
            if binary_on_path("rg") {
                let mut args: Vec<String> = vec!["--line-number".into(), "--no-heading".into()];
                args.extend(translate_exclude_globs_for_rg(extra));
                args.push(pattern.to_string());
                args.push(search_root.to_string_lossy().to_string());
                Ok(("rg".to_string(), args, None))
            } else {
                let mut args: Vec<String> = vec!["-rn".into(), "-E".into(), "--color=never".into()];
                args.extend(translate_extra_args_for_grep(&extra));
                args.push(pattern.to_string());
                args.push(search_root.to_string_lossy().to_string());
                Ok(("grep".to_string(), args, None))
            }
        }
        "jq" => {
            let filter = input["filter"].as_str().ok_or("missing filter")?;
            if let Some(user_path) = input["path"].as_str() {
                let path = canonical_within(root, user_path)?;
                Ok((
                    "jq".to_string(),
                    vec![filter.to_string(), path.to_string_lossy().to_string()],
                    None,
                ))
            } else if let Some(json) = input["json"].as_str() {
                Ok((
                    "jq".to_string(),
                    vec![filter.to_string()],
                    Some(json.to_string()),
                ))
            } else {
                Err("provide either 'path' or 'json'".to_string())
            }
        }
        "fzf" => {
            let query = input["query"].as_str().ok_or("missing query")?;
            let items = str_array(&input["items"]);
            if items.is_empty() {
                return Err("items is empty".to_string());
            }
            Ok((
                "fzf".to_string(),
                vec!["--filter".into(), query.to_string()],
                Some(items.join("\n")),
            ))
        }
        "awk" | "csvkit" => {
            let args = str_array(&input["args"]);
            let stdin = input["stdin"].as_str().map(String::from);
            let (bin, final_args): (String, Vec<String>) = match name {
                "csvkit" => {
                    let sub = input["subcommand"]
                        .as_str()
                        .ok_or("missing subcommand")?
                        .to_string();
                    (sub, args)
                }
                other => (other.to_string(), args),
            };
            Ok((bin, final_args, stdin))
        }
        "git_diff" => {
            let mut args = vec!["diff".to_string()];
            if input["stat"].as_bool().unwrap_or(false) {
                args.push("--stat".to_string());
            }
            if input["staged"].as_bool().unwrap_or(false) {
                args.push("--cached".to_string());
            }
            if let Some(commit) = input["commit"].as_str() {
                args.push(commit.to_string());
            }
            if let Some(path) = input["path"].as_str() {
                args.push("--".to_string());
                args.push(path.to_string());
            }
            Ok(("git".to_string(), args, None))
        }
        "git_log" => {
            let count = input["count"].as_u64().unwrap_or(10).min(50);
            let oneline = input["oneline"].as_bool().unwrap_or(true);
            let mut args = vec!["log".to_string()];
            if oneline {
                args.push("--oneline".to_string());
            }
            args.push(format!("-{count}"));
            if let Some(path) = input["path"].as_str() {
                args.push("--".to_string());
                args.push(path.to_string());
            }
            Ok(("git".to_string(), args, None))
        }
        _ => Err(format!("not an external process tool: {name}")),
    }
}

fn prepare_external_tool_fallback(
    name: &str,
    input: &Value,
    root: &Path,
) -> (String, Vec<String>, Option<String>) {
    match name {
        "rg" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            let user_path = input["path"].as_str().unwrap_or(".");
            let search_root =
                canonical_within(root, user_path).unwrap_or_else(|_| root.to_path_buf());
            let extra = str_array(&input["extra_args"]);
            let mut args: Vec<String> = vec!["-rn".into(), "-E".into(), "--color=never".into()];
            args.extend(translate_extra_args_for_grep(&extra));
            args.push(pattern.to_string());
            args.push(search_root.to_string_lossy().to_string());
            ("grep".to_string(), args, None)
        }
        "fd" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            let user_path = input["path"].as_str().unwrap_or(".");
            let search_root =
                canonical_within(root, user_path).unwrap_or_else(|_| root.to_path_buf());
            let extra = str_array(&input["extra_args"]);
            let args = build_fd_find_fallback_args(&search_root, pattern, &extra);
            ("find".to_string(), args, None)
        }
        _ => {
            let (bin, args, stdin) =
                prepare_external_tool(name, input, root).unwrap_or_else(|_| {
                    (
                        "echo".to_string(),
                        vec!["unsupported fallback".to_string()],
                        None,
                    )
                });
            (bin, args, stdin)
        }
    }
}

fn str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug)]
enum HttpToolBody {
    Json(Value),
    Form(Vec<(String, String)>),
    Raw(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpOutputMode {
    Raw,
    Text,
}

struct PreparedHttpToolRequest {
    method: reqwest::Method,
    url: reqwest::Url,
    headers: Vec<(String, String)>,
    body: Option<HttpToolBody>,
    timeout: std::time::Duration,
    output_mode: HttpOutputMode,
}

fn http_tool_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("build http tool client")
    })
}

fn parse_http_method_token(token: &str) -> Option<reqwest::Method> {
    let upper = token.trim().to_ascii_uppercase();
    if upper.is_empty() || upper.contains("://") {
        return None;
    }
    reqwest::Method::from_bytes(upper.as_bytes()).ok()
}

fn split_http_header_arg(token: &str) -> Option<(String, String)> {
    if token.contains("://")
        || token.starts_with(':')
        || token.contains("==")
        || token.contains(":=")
    {
        return None;
    }
    let (name, value) = token.split_once(':')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

fn parse_http_timeout_value(raw: &str) -> std::result::Result<std::time::Duration, String> {
    let secs = raw
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("invalid http timeout '{raw}'"))?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!("invalid http timeout '{raw}'"));
    }
    Ok(std::time::Duration::from_secs_f64(secs))
}

fn prepare_http_tool_request(
    input: &Value,
    default_timeout: std::time::Duration,
) -> std::result::Result<PreparedHttpToolRequest, String> {
    let args = str_array(&input["args"]);
    if args.is_empty() {
        return Err("missing args".to_string());
    }

    let mut idx = 0usize;
    let method = if let Some(method) = parse_http_method_token(&args[0]) {
        idx = 1;
        method
    } else {
        reqwest::Method::GET
    };

    let mut url: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut query_pairs: Vec<(String, String)> = Vec::new();
    let mut json_items: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut form_fields: Vec<(String, String)> = Vec::new();
    let mut raw_body = input["stdin"].as_str().map(String::from);
    let mut form_mode = false;
    let mut timeout = default_timeout;
    let mut output_mode = HttpOutputMode::Raw;

    while idx < args.len() {
        let token = &args[idx];
        match token.as_str() {
            "--form" | "-f" => {
                form_mode = true;
                idx += 1;
                continue;
            }
            "--json" | "-j" => {
                form_mode = false;
                idx += 1;
                continue;
            }
            "--follow" | "-F" | "--headers" | "-h" | "--body" | "-b" | "--check-status" => {
                idx += 1;
                continue;
            }
            "--ignore-stdin" => {
                raw_body = None;
                idx += 1;
                continue;
            }
            "--extract-text" | "--text" => {
                output_mode = HttpOutputMode::Text;
                idx += 1;
                continue;
            }
            "--timeout" => {
                let Some(value) = args.get(idx + 1) else {
                    return Err("missing value after --timeout".to_string());
                };
                timeout = parse_http_timeout_value(value)?;
                idx += 2;
                continue;
            }
            "--data" | "-d" | "--raw" => {
                let Some(value) = args.get(idx + 1) else {
                    return Err(format!("missing value after {token}"));
                };
                if raw_body.is_some() {
                    return Err("http request body specified more than once".to_string());
                }
                raw_body = Some(value.clone());
                idx += 2;
                continue;
            }
            _ => {}
        }

        if let Some(value) = token.strip_prefix("--timeout=") {
            timeout = parse_http_timeout_value(value)?;
            idx += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--data=") {
            if raw_body.is_some() {
                return Err("http request body specified more than once".to_string());
            }
            raw_body = Some(value.to_string());
            idx += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("--raw=") {
            if raw_body.is_some() {
                return Err("http request body specified more than once".to_string());
            }
            raw_body = Some(value.to_string());
            idx += 1;
            continue;
        }

        if url.is_none() && (token.starts_with("http://") || token.starts_with("https://")) {
            url = Some(token.clone());
            idx += 1;
            continue;
        }
        if let Some((key, value)) = token.split_once("==") {
            let key = key.trim();
            if key.is_empty() {
                return Err(format!("invalid query arg: {token}"));
            }
            query_pairs.push((key.to_string(), value.to_string()));
            idx += 1;
            continue;
        }
        if let Some((key, value)) = token.split_once(":=") {
            let key = key.trim();
            if key.is_empty() {
                return Err(format!("invalid JSON arg: {token}"));
            }
            let parsed = serde_json::from_str::<Value>(value)
                .map_err(|e| format!("invalid JSON value for {key}: {e}"))?;
            json_items.insert(key.to_string(), parsed);
            idx += 1;
            continue;
        }
        if let Some((name, value)) = split_http_header_arg(token) {
            headers.push((name, value));
            idx += 1;
            continue;
        }
        if let Some((key, value)) = token.split_once('=') {
            let key = key.trim();
            if key.is_empty() {
                return Err(format!("invalid request item: {token}"));
            }
            if matches!(
                method,
                reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::OPTIONS
            ) {
                query_pairs.push((key.to_string(), value.to_string()));
            } else if form_mode {
                form_fields.push((key.to_string(), value.to_string()));
            } else {
                json_items.insert(key.to_string(), Value::String(value.to_string()));
            }
            idx += 1;
            continue;
        }
        if token.starts_with('-') {
            return Err(format!("unsupported http arg: {token}"));
        }
        return Err(format!("unrecognized http arg: {token}"));
    }

    let raw_url = url.ok_or_else(|| "missing URL".to_string())?;
    if raw_body.is_some() && (!json_items.is_empty() || !form_fields.is_empty()) {
        return Err("cannot combine raw body/stdin with key=value request items".to_string());
    }

    let mut url =
        reqwest::Url::parse(&raw_url).map_err(|e| format!("invalid URL '{raw_url}': {e}"))?;
    if !query_pairs.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query_pairs {
            pairs.append_pair(&key, &value);
        }
    }

    let body = if !form_fields.is_empty() {
        Some(HttpToolBody::Form(form_fields))
    } else if !json_items.is_empty() {
        Some(HttpToolBody::Json(Value::Object(json_items)))
    } else {
        raw_body.map(HttpToolBody::Raw)
    };

    Ok(PreparedHttpToolRequest {
        method,
        url,
        headers,
        body,
        timeout,
        output_mode,
    })
}

async fn read_http_response_limited(
    resp: reqwest::Response,
    interrupt: Arc<AtomicBool>,
) -> std::result::Result<String, String> {
    let mut stream = resp.bytes_stream();
    let mut capture = LimitedByteCapture::new(PROCESS_STREAM_CAPTURE_CAP);
    loop {
        match read_stream_next_chunk(&mut stream, &interrupt, "killed by interrupt (^C)").await {
            Ok(Some(chunk)) => capture.push(&chunk),
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(capture.render("body"))
}

fn html_entity_decode_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let Some(rel_end) = s[i..].find(';') else {
            out.push('&');
            i += 1;
            continue;
        };
        let end = i + rel_end;
        let entity = &s[i + 1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                let value = &entity[2..];
                u32::from_str_radix(value, 16).ok().and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(ch) = decoded {
            out.push(ch);
            i = end + 1;
        } else {
            out.push('&');
            i += 1;
        }
    }
    out
}

fn push_text_with_space(out: &mut String, text: &str) {
    let trimmed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
        out.push(' ');
    }
    out.push_str(&html_entity_decode_minimal(&trimmed));
}

fn extract_html_text(html: &str) -> String {
    let source = byte_prefix_at_char_boundary(html, HTTP_EXTRACT_INPUT_CAP);
    let mut out = String::new();
    let mut tag = String::new();
    let mut text = String::new();
    let mut in_tag = false;
    let mut skip: Option<&'static str> = None;

    for ch in source.chars() {
        if in_tag {
            if ch == '>' {
                let name = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches(|c: char| c == '/' || c == '!')
                    .to_ascii_lowercase();
                let closing = tag.trim_start().starts_with('/');
                if closing && skip == Some(name.as_str()) {
                    skip = None;
                } else if !closing
                    && matches!(name.as_str(), "script" | "style" | "svg" | "noscript")
                {
                    skip = match name.as_str() {
                        "script" => Some("script"),
                        "style" => Some("style"),
                        "svg" => Some("svg"),
                        "noscript" => Some("noscript"),
                        _ => None,
                    };
                }
                if skip.is_none()
                    && matches!(
                        name.as_str(),
                        "p" | "br"
                            | "div"
                            | "section"
                            | "article"
                            | "li"
                            | "tr"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "title"
                    )
                    && !out.ends_with('\n')
                {
                    out.push('\n');
                }
                tag.clear();
                in_tag = false;
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            if skip.is_none() {
                push_text_with_space(&mut out, &text);
            }
            text.clear();
            in_tag = true;
        } else if skip.is_none() {
            text.push(ch);
        }
    }
    if skip.is_none() {
        push_text_with_space(&mut out, &text);
    }

    let mut compact = String::new();
    let mut blank = false;
    for line in out.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !compact.is_empty() && !blank {
            compact.push('\n');
        }
        compact.push_str(line);
        blank = false;
    }
    cap_bytes_with_hint(
        compact,
        HTTP_EXTRACT_OUTPUT_CAP,
        "extracted text truncated; use raw http or narrower source for full body.",
    )
}

fn extract_response_text(body: String, content_type: Option<&str>) -> String {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.contains("text/html") || body.to_ascii_lowercase().contains("<html") {
        extract_html_text(&body)
    } else if ct.contains("application/json") || ct.contains("+json") {
        match serde_json::from_str::<Value>(&body) {
            Ok(value) => cap_bytes_with_hint(
                serde_json::to_string_pretty(&value).unwrap_or(body),
                HTTP_EXTRACT_OUTPUT_CAP,
                "JSON text truncated; use raw http or narrower source for full body.",
            ),
            Err(_) => cap_bytes_with_hint(body, HTTP_EXTRACT_OUTPUT_CAP, "text truncated."),
        }
    } else {
        cap_bytes_with_hint(body, HTTP_EXTRACT_OUTPUT_CAP, "text truncated.")
    }
}

async fn execute_http_tool_async(
    input: &Value,
    interrupt: Arc<AtomicBool>,
    default_timeout: std::time::Duration,
) -> std::result::Result<String, String> {
    let request = prepare_http_tool_request(input, default_timeout)?;
    if interrupt.load(Ordering::SeqCst) {
        return Err("killed by interrupt (^C)".to_string());
    }

    let mut req = http_tool_client()
        .request(request.method, request.url)
        .timeout(request.timeout)
        .header(reqwest::header::USER_AGENT, "wolf/http");
    for (name, value) in request.headers {
        req = req.header(name, value);
    }
    match request.body {
        Some(HttpToolBody::Json(value)) => {
            req = req.json(&value);
        }
        Some(HttpToolBody::Form(fields)) => {
            req = req.form(&fields);
        }
        Some(HttpToolBody::Raw(body)) => {
            req = req.body(body);
        }
        None => {}
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = read_http_response_limited(resp, interrupt).await?;
    let body = if request.output_mode == HttpOutputMode::Text {
        extract_response_text(body, content_type.as_deref())
    } else {
        body
    };

    if status.is_success() {
        if body.trim().is_empty() {
            Ok(format!("HTTP {status}"))
        } else {
            Ok(body)
        }
    } else if body.trim().is_empty() {
        Err(format!("HTTP {status}"))
    } else {
        Err(format!("HTTP {status}\n{body}"))
    }
}

fn parse_bash_exit_code(content: &str) -> Option<i32> {
    content
        .lines()
        .next()
        .and_then(|line| line.trim().strip_prefix("exit:"))
        .and_then(|raw| raw.trim().parse::<i32>().ok())
}

fn parse_tool_exit_code(name: &str, ok: bool, content: &str) -> Option<i32> {
    match name {
        "bash" => parse_bash_exit_code(content),
        "fd" | "rg" | "jq" | "fzf" | "awk" | "csvkit" | "git_diff" | "git_log" => {
            if ok {
                None
            } else {
                content
                    .lines()
                    .next()
                    .and_then(|line| line.trim().strip_prefix("exit"))
                    .and_then(|raw| raw.trim_start_matches(':').trim().parse::<i32>().ok())
            }
        }
        _ => None,
    }
}

fn looks_like_verification_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "cargo test",
        "cargo nextest",
        "cargo build",
        "cargo check",
        "cargo clippy",
        "cargo install",
        "npm test",
        "pnpm test",
        "yarn test",
        "pytest",
        "go test",
        "mix test",
        "zig build test",
        "swift test",
        "dotnet test",
        "mvn test",
        "gradle test",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn artifact_safe_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "artifact".to_string()
    } else {
        out
    }
}

fn write_verification_artifact(
    root: &Path,
    name: &str,
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    duration: std::time::Duration,
    status: &str,
) -> Option<PathBuf> {
    let hash = sha256_hex_str(&format!("{command}\n{output}"));
    let short_hash = &hash[..12.min(hash.len())];
    let dir = project_state_dir(root).join("artifacts");
    let path = dir.join(format!(
        "verify-{}-{}-{short_hash}.json",
        unix_timestamp_secs(),
        artifact_safe_name(name)
    ));
    let body = json!({
        "type": "verification",
        "name": name,
        "command": command,
        "cwd": root.display().to_string(),
        "status": status,
        "exit_code": exit_code,
        "duration_ms": millis_u64(duration),
        "output_tail": byte_suffix_at_char_boundary(output, VERIFICATION_ARTIFACT_TAIL_CAP),
        "output": output,
        "git": git_summary(root),
    });
    let bytes = serde_json::to_vec_pretty(&body).ok()?;
    atomic_write_bytes(&path, &bytes).ok()?;
    Some(path)
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn symbol_token_at_start(text: &str, symbol: &str) -> bool {
    let Some(after) = text.strip_prefix(symbol) else {
        return false;
    };
    after.chars().next().is_none_or(|ch| !is_ident_continue(ch))
}

fn contains_symbol_token(text: &str, symbol: &str) -> bool {
    text.match_indices(symbol).any(|(idx, _)| {
        let before_ok = text[..idx]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_ident_continue(ch));
        let after_idx = idx + symbol.len();
        let after_ok = text[after_idx..]
            .chars()
            .next()
            .is_none_or(|ch| !is_ident_continue(ch));
        before_ok && after_ok
    })
}

fn keyword_rest<'a>(trimmed: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = trimmed.strip_prefix(keyword)?;
    if rest.chars().next().is_some_and(is_ident_continue) {
        return None;
    }
    Some(rest.trim_start())
}

fn strip_decl_qualifiers(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        if let Some(rest) = trimmed.strip_prefix("pub ") {
            text = rest;
        } else if let Some(rest) = trimmed.strip_prefix("export ") {
            text = rest;
        } else if let Some(rest) = trimmed.strip_prefix("default ") {
            text = rest;
        } else if let Some(rest) = trimmed.strip_prefix("async ") {
            text = rest;
        } else if let Some(rest) = trimmed.strip_prefix("unsafe ") {
            text = rest;
        } else if let Some(rest) = trimmed.strip_prefix("extern ") {
            let rest = rest.trim_start();
            if let Some(abi) = rest.strip_prefix('"')
                && let Some(end) = abi.find('"')
            {
                text = &abi[end + 1..];
            } else {
                text = rest;
            }
        } else if let Some(rest) = trimmed.strip_prefix("pub(") {
            if let Some(end) = rest.find(')') {
                text = &rest[end + 1..];
            } else {
                return trimmed;
            }
        } else {
            return trimmed;
        }
    }
}

fn item_signature_line_match(line: &str) -> bool {
    let trimmed = strip_decl_qualifiers(line);
    if keyword_rest(trimmed, "const")
        .and_then(|rest| keyword_rest(rest, "fn"))
        .is_some()
    {
        return true;
    }
    [
        "fn",
        "struct",
        "enum",
        "trait",
        "impl",
        "type",
        "mod",
        "class",
        "def",
        "function",
        "interface",
    ]
    .iter()
    .any(|keyword| keyword_rest(trimmed, keyword).is_some())
}

fn symbol_line_match(line: &str, symbol: &str) -> bool {
    let trimmed = strip_decl_qualifiers(line);
    for keyword in [
        "fn",
        "struct",
        "enum",
        "trait",
        "type",
        "mod",
        "class",
        "def",
        "function",
        "interface",
    ] {
        if let Some(rest) = keyword_rest(trimmed, keyword)
            && symbol_token_at_start(rest, symbol)
        {
            return true;
        }
    }
    if let Some(rest) = keyword_rest(trimmed, "impl")
        && contains_symbol_token(rest, symbol)
    {
        return true;
    }
    if let Some(rest) = keyword_rest(trimmed, "const") {
        if symbol_token_at_start(rest, symbol) {
            return true;
        }
        if let Some(fn_rest) = keyword_rest(rest, "fn") {
            return symbol_token_at_start(fn_rest, symbol);
        }
    }
    if let Some(rest) = keyword_rest(trimmed, "static") {
        return symbol_token_at_start(rest, symbol);
    }
    false
}

fn source_line_starts(content: &str) -> Vec<usize> {
    if content.is_empty() {
        return Vec::new();
    }
    let bytes = content.as_bytes();
    let mut starts = Vec::with_capacity((bytes.len() / 48).max(1));
    starts.push(0);
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' && idx + 1 < bytes.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

fn source_line_at<'a>(content: &'a str, starts: &[usize], idx: usize) -> Option<&'a str> {
    let start = *starts.get(idx)?;
    let end = starts.get(idx + 1).copied().unwrap_or(content.len());
    let line = &content[start..end];
    let line = line.strip_suffix('\n').unwrap_or(line);
    Some(line.strip_suffix('\r').unwrap_or(line))
}

fn item_start_for_open_brace(content: &str, starts: &[usize], open_line: usize) -> Option<usize> {
    let line = source_line_at(content, starts, open_line)?;
    let before_open = line
        .split_once('{')
        .map(|(before, _)| before)
        .unwrap_or(line)
        .trim();
    if item_signature_line_match(before_open) {
        return Some(open_line);
    }
    if !before_open.is_empty()
        && !before_open.starts_with(')')
        && !before_open.starts_with("where ")
    {
        return None;
    }
    let min = open_line.saturating_sub(12);
    let mut idx = open_line.saturating_sub(1);
    loop {
        let line = source_line_at(content, starts, idx)?;
        let trimmed = line.trim();
        if item_signature_line_match(trimmed) {
            return Some(idx);
        }
        if idx == 0 || idx == min || trimmed.is_empty() {
            return None;
        }
        idx -= 1;
    }
}

fn find_enclosing_source_block(
    content: &str,
    starts: &[usize],
    target: usize,
) -> Option<(usize, usize)> {
    let mut stack: Vec<(usize, bool)> = Vec::new();
    let mut first_any = None;
    for idx in 0..starts.len() {
        let line = source_line_at(content, starts, idx)?;
        for byte in line.bytes() {
            match byte {
                b'{' => {
                    let item_start = item_start_for_open_brace(content, starts, idx);
                    stack.push((item_start.unwrap_or(idx), item_start.is_some()));
                }
                b'}' => {
                    if let Some((start, item_like)) = stack.pop()
                        && start <= target
                        && target <= idx
                    {
                        if item_like {
                            return Some((start, idx));
                        }
                        if first_any.is_none() {
                            first_any = Some((start, idx));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some((start, _)) = stack
        .iter()
        .rev()
        .find(|(start, item_like)| *item_like && *start <= target)
    {
        return Some((*start, starts.len().saturating_sub(1)));
    }
    if first_any.is_some() {
        return first_any;
    }
    stack
        .iter()
        .rev()
        .find(|(start, _)| *start <= target)
        .map(|(start, _)| (*start, starts.len().saturating_sub(1)))
}

fn paragraph_line_window(content: &str, starts: &[usize], target: usize) -> (usize, usize) {
    let mut start = target;
    while start > 0 {
        let prev = source_line_at(content, starts, start - 1).unwrap_or("");
        if prev.trim().is_empty() {
            break;
        }
        start -= 1;
    }
    let mut end = target;
    while end + 1 < starts.len() {
        let next = source_line_at(content, starts, end + 1).unwrap_or("");
        if next.trim().is_empty() {
            break;
        }
        end += 1;
    }
    (start, end)
}

fn find_line_window(
    content: &str,
    starts: &[usize],
    line_no: usize,
    context: usize,
) -> Option<(usize, usize)> {
    if line_no == 0 || line_no > starts.len() {
        return None;
    }
    let target = line_no - 1;
    let (start, end) = find_enclosing_source_block(content, starts, target)
        .unwrap_or_else(|| paragraph_line_window(content, starts, target));
    Some((
        start.saturating_sub(context),
        end.saturating_add(context)
            .min(starts.len().saturating_sub(1)),
    ))
}

fn find_symbol_window(
    content: &str,
    starts: &[usize],
    symbol: &str,
    context: usize,
) -> Option<(usize, usize)> {
    let idx = (0..starts.len()).find(|idx| {
        source_line_at(content, starts, *idx).is_some_and(|line| symbol_line_match(line, symbol))
    })?;
    let mut end = idx;
    let mut brace_depth = 0i32;
    let mut seen_open = false;
    for i in idx..starts.len() {
        let line = source_line_at(content, starts, i)?;
        for byte in line.bytes() {
            match byte {
                b'{' => {
                    brace_depth += 1;
                    seen_open = true;
                }
                b'}' => brace_depth -= 1,
                _ => {}
            }
        }
        end = i;
        if seen_open && brace_depth <= 0 {
            break;
        }
        if !seen_open && i > idx && line.trim().is_empty() {
            end = i.saturating_sub(1);
            break;
        }
    }
    Some((
        idx.saturating_sub(context),
        end.saturating_add(context)
            .min(starts.len().saturating_sub(1)),
    ))
}

fn render_line_window(
    content: &str,
    starts: &[usize],
    start: usize,
    end: usize,
    cap: usize,
) -> String {
    let mut capture = LimitedTextCapture::new(cap);
    for idx in start..=end {
        if let Some(line) = source_line_at(content, starts, idx) {
            let rendered = format!("{}\t{}\n", idx + 1, line);
            if !capture.try_push_unit(&rendered) {
                return capture.finish(&format!(
                    "Pass a smaller context or use read_file offset={} to continue.",
                    idx + 1
                ));
            }
        }
    }
    capture.finish("")
}

fn effective_text_tool_capture_cap() -> usize {
    if ContextMode::from_env().is_frugal() {
        FRUGAL_TEXT_TOOL_CAPTURE_CAP
    } else {
        TEXT_TOOL_CAPTURE_CAP
    }
}

fn effective_read_file_explicit_capture_cap() -> usize {
    if ContextMode::from_env().is_frugal() {
        FRUGAL_READ_FILE_EXPLICIT_CAPTURE_CAP
    } else {
        READ_FILE_EXPLICIT_CAPTURE_CAP
    }
}

fn tool_result_context_cap(
    name: &str,
    input: &Value,
    usage: &Usage,
    model: &str,
    context_mode: ContextMode,
) -> usize {
    if context_mode.is_frugal() {
        return match name {
            "read_file" if input["offset"].is_u64() && input["limit"].is_u64() => {
                FRUGAL_READ_FILE_EXPLICIT_CAPTURE_CAP
            }
            "read_symbol" => FRUGAL_READ_FILE_EXPLICIT_CAPTURE_CAP,
            _ => FRUGAL_TOOL_RESULT_CAP,
        };
    }

    let base_cap = match name {
        "read_file" if input["offset"].is_u64() && input["limit"].is_u64() => {
            READ_FILE_EXPLICIT_CAPTURE_CAP
        }
        "read_symbol" => READ_FILE_EXPLICIT_CAPTURE_CAP,
        _ => TOOL_RESULT_CAP,
    };
    orchestrator::adaptive_tool_result_cap(usage, model, base_cap)
}

#[cfg(test)]
fn execute_tool(name: &str, input: &Value, root: &Path) -> std::result::Result<String, String> {
    execute_tool_with_cache(name, input, root, None)
}

fn todo_status_counts(items: &[Value]) -> (usize, usize, usize) {
    let mut pending = 0usize;
    let mut in_progress = 0usize;
    let mut completed = 0usize;
    for item in items {
        match item["status"].as_str().unwrap_or("pending") {
            "completed" => completed += 1,
            "in_progress" => in_progress += 1,
            _ => pending += 1,
        }
    }
    (pending, in_progress, completed)
}

fn render_todo_list(items: &[Value]) -> String {
    let mut out = String::new();
    for item in items {
        let text = item["text"].as_str().unwrap_or("?");
        let status = item["status"].as_str().unwrap_or("pending");
        let mark = match status {
            "completed" => "✓",
            "in_progress" => "►",
            _ => "○",
        };
        out.push_str(&format!("{mark} {text} [{status}]\n"));
    }
    let (pending, in_progress, completed) = todo_status_counts(items);
    out.push_str(&format!(
        "\n{pending} pending, {in_progress} in progress, {completed} completed"
    ));
    out
}

fn read_todo_counts(path: &Path) -> Option<(usize, usize, usize)> {
    let content = std::fs::read_to_string(path).ok()?;
    let todos = serde_json::from_str::<Value>(&content).ok()?;
    let items = todos.as_array()?;
    Some(todo_status_counts(items))
}

fn format_todo_delta(
    before: Option<(usize, usize, usize)>,
    after: (usize, usize, usize),
) -> String {
    let before = before.unwrap_or((0, 0, 0));
    let values = [
        (after.0 as isize - before.0 as isize, "pending"),
        (after.1 as isize - before.1 as isize, "in_progress"),
        (after.2 as isize - before.2 as isize, "completed"),
    ];
    let parts: Vec<String> = values
        .into_iter()
        .filter(|(delta, _)| *delta != 0)
        .map(|(delta, label)| {
            if delta > 0 {
                format!("+{delta} {label}")
            } else {
                format!("{delta} {label}")
            }
        })
        .collect();
    if parts.is_empty() {
        "delta: no status count changes".to_string()
    } else {
        format!("delta: {}", parts.join(" · "))
    }
}

fn execute_tool_with_cache(
    name: &str,
    input: &Value,
    root: &Path,
    read_cache: Option<&Arc<Mutex<ReadFileCache>>>,
) -> std::result::Result<String, String> {
    match name {
        "read_file" => {
            let path = input["path"].as_str().ok_or("missing path")?;
            let path = canonical_within(root, path)?;
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = input["limit"].as_u64().map(|v| v as usize);
            let explicit_window = input["offset"].is_u64() && limit.is_some();
            let cap = if explicit_window {
                effective_read_file_explicit_capture_cap()
            } else {
                effective_text_tool_capture_cap()
            };
            let metadata = std::fs::metadata(&path).map_err(|e| format!("{e}"))?;
            let signature = file_signature_from_metadata(&metadata);

            if let (Some(limit), Some(cache)) = (limit, read_cache)
                && let Ok(cache) = cache.lock()
                && let Some(out) = cache.get_window(&path, signature, offset, limit, cap)
            {
                return Ok(out);
            }

            let file = std::fs::File::open(&path).map_err(|e| format!("{e}"))?;
            let reader = std::io::BufReader::new(file);
            let mut capture = LimitedTextCapture::new(cap);
            let mut emitted = 0usize;
            let mut remaining = 0usize;
            let mut next_offset = None;
            let mut cached_lines: Vec<(usize, String)> = Vec::new();
            let mut eof_at = None;

            for (i, line) in reader.lines().enumerate() {
                let line_no = i + 1;
                let line = line.map_err(|e| format!("{e}"))?;
                if line_no < offset {
                    continue;
                }
                if let Some(max_lines) = limit
                    && emitted >= max_lines
                {
                    remaining += 1;
                    continue;
                }
                let rendered = format!("{line_no}\t{line}\n");
                if !capture.try_push_unit(&rendered) {
                    next_offset = Some(line_no.max(offset + emitted));
                    break;
                }
                cached_lines.push((line_no, line));
                emitted += 1;
            }
            if next_offset.is_none() && remaining == 0 {
                eof_at = Some(offset.saturating_add(emitted).saturating_sub(1));
            }
            if let Some(cache) = read_cache
                && let Ok(mut cache) = cache.lock()
            {
                cache.record_window(path.clone(), signature, cached_lines, eof_at);
            }

            if let Some(next_offset) = next_offset {
                Ok(capture.finish(&format!(
                    "Pass offset={next_offset} and maybe a smaller limit to continue."
                )))
            } else {
                let mut out = capture.finish("");
                if remaining > 0 {
                    out.push_str(&format!(
                        "\n…[{remaining} more lines remain; pass offset={} to continue]\n",
                        offset + emitted
                    ));
                }
                Ok(out)
            }
        }
        "read_symbol" => {
            let path_str = input["path"].as_str().ok_or("missing path")?;
            let symbol = input["symbol"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let line_no = input["line"]
                .as_u64()
                .filter(|line| *line > 0)
                .map(|v| v as usize);
            let selector_count = (symbol.is_some() as usize) + (line_no.is_some() as usize);
            if selector_count != 1 {
                return Err("provide exactly one of symbol or line".to_string());
            }
            let context = input["context"].as_u64().unwrap_or(5).min(50) as usize;
            let path = canonical_within(root, path_str)?;
            let content = std::fs::read_to_string(&path).map_err(|e| format!("{e}"))?;
            let starts = source_line_starts(&content);
            if starts.is_empty() {
                return Err(format!("{} is empty", path.display()));
            }
            let (start, end) = if let Some(line_no) = line_no {
                let Some(window) = find_line_window(&content, &starts, line_no, context) else {
                    return Err(format!(
                        "line {line_no} is outside {} ({} lines)",
                        path.display(),
                        starts.len()
                    ));
                };
                window
            } else {
                let symbol = symbol.expect("selector count checked");
                let Some(window) = find_symbol_window(&content, &starts, symbol, context) else {
                    let hint = tool_policy::tool_input_advisory("read_symbol", &input)
                        .unwrap_or_else(|| format!("Search first with rg -n '{symbol}' {}, then retry read_symbol with an exact symbol or line.", path.display()));
                    return Err(format!(
                        "symbol '{symbol}' not found in {}\n{hint}",
                        path.display()
                    ));
                };
                window
            };
            Ok(render_line_window(
                &content,
                &starts,
                start,
                end,
                READ_FILE_EXPLICIT_CAPTURE_CAP,
            ))
        }
        "write_file" => {
            let path_str = input["path"].as_str().ok_or("missing path")?;
            let content = input["content"].as_str().ok_or("missing content")?;
            let path = canonical_within(root, path_str)?;
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
            }
            std::fs::write(&path, content).map_err(|e| format!("{e}"))?;
            Ok(format!(
                "wrote {} bytes to {}",
                content.len(),
                path.display()
            ))
        }
        "edit_file" => {
            let path_str = input["path"].as_str().ok_or("missing path")?;
            let old = input["old_string"].as_str().ok_or("missing old_string")?;
            let new = input["new_string"].as_str().ok_or("missing new_string")?;
            let path = canonical_within(root, path_str)?;
            let content = std::fs::read_to_string(&path).map_err(|e| format!("{e}"))?;
            let count = content.matches(old).count();
            if count == 0 {
                return Err(format!("old_string not found in {}", path.display()));
            }
            if count > 1 {
                return Err(format!(
                    "old_string appears {count} times in {} — must be unique",
                    path.display()
                ));
            }
            let updated = content.replacen(old, new, 1);
            std::fs::write(&path, &updated).map_err(|e| format!("{e}"))?;
            Ok(edit_result_with_diff(1, &path, root, &content, &updated))
        }
        "multi_edit" => {
            let path_str = input["path"].as_str().ok_or("missing path")?;
            let edits = input["edits"].as_array().ok_or("missing edits array")?;
            let path = canonical_within(root, path_str)?;
            let before = std::fs::read_to_string(&path).map_err(|e| format!("{e}"))?;
            let mut content = before.clone();
            for (i, edit) in edits.iter().enumerate() {
                let old = edit["old_string"]
                    .as_str()
                    .ok_or_else(|| format!("edit[{i}]: missing old_string"))?;
                let new = edit["new_string"]
                    .as_str()
                    .ok_or_else(|| format!("edit[{i}]: missing new_string"))?;
                let replace_all = edit["replace_all"].as_bool().unwrap_or(false);
                if replace_all {
                    if !content.contains(old) {
                        return Err(format!("edit[{i}]: old_string not found"));
                    }
                    content = content.replace(old, new);
                } else {
                    let count = content.matches(old).count();
                    if count == 0 {
                        return Err(format!("edit[{i}]: old_string not found"));
                    }
                    if count > 1 {
                        return Err(format!(
                            "edit[{i}]: old_string appears {count} times — set replace_all or make it unique"
                        ));
                    }
                    content = content.replacen(old, new, 1);
                }
            }
            std::fs::write(&path, &content).map_err(|e| format!("{e}"))?;
            Ok(edit_result_with_diff(
                edits.len(),
                &path,
                root,
                &before,
                &content,
            ))
        }
        "bash" => Err("bash must go through execute_bash_async".to_string()),
        "git_commit" => {
            let message = input["message"].as_str().ok_or("missing message")?;
            let all = input["all"].as_bool().unwrap_or(false);
            let paths: Vec<String> = input["paths"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            if all {
                run_external("git", &["add".to_string(), "-A".to_string()], None, root)?;
            } else if !paths.is_empty() {
                let mut add_args = vec!["add".to_string()];
                add_args.extend(paths);
                run_external("git", &add_args, None, root)?;
            }

            let commit_args = vec!["commit".to_string(), "-m".to_string(), message.to_string()];
            run_external("git", &commit_args, None, root)
        }
        "todo_read" => {
            let todo_path = root.join("WOLF.todo.json");
            if !todo_path.exists() {
                return Ok("(no todos — use todo_write to create a task list)".to_string());
            }
            let content =
                std::fs::read_to_string(&todo_path).map_err(|e| format!("read todo: {e}"))?;
            let todos: Value =
                serde_json::from_str(&content).map_err(|e| format!("parse todo: {e}"))?;
            let items = todos.as_array().ok_or("todo: expected array")?;
            if items.is_empty() {
                return Ok("(todo list is empty)".to_string());
            }
            Ok(render_todo_list(items))
        }
        "todo_write" => {
            let todos = input["todos"].as_array().ok_or("missing todos array")?;
            let validated: Vec<Value> = todos
                .iter()
                .map(|t| {
                    let text = t["text"].as_str().ok_or("todo item missing text")?;
                    let status = t["status"].as_str().unwrap_or("pending");
                    if !matches!(status, "pending" | "in_progress" | "completed") {
                        return Err(format!("invalid todo status: {status}"));
                    }
                    Ok(json!({"text": text, "status": status}))
                })
                .collect::<std::result::Result<_, String>>()?;
            let todo_path = root.join("WOLF.todo.json");
            let before = read_todo_counts(&todo_path);
            let content = serde_json::to_string_pretty(&json!(validated))
                .map_err(|e| format!("serialize todo: {e}"))?;
            std::fs::write(&todo_path, content).map_err(|e| format!("write todo: {e}"))?;
            let after = todo_status_counts(&validated);
            Ok(format!(
                "{}\n\n{}",
                render_todo_list(&validated),
                format_todo_delta(before, after)
            ))
        }
        "fd" | "rg" | "jq" | "fzf" | "http" | "awk" | "csvkit" => {
            let (bin, args, stdin) = prepare_external_tool(name, input, root)?;
            match run_external(&bin, &args, stdin.as_deref(), root) {
                Err(e) if should_retry_external_tool_with_fallback(name, &bin, &e) => {
                    let (bin2, args2, stdin2) = prepare_external_tool_fallback(name, input, root);
                    run_external(&bin2, &args2, stdin2.as_deref(), root)
                }
                other => other,
            }
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

async fn execute_bash_async_with_timeout(
    cmd: &str,
    root: &Path,
    interrupt: Arc<AtomicBool>,
    timeout: std::time::Duration,
) -> std::result::Result<String, String> {
    use tokio::process::Command as TokioCommand;

    let mut command = TokioCommand::new("bash");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    configure_tokio_process_group(&mut command);
    let mut child = command.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let child_pid = child.id();

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    let out_task = tokio::spawn(collect_async_limited(stdout, PROCESS_STREAM_CAPTURE_CAP));
    let err_task = tokio::spawn(collect_async_limited(stderr, PROCESS_STREAM_CAPTURE_CAP));
    let deadline = tokio::time::Instant::now() + timeout;

    let status = loop {
        let outcome: ProcWaitOutcome = tokio::select! {
            biased;
            res = child.wait() => ProcWaitOutcome::Exited(res),
            _ = tokio::time::sleep_until(deadline) => ProcWaitOutcome::Timeout,
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if interrupt.load(Ordering::SeqCst) {
                    ProcWaitOutcome::Interrupt
                } else {
                    continue;
                }
            }
        };
        match outcome {
            ProcWaitOutcome::Exited(Ok(s)) => {
                if let Some(pid) = child_pid {
                    terminate_process_group_after_exit(pid);
                }
                break s;
            }
            ProcWaitOutcome::Exited(Err(e)) => return Err(format!("wait failed: {e}")),
            ProcWaitOutcome::Interrupt => {
                terminate_tokio_child(&mut child).await;
                let out = out_task.await.unwrap_or_default();
                let err = err_task.await.unwrap_or_default();
                let msg = format!(
                    "killed by interrupt (^C)\n--- stdout ---\n{}--- stderr ---\n{}",
                    out.render("stdout"),
                    err.render("stderr"),
                );
                return Err(msg);
            }
            ProcWaitOutcome::Timeout => {
                terminate_tokio_child(&mut child).await;
                let out = out_task.await.unwrap_or_default();
                let err = err_task.await.unwrap_or_default();
                let msg = format!(
                    "timed out after {}s running bash\n--- stdout ---\n{}--- stderr ---\n{}",
                    timeout.as_secs(),
                    out.render("stdout"),
                    err.render("stderr"),
                );
                return Err(msg);
            }
        }
    };

    let out = out_task.await.unwrap_or_default();
    let err = err_task.await.unwrap_or_default();
    let code = status.code().unwrap_or(-1);
    let stdout = out.render("stdout");
    let stderr = err.render("stderr");
    let mut body = format!("exit: {code}\n--- stdout ---\n{stdout}");
    if let Some(note) = output_suspicious_stderr_note(code, &stderr) {
        body.push_str(&note);
    }
    body.push_str("--- stderr ---\n");
    body.push_str(&stderr);
    Ok(body)
}

async fn execute_external_async(
    bin: &str,
    args: &[String],
    stdin_data: Option<&str>,
    cwd: &Path,
    interrupt: Arc<AtomicBool>,
    timeout: std::time::Duration,
) -> std::result::Result<String, String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command as TokioCommand;

    let mut cmd = TokioCommand::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    configure_tokio_process_group(&mut cmd);
    if stdin_data.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {bin}: {e} (is it on PATH?)"))?;
    let child_pid = child.id();

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let out_task = tokio::spawn(collect_async_limited(stdout, PROCESS_STREAM_CAPTURE_CAP));
    let err_task = tokio::spawn(collect_async_limited(stderr, PROCESS_STREAM_CAPTURE_CAP));

    if let Some(data) = stdin_data
        && let Some(mut si) = child.stdin.take()
    {
        si.write_all(data.as_bytes())
            .await
            .map_err(|e| format!("{e}"))?;
    }

    let deadline = tokio::time::Instant::now() + timeout;
    let status = loop {
        let outcome: ProcWaitOutcome = tokio::select! {
            biased;
            res = child.wait() => ProcWaitOutcome::Exited(res),
            _ = tokio::time::sleep_until(deadline) => ProcWaitOutcome::Timeout,
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if interrupt.load(Ordering::SeqCst) {
                    ProcWaitOutcome::Interrupt
                } else {
                    continue;
                }
            }
        };
        match outcome {
            ProcWaitOutcome::Exited(Ok(s)) => {
                if let Some(pid) = child_pid {
                    terminate_process_group_after_exit(pid);
                }
                break s;
            }
            ProcWaitOutcome::Exited(Err(e)) => return Err(format!("wait failed: {e}")),
            ProcWaitOutcome::Interrupt => {
                terminate_tokio_child(&mut child).await;
                let out = out_task.await.unwrap_or_default();
                let err = err_task.await.unwrap_or_default();
                return Err(format!(
                    "killed by interrupt (^C)\n--- stdout ---\n{}--- stderr ---\n{}",
                    out.render("stdout"),
                    err.render("stderr"),
                ));
            }
            ProcWaitOutcome::Timeout => {
                terminate_tokio_child(&mut child).await;
                let out = out_task.await.unwrap_or_default();
                let err = err_task.await.unwrap_or_default();
                return Err(format!(
                    "timed out after {}s running {bin}\n--- stdout ---\n{}--- stderr ---\n{}",
                    timeout.as_secs(),
                    out.render("stdout"),
                    err.render("stderr"),
                ));
            }
        }
    };

    let out = out_task.await.unwrap_or_default();
    let err = err_task.await.unwrap_or_default();
    format_process_output(
        out.render("stdout"),
        err.render("stderr"),
        status.code().unwrap_or(-1),
    )
}

fn should_retry_external_tool_with_fallback(name: &str, bin: &str, err: &str) -> bool {
    if bin == "grep" || bin == "find" {
        return false;
    }
    if err.contains("failed to spawn") {
        return true;
    }
    let lower = err.to_ascii_lowercase();
    name == "fd"
        && bin == "fd"
        && (lower.contains("find: paths must precede expression")
            || lower.contains("possible unquoted pattern after predicate")
            || lower.contains("unknown option")
            || lower.contains("unrecognized option")
            || lower.contains("invalid option"))
}

async fn execute_builtin_call(
    name: String,
    input: Value,
    root: PathBuf,
    interrupt: Arc<AtomicBool>,
    read_cache: Option<Arc<Mutex<ReadFileCache>>>,
) -> std::result::Result<String, String> {
    if name == "subagent" {
        let request = serde_json::from_value::<SubagentRequest>(input.clone())
            .map_err(|e| format!("invalid subagent request: {e}"))?;
        let (request_path, report_path) = write_subagent_request(&root, &request)
            .map_err(|e| format!("write subagent request: {e:#}"))?;
        let launch = spawn_detached_subagent_process(&root, &request_path, &report_path)
            .map_err(|e| format!("launch detached subagent: {e:#}"))?;
        return Ok(format!(
            "detached subagent launched in {}\ncommand: {}\nrequest: {}\nreport: {}\nstdout/stderr: {}\nmonitor: {}\nUse the monitor command to watch live progress; inspect the report after completion.",
            launch.label,
            launch.command,
            request_path.display(),
            launch.report_path.display(),
            launch.stdout_path.display(),
            launch.monitor_command
        ));
    }
    if name == "bash" {
        let cmd = input["command"].as_str().unwrap_or("").to_string();
        let guarded = tool_policy::apply_bash_guardrails(&cmd)?;
        let timeout = timeout_from_tool_input(&input, bash_tool_timeout());
        execute_bash_async_with_timeout(&guarded, &root, interrupt, timeout).await
    } else if name == "http" {
        execute_http_tool_async(&input, interrupt, external_tool_timeout()).await
    } else if is_external_process_tool(&name) {
        let (bin, args, stdin) = prepare_external_tool(&name, &input, &root)?;
        let result = execute_external_async(
            &bin,
            &args,
            stdin.as_deref(),
            &root,
            interrupt.clone(),
            external_tool_timeout(),
        )
        .await;
        match result {
            Err(e) if should_retry_external_tool_with_fallback(&name, &bin, &e) => {
                let (bin2, args2, stdin2) = prepare_external_tool_fallback(&name, &input, &root);
                execute_external_async(
                    &bin2,
                    &args2,
                    stdin2.as_deref(),
                    &root,
                    interrupt,
                    external_tool_timeout(),
                )
                .await
            }
            other => other,
        }
    } else {
        tokio::task::spawn_blocking(move || {
            execute_tool_with_cache(&name, &input, &root, read_cache.as_ref())
        })
        .await
        .map_err(|e| format!("task panic: {e}"))?
    }
}

#[derive(Debug, Clone, Copy)]
enum Choice {
    Once,
    Always,
    Deny,
}

fn cap_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    for c in s.chars().take(max_chars - 1) {
        out.push(c);
    }
    out.push('…');
    out
}

fn summarize_inline(s: &str, max_chars: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let baseline = if collapsed.is_empty() {
        "?".to_string()
    } else {
        collapsed
    };
    cap_chars(&baseline, max_chars)
}

fn summarize_args(args: &Value, max_chars: usize) -> String {
    let joined = args
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string())
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| "?".to_string());
    summarize_inline(&joined, max_chars)
}

fn summarize_bash_command(command: &str, max_chars: usize) -> String {
    let collapsed_full = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed_full.is_empty() {
        return "?".to_string();
    }
    if collapsed_full.chars().count() <= max_chars {
        return collapsed_full;
    }

    let lines: Vec<&str> = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() <= 1 {
        return cap_chars(&collapsed_full, max_chars);
    }

    let primary = lines
        .iter()
        .copied()
        .find(|line| !line.starts_with('#'))
        .unwrap_or(lines[0]);
    let first = summarize_inline(primary, max_chars.saturating_sub(16).max(12));
    cap_chars(&format!("{first} (+{} lines)", lines.len() - 1), max_chars)
}

fn summarize_call(name: &str, input: &Value) -> String {
    if let Some(issue) = tool_policy::tool_input_issue(name, input) {
        return format!("{name}: invalid args ({issue})");
    }

    match name {
        "bash" => {
            let cmd = summarize_bash_command(
                input["command"].as_str().unwrap_or("?"),
                TOOL_SUMMARY_CHAR_CAP,
            );
            format!("bash: {cmd}")
        }
        "read_file" => {
            let path = summarize_inline(input["path"].as_str().unwrap_or("?"), 90);
            let offset = input["offset"].as_u64();
            let limit = input["limit"].as_u64();
            let mut opts: Vec<String> = Vec::new();
            if let Some(v) = offset {
                opts.push(format!("offset={v}"));
            }
            if let Some(v) = limit {
                opts.push(format!("limit={v}"));
            }
            if opts.is_empty() {
                format!("read_file: {path}")
            } else {
                format!("read_file: {path} ({})", opts.join(", "))
            }
        }
        "read_symbol" => {
            let path = summarize_inline(input["path"].as_str().unwrap_or("?"), 70);
            if let Some(symbol) = input["symbol"].as_str() {
                let symbol = summarize_inline(symbol, 60);
                format!("read_symbol: {symbol} @ {path}")
            } else if let Some(line) = input["line"].as_u64() {
                format!("read_symbol: line {line} @ {path}")
            } else {
                format!("read_symbol: ? @ {path}")
            }
        }
        "write_file" => {
            let path = summarize_inline(input["path"].as_str().unwrap_or("?"), 90);
            let bytes = input["content"].as_str().map(|s| s.len()).unwrap_or(0);
            format!("write_file: {path} ({bytes} bytes)")
        }
        "edit_file" => {
            let path = summarize_inline(input["path"].as_str().unwrap_or("?"), 90);
            format!("edit_file: {path}")
        }
        "multi_edit" => {
            let path = summarize_inline(input["path"].as_str().unwrap_or("?"), 90);
            let n = input["edits"].as_array().map(|a| a.len()).unwrap_or(0);
            format!("multi_edit: {path} ({n} edits)")
        }
        "rg" => {
            let pattern = summarize_inline(input["pattern"].as_str().unwrap_or("?"), 80);
            let path = summarize_inline(input["path"].as_str().unwrap_or("."), 50);
            let extra = input["extra_args"].as_array().map(|a| a.len()).unwrap_or(0);
            if extra > 0 {
                format!("rg: /{pattern}/ in {path} (+{extra} args)")
            } else {
                format!("rg: /{pattern}/ in {path}")
            }
        }
        "fd" => {
            let pattern = summarize_inline(input["pattern"].as_str().unwrap_or("?"), 80);
            let path = summarize_inline(input["path"].as_str().unwrap_or("."), 50);
            let extra = input["extra_args"].as_array().map(|a| a.len()).unwrap_or(0);
            if extra > 0 {
                format!("fd: {pattern} in {path} (+{extra} args)")
            } else {
                format!("fd: {pattern} in {path}")
            }
        }
        "jq" => {
            let filter = summarize_inline(input["filter"].as_str().unwrap_or("?"), 80);
            if let Some(path) = input["path"].as_str() {
                format!("jq: {filter} @ {}", summarize_inline(path, 60))
            } else if input["json"].is_string() {
                format!("jq: {filter} @ inline-json")
            } else {
                format!("jq: {filter}")
            }
        }
        "fzf" => {
            let query = summarize_inline(input["query"].as_str().unwrap_or("?"), 60);
            let items = input["items"].as_array().map(|a| a.len()).unwrap_or(0);
            format!("fzf: query=\"{query}\" items={items}")
        }
        "http" => format!("http: {}", summarize_args(&input["args"], 120)),
        "browser" => format!(
            "browser: agent-browser {}",
            summarize_args(&input["args"], 120)
        ),
        "awk" => format!("awk: {}", summarize_args(&input["args"], 120)),
        "csvkit" => {
            let sub = input["subcommand"].as_str().unwrap_or("?");
            format!("csvkit:{sub} {}", summarize_args(&input["args"], 120))
        }
        "subagent" => {
            let task = summarize_inline(input["task"].as_str().unwrap_or("?"), 84);
            let max_iter = input["max_iterations"].as_u64();
            let tools = match input["allowed_tools"].as_array() {
                Some(arr) => {
                    let names: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).take(4).collect();
                    if names.is_empty() {
                        "custom".to_string()
                    } else if arr.len() > names.len() {
                        format!("{} +{}", names.join(","), arr.len() - names.len())
                    } else {
                        names.join(",")
                    }
                }
                None => "default".to_string(),
            };
            let iter_label = max_iter
                .map(|max| max.to_string())
                .unwrap_or_else(|| "unlimited".to_string());
            format!("subagent: \"{task}\" · detached · tools={tools} · max_iter={iter_label}")
        }
        _ => {
            let compact = summarize_inline(&input.to_string(), TOOL_SUMMARY_CHAR_CAP);
            format!("{name}: {compact}")
        }
    }
}

fn json_byte_len(v: &Value) -> usize {
    match v {
        Value::Null => 4,
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => {
            s.len()
                + 2
                + s.chars()
                    .filter(|c| matches!(c, '"' | '\\' | '\n' | '\r' | '\t'))
                    .count()
        }
        Value::Array(a) => {
            let inner: usize = a.iter().map(json_byte_len).sum();
            2 + inner + a.len().saturating_sub(1)
        }
        Value::Object(m) => {
            let inner: usize = m.iter().map(|(k, v)| k.len() + 3 + json_byte_len(v)).sum();
            2 + inner + m.len().saturating_sub(1)
        }
    }
}

fn prompt_permission(name: &str, input: &Value, pretty: bool) -> Choice {
    let marker = accent("▶", pretty);
    let hint = dim("[y=once / a=always / N]", pretty);
    print!("{marker} {} {hint} ", summarize_call(name, input));
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return Choice::Deny;
    }
    match line.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
        Some('y') => Choice::Once,
        Some('a') => Choice::Always,
        _ => Choice::Deny,
    }
}

const DEFAULT_SYSTEM: &str = "\
You are wolf, a terse coding assistant running as a CLI agent on the user's machine.

Use only tools exposed in the current API tool list. Do not assume unavailable tools exist.

Runtime:
- Privileged operations are approved by the runtime; do not ask permission in text.
- If approval is denied, stop and ask the user how to proceed.
- Do not use '--break-system-packages' unless the user explicitly requests it.
- Avoid direct mutation of external state stores, such as another tool's SQLite DB; use official CLI/API flows or ask first.

Project state:
- For nontrivial or ongoing project work, run todo_read first and use todo_write to track a concise plan/status.
- Skip todo tools for trivial, single-turn, or mechanical benchmark tasks unless the user asks for planning/status.
- Treat injected WOLF.md / WOLF.memory.md as project guidance; reread files only when exact current text matters.
- Update WOLF.memory.md only for durable decisions, preferences, in-progress work, or open questions.
- Keep WOLF.md concise and update it only for durable project guidance.

Discovery and reading:
- Prefer fd for file discovery and rg for content search.
- Read source symbol-first: use rg to find exact functions/types/line hits unless the exact symbol is already known, then use read_symbol or focused read_file windows.
- Avoid broad or overlapping reads. If output is capped, narrow the query, paginate intentionally, or request a summary/stat view.
- read_file output is line-numbered and capped; use offset+limit unless a whole file is truly needed.
- Use independent read-only tools in parallel when useful.

Editing:
- Always read the current target text before editing it.
- Use edit_file for small targeted changes, multi_edit for several edits in one file, and write_file for new files.
- Before large edits, make a concise feasibility/design checkpoint from gathered evidence.

Git:
- Before editing tracked files, inspect relevant status/diff. Use git_log only when history is needed.
- Review git_diff before final reporting or committing.
- Use git_commit, not raw bash git commit.

Shell and external operations:
- For bash pipelines, preserve failures with pipefail and avoid brittle silent-success checks.
- API tools are not shell binaries; use Wolf rg/fd/jq/http tools directly, or probe shell commands with `command -v` before assuming they exist in bash.
- For complex quoting, prefer arrays, single-quoted args, or heredocs.
- Inspect stderr even when exit code is 0.
- Validate external/data sources with one representative probe before scaling.
- If repeated auth failures occur, stop endpoint-chasing and ask for credentials or another path.
- If browser_recipe=agent-browser, use bash to invoke agent-browser only when browser automation is useful; start with `agent-browser skills get core --full`.

Subagents:
- Do not call subagent directly. The user invokes delegation with /subagent.
- If the user asks you to delegate, suggest an appropriate /subagent command.
- After subagent output/report is provided, review it before acting on it.

Verification:
- Run the narrowest useful checks first and use realistic timeouts for long builds/tests.
- Prefer stdlib/declared project test runners before adding new test dependencies; for small Python CLIs, default to unittest unless the repo already uses pytest.
- Compare structured outputs semantically (for example parsed JSON equality) rather than textual diffs when formatting is irrelevant.
- Do not rerun full suites unless code changed after the last pass.
- Record verification commands/results in the work ledger when practical; save large outputs as artifacts when useful.

Context management:
- Keep tool output small and targeted.
- Preserve exact paths, line ranges, commands, errors, and decisions.
- Avoid broad rereads of files just written; prefer focused verification, compile/test checks, or targeted reads when exact text is in doubt.
- Do not paste large logs into responses; summarize and reference artifacts when possible.
- Share useful partial results early when gaps remain.

Communication:
- Be terse.
- Report what you did, what changed, verification results, and remaining gaps.
- Avoid narrating plans unless making a design checkpoint or asking for a decision.";

const DEFAULT_SUBAGENT_SYSTEM: &str = "\
You are a focused worker agent spawned to complete one specific task delegated by a \
parent agent. You share the same sandbox and toolkit, but you do NOT see the parent's \
conversation — only the task you were given.

Work like the main agent: use tools in parallel when independent, inspect exact source \
before conclusions, and make the task self-contained and reviewable. Prefer broad \
searches first, then read small focused windows around relevant symbols. If the task \
needs code changes and write tools are available, make focused edits and run the \
narrowest useful verification. If you only researched, provide concrete implementation \
guidance.

End with a concise handoff for the parent using these exact headings: source inspected, verification run, files touched, uncertainty/open questions, confidence, exact recommended edits, remaining risks. Include what you did, key findings, files and line numbers, edits made or recommended, verification performed, and any blockers. Be specific and terse; avoid vague recaps.

Do not ask clarifying questions. If the task is under-specified, make a reasonable \
interpretation, do the work, and note assumptions in your handoff.";

const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "read_symbol",
    "fd",
    "rg",
    "fzf",
    "jq",
    "git_diff",
    "git_log",
    "todo_read",
];

const PLAN_SYSTEM: &str = "\
You are a planning agent. You have READ-ONLY tools: read_file, fd, rg, fzf, jq, \
git_diff, git_log, todo_read. Explore the codebase and produce a concrete implementation plan.

Output sections, in this order:
1. Task — restate in one sentence.
2. Files — paths you'll touch, one per line, each with a brief reason.
3. Plan — numbered steps, short imperative sentences.
4. Risks — assumptions or open questions (omit if none).

Be terse. Plan only — do NOT write code.";

const COMPACT_SYSTEM: &str = "\
You are a transcript summarizer. Output ONLY a dense, factual resume packet using these exact sections:\n\
Task\n\
Decisions\n\
Files\n\
Open work\n\
Recent state\n\
Keep each section concise. Capture the user's overall goal, key decisions and why, file paths, function names, identifiers, unresolved work, and anything the next assistant must remember to continue safely. No preamble, no meta-commentary, no filler.";

const HISTORY_CHAR_BUDGET_MIN: usize = 24_000;
const HISTORY_CHAR_BUDGET_END_TURN_PERCENT: u8 = 90;
const HISTORY_CHAR_BUDGET_ACTIVE_PERCENT: u8 = 80;
const COMPACT_KEEP_MESSAGES: usize = 6;
const COMPACT_USER_BOUNDARY_BACKTRACK: usize = COMPACT_KEEP_MESSAGES * 4;
const COMPACT_PRESERVE_TOOL_MESSAGES: usize = 10;
const FRUGAL_COMPACT_PRESERVE_TOOL_MESSAGES: usize = 2;
const COMPACT_PRESERVE_TOOL_BYTES: usize = 24_000;
const FRUGAL_COMPACT_PRESERVE_TOOL_BYTES: usize = 8_000;
const COMPACT_SUMMARY_TOOL_RESULT_CAP: usize = 1_000;
const FRUGAL_COMPACT_SUMMARY_TOOL_RESULT_CAP: usize = 360;

#[derive(Deserialize, Clone, Default)]
struct Hook {
    #[serde(rename = "match")]
    tool_match: Option<String>,
    command: String,
}

#[derive(Deserialize, Clone, Default)]
struct Hooks {
    #[serde(default)]
    pre_tool: Vec<Hook>,
    #[serde(default)]
    post_tool: Vec<Hook>,
    #[serde(default)]
    user_prompt: Vec<Hook>,
}

impl Hooks {
    fn load(root: &Path) -> Self {
        let path = std::env::var("WOLF_HOOKS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("WOLF.hooks.json"));
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("[hooks] parse error in {}: {e}", path.display());
                Hooks::default()
            }),
            Err(_) => Hooks::default(),
        }
    }

    fn fire(
        &self,
        phase: &str,
        tool: &str,
        env: &[(&str, &str)],
        root: &Path,
    ) -> Vec<(String, i32)> {
        let hooks: &[Hook] = match phase {
            "pre_tool" => &self.pre_tool,
            "post_tool" => &self.post_tool,
            "user_prompt" => &self.user_prompt,
            _ => return Vec::new(),
        };
        let mut out = Vec::new();
        for h in hooks {
            if let Some(m) = &h.tool_match
                && m != "*"
                && m != tool
            {
                continue;
            }
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(&h.command).current_dir(root);
            for (k, v) in env {
                cmd.env(k, v);
            }
            match run_sync_command_limited(
                cmd,
                None,
                HOOK_OUTPUT_CAPTURE_CAP,
                "hook command",
                hook_timeout(),
            ) {
                Ok((stdout, stderr, code)) => {
                    let combined = merge_process_output_with_status(
                        stdout.render("hook stdout"),
                        stderr.render("hook stderr"),
                        code,
                    );
                    out.push((combined, code));
                }
                Err(e) => out.push((e, -1)),
            }
        }
        out
    }
}

const LATEST_SESSION_NAME: &str = "_latest";
const SESSION_FORMAT_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) enum ContextMode {
    #[default]
    Standard,
    Frugal,
}

impl ContextMode {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "standard" | "default" | "full" | "normal" | "off" => Some(Self::Standard),
            "frugal" | "lean" | "slim" | "minimal" | "min" => Some(Self::Frugal),
            _ => None,
        }
    }

    fn from_env() -> Self {
        std::env::var("WOLF_CONTEXT_MODE")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Frugal => "frugal",
        }
    }

    fn is_frugal(self) -> bool {
        self == Self::Frugal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolContextProfile {
    Full,
    Lean,
}

impl ToolContextProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Lean => "lean",
        }
    }
}

fn tool_name_allowed_in_profile(name: &str, profile: ToolContextProfile) -> bool {
    match profile {
        ToolContextProfile::Full => true,
        ToolContextProfile::Lean => matches!(
            name,
            "read_file"
                | "read_symbol"
                | "fd"
                | "rg"
                | "fzf"
                | "jq"
                | "write_file"
                | "edit_file"
                | "multi_edit"
                | "bash"
                | "http"
                | "awk"
                | "csvkit"
                | "git_diff"
                | "git_log"
                | "git_commit"
                | "todo_read"
                | "todo_write"
        ),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
struct WorkLedger {
    objective: String,
    constraints: Vec<String>,
    current_phase: String,
    decisions: Vec<String>,
    done: Vec<String>,
    in_progress: Vec<String>,
    pending: Vec<String>,
    blocked: Vec<String>,
    steering: Vec<String>,
    files_changed: Vec<String>,
    verification: Vec<VerificationRecord>,
    next_actions: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct VerificationRecord {
    name: String,
    command: String,
    status: String,
    exit_code: Option<i32>,
    duration_ms: u64,
    artifact: Option<String>,
    validates: Vec<String>,
}

impl Default for VerificationRecord {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            status: "unknown".to_string(),
            exit_code: None,
            duration_ms: 0,
            artifact: None,
            validates: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct ProviderHealthLedger {
    providers: BTreeMap<String, ProviderHealthState>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct ProviderHealthState {
    auth: String,
    last_error: Option<String>,
    retry_after: Option<u64>,
    mode: Option<String>,
    disabled_for_turn: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct SessionProvenance {
    wolf_version: String,
    git: Option<String>,
    provider: String,
    api_provider: ApiProvider,
    model: String,
    thinking_effort: ThinkingEffort,
    approval_profile: ApprovalProfile,
    sandbox_profile: SandboxProfile,
    system_prompt_hash: String,
    wolf_md_hash: Option<String>,
    wolf_memory_hash: Option<String>,
    tool_catalog_version: u32,
    prompt_sources: Vec<String>,
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex_str(s: &str) -> String {
    sha256_hex_bytes(s.as_bytes())
}

#[derive(Serialize, Deserialize, Clone)]
struct SessionHeader {
    #[serde(default = "default_session_version")]
    version: u32,
    model: String,
    system: String,
    /// Legacy name: tools approved persistently for this session/profile.
    #[serde(default)]
    allowed: Vec<String>,
    #[serde(default)]
    exposed_tools: Vec<String>,
    #[serde(default)]
    approval_required_tools: Vec<String>,
    #[serde(default)]
    auto_approved_tools: Vec<String>,
    #[serde(default)]
    sandbox: Option<String>,
    #[serde(default)]
    usage: Usage,
    #[serde(default)]
    thinking_effort: ThinkingEffort,
    #[serde(default)]
    compact_threshold_chars: Option<usize>,
    #[serde(default)]
    compact_threshold_percent: Option<u8>,
    #[serde(default)]
    approval_profile: ApprovalProfile,
    #[serde(default)]
    sandbox_profile: SandboxProfile,
    #[serde(default)]
    budget_cap: Option<BudgetCap>,
    #[serde(default)]
    browser_recipe: BrowserRecipe,
    #[serde(default)]
    context_mode: ContextMode,
    #[serde(default)]
    tool_profile: ToolProfile,
    #[serde(default)]
    provenance: SessionProvenance,
    #[serde(default)]
    work_ledger: WorkLedger,
    #[serde(default)]
    provider_health: ProviderHealthLedger,
    #[serde(default)]
    track_origin: Option<TrackOrigin>,
    #[serde(default)]
    privacy: PrivacyPolicy,
}

impl Default for SessionHeader {
    fn default() -> Self {
        Self {
            version: SESSION_FORMAT_VERSION,
            model: String::new(),
            system: DEFAULT_SYSTEM.to_string(),
            allowed: Vec::new(),
            exposed_tools: Vec::new(),
            approval_required_tools: Vec::new(),
            auto_approved_tools: Vec::new(),
            sandbox: None,
            usage: Usage::default(),
            thinking_effort: ThinkingEffort::default(),
            compact_threshold_chars: None,
            compact_threshold_percent: None,
            approval_profile: ApprovalProfile::default(),
            sandbox_profile: SandboxProfile::default(),
            budget_cap: None,
            browser_recipe: BrowserRecipe::default(),
            context_mode: ContextMode::default(),
            tool_profile: ToolProfile::Full,
            provenance: SessionProvenance::default(),
            work_ledger: WorkLedger::default(),
            provider_health: ProviderHealthLedger::default(),
            track_origin: None,
            privacy: PrivacyPolicy::default(),
        }
    }
}

fn render_work_ledger_prompt(ledger: &WorkLedger) -> String {
    let mut out = String::new();
    if !ledger.objective.trim().is_empty() {
        out.push_str(&format!("objective: {}\n", ledger.objective.trim()));
    }
    if !ledger.current_phase.trim().is_empty() {
        out.push_str(&format!("current_phase: {}\n", ledger.current_phase.trim()));
    }
    for (label, items) in [
        ("constraints", &ledger.constraints),
        ("decisions", &ledger.decisions),
        ("done", &ledger.done),
        ("in_progress", &ledger.in_progress),
        ("pending", &ledger.pending),
        ("blocked", &ledger.blocked),
        ("queued_user_updates", &ledger.steering),
        ("files_changed", &ledger.files_changed),
        ("next_actions", &ledger.next_actions),
    ] {
        if !items.is_empty() {
            out.push_str(&format!("{label}:\n"));
            for item in items.iter().take(8) {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
    }
    if !ledger.verification.is_empty() {
        out.push_str("verification:\n");
        for record in ledger.verification.iter().rev().take(6).rev() {
            out.push_str(&format!(
                "- {}: {} ({}ms){}\n",
                record.name,
                record.status,
                record.duration_ms,
                record
                    .artifact
                    .as_ref()
                    .map(|a| format!(" artifact={a}"))
                    .unwrap_or_default()
            ));
        }
    }
    out
}

fn render_provider_health_prompt(health: &ProviderHealthLedger) -> String {
    let mut out = String::new();
    for (provider, state) in &health.providers {
        if state.last_error.is_none() && state.retry_after.is_none() && state.auth.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{provider}: auth={} ",
            if state.auth.is_empty() {
                "unknown"
            } else {
                &state.auth
            }
        ));
        if let Some(mode) = &state.mode {
            out.push_str(&format!("mode={mode} "));
        }
        if let Some(retry_after) = state.retry_after {
            out.push_str(&format!("retry_after={retry_after}s "));
        }
        if state.disabled_for_turn {
            out.push_str("disabled_for_turn=true ");
        }
        if let Some(err) = &state.last_error {
            out.push_str(&format!("last_error={} ", summarize_inline(err, 160)));
        }
        out.push('\n');
    }
    out
}

fn default_session_version() -> u32 {
    SESSION_FORMAT_VERSION
}

fn is_provider_tool_result_id_bug(text: &str) -> bool {
    (text.contains("ClaudeContentBlockToolResult") && text.contains("no attribute 'id'"))
        || text.contains("No tool call found for function call output with call_id")
}

fn parse_compact_slash(line: &str) -> Option<Result<CompactSlash, &'static str>> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("/compact")?;
    let arg = rest.trim();
    if rest.is_empty() || arg.is_empty() {
        return Some(Ok(CompactSlash::RunNow));
    }
    if arg.eq_ignore_ascii_case("status") {
        return Some(Ok(CompactSlash::Status));
    }
    if arg.eq_ignore_ascii_case("auto") {
        return Some(Ok(CompactSlash::Auto));
    }

    let numeric = arg.strip_suffix('%').unwrap_or(arg).trim();
    if let Ok(percent) = numeric.parse::<u8>()
        && (1..=100).contains(&percent)
    {
        return Some(Ok(CompactSlash::SetPercent(percent)));
    }
    Some(Err("usage: /compact [status|auto|<percent>|<percent>%]"))
}

fn apply_runtime_model_command(agent: &mut Agent, arg: &str) -> Result<String> {
    if arg.trim().is_empty() {
        return Ok(format!(
            "model: {} (provider: {})",
            agent.model, agent.provider_id
        ));
    }
    let selection = load_provider_catalog().and_then(|catalog| {
        let store = load_auth_store()?;
        resolve_provider_model_selection(&catalog, &store, &agent.provider_id, arg)
    })?;
    let provider_changed =
        canonical_provider_id(&selection.provider_id) != canonical_provider_id(&agent.provider_id);
    let target_provider = selection.provider_id.clone();
    let target_model = selection.model.clone();
    if provider_changed {
        set_active_provider_in_catalog(&target_provider)?;
        agent.reload_provider(Some(&target_provider), false)?;
    }
    agent.model = target_model.clone();
    agent.pin_model_for_provider(&target_provider, &target_model);
    agent.refresh_tools_for_context();
    match set_provider_default_model_in_catalog(&target_provider, &target_model) {
        Ok(()) if provider_changed => Ok(format!(
            "model -> {} (provider -> {}; saved as default; applies immediately to the next model request)",
            agent.model, agent.provider_id
        )),
        Ok(()) => Ok(format!(
            "model -> {} (saved as default for provider {}; applies immediately to the next model request)",
            agent.model, agent.provider_id
        )),
        Err(e) if provider_changed => Ok(format!(
            "model -> {} (provider -> {}; applies immediately; session-only model change; failed to persist default: {e:#})",
            agent.model, agent.provider_id
        )),
        Err(e) => Ok(format!(
            "model -> {} (applies immediately; session-only; failed to persist default: {e:#})",
            agent.model
        )),
    }
}

fn apply_runtime_control_command(
    agent: &mut Agent,
    text: &str,
    mut emit: impl FnMut(String),
) -> bool {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return false;
    }

    let mut parts = trimmed[1..].splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    if cmd == "model" {
        match apply_runtime_model_command(agent, arg) {
            Ok(msg) => emit(msg),
            Err(e) => emit(format!("[err] {e:#}")),
        }
        return true;
    }

    let effort_arg = trimmed
        .strip_prefix("/effort")
        .or_else(|| trimmed.strip_prefix("/think"))
        .or_else(|| trimmed.strip_prefix("/thinking"))
        .map(str::trim);
    if let Some(arg) = effort_arg {
        let parsed = match arg.to_ascii_lowercase().as_str() {
            "" | "status" => Some(agent.thinking_effort()),
            "next" | "+" => Some(agent.cycle_thinking_effort(1)),
            "prev" | "previous" | "-" => Some(agent.cycle_thinking_effort(-1)),
            _ => ThinkingEffort::parse(arg).map(|level| {
                agent.set_thinking_effort(level);
                agent.thinking_effort()
            }),
        };
        match parsed {
            Some(effort) => emit(format!(
                "thinking effort -> {} (applies immediately to the next model request in this run)",
                effort.as_str()
            )),
            None => emit("usage: /effort [low|medium|high|xhigh|next|prev|status]".to_string()),
        }
        return true;
    }

    if let Some(parsed) = parse_compact_slash(trimmed) {
        match parsed {
            Ok(CompactSlash::RunNow) => emit(
                "/compact runs only when the agent is idle; use /compact status or /compact N% during a run"
                    .to_string(),
            ),
            Ok(CompactSlash::Status) => {
                let current = agent.compact_threshold_chars();
                let active = agent.active_compact_threshold_chars();
                let base = history_char_budget_with_override(&agent.model, None, agent.context_mode);
                match agent.compact_threshold_override_percent() {
                    Some(percent) => emit(format!(
                        "compact threshold: {current} chars ({percent}% of model context window; active-run trigger {active}; auto baseline {base})"
                    )),
                    None => emit(format!(
                        "compact threshold: {current} chars (auto: {} mode; active-run trigger {active})",
                        agent.context_mode.as_str()
                    )),
                }
            }
            Ok(CompactSlash::Auto) => {
                agent.set_compact_threshold_auto();
                emit(format!(
                    "compact threshold reset to auto {} ({})",
                    agent.context_mode.as_str(),
                    agent.compact_threshold_chars()
                ));
            }
            Ok(CompactSlash::SetPercent(percent)) => {
                let chars = agent.set_compact_threshold_percent(percent);
                emit(format!("compact threshold set to {percent}% -> {chars} chars"));
            }
            Err(msg) => emit(msg.to_string()),
        }
        return true;
    }

    false
}

fn compaction_user_text_with_evidence(transcript: &str, evidence: &str) -> String {
    let evidence_section = if evidence.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\nDeterministic evidence packet (preserve these facts and cite anchors like [tool:<id>] when useful):\n{}",
            evidence.trim()
        )
    };
    format!(
        "Summarize the following transcript so a future assistant can resume the work.\n\nUse the exact section headings below and keep each section concise:\n- Task\n- Decisions\n- Files\n- Open work\n- Recent state\n\nPreserve concrete state over prose: latest user intent, active work ledger, verification results, file paths/line refs, subagent reports, provider/runtime errors, unresolved blockers, and cited tool/event IDs. If the transcript conflicts with the deterministic evidence packet, trust the evidence packet.\n{evidence_section}\n\n---\n{transcript}---"
    )
}

#[cfg(test)]
fn compaction_user_text(transcript: &str) -> String {
    compaction_user_text_with_evidence(transcript, "")
}

fn format_compacted_summary(summary: &str, preserved_count: usize) -> String {
    let mut synthetic_text = format!(
        "[prior conversation, summarized for resume]\n\n{}",
        summary.trim()
    );
    if preserved_count > 0 {
        synthetic_text.push_str(&format!(
            "\n\n[compaction] retained {preserved_count} recent tool message(s) verbatim after this summary."
        ));
    }
    synthetic_text
}

fn pair_close_keep_set(old: &[Message], keep_set: &mut HashSet<usize>) {
    // Index every tool call_id seen in `old` to the message that holds each half
    // of the pair. A call_id may legitimately map to at most one ToolUse owner
    // and one ToolResult owner.
    let mut use_owner: HashMap<String, usize> = HashMap::new();
    let mut result_owner: HashMap<String, usize> = HashMap::new();
    for (idx, msg) in old.iter().enumerate() {
        for b in &msg.content {
            match b {
                Block::ToolUse { id, .. } => {
                    use_owner.entry(id.clone()).or_insert(idx);
                }
                Block::ToolResult { tool_use_id, .. } => {
                    result_owner.entry(tool_use_id.clone()).or_insert(idx);
                }
                _ => {}
            }
        }
    }

    // Expand keep_set: whenever a member references a call_id, ensure both halves
    // that exist in `old` are kept. Iterate until no more additions — a newly
    // pulled-in message can itself reference further call_ids.
    loop {
        let mut added = false;
        let snapshot: Vec<usize> = keep_set.iter().copied().collect();
        for idx in snapshot {
            for b in &old[idx].content {
                match b {
                    Block::ToolUse { id, .. } => {
                        if let Some(&owner) = result_owner.get(id)
                            && keep_set.insert(owner)
                        {
                            added = true;
                        }
                    }
                    Block::ToolResult { tool_use_id, .. } => {
                        if let Some(&owner) = use_owner.get(tool_use_id)
                            && keep_set.insert(owner)
                        {
                            added = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !added {
            break;
        }
    }

    // Safety net: drop any member that still references a call_id whose paired
    // half is missing from `old` entirely. This shouldn't trigger if the sender
    // always emits pairs, but keeps the request valid if history arrives
    // partially (e.g. resumed from a checkpoint taken mid-turn).
    let orphans: Vec<usize> = keep_set
        .iter()
        .copied()
        .filter(|&idx| {
            old[idx].content.iter().any(|b| match b {
                Block::ToolUse { id, .. } => !result_owner.contains_key(id),
                Block::ToolResult { tool_use_id, .. } => !use_owner.contains_key(tool_use_id),
                _ => false,
            })
        })
        .collect();
    for idx in orphans {
        keep_set.remove(&idx);
    }
}

fn build_compacted_history(
    summary: &str,
    preserved_tool_msgs: Vec<Message>,
    tail: &[Message],
) -> Vec<Message> {
    let synthetic = Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: format_compacted_summary(summary, preserved_tool_msgs.len()),
        }],
    };
    let ack = Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "Understood. I will continue from this resume packet and the retained recent context."
                .to_string(),
        }],
    };

    let mut new_history = vec![synthetic, ack];
    new_history.extend(preserved_tool_msgs);
    new_history.extend_from_slice(tail);
    new_history
}

fn render_compaction_evidence(
    msgs: &[Message],
    ledger: &WorkLedger,
    health: &ProviderHealthLedger,
) -> String {
    let mut out = String::new();
    if !ledger.objective.trim().is_empty()
        || !ledger.files_changed.is_empty()
        || !ledger.verification.is_empty()
    {
        out.push_str("[ledger:active]\n");
        out.push_str(&render_work_ledger_prompt(ledger));
    }
    let health_text = render_provider_health_prompt(health);
    if !health_text.trim().is_empty() {
        out.push_str("[provider_health:active]\n");
        out.push_str(&health_text);
    }

    let mut latest_user: Option<String> = None;
    let mut tool_facts: Vec<String> = Vec::new();
    let mut last_tool_use: HashMap<String, (String, String)> = HashMap::new();
    for msg in msgs {
        if msg.role == "user" {
            for block in &msg.content {
                if let Block::Text { text } | Block::PartialStream { text } = block {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        latest_user = Some(summarize_inline(trimmed, 220));
                    }
                }
            }
        }
        for block in &msg.content {
            match block {
                Block::ToolUse { id, name, input } => {
                    last_tool_use.insert(id.clone(), (name.clone(), summarize_call(name, input)));
                }
                Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    let status = if is_error.unwrap_or(false) {
                        "err"
                    } else {
                        "ok"
                    };
                    let (name, summary) = last_tool_use
                        .get(tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| ("tool".to_string(), tool_use_id.clone()));
                    tool_facts.push(format!(
                        "[tool:{tool_use_id}] {status} {name}: {summary} => {}",
                        summarize_inline(content, 240)
                    ));
                }
                Block::Text { text } if text.contains("[subagent completed]") => {
                    tool_facts.push(format!("[subagent] {}", summarize_inline(text, 260)));
                }
                _ => {}
            }
        }
    }
    if let Some(latest_user) = latest_user {
        out.push_str(&format!("[intent:latest] {latest_user}\n"));
    }
    if !tool_facts.is_empty() {
        out.push_str("[recent_tool_facts]\n");
        let start = tool_facts.len().saturating_sub(16);
        for fact in &tool_facts[start..] {
            out.push_str("- ");
            out.push_str(fact);
            out.push('\n');
        }
    }
    out
}

fn render_transcript_for_summary(msgs: &[Message], context_mode: ContextMode) -> String {
    let mut out = String::new();
    let tool_use_cap = if context_mode.is_frugal() { 120 } else { 300 };
    let tool_result_cap = if context_mode.is_frugal() { 180 } else { 500 };
    for m in msgs {
        for b in &m.content {
            match b {
                Block::Text { text } => {
                    let t = text.trim();
                    if !t.is_empty() {
                        out.push_str(&format!("[{}] {t}\n", m.role));
                    }
                }
                Block::PartialStream { text } => {
                    let t = text.trim();
                    if !t.is_empty() {
                        out.push_str(&format!("[{}→partial_stream] {t}\n", m.role));
                    }
                }
                Block::Thinking { .. } => {}
                Block::ToolUse { name, input, .. } => {
                    let s = input.to_string();
                    let truncated: String = s.chars().take(tool_use_cap).collect();
                    out.push_str(&format!("[{}→tool:{name}] {truncated}\n", m.role));
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let tag = if is_error.unwrap_or(false) {
                        "tool_err"
                    } else {
                        "tool_ok"
                    };
                    let truncated: String = content.chars().take(tool_result_cap).collect();
                    out.push_str(&format!("[{}→{tag}] {truncated}\n", m.role));
                }
            }
        }
    }
    cap_bytes_with_hint(
        out,
        if context_mode.is_frugal() {
            FRUGAL_SUMMARY_TRANSCRIPT_CAP
        } else {
            SUMMARY_TRANSCRIPT_CAP
        },
        "Older transcript content omitted to keep compaction bounded.",
    )
}

fn parse_model_context_hint_tokens(model: &str) -> Option<u64> {
    let lower = model.to_ascii_lowercase();
    for token in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.len() < 2 {
            continue;
        }
        if let Some(v) = token.strip_suffix('k').and_then(|n| n.parse::<u64>().ok())
            && v > 0
        {
            return Some(v.saturating_mul(1_000));
        }
        if let Some(v) = token.strip_suffix('m').and_then(|n| n.parse::<u64>().ok())
            && v > 0
        {
            return Some(v.saturating_mul(1_000_000));
        }
    }
    None
}

pub(crate) fn model_context_window(model: &str) -> u64 {
    if let Ok(raw) = std::env::var("WOLF_CONTEXT_WINDOW")
        .or_else(|_| std::env::var("WOLF_CONTEXT_WINDOW_TOKENS"))
        && let Ok(v) = raw.trim().parse::<u64>()
        && v > 0
    {
        return v;
    }

    if let Some(hint) = parse_model_context_hint_tokens(model) {
        return hint;
    }

    // Resolution order (first match wins):
    //   1. WOLF_CONTEXT_WINDOW / WOLF_CONTEXT_WINDOW_TOKENS env var (handled above).
    //   2. Model-name suffix hint ("-128k", "-1m", etc).
    //   3. Provider-catalog override: providers.json → profile.context_window
    //      OR profile.model_context_windows[model].
    //   4. Built-in family heuristics keyed by substring match.
    //   5. Hard fallback (200_000).
    // All of 3-5 are overridable by the user without a rebuild.
    if let Some(tokens) = provider_catalog_context_window(model) {
        return tokens;
    }
    if let Some(tokens) = builtin_family_context_window(model) {
        return tokens;
    }
    if let Some(tokens) = builtin_profile_context_window(model) {
        return tokens;
    }
    200_000
}

fn provider_catalog_context_window(model: &str) -> Option<u64> {
    let normalized_model = model.trim().to_ascii_lowercase();
    let catalog = load_provider_catalog().ok()?;
    if let Some(tokens) = context_window_from_profiles(&catalog.providers, &normalized_model, true)
    {
        return Some(tokens);
    }
    context_window_from_profiles(&catalog.providers, &normalized_model, false)
}

fn context_window_from_profiles(
    profiles: &[ProviderProfile],
    normalized_model: &str,
    per_model_only: bool,
) -> Option<u64> {
    for profile in profiles {
        let profile_knows_model = profile
            .models
            .iter()
            .any(|m: &String| m.trim().eq_ignore_ascii_case(normalized_model))
            || profile
                .default_model
                .trim()
                .eq_ignore_ascii_case(normalized_model);
        if !profile_knows_model {
            continue;
        }
        if let Some(per_model) = profile.model_context_windows.get(normalized_model)
            && *per_model > 0
        {
            return Some(*per_model);
        }
        if per_model_only {
            continue;
        }
        if let Some(default) = profile.context_window
            && default > 0
        {
            return Some(default);
        }
    }
    None
}

fn builtin_profile_context_window(model: &str) -> Option<u64> {
    let normalized_model = model.trim().to_ascii_lowercase();
    let profiles = built_in_provider_profiles();
    context_window_from_profiles(&profiles, &normalized_model, false)
}

fn builtin_family_context_window(model: &str) -> Option<u64> {
    // Thin heuristic for families where naming reliably implies the window.
    // Always overridable via env var or provider catalog (see model_context_window).
    let m = model.to_ascii_lowercase();
    if m.contains("gpt-4.1") {
        return Some(1_000_000);
    }
    if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        return Some(128_000);
    }
    if m.contains("claude")
        || m.contains("sonnet")
        || m.contains("haiku")
        || m.contains("opus")
        || m.contains("glm")
    {
        return Some(200_000);
    }
    None
}

fn compact_threshold_chars_for_percent(model: &str, percent: u8) -> usize {
    let window = usize::try_from(model_context_window(model)).unwrap_or(usize::MAX);
    window
        .saturating_mul(percent.clamp(1, 100) as usize)
        .saturating_div(100)
        .max(HISTORY_CHAR_BUDGET_MIN)
}

fn history_char_budget_with_percent(
    model: &str,
    override_chars: Option<usize>,
    context_mode: ContextMode,
    percent: u8,
) -> usize {
    if let Some(v) = override_chars.filter(|v| *v > 0) {
        return v;
    }
    if let Ok(raw) = std::env::var("WOLF_MAX_HISTORY_CHARS")
        && let Ok(v) = raw.trim().parse::<usize>()
        && v > 0
    {
        return v;
    }
    if context_mode.is_frugal() {
        return 60_000;
    }

    compact_threshold_chars_for_percent(model, percent)
}

fn history_char_budget_with_override(
    model: &str,
    override_chars: Option<usize>,
    context_mode: ContextMode,
) -> usize {
    history_char_budget_with_percent(
        model,
        override_chars,
        context_mode,
        HISTORY_CHAR_BUDGET_END_TURN_PERCENT,
    )
}

fn active_history_char_budget_with_override(
    model: &str,
    override_chars: Option<usize>,
    context_mode: ContextMode,
) -> usize {
    history_char_budget_with_percent(
        model,
        override_chars,
        context_mode,
        HISTORY_CHAR_BUDGET_ACTIVE_PERCENT,
    )
}

fn compact_threshold_settings_path() -> PathBuf {
    wolf_state_dir().join("settings.json")
}

fn load_compact_threshold_percent_setting() -> Option<u8> {
    let text = std::fs::read_to_string(compact_threshold_settings_path()).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    json["compact_threshold_percent"]
        .as_u64()
        .and_then(|v| u8::try_from(v).ok())
        .filter(|v| (1..=100).contains(v))
}

fn save_compact_threshold_percent_setting(percent: Option<u8>) -> Result<()> {
    let path = compact_threshold_settings_path();
    let mut json = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    if !json.is_object() {
        json = json!({});
    }
    if let Some(percent) = percent.filter(|v| (1..=100).contains(v)) {
        json["compact_threshold_percent"] = json!(percent);
    } else if let Some(obj) = json.as_object_mut() {
        obj.remove("compact_threshold_percent");
    }
    let bytes = serde_json::to_vec_pretty(&json)?;
    atomic_write_bytes(&path, &bytes)?;
    Ok(())
}

pub(crate) fn git_summary(root: &Path) -> Option<String> {
    let branch_out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !branch_out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_string();
    let status_out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    let dirty = !status_out.stdout.is_empty();
    Some(format!("{branch}{}", if dirty { " (dirty)" } else { "" }))
}

struct Agent {
    client: Arc<OnceLock<reqwest::Client>>,
    provider_id: String,
    api_key: String,
    key_source: String,
    provider_requires_api_key: bool,
    base_url: String,
    model: String,
    api_provider: ApiProvider,
    thinking_effort: ThinkingEffort,
    system: String,
    history: Vec<Message>,
    tools: Vec<Tool>,
    allowed: HashSet<String>,
    deny_tools: HashSet<String>,
    sandbox_root: PathBuf,
    git_context: Option<String>,
    silent: bool,
    pretty: bool,
    max_iterations: Option<u32>,
    session_usage: Usage,
    interrupt: Arc<AtomicBool>,
    hooks: Hooks,
    state_lock: Option<Arc<ProjectStateLock>>,
    session_enabled: bool,
    latest_session_path: PathBuf,
    latest_log_path: PathBuf,
    pending_login_provider: Option<String>,
    // Subagents share the parent's latest_session_path; without this guard,
    // the child would clobber the parent's auto-saved session on every turn,
    // so `--resume` after a crash would restore the subagent's transcript.
    suppress_checkpoints: bool,
    last_checkpoint_at: Option<std::time::Instant>,
    session_model_pins: HashMap<String, String>,
    partial_stream_text: Option<String>,
    compact_threshold_chars: Option<usize>,
    compact_threshold_percent: Option<u8>,
    approval_profile: ApprovalProfile,
    sandbox_profile: SandboxProfile,
    browser_recipe: BrowserRecipe,
    context_mode: ContextMode,
    tool_profile: ToolProfile,
    budget_cap: Option<BudgetCap>,
    budget_exhausted: bool,
    // Caps concurrent builtin tool execution. Model may request 20 read_files at once; without
    // a cap, the runtime spawns 20 tasks that all contend for disk/CPU and open-file limits.
    // Default 8; override via WOLF_MAX_CONCURRENT_BUILTINS=N.
    builtin_semaphore: Arc<tokio::sync::Semaphore>,
    sink: Box<dyn EventSink>,
    steering_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    steering_tx: tokio::sync::mpsc::UnboundedSender<String>,
    read_cache: Arc<Mutex<ReadFileCache>>,
    work_ledger: WorkLedger,
    provider_health: ProviderHealthLedger,
    track_origin: Option<TrackOrigin>,
    privacy: PrivacyPolicy,
}

impl Agent {
    fn new() -> Result<Self> {
        Self::new_with_sandbox(None, true)
    }

    pub(crate) fn new_with_sandbox(
        sandbox: Option<PathBuf>,
        session_enabled: bool,
    ) -> Result<Self> {
        let resolved = resolve_runtime_provider(None, false)?;
        let provider_id = resolved.profile.id.clone();
        let api_key = resolved.api_key;
        let key_source = resolved.key_source;
        let provider_requires_api_key = resolved.requires_api_key;
        let api_provider = resolved.profile.api_provider;
        let base_url = resolved.base_url;
        let model = resolved.model;

        // If resolve_runtime_provider auto-rerouted away from a stale active
        // provider (e.g. chatgpt whose stored default_model actually belongs to
        // glm), heal the catalog on disk so the user doesn't rely on the
        // in-memory reroute every launch. Best-effort — ignore write errors so
        // startup still succeeds on read-only filesystems.
        if let Ok(catalog) = load_provider_catalog()
            && canonical_provider_id(&resolve_active_provider_id(&catalog))
                != canonical_provider_id(&provider_id)
        {
            let _ = set_active_provider_in_catalog(&provider_id);
            let _ = set_provider_default_model_in_catalog(&provider_id, &model);
        }

        let thinking_effort = std::env::var("WOLF_THINKING_EFFORT")
            .ok()
            .and_then(|v| ThinkingEffort::parse(&v))
            .unwrap_or_default();
        let system = std::env::var("WOLF_SYSTEM")
            .ok()
            .and_then(|v| {
                // Support `@path/to/file` to load system prompt from disk
                if let Some(path) = v.strip_prefix('@') {
                    std::fs::read_to_string(path).ok()
                } else {
                    Some(v)
                }
            })
            .unwrap_or_else(|| DEFAULT_SYSTEM.to_string());
        let sandbox_root = std::fs::canonicalize(sandbox.unwrap_or_else(|| {
            PathBuf::from(std::env::var("WOLF_SANDBOX").unwrap_or_else(|_| ".".to_string()))
        }))
        .context("could not canonicalize sandbox root")?;
        let latest_session = latest_session_path(&sandbox_root);
        let latest_log = latest_log_path(&sandbox_root);
        record_crash_session_id(&latest_session);
        let state_lock = if session_enabled {
            Some(Arc::new(ProjectStateLock::acquire(&sandbox_root)?))
        } else {
            None
        };

        let pretty = io::stdout().is_terminal();
        let git_context = git_summary(&sandbox_root);
        let browser_recipe = std::env::var("WOLF_BROWSER_RECIPE")
            .ok()
            .and_then(|v| BrowserRecipe::parse(&v))
            .unwrap_or_default();
        let context_mode = ContextMode::from_env();
        let tool_profile = if context_mode.is_frugal() {
            ToolProfile::Lean
        } else {
            ToolProfile::from_env()
        };
        let budget_cap = BudgetCap::from_env();
        let tool_context_profile = if context_mode.is_frugal() {
            ToolContextProfile::Lean
        } else {
            ToolContextProfile::Full
        };
        let tools: Vec<Tool> = tool_definitions()
            .into_iter()
            .filter(|t| t.name != "subagent")
            .filter(|t| browser_recipe == BrowserRecipe::AgentBrowser || t.name != "browser")
            .filter(|t| tool_name_allowed_in_profile(t.name, tool_context_profile))
            .collect();
        let compact_threshold_percent = load_compact_threshold_percent_setting();
        let compact_threshold_chars = compact_threshold_percent
            .map(|percent| compact_threshold_chars_for_percent(&model, percent));
        Ok(Self {
            client: Arc::new(OnceLock::new()),
            provider_id,
            api_key,
            key_source,
            provider_requires_api_key,
            base_url,
            model,
            api_provider,
            thinking_effort,
            system,
            history: Vec::new(),
            tools,
            allowed: HashSet::from(["subagent".to_string()]),
            deny_tools: HashSet::new(),
            hooks: Hooks::load(&sandbox_root),
            sandbox_root,
            git_context,
            silent: false,
            pretty,
            max_iterations: None,
            session_usage: Usage::default(),
            interrupt: Arc::new(AtomicBool::new(false)),
            state_lock,
            session_enabled,
            latest_session_path: latest_session,
            latest_log_path: latest_log,
            pending_login_provider: None,
            suppress_checkpoints: false,
            last_checkpoint_at: None,
            session_model_pins: HashMap::new(),
            partial_stream_text: None,
            compact_threshold_chars,
            compact_threshold_percent,
            approval_profile: ApprovalProfile::default(),
            sandbox_profile: SandboxProfile::default(),
            browser_recipe,
            context_mode,
            tool_profile,
            budget_cap,
            budget_exhausted: false,
            builtin_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent_builtins())),
            sink: Box::new(ConsoleSink::new(pretty, false)),
            steering_rx: None,
            steering_tx: Self::noop_steering_tx(),
            read_cache: Arc::new(Mutex::new(ReadFileCache::default())),
            work_ledger: WorkLedger::default(),
            provider_health: ProviderHealthLedger::default(),
            track_origin: None,
            privacy: PrivacyPolicy::from_env(),
        })
    }

    fn noop_steering_tx() -> tokio::sync::mpsc::UnboundedSender<String> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tx
    }

    fn steering_sender(&self) -> tokio::sync::mpsc::UnboundedSender<String> {
        self.steering_tx.clone()
    }

    fn install_steering(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        self.steering_rx = Some(rx);
        self.steering_tx = tx;
    }

    fn drain_steering(&mut self) -> Vec<String> {
        let mut messages = Vec::new();
        if let Some(rx) = &mut self.steering_rx {
            while let Ok(msg) = rx.try_recv() {
                messages.push(msg);
            }
        }
        messages
    }

    fn set_sink(&mut self, sink: Box<dyn EventSink>) {
        self.sink = sink;
    }

    fn apply_runtime_provider(&mut self, resolved: ResolvedProviderConfig) {
        self.provider_id = resolved.profile.id;
        self.api_provider = resolved.profile.api_provider;
        self.api_key = resolved.api_key;
        self.key_source = resolved.key_source;
        self.provider_requires_api_key = resolved.requires_api_key;
        self.base_url = resolved.base_url;
        self.model = resolved.model;
        if let Some(pinned) = self
            .session_model_pins
            .get(&canonical_provider_id(&self.provider_id))
        {
            self.model = pinned.clone();
        }
    }

    fn pin_model_for_provider(&mut self, provider_id: &str, model: &str) {
        let provider_id = canonical_provider_id(provider_id);
        if model.trim().is_empty() {
            self.session_model_pins.remove(&provider_id);
        } else {
            self.session_model_pins
                .insert(provider_id, model.trim().to_string());
        }
    }

    fn reload_provider(&mut self, selected: Option<&str>, require_credentials: bool) -> Result<()> {
        let resolved = resolve_runtime_provider(selected, require_credentials)?;
        self.apply_runtime_provider(resolved);
        Ok(())
    }

    fn emit_runtime_provider_state(&mut self) {
        self.sink.emit(AgentEvent::TurnDiagnostics {
            provider: self.provider_id.clone(),
            api_family: api_family_label(self.api_provider).to_string(),
            auth_source: self.key_source.clone(),
            model: self.model.clone(),
            last_retry_reason: None,
            workaround_fired: false,
            turn_duration_ms: None,
            context_mode: Some(self.context_mode),
            tool_profile: Some(self.tool_profile.as_str().to_string()),
            compacted: None,
        });
    }

    fn provider_status_line(&self) -> String {
        format!(
            "provider={} api={} model={} auth={} base={}",
            self.provider_id,
            self.api_provider.as_str(),
            self.model,
            self.key_source,
            self.base_url
        )
    }

    fn api_family_label(&self) -> &'static str {
        api_family_label(self.api_provider)
    }

    fn set_pending_login_provider(&mut self, provider: Option<String>) {
        self.pending_login_provider = provider;
    }

    fn clear_pending_login(&mut self) -> Option<String> {
        self.pending_login_provider.take()
    }

    fn try_consume_pending_login_input(&mut self, raw: &str) -> Result<Option<String>> {
        let provider = match self.pending_login_provider.clone() {
            Some(provider) => provider,
            None if self.provider_requires_api_key
                && self.key_source.starts_with("missing")
                && (looks_like_login_secret_input(raw)
                    || extract_oauth_code_from_callback(raw).is_some()) =>
            {
                self.provider_id.clone()
            }
            None => return Ok(None),
        };

        if let Some(msg) = try_complete_oauth_from_callback(raw)? {
            self.pending_login_provider = None;
            self.reload_provider(None, false)?;
            return Ok(Some(format!(
                "{msg}\nactive -> {}",
                self.provider_status_line()
            )));
        }

        if !looks_like_login_secret_input(raw) {
            return Ok(Some(format!(
                "waiting for {} credentials — paste the callback URL, authorization code, access token, or full session JSON, or run /login cancel",
                provider
            )));
        }

        let login = login_provider(Some(&provider), Some(raw), false)?;
        self.pending_login_provider = None;
        self.reload_provider(None, false)?;
        Ok(Some(format!(
            "{}\nactive -> {}",
            login.message,
            self.provider_status_line()
        )))
    }

    fn set_fangs_out(&mut self, on: bool) -> usize {
        let profile = if on {
            ApprovalProfile::Always
        } else {
            ApprovalProfile::Ask
        };
        self.set_approval_profile(profile)
    }

    fn set_approval_profile(&mut self, profile: ApprovalProfile) -> usize {
        self.approval_profile = profile;
        let gated: Vec<String> = self
            .tools
            .iter()
            .filter(|t| needs_permission(t.name))
            .map(|t| t.name.to_string())
            .collect();
        let mut changed = 0usize;
        for tool in gated {
            let did_change = match profile {
                ApprovalProfile::Always => self.allowed.insert(tool),
                _ => self.allowed.remove(&tool),
            };
            if did_change {
                changed += 1;
            }
        }
        changed
    }

    fn set_sandbox_profile(&mut self, profile: SandboxProfile) {
        self.sandbox_profile = profile;
    }

    fn set_budget_cap(&mut self, cap: Option<BudgetCap>) {
        self.budget_cap = cap;
        self.budget_exhausted = false;
    }

    fn update_work_ledger_from_objective(&mut self, objective: &orchestrator::ObjectiveTracker) {
        self.work_ledger.objective = objective.summary.clone();
        self.work_ledger.current_phase = "probe".to_string();
        self.work_ledger.pending = objective.checkpoints.clone();
        self.work_ledger.in_progress.clear();
        self.work_ledger.blocked.clear();
        self.work_ledger.next_actions = objective.checkpoints.iter().take(6).cloned().collect();
    }

    fn set_work_phase(&mut self, phase: &str) {
        self.work_ledger.current_phase = phase.to_string();
    }

    fn mark_work_done(&mut self, item: &str) {
        if !self.work_ledger.done.iter().any(|v| v == item) {
            self.work_ledger.done.push(item.to_string());
        }
        self.work_ledger.pending.retain(|v| v != item);
        self.work_ledger.in_progress.retain(|v| v != item);
    }

    fn set_browser_recipe(&mut self, recipe: BrowserRecipe) {
        self.browser_recipe = recipe;
        self.refresh_tools_for_context();
        if recipe == BrowserRecipe::AgentBrowser && self.approval_profile == ApprovalProfile::Always
        {
            self.allowed.insert("browser".to_string());
        }
        if recipe != BrowserRecipe::AgentBrowser {
            self.allowed.remove("browser");
            self.deny_tools.remove("browser");
        }
    }

    pub(crate) fn fangs_out_active(&self) -> bool {
        self.approval_profile == ApprovalProfile::Always
    }

    pub(crate) fn approval_profile(&self) -> ApprovalProfile {
        self.approval_profile
    }

    pub(crate) fn sandbox_profile(&self) -> SandboxProfile {
        self.sandbox_profile
    }

    pub(crate) fn browser_recipe(&self) -> BrowserRecipe {
        self.browser_recipe
    }

    pub(crate) fn thinking_effort(&self) -> ThinkingEffort {
        self.thinking_effort
    }

    fn compact_threshold_chars(&self) -> usize {
        history_char_budget_with_override(
            &self.model,
            self.compact_threshold_chars,
            self.context_mode,
        )
    }

    fn active_compact_threshold_chars(&self) -> usize {
        active_history_char_budget_with_override(
            &self.model,
            self.compact_threshold_chars,
            self.context_mode,
        )
    }

    fn compact_threshold_override(&self) -> Option<usize> {
        self.compact_threshold_chars
    }

    fn compact_threshold_override_percent(&self) -> Option<u8> {
        self.compact_threshold_percent
    }

    fn set_compact_threshold_auto(&mut self) {
        self.compact_threshold_chars = None;
        self.compact_threshold_percent = None;
        let _ = save_compact_threshold_percent_setting(None);
    }

    fn set_compact_threshold_percent(&mut self, percent: u8) -> usize {
        let percent = percent.clamp(1, 100);
        self.compact_threshold_chars =
            Some(compact_threshold_chars_for_percent(&self.model, percent));
        self.compact_threshold_percent = Some(percent);
        let _ = save_compact_threshold_percent_setting(Some(percent));
        self.compact_threshold_chars()
    }

    fn set_thinking_effort(&mut self, effort: ThinkingEffort) -> bool {
        if self.thinking_effort == effort {
            return false;
        }
        self.thinking_effort = effort;
        true
    }

    fn cycle_thinking_effort(&mut self, step: i8) -> ThinkingEffort {
        self.thinking_effort = self.thinking_effort.cycle(step);
        self.thinking_effort
    }

    fn http_client(&self) -> &reqwest::Client {
        self.client.get_or_init(reqwest::Client::new)
    }

    async fn interrupt_aware_sleep(&self, secs: u64) {
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(secs));
        tokio::pin!(sleep);
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(25));
        loop {
            if self.interrupt.load(Ordering::SeqCst) {
                return;
            }
            tokio::select! {
                _ = &mut sleep => return,
                _ = ticker.tick() => {}
            }
        }
    }

    fn refresh_state_paths(&mut self) {
        self.latest_session_path = latest_session_path(&self.sandbox_root);
        self.latest_log_path = latest_log_path(&self.sandbox_root);
    }

    fn set_sandbox_root(&mut self, root: PathBuf) -> Result<()> {
        let next_lock_path = project_state_lock_path(&root);
        let same_project = self
            .state_lock
            .as_ref()
            .is_some_and(|lock| lock.path == next_lock_path);
        let next_lock = if !self.session_enabled || same_project {
            None
        } else {
            Some(Arc::new(ProjectStateLock::acquire(&root)?))
        };

        self.sandbox_root = root;
        self.hooks = Hooks::load(&self.sandbox_root);
        self.git_context = git_summary(&self.sandbox_root);
        self.refresh_state_paths();
        record_crash_session_id(&self.latest_session_path);
        if let Some(lock) = next_lock {
            self.state_lock = Some(lock);
        }
        Ok(())
    }

    fn browser_recipe_hint(&self) -> Option<String> {
        if self.browser_recipe != BrowserRecipe::AgentBrowser {
            return None;
        }
        if binary_on_path("agent-browser") {
            Some("browser recipe enabled: use bash with agent-browser (start with `agent-browser skills get core --full`) for browser automation when useful.".to_string())
        } else {
            Some("browser recipe requested, but agent-browser is not on PATH; install it or disable with /browser off.".to_string())
        }
    }

    fn tool_auto_approved(&self, name: &str, input: &Value) -> bool {
        if self.allowed.contains(name) {
            return true;
        }
        match self.approval_profile {
            ApprovalProfile::Always => true,
            ApprovalProfile::AutoRead => {
                tool_policy::classify_command_risk(name, input) == tool_policy::CommandRisk::Read
            }
            ApprovalProfile::AutoWrite => {
                tool_policy::classify_command_risk(name, input) != tool_policy::CommandRisk::Danger
            }
            ApprovalProfile::Ask | ApprovalProfile::Never => false,
        }
    }

    fn sandbox_policy_denial(&self, name: &str, input: &Value) -> Option<String> {
        if name == "browser" {
            if self.browser_recipe != BrowserRecipe::AgentBrowser {
                return Some("browser tool is disabled. Enable with /browser agent-browser or --browser agent-browser, or use bash with explicit agent-browser commands.".to_string());
            }
            if !binary_on_path("agent-browser") {
                return Some("agent-browser is not on PATH; install it or disable browser recipe with /browser off.".to_string());
            }
            return None;
        }
        let risk = tool_policy::classify_command_risk(name, input);
        match self.sandbox_profile {
            SandboxProfile::DangerFullAccess | SandboxProfile::WorkspaceWrite => None,
            SandboxProfile::ReadOnly => {
                (risk != tool_policy::CommandRisk::Read).then(|| {
                    format!(
                        "sandbox profile read-only blocks {name} ({}) — switch with /sandbox-profile workspace-write or /sandbox-profile danger-full-access if you want writes",
                        risk.label()
                    )
                })
            }
        }
    }

    fn budget_cap_denial(&mut self) -> Option<String> {
        let cap = self.budget_cap?;
        let msg = cap.exceeded(self.session_usage)?;
        self.budget_exhausted = true;
        Some(format!(
            "{msg}; refusing another model request. Raise/clear with /budget <cap|off> or restart with --budget. Current usage: {}",
            self.session_usage.line()
        ))
    }

    fn update_budget_state_after_usage(&mut self) -> Option<String> {
        let cap = self.budget_cap?;
        let msg = cap.exceeded(self.session_usage)?;
        if self.budget_exhausted {
            return None;
        }
        self.budget_exhausted = true;
        Some(format!(
            "{msg}; this turn will stop before another model request. Current usage: {}",
            self.session_usage.line()
        ))
    }

    fn append_latest_log(&self, event: &str, detail: &str) {
        if !self.session_enabled {
            return;
        }
        let detail = self.privacy.redact_log_detail(detail);
        session::append_latest_log(&self.sandbox_root, event, &detail);
    }

    fn compose_system_parts(&self) -> (String, String) {
        let mut stable = self.system.clone();
        let mut context_budget = if self.context_mode.is_frugal() {
            FRUGAL_PROJECT_CONTEXT_CAP
        } else {
            PROJECT_CONTEXT_CAP
        };
        let mut wolf_md_sections: Vec<(String, String)> = Vec::new();
        let mut dir = self.sandbox_root.as_path();
        loop {
            let candidate = dir.join("WOLF.md");
            if candidate.exists()
                && let Ok(content) = std::fs::read_to_string(&candidate)
            {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    let display = dir
                        .strip_prefix(&self.sandbox_root)
                        .unwrap_or(dir)
                        .display();
                    let label = if display.to_string().is_empty() {
                        ".".to_string()
                    } else {
                        format!("/{display}")
                    };
                    wolf_md_sections.push((label, trimmed.to_string()));
                }
            }
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent,
                _ => break,
            }
        }
        wolf_md_sections.reverse();
        for (label, content) in &wolf_md_sections {
            if context_budget == 0 {
                break;
            }
            let section = format!("\n\n## Project context (WOLF.md from {label})\n{}", content);
            if section.len() <= context_budget {
                stable.push_str(&section);
                context_budget -= section.len();
            } else {
                let remaining = cap_bytes_with_hint(
                    content.clone(),
                    context_budget.saturating_sub(60),
                    "WOLF.md truncated; keep only the most important project guidance here.",
                );
                stable.push_str(&format!(
                    "\n\n## Project context (WOLF.md from {label})\n{remaining}"
                ));
                context_budget = 0;
                break;
            }
        }

        let mut memory_sections: Vec<(String, String)> = Vec::new();
        let mut dir = self.sandbox_root.as_path();
        loop {
            let candidate = dir.join("WOLF.memory.md");
            if candidate.exists()
                && let Ok(content) = std::fs::read_to_string(&candidate)
            {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    let display = dir
                        .strip_prefix(&self.sandbox_root)
                        .unwrap_or(dir)
                        .display();
                    let label = if display.to_string().is_empty() {
                        ".".to_string()
                    } else {
                        format!("/{display}")
                    };
                    memory_sections.push((label, trimmed.to_string()));
                }
            }
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent,
                _ => break,
            }
        }
        memory_sections.reverse();
        for (label, content) in &memory_sections {
            if context_budget == 0 {
                break;
            }
            let section = format!(
                "\n\n## Persistent memory (WOLF.memory.md from {label})\n{}",
                content
            );
            if section.len() <= context_budget {
                stable.push_str(&section);
                context_budget -= section.len();
            } else {
                let remaining = cap_bytes_with_hint(
                    content.clone(),
                    context_budget.saturating_sub(60),
                    "WOLF.memory.md truncated; keep durable facts concise.",
                );
                stable.push_str(&format!(
                    "\n\n## Persistent memory (WOLF.memory.md from {label})\n{remaining}"
                ));
                break;
            }
        }

        let mut env = String::from("## Environment\n");
        if self.context_mode.is_frugal() {
            if let Some(git) = &self.git_context {
                env.push_str(&format!(
                    "cwd={} os={} git={} provider={} model={} effort={} context={} toolset={} schemas={} approval={} sandbox={}\n",
                    self.sandbox_root.display(),
                    std::env::consts::OS,
                    git,
                    self.provider_id,
                    self.model,
                    self.thinking_effort.as_str(),
                    self.context_mode.as_str(),
                    self.tool_context_profile().as_str(),
                    self.wire_tool_profile().as_str(),
                    self.approval_profile.as_str(),
                    self.sandbox_profile.as_str()
                ));
            } else {
                env.push_str(&format!(
                    "cwd={} os={} provider={} model={} effort={} context={} toolset={} schemas={} approval={} sandbox={}\n",
                    self.sandbox_root.display(),
                    std::env::consts::OS,
                    self.provider_id,
                    self.model,
                    self.thinking_effort.as_str(),
                    self.context_mode.as_str(),
                    self.tool_context_profile().as_str(),
                    self.wire_tool_profile().as_str(),
                    self.approval_profile.as_str(),
                    self.sandbox_profile.as_str()
                ));
            }
            env.push_str(&format!(
                "history_compact_threshold_chars={} active_history_compact_threshold_chars={}\n",
                self.compact_threshold_chars(),
                self.active_compact_threshold_chars()
            ));
            let ledger = self.work_ledger_prompt();
            if !ledger.trim().is_empty() {
                env.push_str("\n## Work ledger\n");
                env.push_str(&cap_bytes_with_hint(
                    ledger,
                    1_200,
                    "work ledger trimmed for frugal context.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
            let health = self.provider_health_prompt();
            if !health.trim().is_empty() {
                env.push_str("\n## Provider health\n");
                env.push_str(&cap_bytes_with_hint(
                    health,
                    600,
                    "provider health trimmed for frugal context.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
            if let Some(cap) = self.budget_cap {
                env.push_str(&format!("budget_cap={}\n", cap.line()));
            }
            if self.browser_recipe != BrowserRecipe::Disabled {
                env.push_str(&format!(
                    "browser_recipe={}\n",
                    self.browser_recipe.as_str()
                ));
            }
            env.push_str(&self.privacy.prompt_status_line());
            env.push('\n');
            return (stable, env);
        }

        let mut env = String::from("## Environment\n");
        if let Some(git) = &self.git_context {
            env.push_str(&format!(
                "cwd={} os={} git={} provider={} model={} effort={} context={} toolset={} schemas={} approval={} sandbox={}\n",
                self.sandbox_root.display(),
                std::env::consts::OS,
                git,
                self.provider_id,
                self.model,
                self.thinking_effort.as_str(),
                self.context_mode.as_str(),
                self.tool_context_profile().as_str(),
                self.wire_tool_profile().as_str(),
                self.approval_profile.as_str(),
                self.sandbox_profile.as_str()
            ));
        } else {
            env.push_str(&format!(
                "cwd={} os={} provider={} model={} effort={} context={} toolset={} schemas={} approval={} sandbox={}\n",
                self.sandbox_root.display(),
                std::env::consts::OS,
                self.provider_id,
                self.model,
                self.thinking_effort.as_str(),
                self.context_mode.as_str(),
                self.tool_context_profile().as_str(),
                self.wire_tool_profile().as_str(),
                self.approval_profile.as_str(),
                self.sandbox_profile.as_str()
            ));
        }
        env.push_str(&format!(
            "history_compact_threshold_chars={} active_history_compact_threshold_chars={}\n",
            self.compact_threshold_chars(),
            self.active_compact_threshold_chars()
        ));
        let ledger = self.work_ledger_prompt();
        if !ledger.trim().is_empty() {
            env.push_str("\n## Work ledger\n");
            env.push_str(&cap_bytes_with_hint(
                ledger,
                2_500,
                "work ledger trimmed for prompt budget.",
            ));
            if !env.ends_with('\n') {
                env.push('\n');
            }
        }
        let health = self.provider_health_prompt();
        if !health.trim().is_empty() {
            env.push_str("\n## Provider health\n");
            env.push_str(&cap_bytes_with_hint(
                health,
                800,
                "provider health trimmed for prompt budget.",
            ));
            if !env.ends_with('\n') {
                env.push('\n');
            }
        }
        if let Some(cap) = self.budget_cap {
            env.push_str(&format!("budget_cap={}\n", cap.line()));
        }
        if self.browser_recipe != BrowserRecipe::Disabled {
            env.push_str(&format!(
                "browser_recipe={}\n",
                self.browser_recipe.as_str()
            ));
        }
        env.push_str(&self.privacy.prompt_status_line());
        env.push('\n');
        (stable, env)
    }

    fn work_ledger_prompt(&self) -> String {
        render_work_ledger_prompt(&self.work_ledger)
    }

    fn provider_health_prompt(&self) -> String {
        render_provider_health_prompt(&self.provider_health)
    }

    fn cleaned_work_ledger(&self) -> WorkLedger {
        let mut ledger = self.work_ledger.clone();
        ledger.files_changed.retain(|path| {
            let p = Path::new(path);
            !p.is_absolute() && !path.starts_with(".wolf/") && !path.starts_with(".pi/")
        });
        if ledger.pending.is_empty() && ledger.in_progress.is_empty() && !ledger.done.is_empty() {
            ledger.next_actions.clear();
            if matches!(
                ledger.current_phase.as_str(),
                "probe" | "execute" | "verify" | "synthesize"
            ) {
                ledger.current_phase = "done".to_string();
            }
        }
        ledger
    }

    fn work_ledger_note_file_change(&mut self, input: &Value) {
        let Some(path) = input["path"].as_str() else {
            return;
        };
        let p = Path::new(path);
        if p.is_absolute() || path.starts_with(".wolf/") || path.starts_with(".pi/") {
            return;
        }
        if !self.work_ledger.files_changed.iter().any(|p| p == path) {
            self.work_ledger.files_changed.push(path.to_string());
        }
    }

    fn unresolved_steering_items(&self) -> Vec<String> {
        self.work_ledger
            .steering
            .iter()
            .filter(|item| !steering_item_acknowledged(item, &self.history))
            .cloned()
            .collect()
    }

    fn note_steering_messages(&mut self, messages: &[String]) -> String {
        if messages.is_empty() {
            return String::new();
        }
        let combined = messages.join("\n\n");
        let preview = summarize_inline(&combined, 180);
        let entry = format!(
            "queued during active turn ({} message{}): {}",
            messages.len(),
            if messages.len() == 1 { "" } else { "s" },
            preview
        );
        if !self.work_ledger.steering.iter().any(|item| item == &entry) {
            self.work_ledger.steering.push(entry);
        }
        if self.work_ledger.steering.len() > 8 {
            let excess = self.work_ledger.steering.len() - 8;
            self.work_ledger.steering.drain(0..excess);
        }
        if !self
            .work_ledger
            .pending
            .iter()
            .any(|item| item == "respond to queued user update")
            && !self
                .work_ledger
                .done
                .iter()
                .any(|item| item == "respond to queued user update")
        {
            self.work_ledger
                .pending
                .push("respond to queued user update".to_string());
        }
        preview
    }

    fn inject_queued_steering(
        &mut self,
        turn_state: &mut orchestrator::TurnRuntimeState,
        iterations: u32,
        tool_count: usize,
        before_final: bool,
    ) -> bool {
        let mut runtime_control_notes = Vec::new();
        let mut pending_steering = Vec::new();
        let steering_messages = self.drain_steering();
        let steering_count = steering_messages.len();
        for message in steering_messages {
            if apply_runtime_control_command(self, &message, |msg| runtime_control_notes.push(msg))
            {
                continue;
            }
            pending_steering.push(message);
        }
        if pending_steering.is_empty() && runtime_control_notes.is_empty() {
            return false;
        }
        let preview = if pending_steering.is_empty() {
            summarize_inline(&runtime_control_notes.join("; "), 180)
        } else {
            self.note_steering_messages(&pending_steering)
        };
        let control_note = if runtime_control_notes.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nRuntime control applied immediately:\n{}",
                runtime_control_notes.join("\n")
            )
        };
        let combined = pending_steering.join("\n\n");
        let user_update = if combined.trim().is_empty() {
            "(runtime control command only)".to_string()
        } else {
            combined
        };
        let progress = format!(
            "[queued-user-update] The user sent this while you were working. This is active scope, not an aside. \
             You must explicitly address it in your next assistant response and say what changed, what you did about it, or why it is blocked. \
             If it adds/removes work, update any active todo list before continuing. Do not let the final answer omit this queued update.\n\n\
             Progress: completed {iterations} iterations, {tool_count} tool calls so far. \
             Phase: {}. Injection point: {}.\n\
             User update:\n{user_update}{control_note}",
            turn_state.phase().label(),
            if before_final {
                "before final response"
            } else {
                "after tool results"
            }
        );
        self.sink.emit(AgentEvent::SteeringReceived {
            messages: steering_count,
            preview: preview.clone(),
        });
        self.append_latest_log(
            "steering_injected",
            &format!("messages={steering_count} preview={preview}"),
        );
        self.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text { text: progress }],
        });
        self.checkpoint_latest_session("after_steering");
        if let Some((_, msg)) = turn_state.advance_phase(orchestrator::PhaseTrigger::Steering) {
            self.set_work_phase(turn_state.phase().label());
            self.sink.emit(AgentEvent::Info(format!(
                "[phase:{}] {msg}",
                turn_state.phase().label()
            )));
        }
        true
    }

    fn record_provider_http_failure(
        &mut self,
        status: reqwest::StatusCode,
        text: &str,
        retry_after: Option<u64>,
    ) {
        let state = self
            .provider_health
            .providers
            .entry(self.provider_id.clone())
            .or_default();
        state.auth = if matches!(status.as_u16(), 401 | 403) {
            "failed".to_string()
        } else {
            "present".to_string()
        };
        state.last_error = Some(format!("HTTP {status}: {}", summarize_inline(text, 220)));
        state.retry_after = retry_after;
        state.mode = Some(api_family_label(self.api_provider).to_string());
        state.disabled_for_turn = matches!(status.as_u16(), 401 | 403 | 429);
    }

    fn record_provider_stream_failure(&mut self, text: &str) {
        let state = self
            .provider_health
            .providers
            .entry(self.provider_id.clone())
            .or_default();
        state.auth = "present".to_string();
        state.last_error = Some(summarize_inline(text, 260));
        state.mode = Some(api_family_label(self.api_provider).to_string());
        state.disabled_for_turn = tool_policy::output_has_auth_failure_markers(text);
    }

    fn tool_context_profile(&self) -> ToolContextProfile {
        if self.context_mode.is_frugal() {
            ToolContextProfile::Lean
        } else {
            ToolContextProfile::Full
        }
    }

    fn refresh_tools_for_context(&mut self) {
        let profile = self.tool_context_profile();
        self.tools = tool_definitions()
            .into_iter()
            .filter(|t| t.name != "subagent")
            .filter(|t| self.browser_recipe == BrowserRecipe::AgentBrowser || t.name != "browser")
            .filter(|t| tool_name_allowed_in_profile(t.name, profile))
            .collect();
    }

    fn wire_tool_profile(&self) -> ToolProfile {
        if self.context_mode.is_frugal() {
            ToolProfile::Lean
        } else {
            self.tool_profile
        }
    }

    fn wire_tools(&self) -> Vec<WireTool> {
        tools::wire_tools(&self.tools, self.wire_tool_profile())
    }

    fn wire_tools_oai(&self) -> Vec<OaiTool> {
        tools::wire_tools_oai(&self.tools, self.wire_tool_profile())
    }

    fn tool_use_ids_in_history(&self) -> HashSet<String> {
        let mut ids = HashSet::new();
        for m in &self.history {
            for b in &m.content {
                if let Block::ToolUse { id, .. } = b {
                    ids.insert(id.clone());
                }
            }
        }
        ids
    }

    fn history_to_oai_messages(&self, system_text: &str) -> Vec<OaiMessage> {
        let valid_ids = self.tool_use_ids_in_history();
        let mut msgs = vec![OaiMessage {
            role: "system".to_string(),
            content: Some(system_text.to_string()),
            tool_calls: None,
            tool_call_id: None,
        }];
        for m in &self.history {
            match m.role.as_str() {
                "user" => {
                    let texts: Vec<&str> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } | Block::PartialStream { text } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    let tool_results: Vec<&Block> = m
                        .content
                        .iter()
                        .filter(|b| matches!(b, Block::ToolResult { .. }))
                        .collect();
                    if !tool_results.is_empty() {
                        for b in &tool_results {
                            if let Block::ToolResult {
                                tool_use_id,
                                content,
                                is_error: _,
                                ..
                            } = b
                            {
                                if !valid_ids.contains(tool_use_id) {
                                    continue;
                                }
                                msgs.push(OaiMessage {
                                    role: "tool".to_string(),
                                    content: Some(content.clone()),
                                    tool_calls: None,
                                    tool_call_id: Some(tool_use_id.clone()),
                                });
                            }
                        }
                    } else if !texts.is_empty() {
                        msgs.push(OaiMessage {
                            role: "user".to_string(),
                            content: Some(texts.join("\n")),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
                "assistant" => {
                    let texts: Vec<&str> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } | Block::PartialStream { text } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    let tool_uses: Vec<&Block> = m
                        .content
                        .iter()
                        .filter(|b| matches!(b, Block::ToolUse { .. }))
                        .collect();
                    let has_tools = !tool_uses.is_empty();
                    let oai_tool_calls: Option<Vec<OaiToolCall>> = if has_tools {
                        Some(
                            tool_uses
                                .iter()
                                .filter_map(|b| {
                                    if let Block::ToolUse { id, name, input } = b {
                                        Some(OaiToolCall {
                                            id: id.clone(),
                                            r#type: "function".to_string(),
                                            function: OaiFunction {
                                                name: name.clone(),
                                                arguments: input.to_string(),
                                            },
                                        })
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                        )
                    } else {
                        None
                    };
                    let content = if texts.is_empty() {
                        None
                    } else {
                        Some(texts.join("\n"))
                    };
                    msgs.push(OaiMessage {
                        role: "assistant".to_string(),
                        content,
                        tool_calls: oai_tool_calls,
                        tool_call_id: None,
                    });
                }
                _ => {}
            }
        }
        msgs
    }

    fn wire_tools_chatgpt(&self) -> Vec<Value> {
        tools::wire_tools_chatgpt(&self.tools, self.wire_tool_profile())
    }

    fn history_to_chatgpt_input(&self) -> Vec<Value> {
        let valid_ids = self.tool_use_ids_in_history();
        let mut items = Vec::new();
        let mut msg_counter = 0usize;

        for msg in &self.history {
            for block in &msg.content {
                match block {
                    Block::Text { text } if text.trim().is_empty() => continue,
                    Block::Text { text } => {
                        let role = if msg.role == "assistant" {
                            "assistant"
                        } else {
                            "user"
                        };
                        items.push(json!({
                            "type": "message",
                            "role": role,
                            "content": [{
                                "type": if role == "assistant" { "output_text" } else { "input_text" },
                                "text": text,
                            }],
                            "id": format!("msg_{msg_counter}"),
                        }));
                        msg_counter += 1;
                    }
                    Block::ToolUse { id, name, input } => {
                        // Responses API requires item `id` to start with "fc_".
                        // Wolf stores the server-provided call_id (starts with "call_") in
                        // Block.id and uses it to pair tool results. `id` is optional —
                        // omit it so the backend auto-assigns a valid item id.
                        items.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": input.to_string(),
                        }));
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        if !valid_ids.contains(tool_use_id) {
                            continue;
                        }
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                    }
                    Block::PartialStream { text } => {
                        items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{
                                "type": "output_text",
                                "text": text,
                            }],
                            "id": format!("msg_{msg_counter}"),
                        }));
                        msg_counter += 1;
                    }
                    _ => {}
                }
            }
        }

        items
    }

    fn build_streaming_request(
        &self,
        sys_stable: &str,
        sys_env: &str,
        sys_blocks: &[SystemBlock<'_>],
        wire_tools: &[WireTool],
        chatgpt_session_id: &str,
    ) -> Result<(String, Vec<u8>)> {
        let url = provider_request_url(&self.base_url, self.api_provider);
        match self.api_provider {
            ApiProvider::ChatGpt => {
                let system_text = format!("{sys_stable}\n\n{sys_env}");
                let body = build_chatgpt_request(
                    &self.model,
                    self.thinking_effort,
                    &system_text,
                    chatgpt_session_id,
                    self.history_to_chatgpt_input(),
                    self.wire_tools_chatgpt(),
                );
                let bytes = serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                Ok((url, bytes))
            }
            ApiProvider::OpenAi => {
                let sys_text = format!("{sys_stable}\n\n{sys_env}");
                let oai_msgs = self.history_to_oai_messages(&sys_text);
                let oai_tools = self.wire_tools_oai();
                let reasoning_effort = openai_reasoning_effort(self.thinking_effort);
                let body = OaiRequest {
                    model: &self.model,
                    max_tokens: 4096,
                    messages: oai_msgs,
                    tools: oai_tools,
                    stream: true,
                    reasoning_effort,
                };
                let bytes = serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                Ok((url, bytes))
            }
            ApiProvider::Anthropic => {
                let thinking =
                    anthropic_thinking_budget_tokens(self.thinking_effort).map(|budget_tokens| {
                        AnthropicThinking {
                            kind: "enabled",
                            budget_tokens,
                        }
                    });
                let messages = strip_tool_result_metadata(&self.history);
                let body = Request {
                    model: &self.model,
                    max_tokens: 4096,
                    system: sys_blocks,
                    messages: &messages,
                    tools: wire_tools,
                    stream: true,
                    thinking,
                };
                let bytes = serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                Ok((url, bytes))
            }
        }
    }

    async fn parse_stream_response(
        &mut self,
        resp: reqwest::Response,
    ) -> Result<(Vec<Block>, Option<String>, Usage)> {
        match self.api_provider {
            ApiProvider::OpenAi => self.read_stream_oai(resp).await,
            ApiProvider::ChatGpt => self.read_stream_chatgpt(resp).await,
            ApiProvider::Anthropic => self.read_stream(resp).await,
        }
    }

    fn session_header(&self) -> SessionHeader {
        self.session_header_with_origin(self.track_origin.clone())
    }

    fn session_header_with_origin(&self, track_origin: Option<TrackOrigin>) -> SessionHeader {
        let mut allowed: Vec<String> = self.allowed.iter().cloned().collect();
        allowed.sort();
        let mut exposed_tools: Vec<String> =
            self.tools.iter().map(|t| t.name.to_string()).collect();
        exposed_tools.sort();
        let mut approval_required_tools: Vec<String> = self
            .tools
            .iter()
            .filter(|t| needs_permission(t.name))
            .map(|t| t.name.to_string())
            .collect();
        approval_required_tools.sort();
        let mut auto_approved_tools: Vec<String> = exposed_tools
            .iter()
            .filter(|name| {
                let input = Value::Null;
                !needs_permission(name.as_str()) || self.tool_auto_approved(name, &input)
            })
            .cloned()
            .collect();
        auto_approved_tools.sort();
        SessionHeader {
            version: SESSION_FORMAT_VERSION,
            model: self.model.clone(),
            system: self.system.clone(),
            allowed,
            exposed_tools,
            approval_required_tools,
            auto_approved_tools,
            sandbox: Some(self.sandbox_root.display().to_string()),
            usage: self.session_usage,
            thinking_effort: self.thinking_effort,
            compact_threshold_chars: self.compact_threshold_override(),
            compact_threshold_percent: self.compact_threshold_override_percent(),
            approval_profile: self.approval_profile,
            sandbox_profile: self.sandbox_profile,
            budget_cap: self.budget_cap,
            browser_recipe: self.browser_recipe,
            context_mode: self.context_mode,
            tool_profile: self.tool_profile,
            provenance: self.session_provenance(),
            work_ledger: self.cleaned_work_ledger(),
            provider_health: self.provider_health.clone(),
            track_origin,
            privacy: self.privacy.clone(),
        }
    }

    fn session_provenance(&self) -> SessionProvenance {
        let mut prompt_sources = Vec::new();
        let wolf_md = self.sandbox_root.join("WOLF.md");
        let wolf_memory = self.sandbox_root.join("WOLF.memory.md");
        let wolf_md_hash = std::fs::read(&wolf_md).ok().map(|bytes| {
            prompt_sources.push(wolf_md.display().to_string());
            sha256_hex_bytes(&bytes)
        });
        let wolf_memory_hash = std::fs::read(&wolf_memory).ok().map(|bytes| {
            prompt_sources.push(wolf_memory.display().to_string());
            sha256_hex_bytes(&bytes)
        });
        SessionProvenance {
            wolf_version: env!("CARGO_PKG_VERSION").to_string(),
            git: self.git_context.clone(),
            provider: self.provider_id.clone(),
            api_provider: self.api_provider,
            model: self.model.clone(),
            thinking_effort: self.thinking_effort,
            approval_profile: self.approval_profile,
            sandbox_profile: self.sandbox_profile,
            system_prompt_hash: sha256_hex_str(&self.compose_system_parts().0),
            wolf_md_hash,
            wolf_memory_hash,
            tool_catalog_version: TOOL_CATALOG_VERSION,
            prompt_sources,
        }
    }

    pub(crate) fn save_session_to_path(&self, path: &Path) -> Result<()> {
        self.save_session_to_path_with_origin(path, self.track_origin.clone())
    }

    fn save_session_to_path_with_origin(
        &self,
        path: &Path,
        track_origin: Option<TrackOrigin>,
    ) -> Result<()> {
        let header = self.session_header_with_origin(track_origin);
        let mut data = Vec::new();
        writeln!(&mut data, "{}", serde_json::to_string(&header)?)?;
        for m in &self.history {
            writeln!(&mut data, "{}", serde_json::to_string(m)?)?;
        }
        atomic_write_bytes(path, &data)?;
        Ok(())
    }

    pub(crate) fn export_session_html_to_path(&self, path: &Path) -> Result<()> {
        let html = render_session_html(&self.session_header(), &self.history, "current session");
        atomic_write_bytes(path, html.as_bytes())?;
        Ok(())
    }

    fn save_session(&self, name: &str) -> Result<PathBuf> {
        let path = named_session_path_for_root(&self.sandbox_root, name)?;
        self.save_session_to_path(&path)?;
        Ok(path)
    }

    fn save_latest_session(&self) -> Result<PathBuf> {
        let path = self.latest_session_path.clone();
        record_crash_session_id(&path);
        self.save_session_to_path(&path)?;
        Ok(path)
    }

    fn checkpoint_latest_session(&mut self, reason: &str) {
        if !self.session_enabled || self.suppress_checkpoints || self.history.is_empty() {
            return;
        }
        // after_user_message: captures intent on first keystroke for free; skip it and user loses
        // input on crash. after_compact and outer_loop_autosave are the end-of-state fences — also
        // critical. The rest (after_assistant_message, after_tool_results, after_compact_attempt)
        // fire repeatedly per turn and are debounced so a 20-tool turn doesn't write 20 times.
        let critical = matches!(
            reason,
            "after_user_message" | "after_compact" | "outer_loop_autosave"
        );
        if !critical
            && let Some(last) = self.last_checkpoint_at
            && last.elapsed() < checkpoint_debounce()
        {
            return;
        }
        match self.save_latest_session() {
            Ok(path) => {
                self.last_checkpoint_at = Some(std::time::Instant::now());
                self.append_latest_log(
                    "session_checkpoint",
                    &format!("{reason} -> {}", path.display()),
                )
            }
            Err(e) => {
                self.append_latest_log("session_checkpoint_failed", &format!("{reason}: {e:#}"));
                self.sink.emit(AgentEvent::Warn(format!(
                    "[warn] latest session autosave failed ({reason}): {e:#}"
                )));
            }
        }
    }

    fn load_session_from_path(&mut self, path: &Path) -> Result<PathBuf> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut lines = content.lines();
        let header = lines.next().context("empty session file")?;
        let SessionHeader {
            model,
            system,
            allowed,
            sandbox,
            usage,
            thinking_effort,
            compact_threshold_chars,
            compact_threshold_percent,
            approval_profile,
            sandbox_profile,
            budget_cap,
            browser_recipe,
            context_mode,
            tool_profile,
            work_ledger,
            provider_health,
            track_origin,
            privacy,
            ..
        } = parse_session_header(header)?;

        self.model = model;
        self.system = system;
        self.allowed = allowed.into_iter().collect();
        self.session_usage = usage;
        self.thinking_effort = thinking_effort;
        self.compact_threshold_percent =
            compact_threshold_percent.filter(|v| (1..=100).contains(v));
        self.compact_threshold_chars = self
            .compact_threshold_percent
            .map(|percent| compact_threshold_chars_for_percent(&self.model, percent))
            .or_else(|| compact_threshold_chars.filter(|v| *v > 0));
        self.approval_profile = approval_profile;
        self.sandbox_profile = sandbox_profile;
        self.budget_cap = budget_cap;
        self.budget_exhausted = false;
        self.work_ledger = work_ledger;
        self.provider_health = provider_health;
        self.track_origin = track_origin;
        self.privacy = privacy;
        self.context_mode = context_mode;
        self.tool_profile = if self.context_mode.is_frugal() {
            ToolProfile::Lean
        } else {
            tool_profile
        };
        self.set_browser_recipe(browser_recipe);
        if let Some(saved_sandbox) = sandbox.as_deref()
            && let Ok(restored) = std::fs::canonicalize(saved_sandbox)
        {
            self.set_sandbox_root(restored)?;
        }

        let mut hist: Vec<Message> = Vec::new();
        for (i, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            hist.push(
                serde_json::from_str(line)
                    .with_context(|| format!("bad message on line {}", i + 2))?,
            );
        }
        self.history = hist;
        self.clear_pending_login();
        Ok(path.to_path_buf())
    }

    fn load_session(&mut self, name: &str) -> Result<PathBuf> {
        let path = named_session_path_for_root(&self.sandbox_root, name)?;
        self.load_session_from_path(&path)
    }

    fn load_latest_session(&mut self) -> Result<PathBuf> {
        let path = self.latest_session_path.clone();
        self.load_session_from_path(&path)
    }

    fn rewrite_latest_tool_results_as_text_fallback(&mut self) -> bool {
        let Some(last) = self.history.last_mut() else {
            return false;
        };
        if last.role != "user" || last.content.is_empty() {
            return false;
        }
        if !last
            .content
            .iter()
            .all(|b| matches!(b, Block::ToolResult { .. }))
        {
            return false;
        }

        let mut merged = String::from(
            "[wolf workaround] provider rejected structured tool_result blocks; flattened results:\n",
        );
        for block in &last.content {
            if let Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } = block
            {
                let tag = if is_error.unwrap_or(false) {
                    "error"
                } else {
                    "ok"
                };
                merged.push_str(&format!("- {tag} ({tool_use_id}): {content}\n"));
            }
        }

        last.content = vec![Block::Text {
            text: cap_tool_output(merged),
        }];
        true
    }

    fn estimated_context_tokens_from_history(&self) -> u64 {
        self.history.iter().map(message_approx_tokens).sum()
    }

    fn history_chars(&self) -> usize {
        self.history
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        Block::Text { text } | Block::PartialStream { text } => text.len(),
                        Block::Thinking { text } => text.len(),
                        Block::ToolUse { input, .. } => json_byte_len(input),
                        Block::ToolResult { content, .. } => content.len(),
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    fn find_compact_split(&self) -> Option<usize> {
        let len = self.history.len();
        if len <= COMPACT_KEEP_MESSAGES {
            return None;
        }
        let preferred = len.saturating_sub(COMPACT_KEEP_MESSAGES);
        let user_boundary_floor = preferred.saturating_sub(COMPACT_USER_BOUNDARY_BACKTRACK);
        let mut start = preferred;
        while start > user_boundary_floor {
            let m = &self.history[start];
            if m.role == "user"
                && !Self::message_has_tool_results(m)
                && Self::compact_split_is_pair_safe(&self.history, start)
            {
                return Some(start);
            }
            start -= 1;
        }
        (1..=preferred)
            .rev()
            .find(|&split| Self::compact_split_is_pair_safe(&self.history, split))
    }

    fn message_has_tool_results(msg: &Message) -> bool {
        msg.content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { .. }))
    }

    fn compact_split_is_pair_safe(msgs: &[Message], split: usize) -> bool {
        if split == 0 || split >= msgs.len() || Self::message_has_tool_results(&msgs[split]) {
            return false;
        }

        let mut use_owner: HashMap<String, usize> = HashMap::new();
        let mut result_owner: HashMap<String, usize> = HashMap::new();
        for (idx, msg) in msgs.iter().enumerate() {
            for block in &msg.content {
                match block {
                    Block::ToolUse { id, .. } => {
                        use_owner.entry(id.clone()).or_insert(idx);
                    }
                    Block::ToolResult { tool_use_id, .. } => {
                        result_owner.entry(tool_use_id.clone()).or_insert(idx);
                    }
                    _ => {}
                }
            }
        }

        for (id, &use_idx) in &use_owner {
            if use_idx < split {
                continue;
            }
            let Some(&result_idx) = result_owner.get(id) else {
                return false;
            };
            if result_idx < split {
                return false;
            }
        }
        for (id, &result_idx) in &result_owner {
            if result_idx < split {
                continue;
            }
            let Some(&use_idx) = use_owner.get(id) else {
                return false;
            };
            if use_idx < split {
                return false;
            }
        }
        true
    }

    fn message_has_tool_blocks(msg: &Message) -> bool {
        msg.content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { .. } | Block::ToolResult { .. }))
    }

    fn message_compaction_bytes(msg: &Message) -> usize {
        msg.role.len()
            + msg
                .content
                .iter()
                .map(|b| match b {
                    Block::Text { text } | Block::PartialStream { text } => text.len(),
                    Block::Thinking { text } => text.len(),
                    Block::ToolUse { id, name, input } => {
                        id.len() + name.len() + json_byte_len(input)
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => tool_use_id.len() + content.len() + usize::from(is_error.is_some()),
                })
                .sum::<usize>()
    }

    fn compact_preserve_tool_messages(&self) -> usize {
        if self.context_mode.is_frugal() {
            FRUGAL_COMPACT_PRESERVE_TOOL_MESSAGES
        } else {
            COMPACT_PRESERVE_TOOL_MESSAGES
        }
    }

    fn compact_preserve_tool_bytes(&self) -> usize {
        if self.context_mode.is_frugal() {
            FRUGAL_COMPACT_PRESERVE_TOOL_BYTES
        } else {
            COMPACT_PRESERVE_TOOL_BYTES
        }
    }

    fn compact_summary_tool_result_cap(&self) -> usize {
        if self.context_mode.is_frugal() {
            FRUGAL_COMPACT_SUMMARY_TOOL_RESULT_CAP
        } else {
            COMPACT_SUMMARY_TOOL_RESULT_CAP
        }
    }

    fn split_compaction_inputs(&self, old: &[Message]) -> (Vec<Message>, Vec<Message>) {
        let mut keep_indices: Vec<usize> = Vec::new();
        let mut keep_bytes = 0usize;

        for idx in (0..old.len()).rev() {
            let msg = &old[idx];
            if !Self::message_has_tool_blocks(msg) {
                continue;
            }
            let weight = Self::message_compaction_bytes(msg);
            if !keep_indices.is_empty() {
                if keep_indices.len() >= self.compact_preserve_tool_messages() {
                    break;
                }
                if keep_bytes.saturating_add(weight) > self.compact_preserve_tool_bytes() {
                    break;
                }
            }
            keep_indices.push(idx);
            keep_bytes = keep_bytes.saturating_add(weight);
            if keep_indices.len() >= self.compact_preserve_tool_messages()
                || keep_bytes >= self.compact_preserve_tool_bytes()
            {
                break;
            }
        }

        let mut keep_set: HashSet<usize> = keep_indices.into_iter().collect();

        // Pair-closure: every kept ToolResult must have its paired ToolUse present
        // (and vice versa) or the ChatGPT Responses API rejects the request with
        // "No tool call found for function call output with call_id ...". The budget
        // loop above walks newest-first, so it can keep a ToolResult while its
        // older paired ToolUse was never considered. Pull those pairs in here,
        // then drop anything that still can't be paired inside `old`.
        pair_close_keep_set(old, &mut keep_set);

        let mut summary_msgs: Vec<Message> = Vec::new();
        let mut preserved_tool_msgs: Vec<Message> = Vec::new();

        for (idx, msg) in old.iter().enumerate() {
            if keep_set.contains(&idx) {
                preserved_tool_msgs.push(msg.clone());
                continue;
            }

            let summary_blocks: Vec<Block> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } | Block::PartialStream { text } => {
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(Block::Text {
                                text: trimmed.to_string(),
                            })
                        }
                    }
                    Block::ToolUse { id, name, input } => Some(Block::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }),
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        metadata,
                    } => Some(Block::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: cap_bytes_with_hint(
                            content.clone(),
                            self.compact_summary_tool_result_cap(),
                            "Tool result truncated before compaction summary.",
                        ),
                        is_error: *is_error,
                        metadata: metadata.clone(),
                    }),
                    Block::Thinking { .. } => None,
                })
                .collect();

            if !summary_blocks.is_empty() {
                summary_msgs.push(Message {
                    role: msg.role.clone(),
                    content: summary_blocks,
                });
            }
        }

        (summary_msgs, preserved_tool_msgs)
    }

    async fn one_shot_summary(
        &mut self,
        old: &[Message],
        evidence: &str,
    ) -> Result<(String, Usage)> {
        if self.context_mode.is_frugal() {
            let summary = if evidence.trim().is_empty() {
                "No prior deterministic resume evidence remained after compaction.".to_string()
            } else {
                format!("Deterministic resume evidence:\n{}", evidence.trim())
            };
            return Ok((summary, Usage::default()));
        }

        let transcript = render_transcript_for_summary(old, self.context_mode);
        let user_text = compaction_user_text_with_evidence(&transcript, evidence);

        #[derive(PartialEq, Eq)]
        enum SummaryParse {
            Anthropic,
            OpenAi,
            ChatGptSse,
        }

        let (mut resp, parse_mode): (reqwest::Response, SummaryParse) =
            if self.api_provider == ApiProvider::ChatGpt {
                let body = build_chatgpt_summary_request(&self.model, COMPACT_SYSTEM, &user_text);
                let url = provider_request_url(&self.base_url, self.api_provider);
                let bytes = serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                let req = apply_provider_headers(
                    self.http_client()
                        .post(&url)
                        .header("content-type", "application/json")
                        .header("accept", "text/event-stream")
                        .body(bytes),
                    self.api_provider,
                    &self.api_key,
                    None,
                )?;
                (req.send().await?, SummaryParse::ChatGptSse)
            } else if self.api_provider == ApiProvider::OpenAi {
                let reasoning_effort = openai_reasoning_effort(self.thinking_effort);
                let messages = vec![
                    OaiMessage {
                        role: "system".to_string(),
                        content: Some(COMPACT_SYSTEM.to_string()),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                    OaiMessage {
                        role: "user".to_string(),
                        content: Some(user_text.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                ];
                let body = OaiRequest {
                    model: &self.model,
                    max_tokens: 2048,
                    messages,
                    tools: Vec::new(),
                    stream: false,
                    reasoning_effort,
                };
                let mut req = self
                    .http_client()
                    .post(provider_request_url(&self.base_url, self.api_provider))
                    .header("content-type", "application/json")
                    .json(&body);
                if !self.api_key.trim().is_empty() {
                    req = req.header("authorization", format!("Bearer {}", self.api_key));
                }
                (req.send().await?, SummaryParse::OpenAi)
            } else {
                let messages = vec![Message {
                    role: "user".to_string(),
                    content: vec![Block::Text {
                        text: user_text.clone(),
                    }],
                }];
                let sys_blocks = [SystemBlock {
                    kind: "text",
                    text: COMPACT_SYSTEM,
                    cache_control: None,
                }];
                let body = Request {
                    model: &self.model,
                    max_tokens: 2048,
                    system: &sys_blocks,
                    messages: &messages,
                    tools: &[],
                    stream: false,
                    thinking: None,
                };
                let mut req = self
                    .http_client()
                    .post(provider_request_url(&self.base_url, self.api_provider))
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body);
                if !self.api_key.trim().is_empty() {
                    req = req.header("x-api-key", &self.api_key);
                }
                (req.send().await?, SummaryParse::Anthropic)
            };

        let status = resp.status();
        if !status.is_success() {
            let text = cap_bytes_with_hint(
                resp.text().await.unwrap_or_default(),
                HTTP_ERROR_BODY_CAP,
                "HTTP error body truncated.",
            );
            anyhow::bail!("summary HTTP {status}: {text}");
        }

        if parse_mode == SummaryParse::ChatGptSse {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match self.read_stream_chatgpt(resp).await {
                    Ok((blocks, _finish_reason, usage)) => {
                        let text = blocks
                            .into_iter()
                            .filter_map(|b| match b {
                                Block::Text { text } | Block::PartialStream { text } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if text.trim().is_empty() {
                            anyhow::bail!("summary response had no text blocks");
                        }
                        return Ok((text, usage));
                    }
                    Err(e) => {
                        let body = stream_error_body(&e);
                        let plan = orchestrator::classify_stream_error(&body);
                        if plan.retry && attempt < MAX_STREAM_ATTEMPTS {
                            let wait = jittered_backoff_secs(1u64 << (attempt - 1));
                            self.append_latest_log(
                                "summary_stream_retry",
                                &format!(
                                    "attempt={attempt} kind={} wait={wait}s body={body}",
                                    plan.label()
                                ),
                            );
                            self.sink.emit(AgentEvent::HttpRetry {
                                attempt,
                                wait_secs: wait,
                                reason: format!("{} summary stream error", plan.label()),
                            });
                            self.interrupt_aware_sleep(wait).await;
                            let body = build_chatgpt_summary_request(
                                &self.model,
                                COMPACT_SYSTEM,
                                &user_text,
                            );
                            let url = provider_request_url(&self.base_url, self.api_provider);
                            let bytes =
                                serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                            let req = apply_provider_headers(
                                self.http_client()
                                    .post(&url)
                                    .header("content-type", "application/json")
                                    .header("accept", "text/event-stream")
                                    .body(bytes),
                                self.api_provider,
                                &self.api_key,
                                None,
                            )?;
                            let retry_resp = req.send().await?;
                            let retry_status = retry_resp.status();
                            if !retry_status.is_success() {
                                let text = cap_bytes_with_hint(
                                    retry_resp.text().await.unwrap_or_default(),
                                    HTTP_ERROR_BODY_CAP,
                                    "HTTP error body truncated.",
                                );
                                anyhow::bail!("summary HTTP {retry_status}: {text}");
                            }
                            resp = retry_resp;
                            continue;
                        }
                        anyhow::bail!(body);
                    }
                }
            }
        }

        let json: Value = resp.json().await?;
        match parse_mode {
            SummaryParse::ChatGptSse => unreachable!("handled above"),
            SummaryParse::OpenAi => {
                let text = json["choices"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|c| c["message"]["content"].as_str())
                    .unwrap_or_default()
                    .to_string();
                if text.trim().is_empty() {
                    anyhow::bail!("summary response had no text: {json}");
                }
                let usage = Usage::parse_openai(&json["usage"]);
                Ok((text, usage))
            }
            SummaryParse::Anthropic => {
                let text = json["content"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter().find_map(|b| {
                            if b["type"] == "text" {
                                b["text"].as_str().map(String::from)
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    anyhow::bail!("summary response had no text: {json}");
                }
                let usage = Usage::parse(&json["usage"]);
                Ok((text, usage))
            }
        }
    }

    #[cfg(test)]
    fn should_auto_compact(&self) -> bool {
        self.history_chars() > self.compact_threshold_chars()
    }

    #[cfg(test)]
    fn should_active_compact(&self) -> bool {
        self.history_chars() > self.active_compact_threshold_chars()
    }

    async fn compact_if_over_threshold(
        &mut self,
        threshold_chars: usize,
        checkpoint_label: &str,
    ) -> bool {
        if self.history_chars() <= threshold_chars {
            return false;
        }
        let before_len = self.history.len();
        let before_chars = self.history_chars();
        let compacted = match self.compact().await {
            Ok(()) => self.history.len() != before_len || self.history_chars() < before_chars,
            Err(_) => {
                self.sink
                    .emit(AgentEvent::Info("[continuing without compaction]".into()));
                false
            }
        };
        self.checkpoint_latest_session(checkpoint_label);
        compacted
    }

    async fn compact(&mut self) -> Result<()> {
        let Some(split) = self.find_compact_split() else {
            self.sink
                .emit(AgentEvent::Info("[compact: nothing to compact yet]".into()));
            return Ok(());
        };
        self.sink.emit(AgentEvent::CompactStart);
        let before = self.history.len();
        let result = self.compact_after_start(split, before).await;
        if let Err(e) = &result {
            let message = format!("{e:#}");
            self.append_latest_log("compact_failed", &message);
            self.sink.emit(AgentEvent::CompactFailed { message });
        }
        result
    }

    async fn compact_after_start(&mut self, split: usize, before: usize) -> Result<()> {
        let old: Vec<Message> = self.history[..split].to_vec();
        let (summary_input, preserved_tool_msgs) = self.split_compaction_inputs(&old);

        let evidence = render_compaction_evidence(&old, &self.work_ledger, &self.provider_health);
        let (summary, usage) = if summary_input.is_empty() {
            (
                if evidence.trim().is_empty() {
                    "No prior text turns remained after preserving recent tool activity verbatim."
                        .to_string()
                } else {
                    format!("Deterministic resume evidence:\n{}", evidence.trim())
                },
                Usage::default(),
            )
        } else {
            match self.one_shot_summary(&summary_input, &evidence).await {
                Ok(v) => v,
                Err(e) if !evidence.trim().is_empty() => {
                    let msg = format!(
                        "summary model failed; using deterministic compaction evidence: {e:#}"
                    );
                    self.append_latest_log("compact_summary_fallback", &msg);
                    self.sink
                        .emit(AgentEvent::Warn(format!("[compact fallback] {msg}")));
                    (
                        format!("Deterministic resume evidence:\n{}", evidence.trim()),
                        Usage::default(),
                    )
                }
                Err(e) => return Err(e),
            }
        };
        self.session_usage.add(usage);

        self.history =
            build_compacted_history(&summary, preserved_tool_msgs, &self.history[split..]);

        let compacted_chars = self.history_chars();
        let compacted_tokens = self.estimated_context_tokens_from_history();
        self.sink.emit(AgentEvent::HistoryContextUpdated {
            tokens: Some(compacted_tokens),
            chars: compacted_chars,
        });

        let after = self.history.len();
        self.sink.emit(AgentEvent::CompactEnd { before, after });
        self.append_latest_log("compact_complete", &format!("{before} -> {after} messages"));
        self.checkpoint_latest_session("after_compact");
        Ok(())
    }

    async fn run_plan(&mut self, task: String) -> Result<()> {
        let input = json!({
            "task": task,
            "system": PLAN_SYSTEM,
            "allowed_tools": READ_ONLY_TOOLS,
            "max_iterations": 15,
        });
        let plan_report = self
            .run_subagent(&input)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let plan = plan_report.final_text;
        self.sink.emit(AgentEvent::Slash(format!(
            "=== PLAN ===\n{plan}\n=== END ==="
        )));
        self.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: format!("Task: {task}\n\nProposed plan:\n\n{plan}"),
            }],
        });
        self.history.push(Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "Plan ready. Say 'go' to execute, or give revisions.".to_string(),
            }],
        });
        Ok(())
    }

    async fn run_subagent_cmd(&mut self, raw: String) -> Result<()> {
        let mut task_parts: Vec<String> = Vec::new();
        let mut tools_override: Option<Vec<String>> = None;
        let mut max_iter: Option<u64> = None;
        let mut system_override: Option<String> = None;
        let mut readonly = false;
        let mut mode = SubagentMode::Detached;

        let mut tokens = raw.split_whitespace().peekable();
        while let Some(tok) = tokens.next() {
            match tok {
                "--tools" => {
                    if let Some(list) = tokens.next() {
                        tools_override = Some(list.split(',').map(String::from).collect());
                    }
                }
                "--max-iter" => {
                    if let Some(n) = tokens.next() {
                        max_iter = n.parse().ok();
                    }
                }
                "--system" => {
                    let mut sys_parts: Vec<String> = Vec::new();
                    while let Some(p) = tokens.peek() {
                        if p.starts_with("--") {
                            break;
                        }
                        sys_parts.push(tokens.next().unwrap().to_string());
                    }
                    if !sys_parts.is_empty() {
                        system_override = Some(sys_parts.join(" "));
                    }
                }
                "--readonly" => {
                    readonly = true;
                }
                "--inline" | "--foreground" => {
                    mode = SubagentMode::Inline;
                }
                "--detached" | "--background" | "--window" | "--tmux" => {
                    mode = SubagentMode::Detached;
                }
                "--mode" => {
                    if let Some(value) = tokens.next()
                        && let Some(parsed) = SubagentMode::parse(value)
                    {
                        mode = parsed;
                    }
                }
                other => task_parts.push(other.to_string()),
            }
        }

        let task = task_parts.join(" ");
        if task.is_empty() {
            self.sink.emit(AgentEvent::Slash(
                "usage: /subagent <task> [--tools t1,t2] [--max-iter N] [--system PROMPT] [--readonly] [--inline|--detached]".into(),
            ));
            return Ok(());
        }

        let allowed_tools = if readonly {
            Some(READ_ONLY_TOOLS.iter().map(|s| s.to_string()).collect())
        } else {
            tools_override
        };

        let mut input = json!({
            "task": task,
        });
        if let Some(max_iter) = max_iter {
            input["max_iterations"] = json!(max_iter);
        }
        if let Some(tools) = &allowed_tools {
            input["allowed_tools"] = json!(tools);
        }
        if let Some(sys) = &system_override {
            input["system"] = json!(sys);
        }

        let request = SubagentRequest::from_input(self, &input).map_err(anyhow::Error::msg)?;
        if mode == SubagentMode::Detached {
            let (request_path, report_path) = write_subagent_request(&self.sandbox_root, &request)?;
            let launch =
                spawn_detached_subagent_process(&self.sandbox_root, &request_path, &report_path)?;
            let iter_label = max_iter
                .map(|max| max.to_string())
                .unwrap_or_else(|| "unlimited".to_string());
            let summary = format!(
                "▶ subagent detached: \"{}\"{} · max_iter={}\nlauncher: {}\nrequest: {}\nreport: {}\nstdout/stderr: {}\nmonitor: {}\nParent remains usable; run the monitor command in another shell to watch live progress, then read the report when complete.",
                task,
                if readonly { " [readonly]" } else { "" },
                iter_label,
                launch.label,
                request_path.display(),
                launch.report_path.display(),
                launch.stdout_path.display(),
                launch.monitor_command
            );
            self.sink.emit(AgentEvent::Slash(summary.clone()));
            self.history.push(Message {
                role: "user".to_string(),
                content: vec![Block::Text {
                    text: format!(
                        "[subagent detached]\nTask: {}\nrequest: {}\nreport: {}\nstdout/stderr: {}\nmonitor: {}\n\nThe subagent is running outside the parent transcript. You can monitor live progress without interrupting parent work using the monitor command. When it completes, inspect the report and decide whether to use, verify, revise, or discard it.",
                        task,
                        request_path.display(),
                        launch.report_path.display(),
                        launch.stdout_path.display(),
                        launch.monitor_command
                    ),
                }],
            });
            return Ok(());
        }

        self.sink.emit(AgentEvent::Slash(format!(
            "▶ subagent inline: \"{task}\"{} · max_iter={}",
            if readonly { " [readonly]" } else { "" },
            max_iter
                .map(|max| max.to_string())
                .unwrap_or_else(|| "unlimited".to_string())
        )));

        let report = self
            .run_subagent(&input)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        let summary = render_subagent_report(&report);

        self.sink.emit(AgentEvent::Slash(summary.clone()));

        self.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: cap_bytes_with_hint(
                    format!(
                        "[subagent completed]\nTask: {}\n\nSubagent report for review:\n\n{}\n\nReview this report and decide whether to use, verify, revise, or discard the subagent work before continuing.",
                        task, summary
                    ),
                    24_000,
                    "Subagent report truncated; rerun /subagent with a narrower task or inspect forwarded tool output if needed.",
                ),
            }],
        });
        self.history.push(Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "Received the subagent report. I will review its findings before using them."
                    .to_string(),
            }],
        });

        Ok(())
    }

    async fn chat(&mut self, user_input: String) -> Result<()> {
        self.interrupt.store(false, Ordering::SeqCst);
        self.sink.emit(AgentEvent::TurnStart);
        self.append_latest_log("chat_start", &format!("chars={}", user_input.len()));
        let result = self.chat_inner(user_input).await;
        if result.is_err() {
            if self.interrupt.load(Ordering::SeqCst) {
                self.sink.emit(AgentEvent::Interrupted);
            }
            self.sink.emit(AgentEvent::TurnEnd {
                usage: Usage::default(),
            });
        }
        result
    }

    async fn chat_inner(&mut self, mut user_input: String) -> Result<()> {
        let mut compacted_this_turn = false;
        let hook_env = [("WOLF_USER_INPUT", user_input.as_str())];
        for (out, _code) in self
            .hooks
            .fire("user_prompt", "", &hook_env, &self.sandbox_root)
        {
            let t = out.trim();
            if !t.is_empty() {
                user_input.push_str(&format!("\n\n[hook:user_prompt]\n{t}"));
            }
        }

        self.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text { text: user_input }],
        });
        self.checkpoint_latest_session("after_user_message");

        let turn_started_at = std::time::Instant::now();

        let objective = orchestrator::ObjectiveTracker::from_user_prompt(
            self.history
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .and_then(|m| {
                    m.content.iter().find_map(|b| match b {
                        Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
                        _ => None,
                    })
                })
                .unwrap_or_default(),
        );
        let objective_line = objective.display_line();
        self.update_work_ledger_from_objective(&objective);
        self.sink
            .emit(AgentEvent::Info(format!("[{}]", objective_line)));
        self.append_latest_log("objective", &objective_line);

        let mut iterations: u32 = 0;
        let mut turn_usage = Usage::default();
        let mut denied_signatures: HashSet<String> = HashSet::new();
        let mut turn_state = orchestrator::TurnRuntimeState::new();
        let read_cache = self.read_cache.clone();
        let mut objective_warning_emitted = false;
        let mut steering_final_followup_emitted = false;
        let mut last_retry_reason: Option<String> = None;
        let mut workaround_fired_this_turn = false;

        self.set_work_phase(turn_state.phase().label());
        self.sink.emit(AgentEvent::Info(format!(
            "[phase:{}] validate one representative source item before scaling",
            turn_state.phase().label()
        )));

        if self.provider_requires_api_key && self.api_key.trim().is_empty() {
            anyhow::bail!(
                "missing credentials for provider '{}'. run `/login {}` in REPL or `wolf auth login {}`.",
                self.provider_id,
                self.provider_id,
                self.provider_id
            );
        }

        if let Some(hint) = self.browser_recipe_hint() {
            self.sink
                .emit(AgentEvent::Info(format!("[browser] {hint}")));
        }

        if self
            .compact_if_over_threshold(
                self.active_compact_threshold_chars(),
                "after_active_compact_attempt",
            )
            .await
        {
            compacted_this_turn = true;
        }

        loop {
            if let Some(msg) = self.budget_cap_denial() {
                self.sink.emit(AgentEvent::Warn(msg.clone()));
                self.append_latest_log("budget_cap_stop", &msg);
                self.history.push(Message {
                    role: "assistant".to_string(),
                    content: vec![Block::Text { text: msg }],
                });
                break;
            }
            if let Some(cap) = self.max_iterations
                && iterations >= cap
            {
                self.sink.emit(AgentEvent::Warn(format!(
                    "[halted: max_iterations={cap} reached]"
                )));
                break;
            }
            iterations += 1;

            let chatgpt_session_id = (self.api_provider == ApiProvider::ChatGpt).then(|| {
                format!(
                    "wolf-{}-{}",
                    self.provider_id,
                    project_key(&self.sandbox_root)
                )
            });
            let (sys_stable, sys_env) = self.compose_system_parts();
            let sys_blocks = vec![
                SystemBlock {
                    kind: "text",
                    text: &sys_stable,
                    cache_control: Some(CacheControl::EPHEMERAL),
                },
                SystemBlock {
                    kind: "text",
                    text: &sys_env,
                    cache_control: None,
                },
            ];
            let wire_tools = self.wire_tools();
            let (url, req_body) = self.build_streaming_request(
                &sys_stable,
                &sys_env,
                &sys_blocks,
                &wire_tools,
                chatgpt_session_id.as_deref().unwrap_or("wolf"),
            )?;
            let mut stream_attempt: u32 = 0;
            let (blocks, _stop_reason, mut usage) = 'stream_retry: loop {
                stream_attempt += 1;
                let mut attempt: u32 = 0;
                let mut provider_workaround_used = false;
                let resp = loop {
                    attempt += 1;
                    let req = apply_provider_headers(
                        self.http_client()
                            .post(&url)
                            .header("content-type", "application/json")
                            .header("accept", "text/event-stream")
                            .body(req_body.clone()),
                        self.api_provider,
                        &self.api_key,
                        chatgpt_session_id.as_deref(),
                    )?;
                    let mut interrupt_ticker =
                        tokio::time::interval(std::time::Duration::from_millis(25));
                    let res = tokio::select! {
                        _ = async {
                            loop {
                                if self.interrupt.load(Ordering::SeqCst) {
                                    break;
                                }
                                interrupt_ticker.tick().await;
                            }
                        } => anyhow::bail!("interrupted by user before provider response"),
                        res = req.send() => res,
                    };
                    match res {
                        Ok(r) if r.status().is_success() => break r,
                        Ok(r) => {
                            let status = r.status();
                            let code = status.as_u16();
                            let retry_after = r
                                .headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.parse::<u64>().ok());
                            let text = cap_bytes_with_hint(
                                r.text().await.unwrap_or_default(),
                                HTTP_ERROR_BODY_CAP,
                                "HTTP error body truncated.",
                            );
                            self.record_provider_http_failure(status, &text, retry_after);
                            let plan = orchestrator::classify_http_failure(code, &text);

                            if matches!(code, 400 | 500)
                                && !provider_workaround_used
                                && is_provider_tool_result_id_bug(&text)
                                && self.rewrite_latest_tool_results_as_text_fallback()
                            {
                                provider_workaround_used = true;
                                workaround_fired_this_turn = true;
                                self.append_latest_log(
                                "provider_workaround",
                                "flattened latest tool_result blocks to plain text after provider tool_result-id bug",
                            );
                                self.sink.emit(AgentEvent::Warn(
                                "[provider workaround] recovered from tool_result parsing bug; retrying request"
                                    .to_string(),
                            ));
                                continue;
                            }

                            if !plan.retry || attempt >= 4 {
                                self.append_latest_log(
                                    "http_error",
                                    &format!("kind={} HTTP {status}: {text}", plan.label()),
                                );
                                anyhow::bail!("HTTP {status}: {text}");
                            }
                            let wait = retry_after
                                .unwrap_or_else(|| jittered_backoff_secs(1u64 << (attempt - 1)));
                            self.append_latest_log(
                                "http_retry",
                                &format!(
                                    "attempt={attempt} kind={} status={status} wait={wait}s",
                                    plan.label()
                                ),
                            );
                            last_retry_reason = Some(format!("{} HTTP {status}", plan.label()));
                            self.sink.emit(AgentEvent::HttpRetry {
                                attempt,
                                wait_secs: wait,
                                reason: format!("{} HTTP {status}", plan.label()),
                            });
                            turn_state.record_http_retry();
                            emit_external_telemetry(self.sink.as_mut(), &turn_state);
                            self.interrupt_aware_sleep(wait).await;
                        }
                        Err(e) => {
                            let plan = orchestrator::classify_transport_failure(
                                e.is_connect(),
                                e.is_timeout(),
                            );
                            if plan.retry && attempt < 4 {
                                let wait = jittered_backoff_secs(1u64 << (attempt - 1));
                                self.append_latest_log(
                                    "http_retry",
                                    &format!(
                                        "attempt={attempt} kind={} err={e} wait={wait}s",
                                        plan.label()
                                    ),
                                );
                                last_retry_reason = Some(format!("{} {e}", plan.label()));
                                self.sink.emit(AgentEvent::HttpRetry {
                                    attempt,
                                    wait_secs: wait,
                                    reason: format!("{} {e}", plan.label()),
                                });
                                turn_state.record_http_retry();
                                emit_external_telemetry(self.sink.as_mut(), &turn_state);
                                self.interrupt_aware_sleep(wait).await;
                                continue;
                            }
                            self.append_latest_log(
                                "http_error",
                                &format!("kind={} request failed: {e}", plan.label()),
                            );
                            return Err(e.into());
                        }
                    }
                };

                match self.parse_stream_response(resp).await {
                    Ok(result) => break 'stream_retry result,
                    Err(e) => {
                        let body = stream_error_body(&e);
                        self.record_provider_stream_failure(&body);
                        let plan = orchestrator::classify_stream_error(&body);
                        if plan.retry && stream_attempt < MAX_STREAM_ATTEMPTS {
                            let wait = jittered_backoff_secs(1u64 << (stream_attempt - 1));
                            self.append_latest_log(
                                "stream_retry",
                                &format!(
                                    "attempt={stream_attempt} kind={} wait={wait}s body={body}",
                                    plan.label()
                                ),
                            );
                            last_retry_reason = Some(format!("{} stream error", plan.label()));
                            self.sink.emit(AgentEvent::HttpRetry {
                                attempt: stream_attempt,
                                wait_secs: wait,
                                reason: format!("{} stream error", plan.label()),
                            });
                            turn_state.record_http_retry();
                            emit_external_telemetry(self.sink.as_mut(), &turn_state);
                            self.interrupt_aware_sleep(wait).await;
                            continue 'stream_retry;
                        }
                        if self.api_provider == ApiProvider::ChatGpt {
                            let partial_blocks = self.partial_chatgpt_stream_blocks();
                            if maybe_preserve_partial_stream(&partial_blocks, &mut self.history) {
                                self.checkpoint_latest_session("after_partial_stream_preserve");
                                self.sink.emit(AgentEvent::Warn(
                                    "provider closed the stream after partial text; preserved partial response instead of replaying the same turn".to_string(),
                                ));
                                break 'stream_retry (
                                    partial_blocks,
                                    Some("partial_stream_eof".to_string()),
                                    Usage::default(),
                                );
                            }
                        }
                        self.append_latest_log(
                            "stream_error",
                            &format!("kind={} {body}", plan.label()),
                        );
                        return Err(e);
                    }
                }
            };

            if usage.input == 0 && usage.output == 0 {
                let chars = self.history_chars() as u64
                    + blocks
                        .iter()
                        .map(|b| match b {
                            Block::Text { text } | Block::PartialStream { text } => text.len(),
                            Block::Thinking { text } => text.len(),
                            Block::ToolUse { input, .. } => json_byte_len(input),
                            Block::ToolResult { content, .. } => content.len(),
                        })
                        .sum::<usize>() as u64;
                let estimated = (chars / 4).max(1);
                usage.input = estimated;
            }

            self.session_usage.add(usage);
            turn_usage.add(usage);
            // `turn` carries the single request that just completed, so the
            // TUI context meter reflects the last request's actual size
            // instead of the running sum of every iteration in this turn.
            self.sink.emit(AgentEvent::UsageUpdate {
                turn: usage,
                session: self.session_usage,
            });
            if let Some(msg) = self.update_budget_state_after_usage() {
                self.sink.emit(AgentEvent::Warn(msg.clone()));
                self.append_latest_log("budget_cap_reached", &msg);
            }

            if maybe_preserve_partial_stream(&blocks, &mut self.history) {
                self.checkpoint_latest_session("after_assistant_message");
            } else {
                // maybe_preserve_partial_stream skips messages that lack Text/PartialStream
                // blocks (e.g. ChatGPT responses that are pure tool_use). If the assistant
                // message wasn't pushed and we're about to execute tool calls, we must
                // push it now — otherwise the history goes user→user instead of
                // assistant→user and the provider loops on repeated tool_result blocks
                // with no matching assistant tool_use.
                let has_tool_use = blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
                if has_tool_use {
                    self.history.push(Message {
                        role: "assistant".to_string(),
                        content: blocks.clone(),
                    });
                    self.checkpoint_latest_session("after_assistant_message");
                }
            }

            let tool_calls: Vec<(String, String, Value)> = blocks
                .into_iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } => Some((id, name, input)),
                    _ => None,
                })
                .collect();

            if tool_calls.is_empty() {
                if self.inject_queued_steering(
                    &mut turn_state,
                    iterations,
                    self.history
                        .iter()
                        .map(|m| {
                            m.content
                                .iter()
                                .filter(|b| matches!(b, Block::ToolResult { .. }))
                                .count()
                        })
                        .sum(),
                    true,
                ) {
                    continue;
                }
                if !objective_warning_emitted
                    && let Some(reminder) =
                        orchestrator::objective_runtime_reminder(&objective, &self.history)
                {
                    self.sink.emit(AgentEvent::Warn(reminder.clone()));
                    self.append_latest_log("objective_unresolved", &reminder);
                    self.history.push(Message {
                        role: "user".to_string(),
                        content: vec![Block::Text {
                            text: format!("[runtime-note] {reminder}"),
                        }],
                    });
                    self.checkpoint_latest_session("after_objective_warning");
                    objective_warning_emitted = true;
                    if let Some((_, msg)) =
                        turn_state.advance_phase(orchestrator::PhaseTrigger::FinalResponse)
                    {
                        self.mark_work_done("deliver requested outcome with verifiable steps");
                        self.mark_work_done("implement requested changes");
                        self.mark_work_done("run verification checks");
                        self.set_work_phase(turn_state.phase().label());
                        self.sink.emit(AgentEvent::Info(format!(
                            "[phase:{}] {msg}",
                            turn_state.phase().label()
                        )));
                    }
                    continue;
                }
                if let Some((_, msg)) =
                    turn_state.advance_phase(orchestrator::PhaseTrigger::FinalResponse)
                {
                    self.mark_work_done("deliver requested outcome with verifiable steps");
                    self.mark_work_done("implement requested changes");
                    self.mark_work_done("run verification checks");
                    self.set_work_phase(turn_state.phase().label());
                    self.sink.emit(AgentEvent::Info(format!(
                        "[phase:{}] {msg}",
                        turn_state.phase().label()
                    )));
                }
                if !steering_final_followup_emitted {
                    let unresolved_steering = self.unresolved_steering_items();
                    if !unresolved_steering.is_empty() {
                        steering_final_followup_emitted = true;
                        let followup = format!(
                            "[queued-update] Your previous final response missed a queued user update. \
                             Reply now with only a concise final addendum that addresses the update and states the outcome.\n\n\
                             Queued update(s): {}",
                            unresolved_steering.join("; ")
                        );
                        self.append_latest_log(
                            "steering_final_followup",
                            &summarize_inline(&unresolved_steering.join("; "), 240),
                        );
                        self.history.push(Message {
                            role: "user".to_string(),
                            content: vec![Block::Text { text: followup }],
                        });
                        self.checkpoint_latest_session("after_queued_update_followup");
                        continue;
                    }
                }
                self.append_latest_log(
                    "chat_response_complete",
                    "assistant replied without tool calls",
                );
                break;
            }

            if self.interrupt.load(Ordering::SeqCst) {
                self.append_latest_log("tool_round_interrupted", "before tool execution");
                let results: Vec<Block> = tool_calls
                    .into_iter()
                    .map(|(id, _, _)| Block::ToolResult {
                        tool_use_id: id,
                        content: "interrupted by user before tool execution".to_string(),
                        is_error: Some(true),
                        metadata: ToolResultMetadata {
                            status: Some("interrupted".to_string()),
                            ..ToolResultMetadata::default()
                        },
                    })
                    .collect();
                self.history.push(Message {
                    role: "user".to_string(),
                    content: results,
                });
                anyhow::bail!("interrupted by user");
            }

            enum Plan {
                Immediate {
                    content: String,
                    is_error: Option<bool>,
                },
                Builtin,
            }

            struct PlannedCall {
                tool_use_id: String,
                event_call_id: String,
                name: String,
                input: Value,
                input_str: String,
                summary: String,
                hosts: Vec<String>,
                bulk_network: bool,
                cache_key: Option<String>,
                bash_similarity_key: Option<String>,
                plan: Plan,
            }

            let mut plans: Vec<PlannedCall> = Vec::new();
            for (ordinal, (id, name, mut input)) in tool_calls.into_iter().enumerate() {
                let event_call_id = normalize_tool_call_id(&id, 0, ordinal);
                let mut input_str = input.to_string();
                let summary = summarize_call(&name, &input);
                let call_sig = format!("{name}\n{input_str}");
                let hosts = tool_policy::hosts_for_tool_call(&name, &input);
                let bulk_network = tool_policy::looks_like_bulk_network_call(&name, &input);
                let cache_key = orchestrator::network_cache_key(&name, &input);
                let bash_similarity_key = if name == "bash" {
                    Some(orchestrator::normalize_bash_similarity_key(
                        input["command"].as_str().unwrap_or(""),
                    ))
                } else {
                    None
                };

                let mut plan: Option<Plan> = None;

                if let Err(msg) = tool_policy::validate_tool_input(&name, &input) {
                    plan = Some(Plan::Immediate {
                        content: msg,
                        is_error: Some(true),
                    });
                }

                // Subagents run detached and report through request/report/stdout files.
                if plan.is_none() && name == "subagent" {
                    let task = input["task"].as_str().unwrap_or_default().trim();
                    if task.is_empty() {
                        plan = Some(Plan::Immediate {
                            content: "subagent task is required".to_string(),
                            is_error: Some(true),
                        });
                    } else {
                        match SubagentRequest::from_input(self, &input)
                            .and_then(|request| {
                                write_subagent_request(&self.sandbox_root, &request)
                                    .map_err(|e| format!("write subagent request: {e:#}"))
                            })
                            .and_then(|(request_path, report_path)| {
                                spawn_detached_subagent_process(
                                    &self.sandbox_root,
                                    &request_path,
                                    &report_path,
                                )
                                .map(|launch| (request_path, launch))
                                .map_err(|e| format!("launch detached subagent: {e:#}"))
                            }) {
                            Ok((request_path, launch)) => {
                                plan = Some(Plan::Immediate {
                                    content: format!(
                                        "detached subagent launched in {}\ncommand: {}\nrequest: {}\nreport: {}\nstdout/stderr: {}\nmonitor: {}\nUse the monitor command to watch live progress; inspect the report after completion.",
                                        launch.label,
                                        launch.command,
                                        request_path.display(),
                                        launch.report_path.display(),
                                        launch.stdout_path.display(),
                                        launch.monitor_command
                                    ),
                                    is_error: None,
                                });
                            }
                            Err(msg) => {
                                plan = Some(Plan::Immediate {
                                    content: msg,
                                    is_error: Some(true),
                                });
                            }
                        }
                    }
                }

                if plan.is_none()
                    && let Some((cached_content, cached_error)) =
                        turn_state.dedupe_guard(cache_key.as_deref())
                {
                    emit_external_telemetry(self.sink.as_mut(), &turn_state);
                    plan = Some(Plan::Immediate {
                        content: cached_content,
                        is_error: cached_error,
                    });
                }

                if plan.is_none()
                    && let Some(msg) = turn_state.bash_similarity_guard(
                        bash_similarity_key.as_deref(),
                        input["command"].as_str(),
                    )
                {
                    emit_external_telemetry(self.sink.as_mut(), &turn_state);
                    plan = Some(Plan::Immediate {
                        content: msg,
                        is_error: Some(true),
                    });
                }

                if plan.is_none()
                    && let Some(msg) = turn_state.blocked_host_guard(&hosts)
                {
                    plan = Some(Plan::Immediate {
                        content: msg,
                        is_error: Some(true),
                    });
                }

                if plan.is_none()
                    && let Some(msg) = turn_state.feasibility_guard(&hosts, bulk_network)
                {
                    plan = Some(Plan::Immediate {
                        content: msg,
                        is_error: Some(true),
                    });
                }

                if plan.is_none()
                    && let Some(msg) = self.sandbox_policy_denial(&name, &input)
                {
                    plan = Some(Plan::Immediate {
                        content: msg,
                        is_error: Some(true),
                    });
                }

                if plan.is_none()
                    && let Some(msg) = self.privacy.path_denial(&name, &input)
                {
                    plan = Some(Plan::Immediate {
                        content: msg,
                        is_error: Some(true),
                    });
                }

                if plan.is_none() && name == "subagent" {
                    match SubagentRequest::from_input(self, &input) {
                        Ok(request) => {
                            input = serde_json::to_value(request).unwrap_or_else(|_| input.clone());
                            input_str = input.to_string();
                            let detached_sig = format!("{name}\n{input_str}");
                            if denied_signatures.contains(&detached_sig) {
                                plan = Some(Plan::Immediate {
                                    content: "permission denied by user — do not retry this tool call; ask the user instead".to_string(),
                                    is_error: Some(true),
                                });
                            } else {
                                match self.sink.request_permission(&name, &input) {
                                    Choice::Once => {}
                                    Choice::Always => {
                                        self.allowed.insert(name.clone());
                                    }
                                    Choice::Deny => {
                                        denied_signatures.insert(detached_sig);
                                        plan = Some(Plan::Immediate {
                                            content: "permission denied by user — do not retry this tool call; ask the user instead".to_string(),
                                            is_error: Some(true),
                                        });
                                    }
                                }
                            }
                        }
                        Err(msg) => {
                            plan = Some(Plan::Immediate {
                                content: msg,
                                is_error: Some(true),
                            });
                        }
                    }
                }

                if plan.is_none() {
                    let approved = if name == "subagent" {
                        true
                    } else if self.deny_tools.contains(&name)
                        || denied_signatures.contains(&call_sig)
                        || (needs_permission(&name)
                            && self.approval_profile == ApprovalProfile::Never)
                    {
                        false
                    } else if needs_permission(&name) && !self.tool_auto_approved(&name, &input) {
                        match self.sink.request_permission(&name, &input) {
                            Choice::Once => true,
                            Choice::Always => {
                                self.allowed.insert(name.clone());
                                true
                            }
                            Choice::Deny => false,
                        }
                    } else {
                        true
                    };

                    if !approved {
                        denied_signatures.insert(call_sig.clone());
                        plan = Some(Plan::Immediate {
                            content: "permission denied by user — do not retry this tool call; ask the user instead".to_string(),
                            is_error: Some(true),
                        });
                    } else {
                        let pre_env = [
                            ("WOLF_TOOL_NAME", name.as_str()),
                            ("WOLF_TOOL_INPUT", input_str.as_str()),
                        ];
                        let mut blocked: Option<String> = None;
                        for (out, code) in
                            self.hooks
                                .fire("pre_tool", &name, &pre_env, &self.sandbox_root)
                        {
                            if code != 0 {
                                blocked = Some(format!(
                                    "pre_tool hook blocked (exit {code}):\n{}",
                                    out.trim()
                                ));
                                break;
                            }
                        }
                        plan = Some(match blocked {
                            Some(msg) => Plan::Immediate {
                                content: msg,
                                is_error: Some(true),
                            },
                            None => Plan::Builtin,
                        });
                    }
                }

                let plan = plan.expect("plan must be set");

                if matches!(plan, Plan::Builtin)
                    && matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit")
                {
                    self.work_ledger_note_file_change(&input);
                }

                if matches!(plan, Plan::Builtin)
                    && bulk_network
                    && let Some((_, msg)) =
                        turn_state.advance_phase(orchestrator::PhaseTrigger::ScaleCollection)
                {
                    self.set_work_phase(turn_state.phase().label());
                    self.sink.emit(AgentEvent::Info(format!(
                        "[phase:{}] {msg}",
                        turn_state.phase().label()
                    )));
                }
                if matches!(plan, Plan::Builtin)
                    && matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit")
                    && let Some((_, msg)) =
                        turn_state.advance_phase(orchestrator::PhaseTrigger::DeliverableWrite)
                {
                    self.set_work_phase(turn_state.phase().label());
                    self.sink.emit(AgentEvent::Info(format!(
                        "[phase:{}] {msg}",
                        turn_state.phase().label()
                    )));
                }

                plans.push(PlannedCall {
                    tool_use_id: id,
                    event_call_id,
                    name,
                    input,
                    input_str,
                    summary,
                    hosts,
                    bulk_network,
                    cache_key,
                    bash_similarity_key,
                    plan,
                });
            }

            let runnable_indices: Vec<usize> = plans
                .iter()
                .enumerate()
                .filter_map(|(idx, p)| {
                    if matches!(p.plan, Plan::Builtin) {
                        Some(idx)
                    } else {
                        None
                    }
                })
                .collect();
            let runnable_set: HashSet<usize> = runnable_indices.iter().copied().collect();
            let batch_id = format!("batch-{iterations}");
            if runnable_indices.len() > 1 {
                let call_ids: Vec<String> = runnable_indices
                    .iter()
                    .map(|idx| plans[*idx].event_call_id.clone())
                    .collect();
                let labels: Vec<String> = runnable_indices
                    .iter()
                    .map(|idx| plans[*idx].summary.clone())
                    .collect();
                self.sink.emit(AgentEvent::ToolBatchStart {
                    batch_id: batch_id.clone(),
                    call_ids,
                    labels,
                });
            }

            let builtin_indices: Vec<usize> = plans
                .iter()
                .enumerate()
                .filter_map(|(idx, p)| {
                    if matches!(p.plan, Plan::Builtin) {
                        Some(idx)
                    } else {
                        None
                    }
                })
                .collect();
            let mut builtin_started_at: HashMap<usize, std::time::Instant> = HashMap::new();
            let builtin_names: Vec<&str> = builtin_indices
                .iter()
                .map(|idx| plans[*idx].name.as_str())
                .collect();
            let parallel_builtin_round = should_parallelize_builtin_tools(&builtin_names);

            let mut builtin_outputs: HashMap<usize, std::result::Result<String, String>> =
                HashMap::new();
            if parallel_builtin_round {
                let builtin_handles: Vec<(
                    usize,
                    tokio::task::JoinHandle<std::result::Result<String, String>>,
                )> = builtin_indices
                    .iter()
                    .map(|idx| {
                        let root = self.sandbox_root.clone();
                        let n = plans[*idx].name.clone();
                        let inp = plans[*idx].input.clone();
                        let summary = plans[*idx].summary.clone();
                        self.sink.emit(AgentEvent::ToolCallStart {
                            call_id: plans[*idx].event_call_id.clone(),
                            name: n.clone(),
                            summary: summary.clone(),
                        });
                        self.append_latest_log("tool_start", &summary);
                        builtin_started_at.insert(*idx, std::time::Instant::now());
                        let interrupt = self.interrupt.clone();
                        let sem = self.builtin_semaphore.clone();
                        let read_cache = read_cache.clone();
                        let handle = tokio::spawn(async move {
                            let _permit = match sem.acquire_owned().await {
                                Ok(p) => p,
                                Err(e) => return Err(format!("builtin semaphore closed: {e}")),
                            };
                            execute_builtin_call(n, inp, root, interrupt, Some(read_cache)).await
                        });
                        (*idx, handle)
                    })
                    .collect();

                for (idx, handle) in builtin_handles {
                    let r = match handle.await {
                        Ok(v) => v,
                        Err(e) => Err(format!("task panic: {e}")),
                    };
                    builtin_outputs.insert(idx, r);
                }
            } else {
                for idx in builtin_indices {
                    let root = self.sandbox_root.clone();
                    let n = plans[idx].name.clone();
                    let inp = plans[idx].input.clone();
                    let summary = plans[idx].summary.clone();
                    self.sink.emit(AgentEvent::ToolCallStart {
                        call_id: plans[idx].event_call_id.clone(),
                        name: n.clone(),
                        summary: summary.clone(),
                    });
                    self.append_latest_log("tool_start", &summary);
                    builtin_started_at.insert(idx, std::time::Instant::now());
                    let interrupt = self.interrupt.clone();
                    let r = execute_builtin_call(n, inp, root, interrupt, Some(read_cache.clone()))
                        .await;
                    builtin_outputs.insert(idx, r);
                }
            }

            let mut batch_failed = 0usize;
            let mut batch_call_ids: Vec<String> = Vec::new();
            let mut batch_labels: Vec<String> = Vec::new();
            let mut results = Vec::new();
            let mut round_external_failures: usize = 0;
            for (idx, p) in plans.into_iter().enumerate() {
                let PlannedCall {
                    tool_use_id,
                    event_call_id,
                    name,
                    input,
                    input_str,
                    summary,
                    hosts,
                    bulk_network: _bulk_network,
                    cache_key,
                    bash_similarity_key,
                    plan,
                } = p;

                let started_at = builtin_started_at.remove(&idx);
                let ran_builtin = matches!(plan, Plan::Builtin);
                let ui_summary = summary.clone();
                let mut followup_warnings: Vec<String> = Vec::new();
                if let Some(advisory) = tool_policy::tool_input_advisory(&name, &input) {
                    followup_warnings.push(advisory);
                }

                let (mut content, is_error) = match plan {
                    Plan::Immediate { content, is_error } => (content, is_error),
                    Plan::Builtin => match builtin_outputs.remove(&idx).unwrap() {
                        Ok(s) => {
                            if name == "bash" {
                                let failed = parse_bash_exit_code(&s).is_some_and(|code| code != 0);
                                (s, failed.then_some(true))
                            } else {
                                (s, None)
                            }
                        }
                        Err(e) => (e, Some(true)),
                    },
                };

                let post_env = [
                    ("WOLF_TOOL_NAME", name.as_str()),
                    ("WOLF_TOOL_INPUT", input_str.as_str()),
                    ("WOLF_TOOL_RESULT", content.as_str()),
                ];
                for (out, _code) in
                    self.hooks
                        .fire("post_tool", &name, &post_env, &self.sandbox_root)
                {
                    let t = out.trim();
                    if !t.is_empty() {
                        content.push_str(&format!("\n\n[hook:post_tool]\n{t}"));
                    }
                }

                let ok = !is_error.unwrap_or(false);
                let privacy_redaction = self.privacy.apply_tool_output(&name, &input, content);
                let redacted_count = privacy_redaction.counts.total();
                content = privacy_redaction.text;
                if redacted_count > 0 {
                    followup_warnings.push(format!(
                        "privacy redacted {} from {name} output before model context",
                        privacy_redaction.counts.summary()
                    ));
                }
                let observation = turn_state.record_external_outcome(
                    &name,
                    &hosts,
                    cache_key.as_deref(),
                    bash_similarity_key.as_deref(),
                    input["command"].as_str(),
                    &mut content,
                    is_error,
                );
                emit_external_telemetry(self.sink.as_mut(), &turn_state);
                round_external_failures =
                    round_external_failures.saturating_add(observation.round_external_failures);
                followup_warnings.extend(observation.followup_warnings);

                if runnable_set.contains(&idx) {
                    if !ok {
                        batch_failed = batch_failed.saturating_add(1);
                    }
                    batch_call_ids.push(event_call_id.clone());
                    batch_labels.push(ui_summary.clone());
                }

                let ui_cap = orchestrator::adaptive_tool_ui_cap(
                    &self.session_usage,
                    &self.model,
                    TOOL_UI_CONTENT_CAP,
                );
                let ui_content = orchestrator::compress_tool_ui_content(&content, ui_cap);
                self.sink.emit(AgentEvent::ToolCallResult {
                    call_id: event_call_id.clone(),
                    name: name.clone(),
                    ok,
                    preview: ui_summary.clone(),
                    content: ui_content,
                });
                for warning in followup_warnings {
                    self.sink.emit(AgentEvent::Warn(warning));
                }
                self.append_latest_log(
                    if ok { "tool_ok" } else { "tool_error" },
                    &format!("{} :: {}", ui_summary, content),
                );
                let verification_command =
                    if ran_builtin && matches!(name.as_str(), "bash" | "csvkit") {
                        if name == "bash" {
                            serde_json::from_str::<Value>(&input_str)
                                .ok()
                                .and_then(|v| v["command"].as_str().map(String::from))
                        } else {
                            Some(ui_summary.clone())
                        }
                    } else {
                        None
                    };
                let is_verification_result = verification_command
                    .as_deref()
                    .is_some_and(looks_like_verification_command);
                let mut artifact_display: Option<String> = None;
                let mut verification_status: Option<String> = None;
                if is_verification_result {
                    let command = verification_command.clone().unwrap_or_default();
                    let duration = started_at
                        .map(|t| t.elapsed())
                        .unwrap_or_else(|| std::time::Duration::from_millis(0));
                    let exit_code = parse_tool_exit_code(&name, ok, &content);
                    let status = if ok && exit_code.unwrap_or(0) == 0 {
                        "passed"
                    } else {
                        "failed"
                    };
                    let artifact = write_verification_artifact(
                        &self.sandbox_root,
                        &ui_summary,
                        &command,
                        &content,
                        exit_code,
                        duration,
                        status,
                    );
                    artifact_display = artifact.as_ref().map(|p| p.display().to_string());
                    verification_status = Some(status.to_string());
                    self.work_ledger.verification.push(VerificationRecord {
                        name: ui_summary.clone(),
                        command: command.clone(),
                        status: status.to_string(),
                        exit_code,
                        duration_ms: millis_u64(duration),
                        artifact: artifact_display.clone(),
                        validates: Vec::new(),
                    });
                    if self.work_ledger.verification.len() > 24 {
                        let excess = self.work_ledger.verification.len() - 24;
                        self.work_ledger.verification.drain(0..excess);
                    }
                    if let Some(path) = artifact_display.as_deref() {
                        self.append_latest_log(
                            "verification",
                            &format!("{status} {ui_summary} artifact={path}"),
                        );
                    }
                }
                let dynamic_result_cap = tool_result_context_cap(
                    &name,
                    &input,
                    &self.session_usage,
                    &self.model,
                    self.context_mode,
                );
                let result_status = verification_status
                    .unwrap_or_else(|| if ok { "ok" } else { "error" }.to_string());
                let exit_code = parse_tool_exit_code(&name, ok, &content);
                let result_duration_ms = started_at.map(|t| millis_u64(t.elapsed()));
                let result_artifact = artifact_display.clone();
                let result_hint = if let Some(path) = result_artifact.as_deref() {
                    format!("Full verification output saved as a structured artifact: {path}")
                } else {
                    "Full verification output saved as a structured artifact; see verification ledger.".to_string()
                };
                results.push(Block::ToolResult {
                    tool_use_id,
                    content: if is_verification_result {
                        cap_bytes_with_hint(content, dynamic_result_cap, &result_hint)
                    } else {
                        cap_tool_output_with_cap(content, dynamic_result_cap)
                    },
                    is_error,
                    metadata: ToolResultMetadata {
                        status: Some(result_status),
                        exit_code,
                        duration_ms: result_duration_ms,
                        artifact: result_artifact,
                    },
                });
            }

            if runnable_indices.len() > 1 {
                self.sink.emit(AgentEvent::ToolBatchEnd {
                    batch_id,
                    call_ids: batch_call_ids,
                    labels: batch_labels,
                    failed: batch_failed,
                });
            }

            self.history.push(Message {
                role: "user".to_string(),
                content: results,
            });
            self.checkpoint_latest_session("after_tool_results");

            if turn_state.should_emit_partial_delivery_hint(round_external_failures) {
                let hint = orchestrator::partial_delivery_hint().to_string();
                self.sink.emit(AgentEvent::Warn(hint.clone()));
                self.append_latest_log("partial_delivery_hint", &hint);
                self.history.push(Message {
                    role: "user".to_string(),
                    content: vec![Block::Text {
                        text: format!("[runtime-note] {hint}"),
                    }],
                });
                self.checkpoint_latest_session("after_partial_delivery_hint");
                turn_state.mark_partial_delivery_hint_emitted();
                emit_external_telemetry(self.sink.as_mut(), &turn_state);
                if let Some((_, msg)) =
                    turn_state.advance_phase(orchestrator::PhaseTrigger::PartialDeliveryFallback)
                {
                    self.set_work_phase(turn_state.phase().label());
                    self.sink.emit(AgentEvent::Info(format!(
                        "[phase:{}] {msg}",
                        turn_state.phase().label()
                    )));
                }
            }

            if self.interrupt.load(Ordering::SeqCst) {
                anyhow::bail!("interrupted by user after tool round");
            }

            if self
                .compact_if_over_threshold(
                    self.active_compact_threshold_chars(),
                    "after_active_compact_attempt",
                )
                .await
            {
                compacted_this_turn = true;
            }

            let tool_count_after_results = self
                .history
                .iter()
                .map(|m| {
                    m.content
                        .iter()
                        .filter(|b| matches!(b, Block::ToolResult { .. }))
                        .count()
                })
                .sum();
            if self.inject_queued_steering(
                &mut turn_state,
                iterations,
                tool_count_after_results,
                false,
            ) {
                continue;
            }
        }

        self.work_ledger
            .blocked
            .retain(|item| !item.starts_with("final objective warning:"));
        if !self.work_ledger.steering.is_empty() {
            let unresolved_steering = self.unresolved_steering_items();
            self.work_ledger
                .blocked
                .retain(|item| !item.starts_with("queued update unresolved:"));
            if unresolved_steering.is_empty() {
                self.mark_work_done("respond to queued user update");
            } else {
                self.append_latest_log(
                    "steering_unresolved_after_final",
                    &summarize_inline(&unresolved_steering.join("; "), 240),
                );
            }
        }
        if let Some(reminder) = final_objective_warning(&objective, &self.history) {
            self.sink.emit(AgentEvent::Warn(reminder.clone()));
            self.append_latest_log("objective_final_warning", &reminder);
            self.work_ledger.blocked.push(reminder);
        }
        if self
            .compact_if_over_threshold(
                self.compact_threshold_chars(),
                "after_end_turn_compact_attempt",
            )
            .await
        {
            compacted_this_turn = true;
        }
        self.sink.emit(AgentEvent::TurnDiagnostics {
            provider: self.provider_id.clone(),
            api_family: api_family_label(self.api_provider).to_string(),
            auth_source: self.key_source.clone(),
            model: self.model.clone(),
            last_retry_reason,
            workaround_fired: workaround_fired_this_turn,
            turn_duration_ms: Some(millis_u64(turn_started_at.elapsed())),
            context_mode: Some(self.context_mode),
            tool_profile: Some(self.tool_profile.as_str().to_string()),
            compacted: Some(compacted_this_turn),
        });
        self.sink.emit(AgentEvent::TurnEnd { usage: turn_usage });
        Ok(())
    }

    async fn read_stream(
        &mut self,
        resp: reqwest::Response,
    ) -> Result<(Vec<Block>, Option<String>, Usage)> {
        use std::collections::BTreeMap;

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut scan_cursor: usize = 0;
        let mut blocks: BTreeMap<usize, PartialBlock> = BTreeMap::new();
        let mut stop_reason: Option<String> = None;
        let mut usage = Usage::default();

        while let Some(chunk) =
            read_stream_next_chunk(&mut stream, &self.interrupt, "interrupted by user").await?
        {
            buf.extend_from_slice(&chunk);

            loop {
                let Some((end, sep_len)) = find_sse_delimiter(&buf, scan_cursor) else {
                    scan_cursor = buf.len();
                    break;
                };
                if end > STREAM_EVENT_BUFFER_CAP {
                    anyhow::bail!(
                        "stream event exceeded {} bytes before a delimiter; aborting to avoid unbounded buffering",
                        STREAM_EVENT_BUFFER_CAP
                    );
                }
                let raw: Vec<u8> = buf.drain(..end + sep_len).collect();
                scan_cursor = 0;
                let event_text = String::from_utf8_lossy(&raw[..end]);

                let mut event_name = String::new();
                let mut data_lines: Vec<&str> = Vec::new();
                for line in event_text.lines() {
                    if let Some(rest) = line.strip_prefix("event:") {
                        event_name = rest.trim().to_string();
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data_lines.push(rest.trim_start());
                    }
                }
                if data_lines.is_empty() {
                    continue;
                }
                let data_str = data_lines.join("\n");
                let data: Value = match serde_json::from_str(&data_str) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match event_name.as_str() {
                    "message_start" => {
                        usage = Usage::parse(&data["message"]["usage"]);
                    }
                    "content_block_start" => {
                        let idx = data["index"].as_u64().unwrap_or(0) as usize;
                        let cb = &data["content_block"];
                        let kind = cb["type"].as_str().unwrap_or("").to_string();
                        let mut pb = PartialBlock {
                            kind: kind.clone(),
                            ..Default::default()
                        };
                        if kind == "tool_use" {
                            pb.id = cb["id"].as_str().unwrap_or("").to_string();
                            pb.name = cb["name"].as_str().unwrap_or("").to_string();
                            if let Some(input) = cb.get("input") {
                                set_tool_input_json_if_meaningful(&mut pb.input_json, input);
                            }
                        }
                        blocks.insert(idx, pb);
                    }
                    "content_block_delta" => {
                        let idx = data["index"].as_u64().unwrap_or(0) as usize;
                        let delta = &data["delta"];
                        let dtype = delta["type"].as_str().unwrap_or("");
                        if let Some(pb) = blocks.get_mut(&idx) {
                            match dtype {
                                "text_delta" => {
                                    if let Some(t) = delta["text"].as_str() {
                                        self.sink.emit(AgentEvent::TextDelta(t.to_string()));
                                        pb.text.push_str(t);
                                    }
                                }
                                "thinking_delta" => {
                                    if let Some(t) = delta["thinking"].as_str() {
                                        self.sink.emit(AgentEvent::ThinkingDelta(t.to_string()));
                                        pb.text.push_str(t);
                                    }
                                }
                                "signature_delta" => {
                                    if let Some(sig) = delta["signature"].as_str() {
                                        pb.thinking_signature = Some(summarize_inline(sig, 96));
                                    }
                                }
                                "input_json_delta" => {
                                    if let Some(pj) = delta["partial_json"].as_str() {
                                        append_tool_input_json_fragment(&mut pb.input_json, pj);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "content_block_stop" => {
                        let idx = data["index"].as_u64().unwrap_or(0) as usize;
                        if let Some(pb) = blocks.get(&idx) {
                            if pb.kind == "text" {
                                self.sink
                                    .emit(AgentEvent::TextBlockComplete(pb.text.clone()));
                            } else if pb.kind == "thinking" {
                                self.sink
                                    .emit(AgentEvent::ThinkingBlockComplete(pb.text.clone()));
                            } else if pb.kind == "tool_use" {
                                let gated =
                                    needs_permission(&pb.name) && !self.allowed.contains(&pb.name);
                                if !gated {
                                    let preview_input = parse_tool_input_json(&pb.input_json);
                                    let summary = summarize_call(&pb.name, &preview_input);
                                    let call_id = normalize_tool_call_id(&pb.id, 0, idx);
                                    self.sink.emit(AgentEvent::ToolCallPreview {
                                        call_id,
                                        name: pb.name.clone(),
                                        summary,
                                    });
                                }
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                            stop_reason = Some(sr.to_string());
                        }
                        if let Some(o) = data["usage"]["output_tokens"].as_u64() {
                            usage.output = o;
                        }
                    }
                    "message_stop" => {}
                    "error" => {
                        anyhow::bail!("stream error: {}", data);
                    }
                    _ => {}
                }
            }

            if buf.len() > STREAM_EVENT_BUFFER_CAP {
                anyhow::bail!(
                    "stream buffer exceeded {} bytes without an event boundary; aborting to avoid unbounded buffering",
                    STREAM_EVENT_BUFFER_CAP
                );
            }
        }

        let finalized: Vec<Block> = blocks
            .into_values()
            .filter_map(|pb| pb.finalize())
            .collect();
        Ok((finalized, stop_reason, usage))
    }

    async fn read_stream_oai(
        &mut self,
        resp: reqwest::Response,
    ) -> Result<(Vec<Block>, Option<String>, Usage)> {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut scan_cursor: usize = 0;
        let mut text_buf = String::new();
        let mut tool_calls: std::collections::BTreeMap<usize, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut usage = Usage::default();
        let mut finish_reason: Option<String> = None;

        while let Some(chunk) =
            read_stream_next_chunk(&mut stream, &self.interrupt, "interrupted by user").await?
        {
            buf.extend_from_slice(&chunk);

            loop {
                let Some((end, sep_len)) = find_sse_delimiter(&buf, scan_cursor) else {
                    scan_cursor = buf.len();
                    break;
                };
                let raw: Vec<u8> = buf.drain(..end + sep_len).collect();
                scan_cursor = 0;
                let event_text = String::from_utf8_lossy(&raw[..end]);

                let mut data_line: Option<&str> = None;
                for line in event_text.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        data_line = Some(rest.trim());
                        break;
                    }
                }
                let data_str = match data_line {
                    Some(d) => d,
                    None => continue,
                };
                if data_str == "[DONE]" {
                    break;
                }
                let data: Value = match serde_json::from_str(data_str) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let choices = &data["choices"];
                if let Some(arr) = choices.as_array() {
                    for choice in arr {
                        let delta = &choice["delta"];
                        if let Some(reason) = choice["finish_reason"].as_str() {
                            finish_reason = Some(reason.to_string());
                        }
                        if let Some(content) = delta["content"].as_str() {
                            self.partial_stream_text
                                .get_or_insert_with(String::new)
                                .push_str(content);
                            self.sink.emit(AgentEvent::TextDelta(content.to_string()));
                            text_buf.push_str(content);
                        }
                        if let Some(tcs) = delta["tool_calls"].as_array() {
                            for tc in tcs {
                                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                                let entry = tool_calls.entry(idx).or_insert_with(|| {
                                    (
                                        tc["id"].as_str().unwrap_or("").to_string(),
                                        tc["function"]["name"].as_str().unwrap_or("").to_string(),
                                        String::new(),
                                    )
                                });
                                if let Some(id) = tc["id"].as_str() {
                                    entry.0 = id.to_string();
                                }
                                if let Some(name) = tc["function"]["name"].as_str() {
                                    entry.1 = name.to_string();
                                }
                                if let Some(args) = tc["function"]["arguments"].as_str() {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                    }
                }

                if let Some(u) = data.get("usage") {
                    usage = Usage::parse_openai(u);
                }
            }

            if buf.len() > STREAM_EVENT_BUFFER_CAP {
                anyhow::bail!(
                    "stream buffer exceeded {} bytes without an event boundary",
                    STREAM_EVENT_BUFFER_CAP
                );
            }
        }

        let mut blocks: Vec<Block> = Vec::new();
        if !text_buf.is_empty() {
            self.partial_stream_text = None;
            self.sink
                .emit(AgentEvent::TextBlockComplete(text_buf.clone()));
            blocks.push(Block::Text { text: text_buf });
        }
        for (idx, (id, name, args)) in tool_calls {
            let input: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
            let summary = summarize_call(&name, &input);
            let call_id = normalize_tool_call_id(&id, 0, idx);
            self.sink.emit(AgentEvent::ToolCallPreview {
                call_id,
                name: name.clone(),
                summary,
            });
            blocks.push(Block::ToolUse { id, name, input });
        }
        Ok((blocks, finish_reason, usage))
    }

    fn partial_chatgpt_stream_blocks(&mut self) -> Vec<Block> {
        let mut blocks = Vec::new();
        let text = self.partial_stream_text.take().unwrap_or_default();
        let text = text.trim_end();
        if !text.is_empty() {
            blocks.push(Block::PartialStream {
                text: format!("{text}\n\n[stream ended early; preserved partial response]"),
            });
        }
        blocks
    }

    async fn read_stream_chatgpt(
        &mut self,
        resp: reqwest::Response,
    ) -> Result<(Vec<Block>, Option<String>, Usage)> {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut scan_cursor: usize = 0;
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut usage = Usage::default();
        let mut finish_reason: Option<String> = None;
        // Keyed by item_id (fc_...). Value: (name, arguments_json, call_id).
        // item_id is what SSE delta/done events carry; call_id is what we store on Block
        // so tool results can pair back to the call.
        let mut tool_calls: std::collections::BTreeMap<String, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut tool_call_order: Vec<String> = Vec::new();

        while let Some(chunk) =
            read_stream_next_chunk(&mut stream, &self.interrupt, "interrupted by user").await?
        {
            buf.extend_from_slice(&chunk);

            loop {
                let Some((end, sep_len)) = find_sse_delimiter(&buf, scan_cursor) else {
                    scan_cursor = buf.len();
                    break;
                };
                let raw: Vec<u8> = buf.drain(..end + sep_len).collect();
                scan_cursor = 0;
                let event_text = String::from_utf8_lossy(&raw[..end]);

                let mut data_line: Option<&str> = None;
                for line in event_text.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        data_line = Some(rest.trim());
                        break;
                    }
                }
                let data_str = match data_line {
                    Some(d) => d,
                    None => continue,
                };
                if data_str == "[DONE]" {
                    finish_reason.get_or_insert_with(|| "completed".to_string());
                    break;
                }
                let data: Value = match serde_json::from_str(data_str) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let event_type = data["type"].as_str().unwrap_or("");
                match event_type {
                    "error" => {
                        let message = data["message"]
                            .as_str()
                            .or_else(|| data["error"]["message"].as_str())
                            .unwrap_or("ChatGPT Codex request failed");
                        anyhow::bail!("{message}");
                    }
                    "response.failed" => {
                        let message = data["response"]["error"]["message"]
                            .as_str()
                            .unwrap_or("ChatGPT Codex response failed");
                        anyhow::bail!("{message}");
                    }
                    "response.output_text.delta" => {
                        if let Some(delta) = data["delta"].as_str()
                            && !delta.is_empty()
                        {
                            self.partial_stream_text
                                .get_or_insert_with(String::new)
                                .push_str(delta);
                            self.sink.emit(AgentEvent::TextDelta(delta.to_string()));
                            text_buf.push_str(delta);
                        }
                    }
                    "response.output_text.done" => {
                        if text_buf.is_empty()
                            && let Some(text) = data["text"].as_str()
                            && !text.is_empty()
                        {
                            self.partial_stream_text
                                .get_or_insert_with(String::new)
                                .push_str(text);
                            self.sink.emit(AgentEvent::TextDelta(text.to_string()));
                            text_buf.push_str(text);
                        }
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        if let Some(delta) = data["delta"].as_str()
                            && !delta.is_empty()
                        {
                            self.sink.emit(AgentEvent::ThinkingDelta(delta.to_string()));
                            reasoning_buf.push_str(delta);
                        }
                    }
                    "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                        if reasoning_buf.is_empty()
                            && let Some(text) = data["text"].as_str()
                            && !text.is_empty()
                        {
                            self.sink.emit(AgentEvent::ThinkingDelta(text.to_string()));
                            reasoning_buf.push_str(text);
                        }
                    }
                    "response.output_item.added" => {
                        let item = &data["item"];
                        if item["type"].as_str() == Some("function_call") {
                            let item_id = item["id"].as_str().unwrap_or("").to_string();
                            let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                            let name = item["name"].as_str().unwrap_or("").to_string();
                            if !item_id.is_empty() {
                                let entry =
                                    tool_calls.entry(item_id.clone()).or_insert_with(|| {
                                        tool_call_order.push(item_id.clone());
                                        (String::new(), String::new(), String::new())
                                    });
                                if entry.0.is_empty() && !name.is_empty() {
                                    entry.0 = name;
                                }
                                if entry.2.is_empty() && !call_id.is_empty() {
                                    entry.2 = call_id;
                                }
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        let item_id = data["item_id"]
                            .as_str()
                            .or_else(|| data["id"].as_str())
                            .unwrap_or("")
                            .to_string();
                        if !item_id.is_empty() {
                            let entry = tool_calls.entry(item_id.clone()).or_insert_with(|| {
                                tool_call_order.push(item_id.clone());
                                (String::new(), String::new(), String::new())
                            });
                            if let Some(args) = data["delta"].as_str() {
                                entry.1.push_str(args);
                            }
                        }
                    }
                    "response.function_call_arguments.done" => {
                        let item_id = data["item_id"]
                            .as_str()
                            .or_else(|| data["id"].as_str())
                            .unwrap_or("")
                            .to_string();
                        if !item_id.is_empty() {
                            let entry = tool_calls.entry(item_id.clone()).or_insert_with(|| {
                                tool_call_order.push(item_id.clone());
                                (String::new(), String::new(), String::new())
                            });
                            if let Some(args) = data["arguments"].as_str() {
                                entry.1 = args.to_string();
                            }
                        }
                    }
                    "response.completed" | "response.done" | "response.incomplete" => {
                        if let Some(status) = data["response"]["status"].as_str() {
                            finish_reason = Some(status.to_string());
                        }
                        if let Some(u) = data["response"].get("usage") {
                            usage = Usage::parse_openai(u);
                        }
                        for output in data["response"]["output"].as_array().into_iter().flatten() {
                            if output["type"].as_str() == Some("function_call") {
                                let item_id = output["id"].as_str().unwrap_or("").to_string();
                                let call_id = output["call_id"].as_str().unwrap_or("").to_string();
                                let name = output["name"].as_str().unwrap_or("").to_string();
                                let arguments =
                                    output["arguments"].as_str().unwrap_or("").to_string();
                                if item_id.is_empty() {
                                    continue;
                                }
                                let entry =
                                    tool_calls.entry(item_id.clone()).or_insert_with(|| {
                                        tool_call_order.push(item_id.clone());
                                        (String::new(), String::new(), String::new())
                                    });
                                if !name.is_empty() {
                                    entry.0 = name;
                                }
                                if !arguments.is_empty() {
                                    entry.1 = arguments;
                                }
                                if !call_id.is_empty() {
                                    entry.2 = call_id;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            if buf.len() > STREAM_EVENT_BUFFER_CAP {
                anyhow::bail!(
                    "stream buffer exceeded {} bytes without an event boundary",
                    STREAM_EVENT_BUFFER_CAP
                );
            }
        }

        let mut blocks: Vec<Block> = Vec::new();
        if !reasoning_buf.is_empty() {
            self.sink
                .emit(AgentEvent::ThinkingBlockComplete(reasoning_buf.clone()));
            blocks.push(Block::Thinking {
                text: reasoning_buf,
            });
        }
        if !text_buf.is_empty() {
            self.partial_stream_text = None;
            self.sink
                .emit(AgentEvent::TextBlockComplete(text_buf.clone()));
            blocks.push(Block::Text { text: text_buf });
        }
        for (idx, item_id) in tool_call_order.iter().enumerate() {
            let Some((name, args, call_id)) = tool_calls.remove(item_id) else {
                continue;
            };
            let input: Value = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
            let summary = summarize_call(&name, &input);
            // Store call_id (not item_id) on the Block — it's the id the server uses to
            // pair `function_call_output.call_id` back to the original call. Fall back to
            // item_id only if the server somehow didn't emit a call_id.
            let block_id = if call_id.is_empty() {
                item_id.clone()
            } else {
                call_id
            };
            let display_call_id = normalize_tool_call_id(&block_id, 0, idx);
            self.sink.emit(AgentEvent::ToolCallPreview {
                call_id: display_call_id,
                name: name.clone(),
                summary,
            });
            blocks.push(Block::ToolUse {
                id: block_id,
                name,
                input,
            });
        }
        Ok((blocks, finish_reason, usage))
    }

    async fn run_subagent(
        &mut self,
        input: &Value,
    ) -> std::result::Result<SubagentRunReport, String> {
        struct SubagentForwardState {
            run_id: u64,
            traces: Vec<SubagentToolTrace>,
            halted_reason: Option<String>,
            anon_counter: usize,
        }

        impl SubagentForwardState {
            fn new(run_id: u64) -> Self {
                Self {
                    run_id,
                    traces: Vec::new(),
                    halted_reason: None,
                    anon_counter: 0,
                }
            }

            fn normalize_local_call_id(&mut self, call_id: String) -> String {
                if call_id.trim().is_empty() {
                    self.anon_counter = self.anon_counter.saturating_add(1);
                    format!("anon-{}", self.anon_counter)
                } else {
                    call_id
                }
            }

            fn namespace_call_id(&self, call_id: &str) -> String {
                namespace_subagent_call_id(self.run_id, call_id)
            }

            fn forward(&mut self, sink: &mut dyn EventSink, ev: AgentEvent) {
                match ev {
                    AgentEvent::ToolCallPreview {
                        call_id,
                        name,
                        summary,
                    } => {
                        let local = self.normalize_local_call_id(call_id);
                        let call_id = self.namespace_call_id(&local);
                        sink.emit(AgentEvent::ToolCallPreview {
                            call_id,
                            name,
                            summary,
                        });
                    }
                    AgentEvent::ToolCallStart {
                        call_id,
                        name,
                        summary,
                    } => {
                        let local = self.normalize_local_call_id(call_id);
                        let call_id = self.namespace_call_id(&local);
                        sink.emit(AgentEvent::ToolCallStart {
                            call_id,
                            name,
                            summary,
                        });
                    }
                    AgentEvent::ToolCallResult {
                        call_id,
                        name,
                        ok,
                        preview,
                        content,
                    } => {
                        let local = self.normalize_local_call_id(call_id);
                        let call_id = self.namespace_call_id(&local);
                        self.traces.push(SubagentToolTrace {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            summary: preview.clone(),
                            content: content.clone(),
                            ok,
                        });
                        sink.emit(AgentEvent::ToolCallResult {
                            call_id,
                            name,
                            ok,
                            preview,
                            content,
                        });
                    }
                    AgentEvent::ToolBatchStart {
                        batch_id,
                        call_ids,
                        labels,
                    } => {
                        let batch_id = namespace_subagent_batch_id(self.run_id, &batch_id);
                        let call_ids = call_ids
                            .into_iter()
                            .enumerate()
                            .map(|(i, id)| {
                                let local = normalize_tool_call_id(&id, 0, i);
                                self.namespace_call_id(&local)
                            })
                            .collect();
                        sink.emit(AgentEvent::ToolBatchStart {
                            batch_id,
                            call_ids,
                            labels,
                        });
                    }
                    AgentEvent::ToolBatchEnd {
                        batch_id,
                        call_ids,
                        labels,
                        failed,
                    } => {
                        let batch_id = namespace_subagent_batch_id(self.run_id, &batch_id);
                        let call_ids = call_ids
                            .into_iter()
                            .enumerate()
                            .map(|(i, id)| {
                                let local = normalize_tool_call_id(&id, 0, i);
                                self.namespace_call_id(&local)
                            })
                            .collect();
                        sink.emit(AgentEvent::ToolBatchEnd {
                            batch_id,
                            call_ids,
                            labels,
                            failed,
                        });
                    }
                    AgentEvent::Info(s) => {
                        let msg = s.trim();
                        if msg.starts_with("[halted:") {
                            self.halted_reason = Some(msg.to_string());
                        }
                    }
                    AgentEvent::Warn(s) => {
                        let msg = s.trim();
                        if msg.starts_with("[halted:") {
                            self.halted_reason = Some(msg.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }

        let started_at = std::time::Instant::now();
        let request = SubagentRequest::from_input(self, input)?;
        let task = request.task.clone();
        let system = request
            .system
            .clone()
            .unwrap_or_else(|| DEFAULT_SUBAGENT_SYSTEM.to_string());
        let max_iter = request.max_iterations.map(|max| max as u32);
        let whitelist = request.allowed_tools.clone();

        let tools: Vec<Tool> = tool_definitions()
            .into_iter()
            .filter(|t| t.name != "subagent")
            .filter(|t| {
                request.browser_recipe == BrowserRecipe::AgentBrowser || t.name != "browser"
            })
            .filter(|t| {
                tool_name_allowed_in_profile(
                    t.name,
                    if request.context_mode.is_frugal() {
                        ToolContextProfile::Lean
                    } else {
                        ToolContextProfile::Full
                    },
                )
            })
            .filter(|t| match &whitelist {
                Some(w) => w.iter().any(|n| n == t.name),
                None => true,
            })
            .collect();

        let run_id = next_subagent_run_id();
        let mut forward = SubagentForwardState::new(run_id);
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let mut sub = Agent {
            client: self.client.clone(),
            provider_id: self.provider_id.clone(),
            api_key: self.api_key.clone(),
            key_source: self.key_source.clone(),
            provider_requires_api_key: self.provider_requires_api_key,
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_provider: self.api_provider,
            thinking_effort: self.thinking_effort,
            system,
            history: Vec::new(),
            tools,
            allowed: self.allowed.clone(),
            deny_tools: self.deny_tools.clone(),
            sandbox_root: self.sandbox_root.clone(),
            git_context: self.git_context.clone(),
            silent: true,
            pretty: false,
            max_iterations: max_iter,
            session_usage: Usage::default(),
            interrupt: self.interrupt.clone(),
            hooks: self.hooks.clone(),
            state_lock: self.state_lock.clone(),
            session_enabled: self.session_enabled,
            latest_session_path: self.latest_session_path.clone(),
            latest_log_path: self.latest_log_path.clone(),
            pending_login_provider: None,
            suppress_checkpoints: true,
            last_checkpoint_at: None,
            session_model_pins: self.session_model_pins.clone(),
            partial_stream_text: None,
            compact_threshold_chars: self.compact_threshold_chars,
            compact_threshold_percent: self.compact_threshold_percent,
            approval_profile: request.approval_profile,
            sandbox_profile: request.sandbox_profile,
            browser_recipe: request.browser_recipe,
            context_mode: request.context_mode,
            tool_profile: request.tool_profile,
            budget_cap: request.budget_cap,
            budget_exhausted: false,
            builtin_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent_builtins())),
            sink: Box::new(ChannelSink { tx: ev_tx }),
            steering_rx: None,
            steering_tx: Agent::noop_steering_tx(),
            read_cache: self.read_cache.clone(),
            work_ledger: self.work_ledger.clone(),
            provider_health: self.provider_health.clone(),
            track_origin: self.track_origin.clone(),
            privacy: self.privacy.clone(),
        };

        if !request.privacy_enabled {
            sub.privacy.enabled = false;
        }
        sub.set_approval_profile(request.approval_profile);
        sub.refresh_tools_for_context();

        {
            let delegated_prompt = format!(
                "{task}\n\n[handoff requirement]\nFinish with these exact headings: source inspected, verification run, files touched, uncertainty/open questions, confidence, exact recommended edits, remaining risks. If the task is getting broad, prefer a partial handoff over endless tool use."
            );
            let chat_fut = Box::pin(sub.chat(delegated_prompt));
            tokio::pin!(chat_fut);
            let chat_result: Result<()> = loop {
                tokio::select! {
                    biased;
                    r = &mut chat_fut => break r,
                    maybe_ev = ev_rx.recv() => {
                        if let Some(ev) = maybe_ev {
                            forward.forward(self.sink.as_mut(), ev);
                        }
                    }
                }
            };
            while let Ok(ev) = ev_rx.try_recv() {
                forward.forward(self.sink.as_mut(), ev);
            }
            chat_result.map_err(|e| format!("subagent error: {e}"))?;
        }

        self.session_usage.add(sub.session_usage);

        let iterations = sub.history.iter().filter(|m| m.role == "assistant").count() as u32;
        let final_text = sub
            .history
            .iter()
            .rev()
            .find_map(|m| {
                if m.role == "assistant" {
                    let t: String = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } | Block::PartialStream { text } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if t.is_empty() { None } else { Some(t) }
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "(subagent returned no text)".to_string());

        let failed_calls = forward.traces.iter().filter(|t| !t.ok).count();
        Ok(SubagentRunReport {
            task,
            max_iterations: max_iter,
            iterations,
            calls: forward.traces.len(),
            failed_calls,
            elapsed: started_at.elapsed(),
            halted_reason: forward.halted_reason,
            traces: forward.traces,
            final_text,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionExportFormat {
    Jsonl,
    Html,
}

impl SessionExportFormat {
    fn ext(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Html => "html",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Jsonl => "JSONL",
            Self::Html => "HTML",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "jsonl" | "json" | "--jsonl" | "--json" => Some(Self::Jsonl),
            "html" | "htm" | "--html" | "--htm" => Some(Self::Html),
            _ => None,
        }
    }
}

fn default_session_export_filename(format: SessionExportFormat) -> String {
    format!("wolf-session-{}.{}", unix_timestamp_secs(), format.ext())
}

fn default_session_export_path(format: SessionExportFormat) -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(default_session_export_filename(format))
}

fn normalize_session_export_path(path: PathBuf, format: SessionExportFormat) -> PathBuf {
    if path.is_dir() {
        path.join(default_session_export_filename(format))
    } else {
        path
    }
}

fn parse_session_export_target(arg: &str) -> (SessionExportFormat, PathBuf) {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return (
            SessionExportFormat::Jsonl,
            default_session_export_path(SessionExportFormat::Jsonl),
        );
    }

    let lower = trimmed.to_ascii_lowercase();
    for (prefix, format) in [
        ("html", SessionExportFormat::Html),
        ("--html", SessionExportFormat::Html),
        ("jsonl", SessionExportFormat::Jsonl),
        ("--jsonl", SessionExportFormat::Jsonl),
    ] {
        if lower == prefix {
            return (format, default_session_export_path(format));
        }
        if let Some(rest) = lower.strip_prefix(&format!("{prefix} ")) {
            let raw_rest = &trimmed[trimmed.len() - rest.len()..];
            return (
                format,
                normalize_session_export_path(PathBuf::from(raw_rest.trim()), format),
            );
        }
    }

    let path = PathBuf::from(trimmed);
    let format = match path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("html" | "htm") => SessionExportFormat::Html,
        _ => SessionExportFormat::Jsonl,
    };
    (format, normalize_session_export_path(path, format))
}

fn html_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn system_time_unix_secs(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn read_session_jsonl(path: &Path) -> Result<(SessionHeader, Vec<Message>)> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines = content.lines();
    let header = parse_session_header(lines.next().context("empty session file")?)?;
    let mut history = Vec::new();
    for (i, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        history.push(
            serde_json::from_str::<Message>(line)
                .with_context(|| format!("bad message on line {} in {}", i + 2, path.display()))?,
        );
    }
    Ok((header, history))
}

fn session_file_line(path: &Path, name: &str, modified: Option<std::time::SystemTime>) -> String {
    let updated = modified
        .and_then(system_time_unix_secs)
        .map(|secs| format!("updated {secs}"))
        .unwrap_or_else(|| "updated unknown".to_string());
    match read_session_jsonl(path) {
        Ok((header, history)) => format!(
            "{name}: {} messages · model {} · {} -> {}",
            history.len(),
            header.model,
            updated,
            path.display()
        ),
        Err(e) => format!("{name}: unreadable ({e:#}) -> {}", path.display()),
    }
}

fn render_session_listing(root: &Path) -> String {
    use std::fmt::Write as _;

    let latest_path = latest_session_path(root);
    let mut out = String::new();
    let _ = writeln!(out, "project latest:");
    if latest_path.exists() {
        let modified = latest_path.metadata().ok().and_then(|m| m.modified().ok());
        let _ = writeln!(
            out,
            "  {}",
            session_file_line(&latest_path, "latest", modified)
        );
    } else {
        let _ = writeln!(
            out,
            "  (none yet; send a message to create {})",
            latest_path.display()
        );
    }

    let _ = writeln!(out, "named sessions:");
    match list_session_records_for_root(root) {
        Ok(records) if records.is_empty() => {
            let _ = writeln!(
                out,
                "  (none in {}; use /save <name>)",
                named_sessions_dir_for_root(root).display()
            );
        }
        Ok(records) => {
            for record in records.iter().take(SLASH_LIST_LIMIT) {
                let _ = writeln!(
                    out,
                    "  {}",
                    session_file_line(&record.path, &record.name, record.modified)
                );
            }
            if records.len() > SLASH_LIST_LIMIT {
                let _ = writeln!(
                    out,
                    "  … [{} more named sessions omitted]",
                    records.len() - SLASH_LIST_LIMIT
                );
            }
        }
        Err(e) => {
            let _ = writeln!(out, "  [err] {e:#}");
        }
    }

    let _ = write!(
        out,
        "commands: /resume [name] · /save <name> · /map · /packet @wNN · /focus @wNN · /export html [path]"
    );
    out
}

fn session_message_class(message: &Message) -> &'static str {
    if message
        .content
        .iter()
        .all(|b| matches!(b, Block::ToolResult { .. }))
    {
        "tool"
    } else {
        match message.role.as_str() {
            "user" => "user",
            "assistant" => "assistant",
            _ => "msg",
        }
    }
}

fn render_session_block_html(out: &mut String, block: &Block) {
    use std::fmt::Write as _;

    match block {
        Block::Text { text } => {
            let _ = write!(out, "<div class=\"block\">{}</div>", html_escape(text));
        }
        Block::PartialStream { text } => {
            let _ = write!(
                out,
                "<div class=\"block\"><em>partial stream</em>\n{}</div>",
                html_escape(text)
            );
        }
        Block::Thinking { text } => {
            let _ = write!(
                out,
                "<details class=\"thinking\"><summary>thinking</summary><pre>{}</pre></details>",
                html_escape(text)
            );
        }
        Block::ToolUse { id, name, input } => {
            let input = serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
            let _ = write!(
                out,
                "<details class=\"block\"><summary><span class=\"tool-name\">tool_use {}</span> <code>{}</code></summary><pre>{}</pre></details>",
                html_escape(name),
                html_escape(id),
                html_escape(&input)
            );
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            let status = if is_error.unwrap_or(false) {
                "err"
            } else {
                "ok"
            };
            let label = if is_error.unwrap_or(false) {
                "error"
            } else {
                "ok"
            };
            let _ = write!(
                out,
                "<details class=\"block\" open><summary><span class=\"tool-name\">tool_result</span> <code>{}</code> <span class=\"{}\">{}</span></summary><pre>{}</pre></details>",
                html_escape(tool_use_id),
                status,
                label,
                html_escape(content)
            );
        }
    }
}

fn render_session_html(header: &SessionHeader, history: &[Message], title: &str) -> String {
    use std::fmt::Write as _;

    let title = html_escape(title);
    let allowed = if header.allowed.is_empty() {
        "(none)".to_string()
    } else {
        html_escape(&header.allowed.join(", "))
    };
    let sandbox = header.sandbox.as_deref().unwrap_or("(none)");
    let mut out = String::new();
    let _ = write!(
        out,
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{}</title><style>{}</style></head><body>",
        title, SESSION_HTML_STYLE
    );
    let _ = write!(out, "<h1>{}</h1>", title);
    let _ = write!(
        out,
        "<div class=\"meta\">model <code>{}</code> · effort <code>{}</code> · {} messages · usage <code>{}</code><br>sandbox <code>{}</code><br>allowed tools <code>{}</code></div>",
        html_escape(&header.model),
        header.thinking_effort.as_str(),
        history.len(),
        html_escape(&header.usage.line()),
        html_escape(sandbox),
        allowed
    );
    let _ = write!(
        out,
        "<details class=\"meta\"><summary>system prompt</summary><pre>{}</pre></details>",
        html_escape(&header.system)
    );

    for (idx, message) in history.iter().enumerate() {
        let class = session_message_class(message);
        let _ = write!(
            out,
            "<section class=\"msg {}\"><div class=\"role\">{} #{}</div>",
            class,
            html_escape(&message.role),
            idx + 1
        );
        for block in &message.content {
            render_session_block_html(&mut out, block);
        }
        out.push_str("</section>");
    }
    let _ = write!(
        out,
        "<div class=\"footer\">Exported by Wolf at unix {}</div></body></html>",
        unix_timestamp_secs()
    );
    out
}

#[derive(Debug, Clone, Default)]
struct SessionAnalysis {
    messages: usize,
    user_intents: Vec<String>,
    decisions: Vec<String>,
    files_touched: BTreeMap<String, usize>,
    commands_run: Vec<String>,
    failures: Vec<String>,
    verification: Vec<VerificationRecord>,
    compactions: usize,
    subagents: usize,
    provider: String,
    model: String,
    usage: Usage,
}

fn analyze_session_history(header: &SessionHeader, history: &[Message]) -> SessionAnalysis {
    let mut analysis = SessionAnalysis {
        messages: history.len(),
        provider: header.provenance.provider.clone(),
        model: header.model.clone(),
        usage: header.usage,
        verification: header.work_ledger.verification.clone(),
        ..Default::default()
    };
    for msg in history {
        for block in &msg.content {
            match block {
                Block::Text { text } | Block::PartialStream { text } => {
                    let lower = text.to_ascii_lowercase();
                    if msg.role == "user" && !text.trim().is_empty() {
                        analysis.user_intents.push(summarize_inline(text, 160));
                    }
                    if lower.contains("decision") || lower.contains("decided") {
                        analysis.decisions.push(summarize_inline(text, 180));
                    }
                    if lower.contains("[prior conversation, summarized for resume]") {
                        analysis.compactions += 1;
                    }
                    if lower.contains("[subagent completed]") {
                        analysis.subagents += 1;
                    }
                    if lower.contains("error")
                        || lower.contains("failed")
                        || lower.contains("panic")
                    {
                        analysis.failures.push(summarize_inline(text, 180));
                    }
                }
                Block::ToolUse { name, input, .. } => {
                    if let Some(path) = input["path"].as_str() {
                        *analysis.files_touched.entry(path.to_string()).or_default() += 1;
                    }
                    if name == "bash"
                        && let Some(command) = input["command"].as_str()
                    {
                        analysis
                            .commands_run
                            .push(summarize_bash_command(command, 180));
                    }
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    if is_error.unwrap_or(false) {
                        analysis.failures.push(summarize_inline(content, 180));
                    }
                }
                Block::Thinking { .. } => {}
            }
        }
    }
    analysis.user_intents.truncate(20);
    analysis.decisions.truncate(20);
    analysis.commands_run.truncate(50);
    analysis.failures.truncate(50);
    analysis
}

fn work_map_kind_rank(kind: WorkMapKind) -> u8 {
    match kind {
        WorkMapKind::Intent => 0,
        WorkMapKind::Decision => 1,
        WorkMapKind::Failure => 2,
        WorkMapKind::Verify => 3,
        WorkMapKind::Change => 4,
        WorkMapKind::Evidence => 5,
        WorkMapKind::Compact => 6,
        WorkMapKind::Result => 7,
    }
}

fn push_unique_limited(items: &mut Vec<String>, item: String, limit: usize) {
    let item = summarize_inline(&item, 240);
    if item.trim().is_empty() || items.iter().any(|existing| existing == &item) {
        return;
    }
    items.push(item);
    if items.len() > limit {
        items.truncate(limit);
    }
}

fn input_paths(input: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    for key in ["path", "old_path", "new_path"] {
        if let Some(path) = input[key].as_str() {
            push_unique_limited(&mut paths, path.to_string(), 16);
        }
    }
    if let Some(items) = input["paths"].as_array() {
        for item in items {
            if let Some(path) = item.as_str() {
                push_unique_limited(&mut paths, path.to_string(), 16);
            }
        }
    }
    paths
}

fn tool_command(name: &str, input: &Value) -> Option<String> {
    match name {
        "bash" => input["command"]
            .as_str()
            .map(|cmd| summarize_bash_command(cmd, 220)),
        "http" => Some(format!("http {}", summarize_args(&input["args"], 220))),
        "csvkit" => Some(format!("csvkit {}", summarize_args(&input["args"], 220))),
        _ => None,
    }
}

fn tool_use_kind(name: &str, input: &Value) -> Option<WorkMapKind> {
    match name {
        "write_file" | "edit_file" | "multi_edit" | "git_commit" => Some(WorkMapKind::Change),
        "read_file" | "read_symbol" | "fd" | "rg" | "grep" | "jq" | "fzf" | "http" | "git_diff"
        | "git_log" | "todo_read" | "awk" | "csvkit" => Some(WorkMapKind::Evidence),
        "bash" => input["command"].as_str().map(|command| {
            if looks_like_verification_command(command) {
                WorkMapKind::Verify
            } else {
                WorkMapKind::Evidence
            }
        }),
        _ => None,
    }
}

fn text_work_map_kind(role: &str, text: &str) -> Option<WorkMapKind> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("[prior conversation, summarized for resume]")
        || lower.contains("[compaction]")
        || lower.contains("compacted")
    {
        return Some(WorkMapKind::Compact);
    }
    if lower.contains("decision")
        || lower.contains("decided")
        || lower.contains("durable decision")
        || lower.contains("user preference")
    {
        return Some(WorkMapKind::Decision);
    }
    if lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("error")
        || lower.contains("blocked")
        || lower.contains("panic")
    {
        return Some(WorkMapKind::Failure);
    }
    if role == "user" && !text.trim().is_empty() {
        return Some(WorkMapKind::Intent);
    }
    if role == "assistant" && !text.trim().is_empty() {
        return Some(WorkMapKind::Result);
    }
    None
}

fn add_work_map_waypoint(
    waypoints: &mut Vec<WorkMapWaypoint>,
    kind: WorkMapKind,
    message_range: std::ops::RangeInclusive<usize>,
    summary: String,
    files: Vec<String>,
    commands: Vec<String>,
    status: Option<String>,
) {
    let message_start = *message_range.start();
    let message_end = *message_range.end();
    let summary = summarize_inline(&summary, 180);
    if summary == "?" {
        return;
    }
    if let Some(existing) = waypoints
        .iter_mut()
        .find(|wp| wp.message_start == message_start && wp.summary == summary && wp.kind == kind)
    {
        existing.message_end = existing.message_end.max(message_end);
        for file in files {
            push_unique_limited(&mut existing.files, file, 8);
        }
        for command in commands {
            push_unique_limited(&mut existing.commands, command, 8);
        }
        if existing.status.is_none() {
            existing.status = status;
        }
        return;
    }
    let mut files_out = Vec::new();
    for file in files {
        push_unique_limited(&mut files_out, file, 8);
    }
    let mut commands_out = Vec::new();
    for command in commands {
        push_unique_limited(&mut commands_out, command, 8);
    }
    waypoints.push(WorkMapWaypoint {
        id: String::new(),
        kind,
        message_start,
        message_end,
        summary,
        files: files_out,
        commands: commands_out,
        status,
    });
}

fn assign_work_map_ids(waypoints: &mut [WorkMapWaypoint]) {
    waypoints.sort_by_key(|wp| {
        (
            wp.message_start,
            wp.message_end,
            work_map_kind_rank(wp.kind),
            wp.summary.clone(),
        )
    });
    for (idx, waypoint) in waypoints.iter_mut().enumerate() {
        waypoint.id = format!("@w{:02}", idx + 1);
    }
}

#[derive(Clone, Debug, Default)]
struct ToolUseInfo {
    summary: String,
    files: Vec<String>,
    commands: Vec<String>,
    kind: Option<WorkMapKind>,
    message_index: usize,
}

fn build_session_work_map(source: &Path, header: &SessionHeader, history: &[Message]) -> WorkMap {
    let mut tool_uses: HashMap<String, ToolUseInfo> = HashMap::new();
    let mut waypoints = Vec::new();

    if !header.work_ledger.objective.trim().is_empty() {
        add_work_map_waypoint(
            &mut waypoints,
            WorkMapKind::Intent,
            0..=0,
            format!("objective: {}", header.work_ledger.objective.trim()),
            Vec::new(),
            Vec::new(),
            None,
        );
    }
    for decision in &header.work_ledger.decisions {
        add_work_map_waypoint(
            &mut waypoints,
            WorkMapKind::Decision,
            0..=0,
            decision.clone(),
            Vec::new(),
            Vec::new(),
            None,
        );
    }
    for blocked in &header.work_ledger.blocked {
        add_work_map_waypoint(
            &mut waypoints,
            WorkMapKind::Failure,
            0..=0,
            blocked.clone(),
            Vec::new(),
            Vec::new(),
            Some("blocked".to_string()),
        );
    }
    for file in &header.work_ledger.files_changed {
        add_work_map_waypoint(
            &mut waypoints,
            WorkMapKind::Change,
            0..=0,
            format!("changed file: {file}"),
            vec![file.clone()],
            Vec::new(),
            None,
        );
    }
    for record in &header.work_ledger.verification {
        add_work_map_waypoint(
            &mut waypoints,
            WorkMapKind::Verify,
            0..=0,
            format!("{}: {}", record.name, record.status),
            Vec::new(),
            vec![record.command.clone()],
            Some(record.status.clone()),
        );
    }

    for (idx, message) in history.iter().enumerate() {
        let message_index = idx + 1;
        for block in &message.content {
            match block {
                Block::Text { text } | Block::PartialStream { text } => {
                    if let Some(kind) = text_work_map_kind(&message.role, text) {
                        add_work_map_waypoint(
                            &mut waypoints,
                            kind,
                            message_index..=message_index,
                            summarize_inline(text, 180),
                            Vec::new(),
                            Vec::new(),
                            (kind == WorkMapKind::Failure).then(|| "noted".to_string()),
                        );
                    }
                }
                Block::ToolUse { id, name, input } => {
                    let files = input_paths(input);
                    let command = tool_command(name, input);
                    let kind = tool_use_kind(name, input);
                    let summary = summarize_call(name, input);
                    tool_uses.insert(
                        id.clone(),
                        ToolUseInfo {
                            summary: summary.clone(),
                            files: files.clone(),
                            commands: command.clone().into_iter().collect(),
                            kind,
                            message_index,
                        },
                    );
                    if let Some(kind) = kind {
                        add_work_map_waypoint(
                            &mut waypoints,
                            kind,
                            message_index..=message_index,
                            summary,
                            files,
                            command.into_iter().collect(),
                            None,
                        );
                    }
                }
                Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    metadata,
                } => {
                    let info = tool_uses.get(tool_use_id);
                    let ok = !is_error.unwrap_or(false)
                        && metadata
                            .status
                            .as_deref()
                            .is_none_or(|status| !matches!(status, "error" | "failed"));
                    let kind = if !ok {
                        WorkMapKind::Failure
                    } else if metadata
                        .status
                        .as_deref()
                        .is_some_and(|status| matches!(status, "passed" | "failed"))
                    {
                        WorkMapKind::Verify
                    } else {
                        info.and_then(|i| i.kind).unwrap_or(WorkMapKind::Evidence)
                    };
                    if matches!(kind, WorkMapKind::Evidence) && ok {
                        continue;
                    }
                    let summary = if let Some(info) = info {
                        format!("{} => {}", info.summary, summarize_inline(content, 120))
                    } else {
                        summarize_inline(content, 180)
                    };
                    add_work_map_waypoint(
                        &mut waypoints,
                        kind,
                        info.map(|i| i.message_index).unwrap_or(message_index)..=message_index,
                        summary,
                        info.map(|i| i.files.clone()).unwrap_or_default(),
                        info.map(|i| i.commands.clone()).unwrap_or_default(),
                        metadata.status.clone().or_else(|| {
                            if ok {
                                Some("ok".to_string())
                            } else {
                                Some("error".to_string())
                            }
                        }),
                    );
                }
                Block::Thinking { .. } => {}
            }
        }
    }

    assign_work_map_ids(&mut waypoints);
    WorkMap {
        source: source.display().to_string(),
        header: header.clone(),
        messages: history.len(),
        waypoints,
    }
}

fn selected_waypoints<'a>(map: &'a WorkMap, selection: &WorkMapSelection) -> &'a [WorkMapWaypoint] {
    let start = selection.start.min(map.waypoints.len());
    let end = selection.end.min(map.waypoints.len().saturating_sub(1));
    if start > end {
        &map.waypoints[0..0]
    } else {
        &map.waypoints[start..=end]
    }
}

fn parse_waypoint_number(raw: &str) -> Option<usize> {
    let trimmed = raw.trim().trim_start_matches('@');
    let number = trimmed.strip_prefix('w').unwrap_or(trimmed);
    number.parse::<usize>().ok()?.checked_sub(1)
}

fn parse_work_map_selection(raw: &str, map: &WorkMap) -> Result<WorkMapSelection> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("missing waypoint id (example: @w03 or @w03..@w08)");
    }
    let (start_raw, end_raw) = raw.split_once("..").unwrap_or((raw, raw));
    let start = parse_waypoint_number(start_raw)
        .ok_or_else(|| anyhow::anyhow!("invalid waypoint id '{start_raw}'"))?;
    let end = parse_waypoint_number(end_raw)
        .ok_or_else(|| anyhow::anyhow!("invalid waypoint id '{end_raw}'"))?;
    if start >= map.waypoints.len() || end >= map.waypoints.len() {
        anyhow::bail!(
            "waypoint out of range; map has {} waypoint(s)",
            map.waypoints.len()
        );
    }
    Ok(WorkMapSelection {
        start: start.min(end),
        end: start.max(end),
    })
}

fn work_map_waypoint_ids(map: &WorkMap) -> Vec<String> {
    map.waypoints.iter().map(|wp| wp.id.clone()).collect()
}

fn render_work_map(map: &WorkMap) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let provider = if map.header.provenance.provider.is_empty() {
        "unknown"
    } else {
        &map.header.provenance.provider
    };
    let _ = writeln!(out, "Work map — {}", map.source);
    let _ = writeln!(
        out,
        "model {} · provider {} · messages {} · waypoints {}",
        map.header.model,
        provider,
        map.messages,
        map.waypoints.len()
    );
    if let Some(origin) = &map.header.track_origin {
        let _ = writeln!(
            out,
            "track origin: {} {} mode={} packet={}",
            origin.source_session,
            origin.source_waypoint,
            origin.mode,
            summarize_inline(&origin.packet_hash, 16)
        );
    }
    if map.waypoints.is_empty() {
        let _ = writeln!(out, "(no waypoints found yet)");
    } else {
        for wp in map.waypoints.iter().take(SLASH_LIST_LIMIT) {
            let status = wp
                .status
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            let mut extra = Vec::new();
            if let Some(file) = wp.files.first() {
                extra.push(format!("file {file}"));
            }
            if let Some(command) = wp.commands.first() {
                extra.push(format!("cmd {command}"));
            }
            let extra = if extra.is_empty() {
                String::new()
            } else {
                format!(" · {}", extra.join(" · "))
            };
            let _ = writeln!(
                out,
                "{} {:8} {:>10}{}  {}{}",
                wp.id,
                wp.kind.as_str(),
                wp.display_range(),
                status,
                wp.summary,
                extra
            );
        }
        if map.waypoints.len() > SLASH_LIST_LIMIT {
            let _ = writeln!(
                out,
                "… [{} more waypoints omitted]",
                map.waypoints.len() - SLASH_LIST_LIMIT
            );
        }
    }
    let _ = write!(
        out,
        "commands: /packet @wNN · /focus @wNN · /track open @wNN [name]"
    );
    out
}

fn collect_waypoint_items<'a, F>(waypoints: &'a [WorkMapWaypoint], mut f: F) -> Vec<String>
where
    F: FnMut(&'a WorkMapWaypoint) -> &'a [String],
{
    let mut out = Vec::new();
    for wp in waypoints {
        for item in f(wp) {
            push_unique_limited(&mut out, item.clone(), 24);
        }
    }
    out
}

fn work_map_selection_label(waypoints: &[WorkMapWaypoint]) -> String {
    match (waypoints.first(), waypoints.last()) {
        (Some(first), Some(last)) if first.id == last.id => first.id.clone(),
        (Some(first), Some(last)) => format!("{}..{}", first.id, last.id),
        _ => "(none)".to_string(),
    }
}

fn render_bullets(out: &mut String, title: &str, items: &[String], limit: usize) {
    use std::fmt::Write as _;
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "{title}:");
    for item in items.iter().take(limit) {
        let _ = writeln!(out, "- {item}");
    }
    if items.len() > limit {
        let _ = writeln!(out, "- … [{} more omitted]", items.len() - limit);
    }
}

fn render_work_map_packet(map: &WorkMap, selection: &WorkMapSelection) -> String {
    use std::fmt::Write as _;
    let waypoints = selected_waypoints(map, selection);
    let mut out = String::new();
    let _ = writeln!(out, "[wolf packet {}]", work_map_selection_label(waypoints));
    let _ = writeln!(out, "source: {}", map.source);
    let ranges = waypoints
        .iter()
        .map(WorkMapWaypoint::display_range)
        .collect::<Vec<_>>()
        .join(", ");
    if !ranges.is_empty() {
        let _ = writeln!(out, "messages: {ranges}");
    }
    if let Some(origin) = &map.header.track_origin {
        let _ = writeln!(
            out,
            "origin: {} {} mode={}",
            origin.source_session, origin.source_waypoint, origin.mode
        );
    }
    if !map.header.work_ledger.objective.trim().is_empty() {
        let _ = writeln!(out, "Intent:");
        let _ = writeln!(out, "- {}", map.header.work_ledger.objective.trim());
    }
    if !waypoints.is_empty() {
        let _ = writeln!(out, "Selected waypoints:");
        for wp in waypoints.iter().take(24) {
            let status = wp
                .status
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "- {} {} {}{}: {}",
                wp.id,
                wp.kind.as_str(),
                wp.display_range(),
                status,
                wp.summary
            );
        }
    }
    let files = collect_waypoint_items(waypoints, |wp| &wp.files);
    let commands = collect_waypoint_items(waypoints, |wp| &wp.commands);
    let decisions = map
        .header
        .work_ledger
        .decisions
        .iter()
        .map(|s| summarize_inline(s, 220))
        .collect::<Vec<_>>();
    let failures = waypoints
        .iter()
        .filter(|wp| wp.kind == WorkMapKind::Failure)
        .map(|wp| wp.summary.clone())
        .chain(map.header.work_ledger.blocked.iter().cloned())
        .collect::<Vec<_>>();
    let verification = waypoints
        .iter()
        .filter(|wp| wp.kind == WorkMapKind::Verify)
        .map(|wp| {
            let status = wp.status.as_deref().unwrap_or("unknown");
            format!("{}: {status}", wp.summary)
        })
        .chain(map.header.work_ledger.verification.iter().map(|v| {
            format!(
                "{}: {} exit={:?} artifact={}",
                v.name,
                v.status,
                v.exit_code,
                v.artifact.clone().unwrap_or_else(|| "(none)".to_string())
            )
        }))
        .collect::<Vec<_>>();
    let evidence = waypoints
        .iter()
        .filter(|wp| wp.kind == WorkMapKind::Evidence)
        .map(|wp| wp.summary.clone())
        .collect::<Vec<_>>();
    render_bullets(&mut out, "Evidence", &evidence, 12);
    render_bullets(&mut out, "Files", &files, 16);
    render_bullets(&mut out, "Commands", &commands, 16);
    render_bullets(&mut out, "Verification", &verification, 12);
    render_bullets(&mut out, "Decisions", &decisions, 12);
    render_bullets(&mut out, "Failures/blockers", &failures, 12);
    let _ = writeln!(out, "Constraints:");
    let _ = writeln!(
        out,
        "- Focus changes model context only; it does not rewind files, git state, or later logs."
    );
    if map.header.browser_recipe == BrowserRecipe::AgentBrowser {
        let _ = writeln!(
            out,
            "- Agent browser is available for browser/web interaction tasks."
        );
    }
    out
}

fn parse_focus_mode(args: &[String]) -> FocusMode {
    let mut carry: Vec<String> = vec!["failures".into(), "decisions".into(), "files".into()];
    for arg in args {
        if arg == "--exact" || arg == "exact" {
            return FocusMode::Exact;
        }
        if let Some(raw) = arg
            .strip_prefix("--carry=")
            .or_else(|| arg.strip_prefix("--carry"))
        {
            let raw = raw.trim_start_matches('=');
            if !raw.is_empty() {
                carry = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
        }
    }
    FocusMode::Carry(carry)
}

fn render_work_map_focus(map: &WorkMap, selection: &WorkMapSelection, mode: &FocusMode) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let selected = selected_waypoints(map, selection);
    let _ = writeln!(
        out,
        "[wolf focus {} mode={}]",
        work_map_selection_label(selected),
        mode.label()
    );
    let _ = writeln!(
        out,
        "Safety: focus changes model context only; it does not rewind files, git state, or later logs."
    );
    if let FocusMode::Carry(items) = mode {
        let carry = if items.is_empty() {
            "(none)".to_string()
        } else {
            items.join(",")
        };
        let _ = writeln!(out, "Carry-forward: {carry}");
    }
    out.push('\n');
    out.push_str(&render_work_map_packet(map, selection));
    out
}

fn parse_work_map_command_args(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(String::from).collect()
}

fn work_map_event_selector(selector: &str) -> Option<String> {
    let selector = selector.trim();
    if selector.is_empty() || matches!(selector, "current" | "memory" | "this") {
        None
    } else {
        Some(selector.to_string())
    }
}

fn load_work_map_for_selector(root: &Path, selector: &str) -> Result<(PathBuf, WorkMap)> {
    let source = resolve_session_selector(root, selector)?;
    let (header, history) = read_session_jsonl(&source)?;
    let map = build_session_work_map(&source, &header, &history);
    Ok((source, map))
}

fn current_work_map(agent: &Agent, label: &str) -> WorkMap {
    let header = agent.session_header();
    build_session_work_map(Path::new(label), &header, &agent.history)
}

fn looks_like_waypoint_token(raw: &str) -> bool {
    parse_waypoint_number(raw).is_some()
}

fn parse_work_map_operation_args<'a>(
    args: &'a [String],
    default_selector: &'a str,
) -> Result<(&'a str, &'a str, Vec<String>)> {
    let Some(id_pos) = args.iter().position(|arg| looks_like_waypoint_token(arg)) else {
        anyhow::bail!("missing waypoint id (example: @w03)");
    };
    let id = args[id_pos].as_str();
    let selector = args[..id_pos]
        .first()
        .map(String::as_str)
        .unwrap_or(default_selector);
    if args[..id_pos].len() > 1 {
        anyhow::bail!("expected at most one session selector before waypoint id");
    }
    let mode_args = args[id_pos + 1..]
        .iter()
        .filter(|arg| arg.starts_with("--") || matches!(arg.as_str(), "exact"))
        .cloned()
        .collect();
    Ok((id, selector, mode_args))
}

fn parse_track_open_args<'a>(
    args: &'a [String],
    default_selector: &'a str,
) -> Result<(&'a str, &'a str, Option<&'a str>, Vec<String>)> {
    let Some(id_pos) = args.iter().position(|arg| looks_like_waypoint_token(arg)) else {
        anyhow::bail!("missing waypoint id (example: @w03)");
    };
    let id = args[id_pos].as_str();
    let selector = args[..id_pos]
        .first()
        .map(String::as_str)
        .unwrap_or(default_selector);
    if args[..id_pos].len() > 1 {
        anyhow::bail!("expected at most one session selector before waypoint id");
    }
    let mut name: Option<&str> = None;
    let mut mode_args = Vec::new();
    for arg in &args[id_pos + 1..] {
        if arg.starts_with("--") || matches!(arg.as_str(), "exact") {
            mode_args.push(arg.clone());
        } else if name.is_none() {
            name = Some(arg.as_str());
        } else {
            mode_args.push(arg.clone());
        }
    }
    Ok((id, selector, name, mode_args))
}

fn choose_work_map_source(
    root: &Path,
    selector: &str,
    agent: Option<&Agent>,
) -> Result<(PathBuf, WorkMap)> {
    let selector = selector.trim();
    if (selector.is_empty() || matches!(selector, "current" | "memory" | "this"))
        && let Some(agent) = agent
    {
        return Ok((PathBuf::from("current"), current_work_map(agent, "current")));
    }
    let selector = if selector.is_empty() {
        "latest"
    } else {
        selector
    };
    load_work_map_for_selector(root, selector)
}

fn render_tracks_listing(root: &Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "tracks:");
    match list_session_records_for_root(root) {
        Ok(records) => {
            let mut shown = 0usize;
            for record in records {
                let Ok((header, _)) = read_session_jsonl(&record.path) else {
                    continue;
                };
                let Some(origin) = header.track_origin else {
                    continue;
                };
                shown += 1;
                let _ = writeln!(
                    out,
                    "- {}: {} {} mode={} -> {}",
                    record.name,
                    origin.source_session,
                    origin.source_waypoint,
                    origin.mode,
                    record.path.display()
                );
                if shown >= SLASH_LIST_LIMIT {
                    break;
                }
            }
            if shown == 0 {
                let _ = writeln!(out, "- (none yet; use /track open @wNN [name])");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "[err] {e:#}");
        }
    }
    let _ = write!(out, "commands: /track open @wNN [name] · /tracks");
    let _ = root;
    out
}

fn default_track_name(waypoint_id: &str) -> String {
    let clean = waypoint_id
        .trim_start_matches('@')
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    format!("track-{clean}-{}", unix_timestamp_secs())
}

fn create_track_from_work_map_with_header(
    root: &Path,
    mut header: SessionHeader,
    map: &WorkMap,
    selection: &WorkMapSelection,
    name: Option<&str>,
    mode: &FocusMode,
) -> Result<PathBuf> {
    let selected = selected_waypoints(map, selection);
    let first = selected
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty waypoint selection"))?;
    let track_name = name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| default_track_name(&first.id));
    session::validate_session_name(&track_name)?;
    let packet = render_work_map_focus(map, selection, mode);
    header.track_origin = Some(TrackOrigin {
        source_session: map.source.clone(),
        source_waypoint: work_map_selection_label(selected),
        mode: mode.label().to_string(),
        packet_hash: sha256_hex_str(&packet),
        created_at: unix_timestamp_secs(),
    });
    header.work_ledger.objective = format!("track from {}", work_map_selection_label(selected));
    let path = named_session_path_for_root(root, &track_name)?;
    let history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: format!(
                    "Start a Wolf track from {}. Use this focus packet as the starting context.\n\n{}",
                    work_map_selection_label(selected),
                    packet
                ),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text:
                    "Track ready. Files were not rewound; verify current repo state before editing."
                        .to_string(),
            }],
        },
    ];
    let mut data = Vec::new();
    writeln!(&mut data, "{}", serde_json::to_string(&header)?)?;
    for message in history {
        writeln!(&mut data, "{}", serde_json::to_string(&message)?)?;
    }
    atomic_write_bytes(&path, &data)?;
    Ok(path)
}

fn create_track_from_work_map(
    agent: &Agent,
    map: &WorkMap,
    selection: &WorkMapSelection,
    name: Option<&str>,
    mode: &FocusMode,
) -> Result<PathBuf> {
    create_track_from_work_map_with_header(
        &agent.sandbox_root,
        agent.session_header(),
        map,
        selection,
        name,
        mode,
    )
}

fn render_session_analysis(
    path: &Path,
    header: &SessionHeader,
    analysis: &SessionAnalysis,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "session: {}", path.display());
    let provider = if analysis.provider.is_empty() {
        "unknown"
    } else {
        &analysis.provider
    };
    let _ = writeln!(
        out,
        "model: {} · provider: {} · messages: {} · usage: {}",
        analysis.model,
        provider,
        analysis.messages,
        analysis.usage.line()
    );
    if !header.provenance.system_prompt_hash.is_empty() {
        let _ = writeln!(out, "provenance:");
        let _ = writeln!(out, "- wolf_version: {}", header.provenance.wolf_version);
        let _ = writeln!(
            out,
            "- api_provider: {}",
            header.provenance.api_provider.as_str()
        );
        let _ = writeln!(
            out,
            "- thinking_effort: {}",
            header.provenance.thinking_effort.as_str()
        );
        let _ = writeln!(
            out,
            "- system_prompt_hash: {}",
            summarize_inline(&header.provenance.system_prompt_hash, 24)
        );
        let _ = writeln!(
            out,
            "- tool_catalog_version: {}",
            header.provenance.tool_catalog_version
        );
    }
    if !analysis.user_intents.is_empty() {
        let _ = writeln!(out, "user intents:");
        for item in analysis.user_intents.iter().rev().take(8).rev() {
            let _ = writeln!(out, "- {item}");
        }
    }
    if !analysis.decisions.is_empty() {
        let _ = writeln!(out, "decisions:");
        for item in analysis.decisions.iter().take(8) {
            let _ = writeln!(out, "- {item}");
        }
    }
    if !analysis.files_touched.is_empty() {
        let _ = writeln!(out, "files touched:");
        for (path, count) in analysis.files_touched.iter().take(20) {
            let _ = writeln!(out, "- {path} ({count})");
        }
    }
    if !analysis.commands_run.is_empty() {
        let _ = writeln!(out, "commands:");
        for cmd in analysis.commands_run.iter().take(12) {
            let _ = writeln!(out, "- {cmd}");
        }
    }
    if !analysis.verification.is_empty() {
        let _ = writeln!(out, "verification:");
        for v in analysis.verification.iter().rev().take(8).rev() {
            let _ = writeln!(out, "- {}: {} ({:?})", v.name, v.status, v.artifact);
        }
    }
    if !analysis.failures.is_empty() {
        let _ = writeln!(out, "failures:");
        for item in analysis.failures.iter().take(12) {
            let _ = writeln!(out, "- {item}");
        }
    }
    let _ = writeln!(
        out,
        "compactions: {} · subagents: {}",
        analysis.compactions, analysis.subagents
    );
    out
}

fn grep_session_history(history: &[Message], needle: &str) -> Vec<String> {
    let needle_lower = needle.to_ascii_lowercase();
    let mut hits = Vec::new();
    for (idx, msg) in history.iter().enumerate() {
        for block in &msg.content {
            match block {
                Block::Text { text }
                | Block::PartialStream { text }
                | Block::Thinking { text }
                | Block::ToolResult { content: text, .. } => {
                    if text.to_ascii_lowercase().contains(&needle_lower) {
                        hits.push(format!(
                            "#{} {}: {}",
                            idx + 1,
                            msg.role,
                            summarize_inline(text, 220)
                        ));
                    }
                }
                Block::ToolUse { name, input, .. } => {
                    let haystack = format!("{name} {input}");
                    if haystack.to_ascii_lowercase().contains(&needle_lower) {
                        hits.push(format!(
                            "#{} {} tool_use: {}",
                            idx + 1,
                            msg.role,
                            summarize_inline(&haystack, 220)
                        ));
                    }
                }
            }
        }
    }
    hits
}

fn export_session_file(source: &Path, format: SessionExportFormat, target: &Path) -> Result<()> {
    match format {
        SessionExportFormat::Jsonl => {
            let bytes =
                std::fs::read(source).with_context(|| format!("reading {}", source.display()))?;
            atomic_write_bytes(target, &bytes)?;
        }
        SessionExportFormat::Html => {
            let (header, history) = read_session_jsonl(source)?;
            let title = source
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("wolf session");
            let html = render_session_html(&header, &history, title);
            atomic_write_bytes(target, html.as_bytes())?;
        }
    }
    Ok(())
}

fn resolve_session_selector(root: &Path, selector: &str) -> Result<PathBuf> {
    let trimmed = selector.trim();
    if trimmed.is_empty() || trimmed == "latest" || trimmed == LATEST_SESSION_NAME {
        return Ok(latest_session_path(root));
    }
    let path = PathBuf::from(trimmed);
    if path.exists() || path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        return Ok(path);
    }
    named_session_path_for_root(root, trimmed)
}

fn handle_session_cli(argv: &[String]) -> Result<Option<i32>> {
    if argv.is_empty() {
        return Ok(None);
    }
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match argv[0].as_str() {
        "sessions" => {
            println!("{}", render_session_listing(&root));
            Ok(Some(0))
        }
        "sessions-analyze" => {
            let source = latest_session_path(&root);
            let (header, history) = read_session_jsonl(&source)?;
            let analysis = analyze_session_history(&header, &history);
            print!("{}", render_session_analysis(&source, &header, &analysis));
            Ok(Some(0))
        }
        "sessions-grep" => {
            let Some(needle) = argv.get(1) else {
                eprintln!("usage: wolf sessions-grep <text>");
                return Ok(Some(2));
            };
            let source = latest_session_path(&root);
            let (_, history) = read_session_jsonl(&source)?;
            for hit in grep_session_history(&history, needle)
                .into_iter()
                .take(SLASH_LIST_LIMIT)
            {
                println!("{hit}");
            }
            Ok(Some(0))
        }
        "session" => {
            let sub = argv.get(1).map(|s| s.as_str()).unwrap_or("list");
            match sub {
                "list" | "sessions" => {
                    println!("{}", render_session_listing(&root));
                    Ok(Some(0))
                }
                "export" => {
                    let args = &argv[2..];
                    let mut idx = 0usize;
                    let mut selector = "latest";
                    let mut format = SessionExportFormat::Html;
                    if let Some(first) = args.first() {
                        if let Some(parsed) = SessionExportFormat::parse(first) {
                            format = parsed;
                            idx = 1;
                        } else {
                            selector = first;
                            idx = 1;
                            if let Some(next) =
                                args.get(idx).and_then(|s| SessionExportFormat::parse(s))
                            {
                                format = next;
                                idx += 1;
                            }
                        }
                    }
                    let target = match args.get(idx) {
                        Some(path) => normalize_session_export_path(PathBuf::from(path), format),
                        None => default_session_export_path(format),
                    };
                    if args.len() > idx + 1 {
                        eprintln!(
                            "usage: wolf session export [latest|NAME|PATH] [html|jsonl] [OUT]"
                        );
                        return Ok(Some(2));
                    }
                    let source = resolve_session_selector(&root, selector)?;
                    export_session_file(&source, format, &target)?;
                    let (_, history) = read_session_jsonl(&source)?;
                    println!(
                        "exported {} {} messages from {} -> {}",
                        format.label(),
                        history.len(),
                        source.display(),
                        target.display()
                    );
                    Ok(Some(0))
                }
                "analyze" | "analysis" => {
                    let selector = argv.get(2).map(|s| s.as_str()).unwrap_or("latest");
                    let source = resolve_session_selector(&root, selector)?;
                    let (header, history) = read_session_jsonl(&source)?;
                    let analysis = analyze_session_history(&header, &history);
                    print!("{}", render_session_analysis(&source, &header, &analysis));
                    Ok(Some(0))
                }
                "grep" => {
                    let Some(needle) = argv.get(2) else {
                        eprintln!("usage: wolf session grep <text> [latest|NAME|PATH]");
                        return Ok(Some(2));
                    };
                    let selector = argv.get(3).map(|s| s.as_str()).unwrap_or("latest");
                    let source = resolve_session_selector(&root, selector)?;
                    let (_, history) = read_session_jsonl(&source)?;
                    for hit in grep_session_history(&history, needle)
                        .into_iter()
                        .take(SLASH_LIST_LIMIT)
                    {
                        println!("{hit}");
                    }
                    Ok(Some(0))
                }
                "failures" => {
                    let selector = argv.get(2).map(|s| s.as_str()).unwrap_or("latest");
                    let source = resolve_session_selector(&root, selector)?;
                    let (header, history) = read_session_jsonl(&source)?;
                    let analysis = analyze_session_history(&header, &history);
                    for item in analysis.failures {
                        println!("- {item}");
                    }
                    Ok(Some(0))
                }
                "verify-log" | "verification" => {
                    let selector = argv.get(2).map(|s| s.as_str()).unwrap_or("latest");
                    let source = resolve_session_selector(&root, selector)?;
                    let (header, history) = read_session_jsonl(&source)?;
                    let analysis = analyze_session_history(&header, &history);
                    for v in analysis.verification {
                        println!(
                            "{}: {} exit={:?} artifact={}",
                            v.name,
                            v.status,
                            v.exit_code,
                            v.artifact.unwrap_or_else(|| "(none)".to_string())
                        );
                    }
                    Ok(Some(0))
                }
                "decisions" => {
                    let selector = argv.get(2).map(|s| s.as_str()).unwrap_or("latest");
                    let source = resolve_session_selector(&root, selector)?;
                    let (header, history) = read_session_jsonl(&source)?;
                    let analysis = analyze_session_history(&header, &history);
                    for item in analysis.decisions {
                        println!("- {item}");
                    }
                    Ok(Some(0))
                }
                "map" => {
                    let selector = argv.get(2).map(|s| s.as_str()).unwrap_or("latest");
                    let (_, map) = load_work_map_for_selector(&root, selector)?;
                    println!("{}", render_work_map(&map));
                    Ok(Some(0))
                }
                "packet" => {
                    let args = parse_work_map_command_args(&argv[2..].join(" "));
                    let (id, selector, _) = parse_work_map_operation_args(&args, "latest")?;
                    let (_, map) = load_work_map_for_selector(&root, selector)?;
                    let selection = parse_work_map_selection(id, &map)?;
                    println!("{}", render_work_map_packet(&map, &selection));
                    Ok(Some(0))
                }
                "focus" => {
                    let args = parse_work_map_command_args(&argv[2..].join(" "));
                    let (id, selector, mode_args) = parse_work_map_operation_args(&args, "latest")?;
                    let (_, map) = load_work_map_for_selector(&root, selector)?;
                    let selection = parse_work_map_selection(id, &map)?;
                    let mode = parse_focus_mode(&mode_args);
                    println!("{}", render_work_map_focus(&map, &selection, &mode));
                    Ok(Some(0))
                }
                "tracks" => {
                    println!("{}", render_tracks_listing(&root));
                    Ok(Some(0))
                }
                "track" => match argv.get(2).map(|s| s.as_str()) {
                    Some("list") | Some("tracks") | None => {
                        println!("{}", render_tracks_listing(&root));
                        Ok(Some(0))
                    }
                    Some("open") => {
                        let args = parse_work_map_command_args(&argv[3..].join(" "));
                        let (id, selector, name, mode_args) =
                            parse_track_open_args(&args, "latest")?;
                        let (source, map) = load_work_map_for_selector(&root, selector)?;
                        let selection = parse_work_map_selection(id, &map)?;
                        let (header, _) = read_session_jsonl(&source)?;
                        let path = create_track_from_work_map_with_header(
                            &root,
                            header,
                            &map,
                            &selection,
                            name,
                            &parse_focus_mode(&mode_args),
                        )?;
                        println!("created track -> {}", path.display());
                        Ok(Some(0))
                    }
                    _ => {
                        eprintln!(
                            "usage: wolf session track [list]|open [latest|NAME|PATH] @wNN [name] [--exact]"
                        );
                        Ok(Some(2))
                    }
                },
                _ => {
                    eprintln!(
                        "usage: wolf session [list|map [session]|packet [session] @wNN|focus [session] @wNN [--exact]|tracks|track open [session] @wNN [name]|export [latest|NAME|PATH] [html|jsonl] [OUT]|analyze [latest|NAME|PATH]|grep <text> [session]|failures [session]|verify-log [session]|decisions [session]]"
                    );
                    Ok(Some(2))
                }
            }
        }
        _ => Ok(None),
    }
}

fn handle_slash(line: &str, agent: &mut Agent) -> Option<bool> {
    use std::fmt::Write as _;
    let mut ui_update = SlashUiUpdate::None;
    let line = line.trim();
    if !line.starts_with('/') {
        return None;
    }
    let mut parts = line[1..].splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();

    let mut out = String::new();
    let w = &mut out;

    match cmd {
        "help" | "?" => {
            let _ = writeln!(w, "commands:");
            let _ = writeln!(w, "  /help                     show this");
            let _ = writeln!(w, "  /quit, /exit              exit wolf");
            let _ = writeln!(w, "  /reset                    clear conversation history");
            let _ = writeln!(w, "  /tools                    list available tools");
            let _ = writeln!(
                w,
                "  /history                  show turn count and last 5 messages"
            );
            let _ = writeln!(
                w,
                "  /system [text]            show or replace the system prompt"
            );
            let _ = writeln!(
                w,
                "  /allow <tool>             auto-approve this tool for the session"
            );
            let _ = writeln!(w, "  /revoke <tool>            remove auto-approval");
            let _ = writeln!(w, "  /allowed                  list auto-approved tools");
            let _ = writeln!(
                w,
                "  /fangs [on|off|status]    dangerous: auto-approve all gated tools"
            );
            let _ = writeln!(
                w,
                "  /privacy [on|off|status]  redact sensitive tool output before model context"
            );
            let _ = writeln!(
                w,
                "  /approval [profile]       ask|auto-read|auto-write|never|always"
            );
            let _ = writeln!(
                w,
                "  /sandbox-profile [profile] read-only|workspace-write|danger-full-access"
            );
            let _ = writeln!(
                w,
                "  /budget [cap|off]         show/set budget cap ($ or tokens)"
            );
            let _ = writeln!(
                w,
                "  /browser [off|agent-browser] optional browser automation recipe"
            );
            let _ = writeln!(
                w,
                "  /sandbox [path]           show or change the sandbox root"
            );
            let _ = writeln!(
                w,
                "  /model [id]               show or change model (persists per provider)"
            );
            let _ = writeln!(
                w,
                "  /providers                list providers + auth status"
            );
            let _ = writeln!(
                w,
                "  /provider [id|#]          show or switch active provider"
            );
            let _ = writeln!(
                w,
                "  /models [provider|#|all]  list curated models for active/authenticated providers"
            );
            let _ = writeln!(
                w,
                "  /login [provider|#] [token|web|import] login or store token/key"
            );
            let _ = writeln!(
                w,
                "  /logout [provider|#]      remove stored key for provider"
            );
            let _ = writeln!(
                w,
                "  /login cancel             abort a pending OAuth or browser login"
            );
            let _ = writeln!(
                w,
                "  /effort [level]           set model reasoning depth/tool persistence: low|medium|high|xhigh"
            );
            let _ = writeln!(
                w,
                "  /context [standard|frugal] context/cap mode; frugal minimizes token spend"
            );
            let _ = writeln!(
                w,
                "  /tool-profile [lean|full] provider tool schema verbosity (default lean)"
            );
            let _ = writeln!(
                w,
                "  /compact [status|auto|N]   summarize older history or set the auto-compaction threshold"
            );
            let _ = writeln!(
                w,
                "  /usage                    show cumulative token usage this session"
            );
            let _ = writeln!(
                w,
                "  /status                   show runtime diagnostics (provider, auth, model)"
            );
            let _ = writeln!(
                w,
                "  /tokens                   approximate tokens per message + top hogs"
            );
            let _ = writeln!(
                w,
                "  /save <name>              write history + config to sessions dir as JSONL"
            );
            let _ = writeln!(
                w,
                "  /export [html|jsonl] [path] export session (HTML or JSONL; default JSONL)"
            );
            let _ = writeln!(
                w,
                "  /resume [name]            load the project latest or a named session"
            );
            let _ = writeln!(w, "  /map [session]            show Work Map waypoints");
            let _ = writeln!(
                w,
                "  /packet @wNN [session]    show evidence packet for waypoint/range"
            );
            let _ = writeln!(
                w,
                "  /focus @wNN [--exact]     seed context from waypoint with safety notice"
            );
            let _ = writeln!(
                w,
                "  /track open @wNN [name]   create continuation session; /tracks lists them"
            );
            let _ = writeln!(
                w,
                "  /sessions                 list project latest + named sessions; /sessions analyze|map|packet|focus|tracks|grep|failures|verify-log|decisions"
            );
            let _ = writeln!(w, "  /session                  alias for /sessions");
            let _ = writeln!(
                w,
                "  /plan <task>              run a read-only planner, seed the plan into history"
            );
            let _ = writeln!(
                w,
                "  /subagent <task> [opts]   run a detached subagent (tmux/window if available)"
            );
            let _ = writeln!(w, "    --tools t1,t2           restrict tool whitelist");
            let _ = writeln!(
                w,
                "    --max-iter N            optional max tool-use cycles (default unlimited)"
            );
            let _ = writeln!(
                w,
                "    --system PROMPT         override subagent system prompt"
            );
            let _ = writeln!(w, "    --readonly              read-only tools only");
            let _ = writeln!(
                w,
                "    --inline                legacy foreground mode; default is detached"
            );
            let _ = writeln!(
                w,
                "  /hooks [reload]           show hook config or reload from disk"
            );
            let _ = writeln!(w, "  /version                  show wolf version");
        }
        "version" => {
            let _ = writeln!(w, "wolf {}", env!("CARGO_PKG_VERSION"));
        }
        "quit" | "exit" => {
            return Some(false);
        }
        "reset" => {
            let n = agent.history.len();
            agent.history.clear();
            agent.clear_pending_login();
            let _ = std::fs::remove_file(&agent.latest_session_path);
            let _ = writeln!(w, "cleared {n} messages");
        }
        "tools" => {
            let header = agent.session_header();
            let _ = writeln!(
                w,
                "exposed ({}): {}",
                header.exposed_tools.len(),
                render_limited_csv(&header.exposed_tools, SLASH_LIST_LIMIT, "(none)", "tools")
            );
            let _ = writeln!(
                w,
                "approval-required ({}): {}",
                header.approval_required_tools.len(),
                render_limited_csv(
                    &header.approval_required_tools,
                    SLASH_LIST_LIMIT,
                    "(none)",
                    "tools"
                )
            );
            let _ = writeln!(
                w,
                "auto-approved now ({}): {}",
                header.auto_approved_tools.len(),
                render_limited_csv(
                    &header.auto_approved_tools,
                    SLASH_LIST_LIMIT,
                    "(none)",
                    "tools"
                )
            );
        }
        "history" => {
            let _ = writeln!(w, "history: {} messages", agent.history.len());
            let start = agent.history.len().saturating_sub(5);
            for (i, msg) in agent.history.iter().enumerate().skip(start) {
                let kinds: Vec<&str> = msg
                    .content
                    .iter()
                    .map(|b| match b {
                        Block::Text { .. } => "text",
                        Block::Thinking { .. } => "thinking",
                        Block::ToolUse { .. } => "tool_use",
                        Block::ToolResult { .. } => "tool_result",
                        Block::PartialStream { .. } => "partial_stream",
                    })
                    .collect();
                let _ = writeln!(w, "  [{i}] {} [{}]", msg.role, kinds.join(","));
            }
        }
        "system" => {
            if arg.is_empty() {
                let _ = writeln!(
                    w,
                    "{}",
                    cap_bytes_with_hint(
                        agent.system.clone(),
                        SLASH_TEXT_CAP,
                        "system prompt display truncated; use /system <text> to replace it.",
                    )
                );
            } else {
                agent.system = arg.to_string();
                let _ = writeln!(w, "system prompt replaced ({} chars)", agent.system.len());
            }
        }
        "allow" => {
            if arg.is_empty() {
                let _ = writeln!(w, "usage: /allow <tool>");
            } else if !agent.tools.iter().any(|t| t.name == arg) {
                let _ = writeln!(w, "no such tool: {arg}");
            } else {
                agent.allowed.insert(arg.to_string());
                let _ = writeln!(w, "auto-approving: {arg}");
            }
        }
        "revoke" => {
            if agent.allowed.remove(arg) {
                let _ = writeln!(w, "revoked: {arg}");
            } else {
                let _ = writeln!(w, "not in allow-list: {arg}");
            }
        }
        "allowed" => {
            if agent.allowed.is_empty() {
                let _ = writeln!(w, "(none)");
            } else {
                let mut v: Vec<String> = agent.allowed.iter().cloned().collect();
                v.sort();
                let _ = writeln!(
                    w,
                    "{}",
                    render_limited_csv(&v, SLASH_LIST_LIMIT, "(none)", "allowed tools")
                );
            }
        }
        "fangs" => match arg {
            "" | "status" => {
                let mode = if agent.fangs_out_active() {
                    "ON"
                } else {
                    "off"
                };
                let _ = writeln!(w, "fangs-out: {mode}");
                let _ = writeln!(w, "approval profile: {}", agent.approval_profile().as_str());
                let _ = writeln!(
                    w,
                    "danger zone: privileged tools run without per-call confirmation"
                );
            }
            "on" => {
                let changed = agent.set_fangs_out(true);
                ui_update = SlashUiUpdate::ApprovalProfile;
                let _ = writeln!(
                    w,
                    "⚠ fangs-out ON: {changed} gated tools now auto-approving"
                );
            }
            "off" => {
                let changed = agent.set_fangs_out(false);
                ui_update = SlashUiUpdate::ApprovalProfile;
                let _ = writeln!(
                    w,
                    "fangs-out off: {changed} gated tools returned to per-call prompts"
                );
            }
            _ => {
                let _ = writeln!(w, "usage: /fangs [on|off|status]");
            }
        },
        "privacy" => match arg {
            "" | "status" => {
                let _ = writeln!(w, "{}", agent.privacy.status_text());
            }
            "on" | "redact" | "strict" => {
                agent.privacy.enabled = true;
                agent.privacy.strict_paths = true;
                let _ = writeln!(w, "privacy -> redact");
                let _ = writeln!(
                    w,
                    "raw sensitive-looking tool output will be withheld before model context/session logging"
                );
                agent.checkpoint_latest_session("privacy_changed");
            }
            "off" | "none" | "disabled" => {
                agent.privacy.enabled = false;
                let _ = writeln!(w, "privacy -> off");
                agent.checkpoint_latest_session("privacy_changed");
            }
            _ => {
                let _ = writeln!(w, "usage: /privacy [on|off|status]");
            }
        },
        "approval" | "approval-profile" => {
            if arg.is_empty() || arg == "status" {
                let _ = writeln!(w, "approval profile: {}", agent.approval_profile().as_str());
                let _ = writeln!(w, "profiles: ask, auto-read, auto-write, never, always");
            } else if let Some(profile) = ApprovalProfile::parse(arg) {
                let changed = agent.set_approval_profile(profile);
                ui_update = SlashUiUpdate::ApprovalProfile;
                let _ = writeln!(
                    w,
                    "approval profile -> {} (updated {changed} allow-list entries)",
                    profile.as_str()
                );
            } else {
                let _ = writeln!(
                    w,
                    "usage: /approval [ask|auto-read|auto-write|never|always]"
                );
            }
        }
        "sandbox-profile" => {
            if arg.is_empty() || arg == "status" {
                let _ = writeln!(w, "sandbox profile: {}", agent.sandbox_profile().as_str());
                let _ = writeln!(
                    w,
                    "profiles: read-only, workspace-write, danger-full-access"
                );
            } else if let Some(profile) = SandboxProfile::parse(arg) {
                agent.set_sandbox_profile(profile);
                let _ = writeln!(w, "sandbox profile -> {}", profile.as_str());
            } else {
                let _ = writeln!(
                    w,
                    "usage: /sandbox-profile [read-only|workspace-write|danger-full-access]"
                );
            }
        }
        "budget" => {
            if arg.is_empty() || arg == "status" {
                match agent.budget_cap {
                    Some(cap) => {
                        let _ = writeln!(w, "budget cap: {}", cap.line());
                    }
                    None => {
                        let _ = writeln!(w, "budget cap: off");
                    }
                }
                let _ = writeln!(w, "session usage: {}", agent.session_usage.line());
            } else if matches!(arg, "off" | "none" | "disabled" | "0") {
                agent.set_budget_cap(None);
                let _ = writeln!(w, "budget cap: off");
            } else if let Some(cap) = BudgetCap::parse(arg) {
                agent.set_budget_cap(Some(cap));
                let _ = writeln!(w, "budget cap -> {}", cap.line());
            } else {
                let _ = writeln!(w, "usage: /budget [off|$0.25|0.25|200k tokens]");
            }
        }
        "browser" | "browser-recipe" => {
            if arg.is_empty() || arg == "status" {
                let _ = writeln!(w, "browser recipe: {}", agent.browser_recipe().as_str());
                if let Some(hint) = agent.browser_recipe_hint() {
                    let _ = writeln!(w, "{hint}");
                }
            } else if let Some(recipe) = BrowserRecipe::parse(arg) {
                agent.set_browser_recipe(recipe);
                let _ = writeln!(w, "browser recipe -> {}", recipe.as_str());
                if let Some(hint) = agent.browser_recipe_hint() {
                    let _ = writeln!(w, "{hint}");
                }
            } else {
                let _ = writeln!(w, "usage: /browser [off|agent-browser]");
            }
        }
        "sandbox" => {
            if arg.is_empty() {
                let _ = writeln!(w, "sandbox: {}", agent.sandbox_root.display());
            } else {
                match std::fs::canonicalize(arg) {
                    Ok(p) => match agent.set_sandbox_root(p) {
                        Ok(()) => {
                            let _ = writeln!(w, "sandbox -> {}", agent.sandbox_root.display());
                        }
                        Err(e) => {
                            let _ = writeln!(w, "[err] {e:#}");
                        }
                    },
                    Err(e) => {
                        let _ = writeln!(w, "[err] {e}");
                    }
                }
            }
        }
        "model" => match apply_runtime_model_command(agent, arg) {
            Ok(msg) => {
                let _ = writeln!(w, "{msg}");
                ui_update = SlashUiUpdate::ModelProvider;
            }
            Err(e) => {
                let _ = writeln!(w, "[err] {e:#}");
            }
        },
        "providers" => match (load_provider_catalog(), load_auth_store()) {
            (Ok(catalog), Ok(store)) => {
                let active = resolve_active_provider_id(&catalog);
                let _ = writeln!(w, "active provider: {active}");
                let _ = writeln!(w, "{}", render_provider_list(&catalog, &store, &active));
                let _ = writeln!(w, "provider catalog: {}", provider_catalog_path().display());
                let _ = writeln!(w, "auth store: {}", auth_store_path().display());
                let _ = writeln!(w, "runtime: {}", agent.provider_status_line());
            }
            (Err(e), _) | (_, Err(e)) => {
                let _ = writeln!(w, "[err] {e:#}");
            }
        },
        "provider" => {
            if arg.is_empty() {
                let _ = writeln!(w, "{}", agent.provider_status_line());
            } else {
                match load_provider_catalog().and_then(|catalog| {
                    provider_id_from_selector(&catalog, arg).and_then(|target| {
                        set_active_provider_in_catalog(&target).and_then(|_| {
                            agent.reload_provider(Some(&target), false)?;
                            agent.refresh_tools_for_context();
                            Ok(())
                        })
                    })
                }) {
                    Ok(()) => {
                        if agent.pending_login_provider.as_deref()
                            != Some(agent.provider_id.as_str())
                        {
                            agent.clear_pending_login();
                        }
                        ui_update = SlashUiUpdate::ModelProvider;
                        let _ = writeln!(w, "switched -> {}", agent.provider_status_line());
                    }
                    Err(e) => {
                        let _ = writeln!(w, "[err] {e:#}");
                    }
                }
            }
        }
        "models" => match (load_provider_catalog(), load_auth_store()) {
            (Ok(catalog), Ok(store)) => {
                let active = resolve_active_provider_id(&catalog);
                let list = match arg {
                    "" | "all" => list_models_for_available_providers(&catalog, &store, &active),
                    _ => provider_id_from_selector(&catalog, arg)
                        .and_then(|target| list_models_for_provider(&catalog, &target)),
                };
                match list {
                    Ok(list) => {
                        let _ = writeln!(w, "{list}");
                    }
                    Err(e) => {
                        let _ = writeln!(w, "[err] {e:#}");
                    }
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                let _ = writeln!(w, "[err] {e:#}");
            }
        },
        "login" => {
            if arg.eq_ignore_ascii_case("cancel") {
                cancel_pending_oauth_login();
                if let Some(provider) = agent.clear_pending_login() {
                    let _ = writeln!(w, "cancelled pending login for {provider}");
                } else {
                    let _ = writeln!(w, "no login is waiting for credentials");
                }
            } else if arg.is_empty() {
                match (load_provider_catalog(), load_auth_store()) {
                    (Ok(catalog), Ok(store)) => {
                        let active = resolve_active_provider_id(&catalog);
                        let _ = writeln!(w, "select provider for login (id or index):");
                        let _ =
                            writeln!(w, "{}", render_provider_picker(&catalog, &store, &active));
                        let _ = writeln!(w, "usage: /login <provider|#> [token|web|import]");
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        let _ = writeln!(w, "[err] {e:#}");
                    }
                }
            } else {
                let mut bits = arg.splitn(2, char::is_whitespace);
                let provider = bits.next();
                let key = bits.next().map(str::trim).filter(|s| !s.is_empty());
                match login_provider(provider, key, false).map(|login| {
                    let awaiting = login.awaiting_credentials;
                    let provider_id = login.provider_id.clone();
                    match agent.reload_provider(None, false) {
                        Ok(()) => {
                            agent.refresh_tools_for_context();
                            if awaiting {
                                agent.set_pending_login_provider(Some(provider_id));
                                format!(
                                    "{}\nAfter authenticating, paste the callback URL (http://localhost:...), authorization code, or access token here. /login cancel aborts.\nactive -> {}",
                                    login.message,
                                    agent.provider_status_line()
                                )
                            } else {
                                cancel_pending_oauth_login();
                                agent.clear_pending_login();
                                format!(
                                    "{}\nactive -> {}",
                                    login.message,
                                    agent.provider_status_line()
                                )
                            }
                        }
                        Err(e) => format!(
                            "{}\n[warn] runtime provider refresh failed: {e:#}",
                            login.message
                        ),
                    }
                }) {
                    Ok(msg) => {
                        let _ = writeln!(w, "{msg}");
                    }
                    Err(e) => {
                        let _ = writeln!(w, "[err] {e:#}");
                    }
                }
            }
        }
        "logout" => {
            let provider = arg.split_whitespace().next().filter(|s| !s.is_empty());
            match logout_provider(provider).inspect(|_| {
                if provider.is_none() || provider == agent.pending_login_provider.as_deref() {
                    cancel_pending_oauth_login();
                    agent.clear_pending_login();
                }
                let _ = agent.reload_provider(None, false);
                agent.refresh_tools_for_context();
            }) {
                Ok(msg) => {
                    let _ = writeln!(w, "{msg}");
                    let _ = writeln!(w, "runtime: {}", agent.provider_status_line());
                }
                Err(e) => {
                    let _ = writeln!(w, "[err] {e:#}");
                }
            }
        }
        "auth" => {
            let _ = writeln!(w, "use /providers, /provider, /models, /login, /logout");
        }
        "effort" | "think" | "thinking" => {
            let mut changed = false;
            let raw = arg.to_ascii_lowercase();
            let next_effort = match raw.as_str() {
                "" | "status" => None,
                "next" | "+" => {
                    changed = true;
                    Some(agent.cycle_thinking_effort(1))
                }
                "prev" | "previous" | "-" => {
                    changed = true;
                    Some(agent.cycle_thinking_effort(-1))
                }
                _ => match ThinkingEffort::parse(arg) {
                    Some(level) => {
                        changed = agent.set_thinking_effort(level);
                        Some(agent.thinking_effort())
                    }
                    None => {
                        let _ =
                            writeln!(w, "usage: /effort [low|medium|high|xhigh|next|prev|status]");
                        None
                    }
                },
            };
            if let Some(level) = next_effort {
                let _ = writeln!(
                    w,
                    "thinking effort: {} (model reasoning depth/tool persistence)",
                    level.as_str()
                );
                let _ = writeln!(w, "{}", level.guidance());
                if changed {
                    ui_update = SlashUiUpdate::ThinkingEffort;
                }
            }
        }
        "context" | "context-mode" => {
            if arg.is_empty() || arg.eq_ignore_ascii_case("status") {
                let _ = writeln!(w, "context mode: {}", agent.context_mode.as_str());
            } else if let Some(mode) = ContextMode::parse(arg) {
                agent.context_mode = mode;
                if mode.is_frugal() {
                    agent.tool_profile = ToolProfile::Lean;
                }
                agent.refresh_tools_for_context();
                let _ = writeln!(
                    w,
                    "context mode -> {} (compact threshold {}, tool profile {})",
                    agent.context_mode.as_str(),
                    agent.compact_threshold_chars(),
                    agent.tool_context_profile().as_str()
                );
            } else {
                let _ = writeln!(w, "usage: /context [standard|frugal|status]");
            }
        }
        "tool-profile" | "tools-profile" => {
            if arg.is_empty() || arg.eq_ignore_ascii_case("status") {
                let _ = writeln!(w, "tool profile: {}", agent.tool_context_profile().as_str());
            } else if let Some(profile) = ToolProfile::parse(arg) {
                agent.tool_profile = profile;
                if agent.context_mode.is_frugal() {
                    agent.tool_profile = ToolProfile::Lean;
                }
                agent.refresh_tools_for_context();
                let _ = writeln!(
                    w,
                    "tool profile -> {}",
                    agent.tool_context_profile().as_str()
                );
            } else {
                let _ = writeln!(w, "usage: /tool-profile [lean|full|status]");
            }
        }
        "compact" => match arg.to_ascii_lowercase().as_str() {
            "" => {
                let _ = writeln!(
                    w,
                    "usage: /compact is handled before generic slash dispatch"
                );
            }
            "status" => {
                let current = agent.compact_threshold_chars();
                let active = agent.active_compact_threshold_chars();
                let base =
                    history_char_budget_with_override(&agent.model, None, agent.context_mode);
                match agent.compact_threshold_override_percent() {
                    Some(percent) => {
                        let _ = writeln!(
                            w,
                            "compact threshold: {current} chars ({percent}% of model context window; active-run trigger {active}; auto baseline {base})"
                        );
                    }
                    None => {
                        let _ = writeln!(
                            w,
                            "compact threshold: {current} chars (auto: {} mode; active-run trigger {active})",
                            agent.context_mode.as_str()
                        );
                    }
                }
            }
            "auto" => {
                agent.set_compact_threshold_auto();
                let _ = writeln!(
                    w,
                    "compact threshold reset to auto {} ({})",
                    agent.context_mode.as_str(),
                    agent.compact_threshold_chars()
                );
            }
            _ => {
                let numeric = arg.trim().strip_suffix('%').unwrap_or(arg.trim()).trim();
                match numeric.parse::<u8>() {
                    Ok(percent) if (1..=100).contains(&percent) => {
                        let chars = agent.set_compact_threshold_percent(percent);
                        let _ = writeln!(w, "compact threshold set to {percent}% -> {chars} chars");
                    }
                    _ => {
                        let _ = writeln!(w, "usage: /compact [status|auto|<percent>|<percent>%]");
                    }
                }
            }
        },
        "usage" => {
            let _ = writeln!(w, "session: {}", agent.session_usage.line());
        }
        "status" => {
            let family = agent.api_family_label();
            let _ = writeln!(w, "provider: {}", agent.provider_id);
            let _ = writeln!(w, "api family: {}", family);
            let _ = writeln!(w, "model: {}", agent.model);
            let _ = writeln!(w, "auth source: {}", agent.key_source);
            let _ = writeln!(w, "base url: {}", agent.base_url);
            let _ = writeln!(w, "sandbox: {}", agent.sandbox_root.display());
            let _ = writeln!(w, "history: {} messages", agent.history.len());
            let _ = writeln!(w, "session usage: {}", agent.session_usage.line());
            match agent.budget_cap {
                Some(cap) => {
                    let _ = writeln!(w, "budget cap: {}", cap.line());
                }
                None => {
                    let _ = writeln!(w, "budget cap: off");
                }
            }
            let _ = writeln!(
                w,
                "effort: {} (model reasoning depth/tool persistence)",
                agent.thinking_effort.as_str()
            );
            let _ = writeln!(w, "context mode: {}", agent.context_mode.as_str());
            let _ = writeln!(w, "tool profile: {}", agent.tool_context_profile().as_str());
            let _ = writeln!(w, "compact threshold: {}", agent.compact_threshold_chars());
            let _ = writeln!(w, "approval profile: {}", agent.approval_profile().as_str());
            let _ = writeln!(w, "sandbox profile: {}", agent.sandbox_profile().as_str());
            let _ = writeln!(w, "browser recipe: {}", agent.browser_recipe().as_str());
            let ledger = agent.work_ledger_prompt();
            if !ledger.trim().is_empty() {
                let _ = writeln!(w, "work ledger:\n{}", ledger.trim_end());
            }
            let health = agent.provider_health_prompt();
            if !health.trim().is_empty() {
                let _ = writeln!(w, "provider health:\n{}", health.trim_end());
            }
        }
        "tokens" => {
            let report = render_tokens_report(&agent.history);
            let _ = write!(w, "{report}");
        }
        "save" => {
            if arg.is_empty() {
                let _ = writeln!(w, "usage: /save <name>");
            } else {
                match agent.save_session(arg) {
                    Ok(p) => {
                        let _ = writeln!(
                            w,
                            "saved {} messages -> {}",
                            agent.history.len(),
                            p.display()
                        );
                    }
                    Err(e) => {
                        let _ = writeln!(w, "[err] {e:#}");
                    }
                }
            }
        }
        "export" => {
            let (format, target) = parse_session_export_target(arg);
            let result = match format {
                SessionExportFormat::Jsonl => agent.save_session_to_path(&target),
                SessionExportFormat::Html => agent.export_session_html_to_path(&target),
            };
            match result {
                Ok(()) => {
                    let _ = writeln!(
                        w,
                        "exported {} {} messages -> {}",
                        format.label(),
                        agent.history.len(),
                        target.display()
                    );
                }
                Err(e) => {
                    let _ = writeln!(w, "[err] {e:#}");
                }
            }
        }
        "map" => {
            let selector = if arg.trim().is_empty() {
                "current"
            } else {
                arg.trim()
            };
            match choose_work_map_source(&agent.sandbox_root, selector, Some(agent)) {
                Ok((_source, map)) => emit_work_map_event(
                    agent.sink.as_mut(),
                    WorkMapEventKind::Map,
                    render_work_map(&map),
                    work_map_waypoint_ids(&map),
                    work_map_event_selector(selector),
                ),
                Err(e) => {
                    let _ = writeln!(w, "[err] {e:#}");
                }
            }
        }
        "packet" => {
            let args = parse_work_map_command_args(arg);
            match parse_work_map_operation_args(&args, "current").and_then(|(id, selector, _)| {
                let (_, map) = choose_work_map_source(&agent.sandbox_root, selector, Some(agent))?;
                let selection = parse_work_map_selection(id, &map)?;
                Ok((map, selection, selector))
            }) {
                Ok((map, selection, selector)) => emit_work_map_event(
                    agent.sink.as_mut(),
                    WorkMapEventKind::Packet,
                    render_work_map_packet(&map, &selection),
                    work_map_waypoint_ids(&map),
                    work_map_event_selector(selector),
                ),
                Err(e) => {
                    let _ = writeln!(w, "[err] {e:#}\nusage: /packet @wNN [session]");
                }
            }
        }
        "focus" => {
            let args = parse_work_map_command_args(arg);
            match parse_work_map_operation_args(&args, "current").and_then(
                |(id, selector, mode_args)| {
                    let (_, map) =
                        choose_work_map_source(&agent.sandbox_root, selector, Some(agent))?;
                    let selection = parse_work_map_selection(id, &map)?;
                    Ok((map, selection, parse_focus_mode(&mode_args), selector))
                },
            ) {
                Ok((map, selection, mode, selector)) => {
                    let text = render_work_map_focus(&map, &selection, &mode);
                    agent.history.push(Message {
                        role: "user".to_string(),
                        content: vec![Block::Text {
                            text: format!("[wolf focus packet loaded]\n{text}"),
                        }],
                    });
                    emit_work_map_event(
                        agent.sink.as_mut(),
                        WorkMapEventKind::Focus,
                        text,
                        work_map_waypoint_ids(&map),
                        work_map_event_selector(selector),
                    );
                }
                Err(e) => {
                    let _ = writeln!(w, "[err] {e:#}\nusage: /focus @wNN [--exact]");
                }
            }
        }
        "tracks" => emit_work_map_event(
            agent.sink.as_mut(),
            WorkMapEventKind::Tracks,
            render_tracks_listing(&agent.sandbox_root),
            Vec::new(),
            None,
        ),
        "track" => {
            let args = parse_work_map_command_args(arg);
            match args.first().map(|s| s.as_str()) {
                Some("open") => {
                    let rest = &args[1..];
                    match parse_track_open_args(rest, "current").and_then(
                        |(id, selector, name, mode_args)| {
                            let (_, map) =
                                choose_work_map_source(&agent.sandbox_root, selector, Some(agent))?;
                            let selection = parse_work_map_selection(id, &map)?;
                            let mode = parse_focus_mode(&mode_args);
                            let path =
                                create_track_from_work_map(agent, &map, &selection, name, &mode)?;
                            Ok((map, path, selector))
                        },
                    ) {
                        Ok((map, path, selector)) => {
                            let text = format!(
                                "created track -> {}\n{}",
                                path.display(),
                                render_tracks_listing(&agent.sandbox_root)
                            );
                            emit_work_map_event(
                                agent.sink.as_mut(),
                                WorkMapEventKind::Tracks,
                                text,
                                work_map_waypoint_ids(&map),
                                work_map_event_selector(selector),
                            );
                        }
                        Err(e) => {
                            let _ = writeln!(w, "[err] {e:#}\nusage: /track open @wNN [name]");
                        }
                    }
                }
                Some("list") | Some("tracks") | None => emit_work_map_event(
                    agent.sink.as_mut(),
                    WorkMapEventKind::Tracks,
                    render_tracks_listing(&agent.sandbox_root),
                    Vec::new(),
                    None,
                ),
                _ => {
                    let _ = writeln!(w, "usage: /track open @wNN [name] · /tracks");
                }
            }
        }
        "resume" => {
            let loaded = if arg.is_empty() {
                agent.load_latest_session()
            } else {
                agent.load_session(arg)
            };
            match loaded {
                Ok(p) => {
                    let _ = writeln!(
                        w,
                        "loaded {} messages from {}",
                        agent.history.len(),
                        p.display()
                    );
                    agent.sink.emit(AgentEvent::ThinkingEffortChanged {
                        effort: agent.thinking_effort(),
                    });
                }
                Err(e) => {
                    let _ = writeln!(w, "[err] {e:#}");
                }
            }
        }
        "sessions" | "session" => {
            let mut parts = arg.splitn(2, char::is_whitespace);
            let sub = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("").trim();
            match sub {
                "" | "list" => {
                    let _ = write!(w, "{}", render_session_listing(&agent.sandbox_root));
                }
                "map" => {
                    let selector = if rest.is_empty() { "current" } else { rest };
                    match choose_work_map_source(&agent.sandbox_root, selector, Some(agent)) {
                        Ok((_source, map)) => emit_work_map_event(
                            agent.sink.as_mut(),
                            WorkMapEventKind::Map,
                            render_work_map(&map),
                            work_map_waypoint_ids(&map),
                            work_map_event_selector(selector),
                        ),
                        Err(e) => {
                            let _ = writeln!(w, "[err] {e:#}");
                        }
                    }
                }
                "packet" => {
                    let args = parse_work_map_command_args(rest);
                    match parse_work_map_operation_args(&args, "current").and_then(
                        |(id, selector, _)| {
                            let (_, map) =
                                choose_work_map_source(&agent.sandbox_root, selector, Some(agent))?;
                            let selection = parse_work_map_selection(id, &map)?;
                            Ok((map, selection, selector))
                        },
                    ) {
                        Ok((map, selection, selector)) => emit_work_map_event(
                            agent.sink.as_mut(),
                            WorkMapEventKind::Packet,
                            render_work_map_packet(&map, &selection),
                            work_map_waypoint_ids(&map),
                            work_map_event_selector(selector),
                        ),
                        Err(e) => {
                            let _ =
                                writeln!(w, "[err] {e:#}\nusage: /sessions packet @wNN [session]");
                        }
                    }
                }
                "focus" => {
                    let args = parse_work_map_command_args(rest);
                    match parse_work_map_operation_args(&args, "current").and_then(
                        |(id, selector, mode_args)| {
                            let (_, map) =
                                choose_work_map_source(&agent.sandbox_root, selector, Some(agent))?;
                            let selection = parse_work_map_selection(id, &map)?;
                            Ok((map, selection, parse_focus_mode(&mode_args), selector))
                        },
                    ) {
                        Ok((map, selection, mode, selector)) => emit_work_map_event(
                            agent.sink.as_mut(),
                            WorkMapEventKind::Focus,
                            render_work_map_focus(&map, &selection, &mode),
                            work_map_waypoint_ids(&map),
                            work_map_event_selector(selector),
                        ),
                        Err(e) => {
                            let _ =
                                writeln!(w, "[err] {e:#}\nusage: /sessions focus @wNN [--exact]");
                        }
                    }
                }
                "tracks" | "track" => emit_work_map_event(
                    agent.sink.as_mut(),
                    WorkMapEventKind::Tracks,
                    render_tracks_listing(&agent.sandbox_root),
                    Vec::new(),
                    None,
                ),
                "analyze" | "analysis" => {
                    let selector = if rest.is_empty() { "latest" } else { rest };
                    match resolve_session_selector(&agent.sandbox_root, selector)
                        .and_then(|source| read_session_jsonl(&source).map(|pair| (source, pair)))
                    {
                        Ok((source, (header, history))) => {
                            let analysis = analyze_session_history(&header, &history);
                            let _ = write!(
                                w,
                                "{}",
                                render_session_analysis(&source, &header, &analysis)
                            );
                        }
                        Err(e) => {
                            let _ = writeln!(w, "[err] {e:#}");
                        }
                    }
                }
                "grep" => {
                    let mut grep_parts = rest.splitn(2, char::is_whitespace);
                    let needle = grep_parts.next().unwrap_or("");
                    let selector = grep_parts.next().unwrap_or("latest").trim();
                    if needle.is_empty() {
                        let _ = writeln!(w, "usage: /sessions grep <text> [latest|NAME|PATH]");
                    } else {
                        match resolve_session_selector(&agent.sandbox_root, selector).and_then(
                            |source| read_session_jsonl(&source).map(|pair| (source, pair)),
                        ) {
                            Ok((_source, (_header, history))) => {
                                for hit in grep_session_history(&history, needle)
                                    .into_iter()
                                    .take(SLASH_LIST_LIMIT)
                                {
                                    let _ = writeln!(w, "{hit}");
                                }
                            }
                            Err(e) => {
                                let _ = writeln!(w, "[err] {e:#}");
                            }
                        }
                    }
                }
                "failures" | "verify-log" | "verification" | "decisions" => {
                    let selector = if rest.is_empty() { "latest" } else { rest };
                    match resolve_session_selector(&agent.sandbox_root, selector)
                        .and_then(|source| read_session_jsonl(&source).map(|pair| (source, pair)))
                    {
                        Ok((_source, (header, history))) => {
                            let analysis = analyze_session_history(&header, &history);
                            match sub {
                                "failures" => {
                                    for item in analysis.failures {
                                        let _ = writeln!(w, "- {item}");
                                    }
                                }
                                "decisions" => {
                                    for item in analysis.decisions {
                                        let _ = writeln!(w, "- {item}");
                                    }
                                }
                                _ => {
                                    for v in analysis.verification {
                                        let _ = writeln!(
                                            w,
                                            "{}: {} exit={:?} artifact={}",
                                            v.name,
                                            v.status,
                                            v.exit_code,
                                            v.artifact.unwrap_or_else(|| "(none)".to_string())
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = writeln!(w, "[err] {e:#}");
                        }
                    }
                }
                _ => {
                    let _ = writeln!(
                        w,
                        "usage: /sessions [list|analyze|map|packet|focus|tracks|grep|failures|verify-log|decisions]"
                    );
                }
            }
        }
        "hooks" => {
            if arg == "reload" {
                agent.hooks = Hooks::load(&agent.sandbox_root);
                let _ = writeln!(w, "hooks reloaded");
            }
            let _ = writeln!(
                w,
                "pre_tool: {}, post_tool: {}, user_prompt: {}",
                agent.hooks.pre_tool.len(),
                agent.hooks.post_tool.len(),
                agent.hooks.user_prompt.len()
            );
        }
        _ => {
            let _ = writeln!(w, "unknown command: /{cmd} — try /help");
        }
    }

    if !out.is_empty() {
        if out.ends_with('\n') {
            out.pop();
        }
        agent.sink.emit(AgentEvent::Slash(out));
    }
    match ui_update {
        SlashUiUpdate::None => {}
        SlashUiUpdate::ModelProvider => agent.emit_runtime_provider_state(),
        SlashUiUpdate::ThinkingEffort => agent.sink.emit(AgentEvent::ThinkingEffortChanged {
            effort: agent.thinking_effort(),
        }),
        SlashUiUpdate::ApprovalProfile => agent.sink.emit(AgentEvent::ApprovalProfileChanged {
            profile: agent.approval_profile(),
        }),
    }
    Some(true)
}

enum Assertion {
    FileExists(&'static str),
    FileNotExists(&'static str),
    FileContains(&'static str, &'static str),
    FileNotContains(&'static str, &'static str),
    FileEquals(&'static str, &'static str),
    ToolCalled(&'static str),
    ToolCalledTimes(&'static str, usize),
    ToolCalledAny(&'static [&'static str]),
    ToolNotCalled(&'static str),
    AssistantContains(&'static str),
    MaxAssistantTurns(usize),
    CommandSucceeds(&'static str),
    CommandOutputContains(&'static str, &'static str),
    CommandOutputEquals(&'static str, &'static str),
}

struct EvalCase {
    name: &'static str,
    task: &'static str,
    setup: fn(&Path) -> Result<()>,
    configure: fn(&mut Agent),
    assertions: &'static [Assertion],
}

fn noop_eval_config(_: &mut Agent) {}

// Approximation: 4 bytes ≈ 1 token for prose/code. This is a rough estimate — the actual
// tokenizer is provider-side. Use this to spot context-window hogs, not for billing math.
const BYTES_PER_TOKEN_APPROX: usize = 4;

fn approx_tokens_for_message(m: &Message) -> usize {
    let bytes: usize = m
        .content
        .iter()
        .map(|b| match b {
            Block::Text { text } | Block::PartialStream { text } => text.len(),
            Block::Thinking { text } => text.len(),
            Block::ToolUse { input, name, .. } => json_byte_len(input) + name.len(),
            Block::ToolResult { content, .. } => content.len(),
        })
        .sum();
    bytes.div_ceil(BYTES_PER_TOKEN_APPROX)
}

fn render_tokens_report(history: &[Message]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if history.is_empty() {
        out.push_str("history is empty\n");
        return out;
    }
    let per_msg: Vec<usize> = history.iter().map(approx_tokens_for_message).collect();
    let total: usize = per_msg.iter().sum();
    let _ = writeln!(
        out,
        "approx tokens in history: {total} (messages: {}, ~{BYTES_PER_TOKEN_APPROX}b/tok)",
        history.len()
    );
    let mut indexed: Vec<(usize, usize)> = per_msg.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| b.1.cmp(&a.1));
    let top = indexed.iter().take(5);
    let _ = writeln!(out, "top hogs:");
    for (i, toks) in top {
        let m = &history[*i];
        let kinds: Vec<&str> = m
            .content
            .iter()
            .map(|b| match b {
                Block::Text { .. } => "text",
                Block::Thinking { .. } => "thinking",
                Block::ToolUse { .. } => "tool_use",
                Block::ToolResult { .. } => "tool_result",
                Block::PartialStream { .. } => "partial_stream",
            })
            .collect();
        let _ = writeln!(out, "  [{i}] {} ~{toks}t [{}]", m.role, kinds.join(","));
    }
    out
}

fn history_tool_count(history: &[Message], target: &str) -> usize {
    history
        .iter()
        .map(|m| {
            m.content
                .iter()
                .filter(|b| matches!(b, Block::ToolUse { name, .. } if name == target))
                .count()
        })
        .sum()
}

fn history_has_tool(history: &[Message], target: &str) -> bool {
    history_tool_count(history, target) > 0
}

fn assistant_turns(history: &[Message]) -> usize {
    history.iter().filter(|m| m.role == "assistant").count()
}

fn assistant_text(history: &[Message]) -> String {
    history
        .iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn steering_item_keywords(item: &str) -> Vec<String> {
    let body = item
        .split_once(": ")
        .map(|(_, rest)| rest)
        .unwrap_or(item)
        .to_ascii_lowercase();
    let mut keywords = Vec::new();
    for word in body
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| w.chars().count() >= 4)
    {
        if matches!(
            word,
            "queued"
                | "during"
                | "active"
                | "turn"
                | "message"
                | "messages"
                | "this"
                | "that"
                | "what"
                | "tell"
                | "about"
                | "happened"
                | "please"
                | "should"
                | "must"
        ) {
            continue;
        }
        let token = word.to_string();
        if !keywords.iter().any(|existing| existing == &token) {
            keywords.push(token);
        }
        if keywords.len() >= 8 {
            break;
        }
    }
    keywords
}

fn steering_item_acknowledged(item: &str, history: &[Message]) -> bool {
    let assistant = assistant_text(history).to_ascii_lowercase();
    if assistant.trim().is_empty() {
        return false;
    }
    let keywords = steering_item_keywords(item);
    if keywords.is_empty() {
        return assistant.contains("steer") || assistant.contains("queued guidance");
    }
    let hits = keywords
        .iter()
        .filter(|keyword| assistant.contains(keyword.as_str()))
        .count();
    hits >= keywords.len().min(2)
}

fn final_objective_warning(
    objective: &orchestrator::ObjectiveTracker,
    history: &[Message],
) -> Option<String> {
    let coverage = objective.assess_history(history);
    if coverage.unresolved.is_empty() {
        None
    } else {
        Some(format!(
            "final objective warning: unresolved checkpoint(s): {}. If these are complete, mention the evidence; otherwise say what is blocked or still pending.",
            coverage.unresolved.join("; ")
        ))
    }
}

fn run_eval_shell_command(
    root: &Path,
    command: &str,
) -> std::result::Result<(i32, String, String), String> {
    let mut cmd = Command::new("bash");
    cmd.arg("-lc").arg(command).current_dir(root);
    let started_at = std::time::Instant::now();
    let (stdout, stderr, code) = run_sync_command_limited(
        cmd,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "eval command",
        std::time::Duration::from_secs(15),
    )?;
    let duration = started_at.elapsed();
    let stdout = stdout.render("stdout");
    let stderr = stderr.render("stderr");
    let combined = format!("--- stdout ---\n{stdout}--- stderr ---\n{stderr}");
    let _ = write_verification_artifact(
        root,
        "eval-command",
        command,
        &combined,
        Some(code),
        duration,
        if code == 0 { "passed" } else { "failed" },
    );
    Ok((code, stdout, stderr))
}

async fn run_eval_case(case: &EvalCase) -> (bool, Vec<String>) {
    let sandbox = PathBuf::from(format!("target/evals/{}", case.name));
    let _ = std::fs::remove_dir_all(&sandbox);
    if let Err(e) = std::fs::create_dir_all(&sandbox) {
        return (false, vec![format!("setup mkdir failed: {e}")]);
    }
    if let Err(e) = (case.setup)(&sandbox) {
        return (false, vec![format!("fixture setup failed: {e:#}")]);
    }
    let sandbox = std::fs::canonicalize(&sandbox).unwrap_or(sandbox);

    let mut agent = match Agent::new() {
        Ok(a) => a,
        Err(e) => return (false, vec![format!("agent init failed: {e:#}")]),
    };
    if let Err(e) = agent.set_sandbox_root(sandbox.clone()) {
        return (false, vec![format!("sandbox switch failed: {e:#}")]);
    }
    agent.pretty = false;
    agent.silent = true;
    agent.max_iterations = Some(10);
    for t in &agent.tools {
        agent.allowed.insert(t.name.to_string());
    }
    (case.configure)(&mut agent);

    let chat_err = agent
        .chat(case.task.to_string())
        .await
        .err()
        .map(|e| format!("chat error: {e:#}"));

    let final_assistant = assistant_text(&agent.history);
    let mut failures = Vec::new();
    for a in case.assertions {
        let (ok, label) = match a {
            Assertion::FileExists(p) => (sandbox.join(p).exists(), format!("file exists: {p}")),
            Assertion::FileNotExists(p) => (
                !sandbox.join(p).exists(),
                format!("file does not exist: {p}"),
            ),
            Assertion::FileContains(p, needle) => {
                let content = std::fs::read_to_string(sandbox.join(p)).unwrap_or_default();
                (content.contains(needle), format!("{p} contains '{needle}'"))
            }
            Assertion::FileNotContains(p, needle) => {
                let content = std::fs::read_to_string(sandbox.join(p)).unwrap_or_default();
                (
                    !content.contains(needle),
                    format!("{p} does not contain '{needle}'"),
                )
            }
            Assertion::FileEquals(p, expected) => {
                let content = std::fs::read_to_string(sandbox.join(p)).unwrap_or_default();
                (content == *expected, format!("{p} equals expected content"))
            }
            Assertion::ToolCalled(name) => (
                history_has_tool(&agent.history, name),
                format!("tool called: {name}"),
            ),
            Assertion::ToolCalledTimes(name, expected) => {
                let seen = history_tool_count(&agent.history, name);
                (
                    seen == *expected,
                    format!("tool called exactly {expected} time(s): {name} (was {seen})"),
                )
            }
            Assertion::ToolCalledAny(names) => (
                names
                    .iter()
                    .any(|name| history_has_tool(&agent.history, name)),
                format!("any tool called: {}", names.join(" | ")),
            ),
            Assertion::ToolNotCalled(name) => (
                !history_has_tool(&agent.history, name),
                format!("tool NOT called: {name}"),
            ),
            Assertion::AssistantContains(needle) => (
                final_assistant.contains(needle),
                format!("assistant contains '{needle}'"),
            ),
            Assertion::MaxAssistantTurns(n) => {
                let t = assistant_turns(&agent.history);
                (t <= *n, format!("assistant turns ≤ {n} (was {t})"))
            }
            Assertion::CommandSucceeds(command) => {
                match run_eval_shell_command(&sandbox, command) {
                    Ok((code, stdout, stderr)) => (
                        code == 0,
                        format!(
                            "command succeeded: `{command}` (exit={code}, stdout='{}', stderr='{}')",
                            summarize_inline(&stdout, 80),
                            summarize_inline(&stderr, 80)
                        ),
                    ),
                    Err(e) => (false, format!("command succeeded: `{command}` (err: {e})")),
                }
            }
            Assertion::CommandOutputContains(command, needle) => {
                match run_eval_shell_command(&sandbox, command) {
                    Ok((code, stdout, stderr)) => (
                        code == 0 && stdout.contains(needle),
                        format!(
                            "command output contains '{needle}': `{command}` (exit={code}, stdout='{}', stderr='{}')",
                            summarize_inline(&stdout, 80),
                            summarize_inline(&stderr, 80)
                        ),
                    ),
                    Err(e) => (
                        false,
                        format!("command output contains '{needle}': `{command}` (err: {e})"),
                    ),
                }
            }
            Assertion::CommandOutputEquals(command, expected) => {
                match run_eval_shell_command(&sandbox, command) {
                    Ok((code, stdout, stderr)) => (
                        code == 0 && stdout == *expected,
                        format!(
                            "command output equals expected: `{command}` (exit={code}, stdout='{}', stderr='{}')",
                            summarize_inline(&stdout, 80),
                            summarize_inline(&stderr, 80)
                        ),
                    ),
                    Err(e) => (
                        false,
                        format!("command output equals expected: `{command}` (err: {e})"),
                    ),
                }
            }
        };
        if !ok {
            failures.push(label);
        }
    }
    if let Some(e) = chat_err {
        failures.push(e);
    }
    (failures.is_empty(), failures)
}

fn default_eval_cases() -> &'static [EvalCase] {
    &[
        EvalCase {
            name: "read_file",
            task: "Read foo.txt and tell me what it says. Do not modify any files.",
            setup: |p| {
                std::fs::write(p.join("foo.txt"), "alpha beta gamma")?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::AssistantContains("alpha beta gamma"),
                Assertion::ToolNotCalled("write_file"),
                Assertion::MaxAssistantTurns(4),
            ],
        },
        EvalCase {
            name: "create_file",
            task: "Create a file named notes.md whose content is exactly the heading '# Notes'. Then stop.",
            setup: |_| Ok(()),
            configure: noop_eval_config,
            assertions: &[
                Assertion::ToolCalled("write_file"),
                Assertion::FileExists("notes.md"),
                Assertion::FileContains("notes.md", "# Notes"),
            ],
        },
        EvalCase {
            name: "fd_listing",
            task: "List all .rs files under the current directory. Report their paths. Do not modify files.",
            setup: |p| {
                std::fs::write(p.join("a.rs"), "fn a() {}")?;
                std::fs::write(p.join("b.rs"), "fn b() {}")?;
                std::fs::write(p.join("c.txt"), "not rust")?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::AssistantContains("a.rs"),
                Assertion::AssistantContains("b.rs"),
                Assertion::ToolNotCalled("write_file"),
            ],
        },
        EvalCase {
            name: "edit_in_place",
            task: "Edit notes.md in place: replace 'world' with 'wolf'. Do not create a new file.",
            setup: |p| {
                std::fs::write(p.join("notes.md"), "hello world\n")?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::ToolCalledAny(&["edit_file", "multi_edit"]),
                Assertion::ToolNotCalled("write_file"),
                Assertion::FileEquals("notes.md", "hello wolf\n"),
            ],
        },
        EvalCase {
            name: "multi_step_read_then_write",
            task: "Read brief.txt, then create summary.md with exactly this content: '# Summary\n\nalpha beta gamma'. Do not modify brief.txt.",
            setup: |p| {
                std::fs::write(p.join("brief.txt"), "alpha beta gamma")?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::ToolCalled("read_file"),
                Assertion::ToolCalled("write_file"),
                Assertion::FileEquals("summary.md", "# Summary\n\nalpha beta gamma"),
            ],
        },
        EvalCase {
            name: "permission_denied_write",
            task: "Use write_file to create deny.txt with exactly the text 'nope'. If permission is denied, explain that and stop.",
            setup: |_| Ok(()),
            configure: |agent| {
                agent.deny_tools.insert("write_file".to_string());
            },
            assertions: &[
                Assertion::ToolCalledTimes("write_file", 1),
                Assertion::FileNotExists("deny.txt"),
                Assertion::ToolNotCalled("bash"),
                Assertion::MaxAssistantTurns(4),
            ],
        },
        EvalCase {
            name: "rg_search",
            task: "Find lines containing ERROR under the current directory. Do not use bash and do not modify files.",
            setup: |p| {
                std::fs::write(p.join("app.log"), "INFO ok\nERROR failed\nWARN later\n")?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::AssistantContains("ERROR"),
                Assertion::ToolNotCalled("write_file"),
            ],
        },
        EvalCase {
            name: "json_migration_v2",
            task: "Create config/app.v2.json from config/app.json with this migration: keep service, retries, and features; rename timeout_ms to request_timeout_ms; add retry_backoff_ms=250; remove timeout_ms. Do not modify config/app.json.",
            setup: |p| {
                std::fs::create_dir_all(p.join("config"))?;
                std::fs::write(
                    p.join("config/app.json"),
                    "{\n  \"service\": \"billing\",\n  \"timeout_ms\": 5000,\n  \"retries\": 2,\n  \"features\": {\n    \"beta\": false\n  }\n}\n",
                )?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::FileExists("config/app.v2.json"),
                Assertion::FileContains("config/app.v2.json", "\"service\": \"billing\""),
                Assertion::FileContains("config/app.v2.json", "\"request_timeout_ms\": 5000"),
                Assertion::FileContains("config/app.v2.json", "\"retry_backoff_ms\": 250"),
                Assertion::FileContains("config/app.v2.json", "\"retries\": 2"),
                Assertion::FileContains("config/app.v2.json", "\"beta\": false"),
                Assertion::FileNotContains("config/app.v2.json", "\"timeout_ms\""),
                Assertion::CommandSucceeds("test -s config/app.v2.json"),
                Assertion::FileContains("config/app.json", "\"timeout_ms\": 5000"),
            ],
        },
        EvalCase {
            name: "shell_bugfix_discount",
            task: "Fix scripts/calc_total.sh so the third argument is a percentage discount (0-100), not an absolute subtraction. Keep integer arithmetic and clamp totals below 0 to 0. Do not create new files.",
            setup: |p| {
                std::fs::create_dir_all(p.join("scripts"))?;
                std::fs::write(
                    p.join("scripts/calc_total.sh"),
                    "#!/usr/bin/env bash\nset -euo pipefail\nqty=\"${1:-0}\"\nunit=\"${2:-0}\"\ndiscount_pct=\"${3:-0}\"\nsubtotal=$((qty * unit))\n# BUG: discount_pct is treated as an absolute amount.\ntotal=$((subtotal - discount_pct))\necho \"$total\"\n",
                )?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::CommandOutputEquals("bash scripts/calc_total.sh 4 50 10", "180\n"),
                Assertion::CommandOutputContains("bash scripts/calc_total.sh 4 50 10", "180"),
                Assertion::CommandOutputEquals("bash scripts/calc_total.sh 3 20 0", "60\n"),
                Assertion::CommandOutputEquals("bash scripts/calc_total.sh 1 5 200", "0\n"),
            ],
        },
        EvalCase {
            name: "incident_report_from_logs",
            task: "Read logs/api.log and create reports/incident.md with exactly this content:\n# Incident Summary\n\n- ERROR: 2\n- WARN: 1\n- Last ERROR: 2026-04-01T10:05:00Z ERROR payment timeout id=18\n",
            setup: |p| {
                std::fs::create_dir_all(p.join("logs"))?;
                std::fs::write(
                    p.join("logs/api.log"),
                    "2026-04-01T10:00:00Z INFO service started\n2026-04-01T10:01:00Z WARN slow query id=11\n2026-04-01T10:03:00Z ERROR db connection reset id=12\n2026-04-01T10:04:00Z INFO retry succeeded id=12\n2026-04-01T10:05:00Z ERROR payment timeout id=18\n",
                )?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::FileExists("reports/incident.md"),
                Assertion::FileContains("reports/incident.md", "# Incident Summary"),
                Assertion::FileContains("reports/incident.md", "- ERROR: 2"),
                Assertion::FileContains("reports/incident.md", "- WARN: 1"),
                Assertion::FileContains(
                    "reports/incident.md",
                    "- Last ERROR: 2026-04-01T10:05:00Z ERROR payment timeout id=18",
                ),
            ],
        },
    ]
}

async fn run_all_evals(filter: Option<&str>) -> bool {
    let cases = default_eval_cases();
    let mut all_ok = true;
    let mut total = 0usize;
    let mut passed = 0usize;
    let selected: Vec<&EvalCase> = cases
        .iter()
        .filter(|c| filter.is_none_or(|f| c.name == f))
        .collect();
    if selected.is_empty() {
        println!("no cases matched filter");
        return false;
    }
    println!("running {} eval case(s)", selected.len());
    for (i, case) in selected.iter().enumerate() {
        print!("[{}/{}] {}: ", i + 1, selected.len(), case.name);
        io::stdout().flush().ok();
        let (ok, failures) = run_eval_case(case).await;
        total += 1;
        if ok {
            passed += 1;
            println!("PASS");
        } else {
            all_ok = false;
            println!("FAIL");
            for f in failures {
                println!("    - {f}");
            }
        }
    }
    println!("\n=== {} / {} cases passed ===", passed, total);
    all_ok
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CliOptions {
    pub(crate) argv: Vec<String>,
    pub(crate) positional: Vec<String>,
    pub(crate) print: bool,
    pub(crate) resume_latest: bool,
    pub(crate) no_session: bool,
    pub(crate) no_tui: bool,
    pub(crate) fangs_out: bool,
    pub(crate) output: OutputMode,
    pub(crate) cd: Option<PathBuf>,
    pub(crate) fork: bool,
    pub(crate) budget_cap: Option<BudgetCap>,
    pub(crate) approval_profile: Option<ApprovalProfile>,
    pub(crate) sandbox_profile: Option<SandboxProfile>,
    pub(crate) browser_recipe: Option<BrowserRecipe>,
    pub(crate) thinking_effort: Option<ThinkingEffort>,
    pub(crate) context_mode: Option<ContextMode>,
    pub(crate) tool_profile: Option<ToolProfile>,
}

pub(crate) fn parse_cli_options(argv: Vec<String>) -> Result<CliOptions> {
    let mut positional = Vec::new();
    let mut print = false;
    let mut resume_latest = false;
    let mut no_session = false;
    let mut no_tui = false;
    let mut fangs_out = false;
    let mut output = OutputMode::Text;
    let mut cd: Option<PathBuf> = None;
    let mut fork = false;
    let mut budget_cap: Option<BudgetCap> = None;
    let mut approval_profile: Option<ApprovalProfile> = None;
    let mut sandbox_profile: Option<SandboxProfile> = None;
    let mut browser_recipe: Option<BrowserRecipe> = None;
    let mut thinking_effort: Option<ThinkingEffort> = None;
    let mut context_mode: Option<ContextMode> = None;
    let mut tool_profile: Option<ToolProfile> = None;
    let mut i = 0usize;
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "-p" | "--print" => print = true,
            "--resume" => resume_latest = true,
            "--no-session" => no_session = true,
            "--no-tui" => no_tui = true,
            "--fangs-out" | "--mask-off" | "--risk-on" => fangs_out = true,
            "--fork" => fork = true,
            "--frugal" => {
                context_mode = Some(ContextMode::Frugal);
                tool_profile = Some(ToolProfile::Lean);
                if thinking_effort.is_none() {
                    thinking_effort = Some(ThinkingEffort::Medium);
                }
            }
            "--context-mode" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--context-mode requires standard|frugal"))?;
                context_mode = Some(ContextMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --context-mode '{value}' (expected standard|frugal)")
                })?);
            }
            "--tool-profile" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--tool-profile requires full|lean"))?;
                tool_profile = Some(ToolProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --tool-profile '{value}' (expected full|lean)")
                })?);
            }
            "--budget" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--budget requires a positive USD cap (e.g. 0.25) or token cap (e.g. 200k tokens)"))?;
                let cap = BudgetCap::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid --budget '{value}' (expected positive dollars or tokens)"
                    )
                })?;
                budget_cap = Some(cap);
            }
            "--approval" | "--approval-profile" => {
                i += 1;
                let value = argv.get(i).ok_or_else(|| {
                    anyhow::anyhow!("--approval requires ask|auto-read|auto-write|never|always")
                })?;
                approval_profile = Some(ApprovalProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --approval '{value}' (expected ask|auto-read|auto-write|never|always)")
                })?);
            }
            "--sandbox-profile" => {
                i += 1;
                let value = argv.get(i).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--sandbox-profile requires read-only|workspace-write|danger-full-access"
                    )
                })?;
                sandbox_profile = Some(SandboxProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --sandbox-profile '{value}' (expected read-only|workspace-write|danger-full-access)")
                })?);
            }
            "--sandbox" => {
                i += 1;
                let value = argv.get(i).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--sandbox requires DIR or read-only|workspace-write|danger-full-access"
                    )
                })?;
                if matches!(
                    value.as_str(),
                    "read-only"
                        | "readonly"
                        | "ro"
                        | "workspace-write"
                        | "workspace"
                        | "write"
                        | "danger-full-access"
                        | "full-access"
                        | "danger"
                        | "unrestricted"
                ) {
                    sandbox_profile = Some(SandboxProfile::parse(value).ok_or_else(|| {
                        anyhow::anyhow!("invalid --sandbox '{value}' (expected read-only|workspace-write|danger-full-access)")
                    })?);
                } else {
                    cd = Some(PathBuf::from(value));
                }
            }
            "--browser" | "--browser-recipe" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--browser requires off|agent-browser"))?;
                browser_recipe = Some(BrowserRecipe::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --browser '{value}' (expected off|agent-browser)")
                })?);
            }
            "--agent-browser" => browser_recipe = Some(BrowserRecipe::AgentBrowser),
            "--effort" | "--thinking-effort" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--effort requires low|medium|high|xhigh"))?;
                thinking_effort = Some(ThinkingEffort::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid thinking effort '{value}' (expected low|medium|high|xhigh)"
                    )
                })?);
            }
            "--output" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--output requires json or stream-json"))?;
                output = OutputMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --output '{value}' (expected json or stream-json)")
                })?;
            }
            "--cd" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--cd requires a directory"))?;
                cd = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--output=") => {
                let value = arg.trim_start_matches("--output=");
                output = OutputMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --output '{value}' (expected json or stream-json)")
                })?;
            }
            _ if arg.starts_with("--cd=") => {
                cd = Some(PathBuf::from(arg.trim_start_matches("--cd=")));
            }
            _ if arg.starts_with("--effort=") || arg.starts_with("--thinking-effort=") => {
                let value = arg
                    .strip_prefix("--effort=")
                    .or_else(|| arg.strip_prefix("--thinking-effort="))
                    .unwrap_or_default();
                thinking_effort = Some(ThinkingEffort::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid thinking effort '{value}' (expected low|medium|high|xhigh)"
                    )
                })?);
            }
            _ if arg.starts_with("--context-mode=") => {
                let value = arg.trim_start_matches("--context-mode=");
                context_mode = Some(ContextMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid context mode '{value}' (expected standard|frugal)")
                })?);
            }
            _ if arg.starts_with("--tool-profile=") => {
                let value = arg.trim_start_matches("--tool-profile=");
                tool_profile = Some(ToolProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid tool profile '{value}' (expected full|lean)")
                })?);
            }
            _ if arg.starts_with("--budget=") => {
                let value = arg.trim_start_matches("--budget=");
                let cap = BudgetCap::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid --budget '{value}' (expected positive dollars or tokens)"
                    )
                })?;
                budget_cap = Some(cap);
            }
            _ if arg.starts_with("--approval=") || arg.starts_with("--approval-profile=") => {
                let value = arg
                    .strip_prefix("--approval=")
                    .or_else(|| arg.strip_prefix("--approval-profile="))
                    .unwrap_or_default();
                approval_profile = Some(ApprovalProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid approval profile '{value}' (expected ask|auto-read|auto-write|never|always)")
                })?);
            }
            _ if arg.starts_with("--sandbox-profile=") => {
                let value = arg.strip_prefix("--sandbox-profile=").unwrap_or_default();
                sandbox_profile = Some(SandboxProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid sandbox profile '{value}' (expected read-only|workspace-write|danger-full-access)")
                })?);
            }
            _ if arg.starts_with("--sandbox=") => {
                let value = arg.strip_prefix("--sandbox=").unwrap_or_default();
                if matches!(
                    value,
                    "read-only"
                        | "readonly"
                        | "ro"
                        | "workspace-write"
                        | "workspace"
                        | "write"
                        | "danger-full-access"
                        | "full-access"
                        | "danger"
                        | "unrestricted"
                ) {
                    sandbox_profile = Some(SandboxProfile::parse(value).ok_or_else(|| {
                        anyhow::anyhow!("invalid sandbox profile '{value}' (expected read-only|workspace-write|danger-full-access)")
                    })?);
                } else {
                    cd = Some(PathBuf::from(value));
                }
            }
            _ if arg.starts_with("--browser=") || arg.starts_with("--browser-recipe=") => {
                let value = arg
                    .strip_prefix("--browser=")
                    .or_else(|| arg.strip_prefix("--browser-recipe="))
                    .unwrap_or_default();
                browser_recipe = Some(BrowserRecipe::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid browser recipe '{value}' (expected off|agent-browser)")
                })?);
            }
            _ if arg.starts_with('@') => {
                let path = arg.trim_start_matches('@');
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("reading task file {path}"))?;
                positional.push(content);
            }
            _ => positional.push(arg.clone()),
        }
        i += 1;
    }
    Ok(CliOptions {
        argv,
        positional,
        print,
        resume_latest,
        no_session,
        no_tui,
        fangs_out,
        output,
        cd,
        fork,
        budget_cap,
        approval_profile,
        sandbox_profile,
        browser_recipe,
        thinking_effort,
        context_mode,
        tool_profile,
    })
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|v| {
        let t = v.trim().to_ascii_lowercase();
        !(t.is_empty() || t == "0" || t == "false" || t == "off" || t == "no")
    })
}

fn autosave_latest(agent: &mut Agent) {
    if agent.session_enabled {
        agent.checkpoint_latest_session("outer_loop_autosave");
    }
}

fn fixup_path() {
    #[cfg(windows)]
    {
        let mut extras: Vec<String> = Vec::new();
        if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
            extras.push(format!(r"{local_app}\Microsoft\WinGet\Links"));
        }
        if let Ok(roaming) = std::env::var("APPDATA") {
            extras.push(format!(r"{roaming}\Python\Python312\Scripts"));
        }
        let current = std::env::var("PATH").unwrap_or_default();
        let existing: Vec<&str> = current.split(';').collect();
        let to_add: Vec<String> = extras
            .into_iter()
            .filter(|e| !existing.iter().any(|p| p.eq_ignore_ascii_case(e)))
            .collect();
        if !to_add.is_empty() {
            let new = format!("{};{}", to_add.join(";"), current);
            // Safe: single-threaded, called once before any child spawn.
            unsafe {
                std::env::set_var("PATH", new);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    fixup_path();

    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_if_tui();
        release_registered_locks();
        if panic_info_is_broken_pipe(info) {
            std::process::exit(0);
        }
        if let Some(path) = write_crash_snapshot(info) {
            eprintln!("[wolf crash snapshot: {}]", path.display());
        }
        default_panic(info);
    }));

    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().is_some_and(|a| a == "subagent-worker") {
        if argv.len() != 3 {
            eprintln!("usage: wolf subagent-worker REQUEST_JSON REPORT_MD");
            release_registered_locks();
            std::process::exit(2);
        }
        let result = run_subagent_worker(Path::new(&argv[1]), Path::new(&argv[2])).await;
        release_registered_locks();
        return result;
    }
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        println!("wolf {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(code) = handle_auth_cli(&argv)? {
        release_registered_locks();
        std::process::exit(code);
    }
    if let Some(code) = handle_session_cli(&argv)? {
        release_registered_locks();
        std::process::exit(code);
    }
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: wolf [TASK...]        run one-shot with TASK (joined with spaces)");
        println!("       wolf -p               read task from stdin, run one-shot");
        println!(
            "       wolf --resume         resume the project-scoped auto-saved latest session"
        );
        println!("       wolf sessions         list project latest + named sessions");
        println!("       wolf session map [latest|NAME|PATH]");
        println!("       wolf session packet [latest|NAME|PATH] @wNN");
        println!("       wolf session focus [latest|NAME|PATH] @wNN [--exact]");
        println!("       wolf session tracks");
        println!("       wolf session track open [latest|NAME|PATH] @wNN [name]");
        println!("       wolf session export [latest|NAME|PATH] [html|jsonl] [OUT]");
        println!("       wolf subagent-worker REQUEST_JSON REPORT_MD");
        println!("       wolf session analyze|grep|failures|verify-log|decisions [session]");
        println!(
            "       wolf --no-session     run without project state lock, latest session, or log writes"
        );
        println!("       wolf --fork           resume latest into an isolated, unsaved branch");
        println!("       wolf --cd DIR         use DIR as sandbox/cwd");
        println!("       wolf --output json|stream-json  emit machine-readable output");
        println!(
            "       wolf --budget CAP     stop before more model calls once CAP is reached ($ or tokens)"
        );
        println!("       wolf --approval ask|auto-read|auto-write|never|always");
        println!("       wolf --sandbox read-only|workspace-write|danger-full-access");
        println!("       wolf --browser agent-browser    add optional browser automation recipe");
        println!("       wolf --effort low|medium|high|xhigh  set provider reasoning effort");
        println!(
            "       wolf --frugal        minimize prompt/tool/history context for lower token cost"
        );
        println!("       wolf --context-mode standard|frugal");
        println!(
            "       wolf --tool-profile lean|full  choose provider tool schema verbosity (default lean)"
        );
        println!("       wolf --eval [NAME]    run eval harness (optionally a single case)");
        println!("       wolf --fangs-out      dangerous mode: auto-approve gated tools");
        println!("       wolf auth ...         provider/model/auth management commands");
        println!("       wolf                  interactive REPL (or reads stdin if piped)");
        println!(
            "env:   WOLF_PROVIDER, WOLF_PROFILE, WOLF_MODEL, WOLF_MODEL_<PROVIDER>, WOLF_MODEL_FORCE=1, WOLF_BASE_URL, WOLF_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY, CHATGPT_ACCESS_TOKEN, OPENROUTER_API_KEY, ZAI_API_KEY, ANTHROPIC_BASE_URL, OPENAI_BASE_URL, WOLF_SYSTEM, WOLF_SANDBOX, WOLF_EXTERNAL_TIMEOUT_SECS, WOLF_BASH_TIMEOUT_SECS, WOLF_HOOK_TIMEOUT_SECS, WOLF_SESSIONS_DIR, WOLF_LOGS_DIR, WOLF_LOG_ARCHIVES (0-16 rotated archives of latest.log; default 0 keeps truncation-only), WOLF_FANGS_OUT=1, WOLF_NO_TUI=1, WOLF_THINKING_EFFORT=low|medium|high|xhigh, WOLF_CONTEXT_MODE=standard|frugal, WOLF_TOOL_PROFILE=lean|full, WOLF_BUDGET_CAP, WOLF_APPROVAL, WOLF_SANDBOX_PROFILE, WOLF_BROWSER_RECIPE"
        );
        return Ok(());
    }
    if let Some(pos) = argv.iter().position(|a| a == "--eval") {
        let filter = argv.get(pos + 1).map(|s| s.as_str());
        let ok = run_all_evals(filter).await;
        release_registered_locks();
        std::process::exit(if ok { 0 } else { 1 });
    }
    let opts = parse_cli_options(argv.clone())?;
    let fangs_out = opts.fangs_out || env_flag("WOLF_FANGS_OUT");

    let one_shot_task: Option<String> = if !opts.positional.is_empty() {
        Some(opts.positional.join(" "))
    } else if opts.print {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s)?;
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    } else {
        None
    };

    let mut agent = Agent::new_with_sandbox(opts.cd.clone(), !opts.no_session && !opts.fork)?;
    if let Some(profile) = std::env::var("WOLF_APPROVAL")
        .ok()
        .and_then(|v| ApprovalProfile::parse(&v))
    {
        agent.set_approval_profile(profile);
    }
    if let Some(profile) = std::env::var("WOLF_SANDBOX_PROFILE")
        .ok()
        .and_then(|v| SandboxProfile::parse(&v))
    {
        agent.set_sandbox_profile(profile);
    }
    if let Some(cap) = opts.budget_cap {
        agent.set_budget_cap(Some(cap));
    }
    if let Some(profile) = opts.approval_profile {
        agent.set_approval_profile(profile);
    }
    if let Some(profile) = opts.sandbox_profile {
        agent.set_sandbox_profile(profile);
    }
    if let Some(recipe) = opts.browser_recipe {
        agent.set_browser_recipe(recipe);
    }
    if let Some(effort) = opts.thinking_effort {
        agent.set_thinking_effort(effort);
    }
    if let Some(mode) = opts.context_mode {
        agent.context_mode = mode;
        if mode.is_frugal() {
            agent.tool_profile = ToolProfile::Lean;
        }
        agent.refresh_tools_for_context();
    }
    if let Some(profile) = opts.tool_profile {
        agent.tool_profile = profile;
    }
    if agent.context_mode.is_frugal() {
        agent.tool_profile = ToolProfile::Lean;
    }
    agent.refresh_tools_for_context();
    if opts.fork {
        agent.suppress_checkpoints = true;
        agent.session_enabled = false;
        agent.state_lock = None;
    }
    if opts.output.is_json() {
        agent.pretty = false;
        agent.set_sink(Box::new(JsonSink::new(opts.output, false, false)));
    }
    if opts.resume_latest || opts.fork {
        match agent.load_latest_session() {
            Ok(path) => {
                if let Some(mode) = opts.context_mode {
                    agent.context_mode = mode;
                    if mode.is_frugal() {
                        agent.tool_profile = ToolProfile::Lean;
                    }
                    agent.refresh_tools_for_context();
                }
                if let Some(profile) = opts.tool_profile {
                    agent.tool_profile = profile;
                }
                if agent.context_mode.is_frugal() {
                    agent.tool_profile = ToolProfile::Lean;
                }
                agent.refresh_tools_for_context();
                if opts.fork {
                    agent.suppress_checkpoints = true;
                    agent.session_enabled = false;
                    agent.state_lock = None;
                }
                if !opts.output.is_json() {
                    if opts.fork {
                        eprintln!(
                            "[forked {} messages from {}; autosave disabled]",
                            agent.history.len(),
                            path.display()
                        );
                    } else {
                        eprintln!(
                            "[resumed {} messages from {}]",
                            agent.history.len(),
                            path.display()
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("[error] failed to resume latest session: {e:#}");
                release_registered_locks();
                std::process::exit(1);
            }
        }
    }
    if fangs_out {
        let changed = agent.set_fangs_out(true);
        if !opts.output.is_json() {
            eprintln!("[danger] fangs-out enabled — {changed} gated tools now auto-approving");
        }
    }
    if !opts.output.is_json() {
        if let Some(cap) = agent.budget_cap {
            eprintln!("[budget] cap {}", cap.line());
        }
        if agent.approval_profile() != ApprovalProfile::Ask {
            eprintln!("[approval] profile {}", agent.approval_profile().as_str());
        }
        if agent.sandbox_profile() != SandboxProfile::WorkspaceWrite {
            eprintln!("[sandbox] profile {}", agent.sandbox_profile().as_str());
        }
        if agent.context_mode.is_frugal() {
            eprintln!("[context] frugal mode — lean tools, smaller caps, deterministic compaction");
        } else if agent.tool_context_profile() != ToolContextProfile::Full {
            eprintln!("[tools] profile {}", agent.tool_context_profile().as_str());
        }
        if agent.browser_recipe() != BrowserRecipe::Disabled {
            eprintln!("[browser] recipe {}", agent.browser_recipe().as_str());
        }
    }

    let use_tui = !opts.no_tui
        && !opts.output.is_json()
        && std::env::var("WOLF_NO_TUI").is_err()
        && io::stdout().is_terminal();

    if use_tui && !opts.print {
        let result = tui::run(agent, one_shot_task).await;
        release_registered_locks();
        return result;
    }

    let interrupt = agent.interrupt.clone();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_ok() {
                if interrupt.swap(true, Ordering::SeqCst) {
                    eprintln!("\n[^C again — exiting]");
                    release_registered_locks();
                    std::process::exit(130);
                }
                eprintln!("\n[^C received — interrupting current response; press again to exit]");
            }
        }
    });

    if let Some(task) = one_shot_task {
        let result = agent.chat(task).await;
        autosave_latest(&mut agent);
        return match result {
            Ok(()) => {
                if !opts.output.is_json() {
                    println!();
                }
                Ok(())
            }
            Err(e) => {
                if opts.output.is_json() {
                    agent.sink.emit(AgentEvent::Error(format!("{e:#}")));
                } else {
                    eprintln!("[error] {e:#}");
                }
                release_registered_locks();
                std::process::exit(1);
            }
        };
    }

    let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let agent_busy_flag: std::sync::Arc<std::sync::atomic::AtomicBool> =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    agent.install_steering(steer_rx, steer_tx.clone());
    if opts.fork {
        println!(
            "fork: autosave disabled; use /save <name> or /export [path] to keep this branch."
        );
    }
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let input_tx = input_tx.clone();
        let steering_tx = steer_tx.clone();
        let busy = agent_busy_flag.clone();
        std::thread::spawn(move || {
            let stdin = io::stdin();
            loop {
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if route_interactive_input_line(line, &busy, &input_tx, &steering_tx)
                            == InteractiveInputRoute::SteeringQueued
                        {
                            eprintln!("[queued for next response]");
                        }
                    }
                }
            }
        });
    }

    let mut stdout = io::stdout();

    println!("wolf — chat loop with tools. /help for commands, empty line or Ctrl+D to exit.");
    println!("sandbox: {}", agent.sandbox_root.display());
    println!("{}", agent.provider_status_line());

    loop {
        print!("you> ");
        stdout.flush()?;

        let input = match input_rx.recv().await {
            Some(line) => line,
            None => {
                println!();
                break;
            }
        };

        if input.is_empty() {
            break;
        }

        if agent_busy_flag.load(std::sync::atomic::Ordering::SeqCst) {
            if apply_runtime_control_command(&mut agent, &input, |msg| println!("{msg}")) {
                autosave_latest(&mut agent);
                continue;
            }
            let _ = agent.steering_sender().send(input.clone());
            println!("[queued for next response]");
            autosave_latest(&mut agent);
            continue;
        }

        if let Some(parsed) = parse_compact_slash(&input) {
            match parsed {
                Ok(CompactSlash::RunNow) => {
                    if let Err(e) = agent.compact().await {
                        eprintln!("[compact error] {e:#}");
                    }
                }
                Ok(CompactSlash::Status) => {
                    let current = agent.compact_threshold_chars();
                    let active = agent.active_compact_threshold_chars();
                    let base =
                        history_char_budget_with_override(&agent.model, None, agent.context_mode);
                    match agent.compact_threshold_override_percent() {
                        Some(percent) => {
                            println!(
                                "compact threshold: {current} chars ({percent}% of model context window; active-run trigger {active}; auto baseline {base})"
                            );
                        }
                        None => {
                            println!(
                                "compact threshold: {current} chars (auto: {} mode; active-run trigger {active})",
                                agent.context_mode.as_str()
                            );
                        }
                    }
                }
                Ok(CompactSlash::Auto) => {
                    agent.set_compact_threshold_auto();
                    println!(
                        "compact threshold reset to auto {} ({})",
                        agent.context_mode.as_str(),
                        agent.compact_threshold_chars()
                    );
                }
                Ok(CompactSlash::SetPercent(percent)) => {
                    let chars = agent.set_compact_threshold_percent(percent);
                    println!("compact threshold set to {percent}% -> {chars} chars");
                }
                Err(msg) => println!("{msg}"),
            }
            autosave_latest(&mut agent);
            continue;
        }

        if input == "/plan" || input.starts_with("/plan ") {
            let task = input.strip_prefix("/plan").unwrap_or("").trim();
            if task.is_empty() {
                println!("usage: /plan <task>");
            } else {
                agent_busy_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Err(e) = agent.run_plan(task.to_string()).await {
                    eprintln!("[plan error] {e:#}");
                }
                agent_busy_flag.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            autosave_latest(&mut agent);
            continue;
        }

        if input == "/subagent" || input.starts_with("/subagent ") {
            let raw = input.strip_prefix("/subagent").unwrap_or("").trim();
            if raw.is_empty() {
                println!(
                    "usage: /subagent <task> [--tools t1,t2] [--max-iter N] [--system PROMPT] [--readonly] [--inline|--detached]"
                );
            } else {
                agent_busy_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Err(e) = agent.run_subagent_cmd(raw.to_string()).await {
                    eprintln!("[subagent error] {e:#}");
                }
                agent_busy_flag.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            autosave_latest(&mut agent);
            continue;
        }

        if let Some(keep_going) = handle_slash(&input, &mut agent) {
            autosave_latest(&mut agent);
            if !keep_going {
                break;
            }
            continue;
        }

        match agent.try_consume_pending_login_input(&input) {
            Ok(Some(msg)) => {
                println!("{msg}");
                autosave_latest(&mut agent);
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[login error] {e:#}\n");
                autosave_latest(&mut agent);
                continue;
            }
        }

        agent_busy_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let chat_result = agent.chat(input).await;
        agent_busy_flag.store(false, std::sync::atomic::Ordering::SeqCst);

        if let Err(e) = chat_result {
            eprintln!("[error] {e:#}\n");
        } else {
            println!();
        }
        autosave_latest(&mut agent);
    }

    autosave_latest(&mut agent);
    Ok(())
}
