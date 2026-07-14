mod git_checkpoints;
mod list_render;
mod memory_merge;
mod mutation_preview;
mod orchestrator;
mod packs;
mod provider;
mod sandbox;
mod session;
mod shelves;
mod tool_policy;
mod tools;
mod tui;

#[cfg(test)]
mod main_tests;

use anyhow::{Context, Result};
use provider::{
    ANTHROPIC_API_VERSION, ApiProvider, ModelPricing, ProviderProfile, RequestContract,
    ResolvedModelSpec, ResolvedProviderConfig, apply_provider_headers, auth_store_path,
    build_chatgpt_request, build_chatgpt_summary_request, built_in_provider_profiles,
    cancel_pending_oauth_login, canonical_provider_id, extract_oauth_code_from_callback,
    find_provider_profile, handle_auth_cli, list_models_for_available_providers,
    list_models_for_provider, load_auth_store, load_provider_catalog, login_provider,
    logout_provider, looks_like_login_secret_input, normalize_provider_model_value,
    provider_auth_status, provider_catalog_path, provider_id_from_selector, provider_request_url,
    refresh_local_llama_context_window, render_provider_list, render_provider_picker,
    request_contract_for_profile, resolve_active_provider_id, resolve_model_spec,
    resolve_provider_model_selection, resolve_runtime_provider, set_active_provider_in_catalog,
    set_provider_default_model_in_catalog, try_complete_oauth_from_callback,
};
use session::{
    SessionStateLock, append_log_event, atomic_write_bytes, canonicalize_read_tool_path,
    canonicalize_tool_path, dext_state_dir, expand_user_path, latest_session_path,
    list_session_records_for_root, named_session_path_for_root, named_sessions_dir_for_root,
    new_session_id, parse_session_header, project_key, project_latest_session_path,
    release_registered_locks, remove_stale_session_state_lock, render_limited_csv,
    restore_terminal_if_tui, session_artifacts_dir, session_latest_log_path,
    session_latest_session_path, session_state_lock_is_live, session_state_lock_path,
    session_sudo_dir, session_todo_path, unix_timestamp_secs,
};
use tools::{
    Tool, ToolProfile, is_external_process_tool, needs_permission, provider_tool_definitions,
    should_parallelize_builtin_tools,
};

#[cfg(test)]
use provider::{
    AuthStore, ProviderCatalog, StoredCredential, login_provider_with_key, normalize_login_secret,
    oauth_exchange_failure_result_message, resolve_provider_api_key, save_auth_store,
    save_provider_catalog,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const TOOL_RESULT_CAP: usize = 12_000;
const FRUGAL_TOOL_RESULT_CAP: usize = 6_000;
const TEXT_TOOL_CAPTURE_CAP: usize = 10_000;
const FRUGAL_TEXT_TOOL_CAPTURE_CAP: usize = 6_000;
const READ_FILE_EXPLICIT_CAPTURE_CAP: usize = 16_000;
const FRUGAL_READ_FILE_EXPLICIT_CAPTURE_CAP: usize = 10_000;
const PROCESS_STREAM_CAPTURE_CAP: usize = 6_000;
const LIVE_OUTPUT_EVENT_QUEUE_CAP: usize = 256;
const EDIT_MATCH_CONTEXT_LINES: usize = 2;
const EDIT_MATCH_DISPLAY_LIMIT: usize = 8;
const EDIT_MATCH_CONTEXT_CAP: usize = 6_000;
const READ_SYMBOL_SUGGESTION_LIMIT: usize = 5;
const CARGO_DIAGNOSTIC_SUMMARY_LIMIT: usize = 20;
const HTTP_EXTRACT_INPUT_CAP: usize = 128_000;
const HTTP_EXTRACT_OUTPUT_CAP: usize = 24_000;
const HTTP_TOOL_REDIRECT_LIMIT: usize = 10;
const HTTP_TOOL_ALLOW_LINK_LOCAL_ENV: &str = "DEXT_HTTP_ALLOW_LINK_LOCAL";
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
const SESSION_STATE_LOCK_NAME: &str = "session.lock.json";
const STREAM_EVENT_BUFFER_CAP: usize = 256_000;
const TOOL_SUMMARY_CHAR_CAP: usize = 180;
const TOOL_UI_CONTENT_CAP: usize = 8_000;
const SUDO_ASKPASS_ENV: &str = "DEXT_SUDO_ASKPASS";
const SUDO_PASSWORD_FIFO_ENV: &str = "DEXT_SUDO_PASSWORD_FIFO";
const SUDO_AUTH_GUIDANCE: &str = "sudo auth is local only. If this command needs sudo, use the local prompt Dext opens; never type sudo passwords into chat/steering input.";
const GIT_AUTH_GUIDANCE: &str = "git needs credentials for an HTTPS remote. Enter the token/password here (masked, kept local, never sent to the model). Use username:token if a specific username is required; otherwise the username defaults to x-access-token. Never paste tokens into chat input.";
const VERIFICATION_ARTIFACT_TAIL_CAP: usize = 2_000;
pub(crate) const BASH_UNSAFE_FLAG_OVERRIDE_ENV: &str = "DEXT_ALLOW_BREAK_SYSTEM_PACKAGES";
const AUTH_CIRCUIT_BREAKER_THRESHOLD: usize = 2;
const TOOL_CATALOG_VERSION: u32 = 4;
const DEFAULT_DISCOVERY_EXCLUDES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".dext",
    "target",
    "node_modules",
    ".next",
    ".nuxt",
    ".turbo",
    "dist",
    "build",
    "coverage",
    ".venv",
    "venv",
    "__pycache__",
];
const MAX_STREAM_ATTEMPTS: u32 = 4;
// Inner per-request HTTP retry budget (distinct from the outer stream-restart
// budget MAX_STREAM_ATTEMPTS, even though they currently share the value).
const MAX_HTTP_ATTEMPTS: u32 = 4;
// Consecutive 5xx responses from one provider before it is disabled for the turn.
const MAX_CONSECUTIVE_SERVER_ERRORS: usize = 3;
const DEFAULT_INPUT_USD_PER_MTOK: f64 = 1.0;
const DEFAULT_OUTPUT_USD_PER_MTOK: f64 = 5.0;
const DEFAULT_CACHE_READ_USD_PER_MTOK: f64 = 0.1;
const DEFAULT_CACHE_CREATE_USD_PER_MTOK: f64 = 1.25;
const SESSION_HTML_STYLE: &str = r#"body{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;max-width:980px;margin:2rem auto;padding:0 1rem;background:#0f1115;color:#e6edf3}a{color:#8ab4ff}.meta{color:#9aa4b2;margin-bottom:1.5rem}.msg{border:1px solid #283241;border-radius:12px;margin:1rem 0;padding:1rem;background:#151922}.role{font-weight:700;text-transform:uppercase;font-size:.8rem;letter-spacing:.08em;margin-bottom:.6rem;color:#9aa4b2}.user{border-left:4px solid #7dd3fc}.assistant{border-left:4px solid #a78bfa}.tool{border-left:4px solid #f59e0b}.thinking{color:#9aa4b2}.block{white-space:pre-wrap;line-height:1.45}.tool-name{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;color:#fbbf24}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#0b0d12;border:1px solid #283241;border-radius:8px;padding:.8rem}code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}.err{color:#fca5a5}.ok{color:#86efac}summary{cursor:pointer}.footer{margin:2rem 0;color:#687385;font-size:.85rem}"#;

fn api_family_label(contract: RequestContract) -> &'static str {
    contract.as_str()
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
    Off,
    Low,
    #[default]
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingEffort {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    fn guidance(self) -> &'static str {
        match self {
            Self::Off => {
                "Disable provider-side reasoning controls where supported; answer directly."
            }
            Self::Low => "Keep reasoning concise and deliver a direct answer quickly.",
            Self::Medium => "Use balanced reasoning depth and concise explanations.",
            Self::High => "Reason carefully through edge cases before answering.",
            Self::XHigh => {
                "Use deep, methodical reasoning with explicit verification before answering."
            }
            Self::Max => {
                "Use maximum supported reasoning depth with explicit verification before answering."
            }
        }
    }

    fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disable" | "disabled" | "0" => Some(Self::Off),
            "low" | "l" => Some(Self::Low),
            "medium" | "med" | "m" | "default" => Some(Self::Medium),
            "high" | "h" => Some(Self::High),
            "xhigh" | "x-high" | "veryhigh" | "very-high" | "xh" => Some(Self::XHigh),
            "max" | "maximum" | "ultra" | "ultracode" => Some(Self::Max),
            _ => None,
        }
    }

    fn cycle(self, step: i8) -> Self {
        let levels = [
            Self::Off,
            Self::Low,
            Self::Medium,
            Self::High,
            Self::XHigh,
            Self::Max,
        ];
        let idx = levels.iter().position(|v| *v == self).unwrap_or(2) as i32;
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

pub(crate) fn pseudo_tool_redaction_marker() -> &'static str {
    "[tool call redacted; waiting for structured tool event]"
}

fn text_line_looks_like_pseudo_tool_payload(line: &str) -> bool {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.starts_with('{')
        || trimmed.starts_with('}')
        || trimmed.starts_with('[')
        || trimmed.starts_with(']')
        || trimmed.starts_with('"')
        || lower.starts_with("recipient_name")
        || lower.starts_with("parameters")
        || lower.starts_with("arguments")
        || lower.starts_with("command")
        || lower.starts_with("input")
        || lower.starts_with("name")
        || lower.starts_with("type")
}

fn pseudo_tool_line_opens_payload_block(line: &str) -> bool {
    let trimmed = line.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("<tool_call") {
        return !lower.contains("</tool_call>");
    }
    if lower == "to=" || lower == "to =" {
        return true;
    }
    if lower.starts_with("to=")
        || lower.starts_with("to =")
        || lower.starts_with("functions.")
        || lower.starts_with("multi_tool_use.")
        || lower.starts_with("tool_use")
        || lower.starts_with("function_call")
        || lower.starts_with("<|tool")
        || lower.starts_with("<tool")
    {
        return !trimmed.contains('}');
    }
    if trimmed.starts_with('{') {
        return !trimmed.ends_with('}');
    }
    text_line_looks_like_pseudo_tool_payload(trimmed)
}

pub(crate) fn redact_pseudo_tool_protocol_text(text: &str) -> String {
    let mut redacted = false;
    let mut redacting_payload = false;
    let mut redacting_xml = false;
    let mut lines = Vec::new();
    let marker = pseudo_tool_redaction_marker();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if redacting_xml {
            if let Some(end) = lower.find("</tool_call>") {
                redacting_xml = false;
                let tail = line[end + "</tool_call>".len()..].trim();
                if !tail.is_empty() {
                    lines.push(tail.to_string());
                }
            }
            continue;
        }
        if let Some(start) = lower.find("<tool_call") {
            redacted = true;
            let prefix = line[..start].trim_end();
            if !prefix.is_empty() {
                lines.push(prefix.to_string());
            }
            if lines.last().is_none_or(|previous| previous != marker) {
                lines.push(marker.to_string());
            }
            redacting_xml = !lower[start..].contains("</tool_call>");
            continue;
        }
        if redacting_payload {
            if line.trim().is_empty() {
                redacting_payload = false;
                lines.push(line.to_string());
                continue;
            }
            if text_line_looks_like_pseudo_tool_payload(line) {
                continue;
            }
            redacting_payload = false;
        }
        if text_line_looks_like_pseudo_tool_syntax(line)
            || text_line_looks_like_pseudo_tool_start(line)
        {
            redacted = true;
            while lines
                .last()
                .is_some_and(|previous| matches!(previous.trim(), "{" | "["))
            {
                lines.pop();
            }
            if lines.last().is_none_or(|previous| previous != marker) {
                lines.push(marker.to_string());
            }
            redacting_payload = pseudo_tool_line_opens_payload_block(line);
        } else {
            lines.push(line.to_string());
        }
    }
    if !redacted {
        return text.to_string();
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn redact_pseudo_tool_protocol_blocks(blocks: &[Block]) -> Vec<Block> {
    blocks
        .iter()
        .map(|block| match block {
            Block::Text { text } => Block::Text {
                text: redact_pseudo_tool_protocol_text(text),
            },
            Block::PartialStream { text } => Block::PartialStream {
                text: redact_pseudo_tool_protocol_text(text),
            },
            Block::Thinking { text, signature } => Block::Thinking {
                text: redact_pseudo_tool_protocol_text(text),
                signature: signature.clone(),
            },
            other => other.clone(),
        })
        .collect()
}

fn assistant_blocks_for_context(blocks: &[Block], context_mode: ContextMode) -> Vec<Block> {
    if context_mode.is_frugal() {
        redact_pseudo_tool_protocol_blocks(blocks)
    } else {
        blocks.to_vec()
    }
}

fn maybe_preserve_partial_stream(
    blocks: &[Block],
    history: &mut Vec<Message>,
    context_mode: ContextMode,
) -> bool {
    if blocks.is_empty()
        || !blocks
            .iter()
            .any(|b| matches!(b, Block::Text { .. } | Block::PartialStream { .. }))
    {
        return false;
    }
    let visible_blocks = assistant_blocks_for_context(blocks, context_mode);
    if let Some(last) = history.last()
        && last.role == "assistant"
        && last.content == visible_blocks
    {
        return false;
    }
    history.push(Message {
        role: "assistant".to_string(),
        content: visible_blocks,
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
        AgentEvent::ToolOutputDelta { .. } => None,
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
        AgentEvent::RuntimeControl(s) => {
            Some(format!("runtime_control:{}", summarize_inline(s, 80)))
        }
        AgentEvent::RuntimeControlApplied {
            commands,
            model_changed,
            effort_changed,
            stream_aborted,
        } => Some(format!(
            "runtime_control_applied:{commands}:model={model_changed}:effort={effort_changed}:abort={stream_aborted}"
        )),
        AgentEvent::SteeringReceived { messages, preview } => Some(format!(
            "steering:{messages}:{}",
            summarize_inline(preview, 80)
        )),
        AgentEvent::TurnEnd { failed, .. } => Some(if *failed {
            "turn_end:failed".to_string()
        } else {
            "turn_end".to_string()
        }),
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
    let path = dext_state_dir().join("crashes").join(format!("{id}.json"));
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
    head: Vec<u8>,
    tail: Vec<u8>,
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

    fn tail_cap(&self) -> usize {
        self.cap / 2
    }

    fn head_cap(&self) -> usize {
        self.cap - self.tail_cap()
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        let cap = self.tail_cap();
        if cap == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() >= cap {
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - cap..]);
            return;
        }
        let overflow = self
            .tail
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(cap);
        if overflow > 0 {
            self.tail.drain(0..overflow);
        }
        self.tail.extend_from_slice(bytes);
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.observed_bytes += chunk.len();
        if !self.truncated && self.head.len() + chunk.len() <= self.cap {
            self.head.extend_from_slice(chunk);
            return;
        }
        if self.cap == 0 {
            self.truncated = true;
            return;
        }
        if !self.truncated {
            self.truncated = true;
            let head_cap = self.head_cap();
            if self.head.len() > head_cap {
                let overflow = self.head.split_off(head_cap);
                self.push_tail(&overflow);
            }
        }

        let head_cap = self.head_cap();
        let mut rest = chunk;
        if self.head.len() < head_cap {
            let take = (head_cap - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        self.push_tail(rest);
    }

    fn render(&self, label: &str) -> String {
        let mut out = String::from_utf8_lossy(&self.head).to_string();
        if self.truncated {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!(
                "\n…[{label} capped after {} bytes observed; kept first {} and last {}]\n",
                self.observed_bytes,
                self.head.len(),
                self.tail.len()
            ));
            if !self.tail.is_empty() {
                out.push_str(&String::from_utf8_lossy(&self.tail));
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
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

const TOOL_OUTPUT_NARROW_HINT: &str =
    "Narrow/paginate; prefer symbol-scoped reads or targeted diff/stat before retrying.";

fn cap_tool_output_with_cap(s: String, cap: usize) -> String {
    cap_bytes_with_hint(s, cap, TOOL_OUTPUT_NARROW_HINT)
}

/// Head+tail cap for process-style output: build/test runs put the verdict at
/// the end, so the tail must survive capping (head-only capping made the model
/// re-run commands just to see the failure summary).
fn cap_bytes_head_tail_with_hint(s: String, cap: usize, hint: &str) -> String {
    if s.len() <= cap {
        return s;
    }
    let head_cap = cap.saturating_mul(2) / 3;
    let tail_cap = cap.saturating_sub(head_cap);
    let head = byte_prefix_at_char_boundary(&s, head_cap);
    let tail = byte_suffix_at_char_boundary(&s, tail_cap);
    let suffix = if hint.is_empty() {
        String::new()
    } else {
        format!(" {hint}")
    };
    format!(
        "{head}\n\n…[truncated {} bytes total; kept first {} and last {}.{}]\n\n{tail}",
        s.len(),
        head.len(),
        tail.len(),
        suffix
    )
}

fn cap_tool_output(s: String) -> String {
    cap_tool_output_with_cap(s, TOOL_RESULT_CAP)
}

fn insert_runtime_notes(content: &mut String, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    let notes_block = notes
        .iter()
        .map(|note| format!("[runtime-note] {note}"))
        .collect::<Vec<_>>()
        .join("\n");
    let updated = if content.starts_with("exit:") {
        if let Some((first, tail)) = content.split_once('\n') {
            if tail.is_empty() {
                format!("{first}\n\n{notes_block}")
            } else {
                format!("{first}\n\n{notes_block}\n{tail}")
            }
        } else {
            format!("{content}\n\n{notes_block}")
        }
    } else if content.is_empty() {
        notes_block
    } else {
        format!("{notes_block}\n{content}")
    };
    *content = updated;
}

/// Squash repeated identical error text while preserving one ToolResult block
/// per tool call.  Providers require every tool_use id to receive a matching
/// result, so this must not remove blocks.
fn squash_identical_error_result_content(results: Vec<Block>) -> Vec<Block> {
    if results.len() <= 2 {
        return results;
    }

    let mut squashed = Vec::with_capacity(results.len());
    let mut idx = 0usize;
    while idx < results.len() {
        let Some(content) = error_result_content(&results[idx]) else {
            squashed.push(results[idx].clone());
            idx += 1;
            continue;
        };

        let mut end = idx + 1;
        while end < results.len()
            && error_result_content(&results[end]).is_some_and(|next| next == content)
        {
            end += 1;
        }

        let run_len = end - idx;
        if run_len < 3 {
            squashed.extend(results[idx..end].iter().cloned());
        } else {
            for (offset, block) in results[idx..end].iter().cloned().enumerate() {
                let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    metadata,
                } = block
                else {
                    squashed.push(block);
                    continue;
                };
                let content = if offset == 0 {
                    format!(
                        "{content}\n\n[squashed: {run_len} identical error results in this run; duplicate payloads elided below]"
                    )
                } else {
                    format!(
                        "[duplicate error elided: same as previous tool result; item {}/{} in run]",
                        offset + 1,
                        run_len
                    )
                };
                squashed.push(Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    metadata,
                });
            }
        }

        idx = end;
    }
    squashed
}

fn error_result_content(block: &Block) -> Option<&str> {
    let Block::ToolResult {
        content,
        is_error: Some(true),
        ..
    } = block
    else {
        return None;
    };
    Some(content.as_str())
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
    canonicalize_tool_path(root, user_path)
}

fn canonical_read_path(root: &Path, user_path: &str) -> std::result::Result<PathBuf, String> {
    canonicalize_read_tool_path(root, user_path)
}

fn regular_file_metadata(path: &Path) -> std::result::Result<std::fs::Metadata, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("{e}"))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(metadata)
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
pub(crate) enum MutationPreviewMode {
    Off,
    #[default]
    Simple,
    Git,
}

impl MutationPreviewMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" | "0" => Some(Self::Off),
            "simple" | "on" | "true" | "1" => Some(Self::Simple),
            "git" => Some(Self::Git),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Simple => "simple",
            Self::Git => "git",
        }
    }

    fn from_env() -> Self {
        std::env::var("DEXT_MUTATION_PREVIEW")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }
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
            "always" | "auto" | "trust" | "danger" => Some(Self::Always),
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
            "1" | "true" | "on" | "agent-browser" | "agent_browser" | "browser"
            | "agentbrowser" => Some(Self::AgentBrowser),
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractiveInputRoute {
    Submitted,
    RuntimeControlQueued,
    SteeringQueued,
    UnsupportedBusySlash(String),
    SecretWithheld,
    Dropped,
}

fn route_interactive_input_line(
    line: String,
    agent_busy: &AtomicBool,
    input_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    runtime_control_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    steering_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    pending_secret_send: &mut Option<String>,
) -> InteractiveInputRoute {
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        *pending_secret_send = None;
        return InteractiveInputRoute::Dropped;
    }
    if agent_busy.load(Ordering::SeqCst) {
        *pending_secret_send = None;
        if is_active_runtime_control_command(&trimmed) {
            let commands = parse_active_runtime_control_sequence(&trimmed)
                .unwrap_or_else(|| vec![trimmed.clone()]);
            let mut queued = false;
            for command in commands {
                if runtime_control_tx.send(command).is_ok() {
                    queued = true;
                }
            }
            if queued {
                InteractiveInputRoute::RuntimeControlQueued
            } else {
                InteractiveInputRoute::Dropped
            }
        } else if text_is_potential_local_secret(&trimmed) {
            InteractiveInputRoute::Dropped
        } else if is_slash_command(&trimmed) {
            InteractiveInputRoute::UnsupportedBusySlash(trimmed)
        } else if steering_tx.send(trimmed).is_ok() {
            InteractiveInputRoute::SteeringQueued
        } else {
            InteractiveInputRoute::Dropped
        }
    } else if text_is_potential_local_secret(&trimmed)
        && pending_secret_send.as_deref() != Some(trimmed.as_str())
    {
        // Same double-confirm as the TUI: never forward credential-looking
        // input to the model on the first Enter.
        *pending_secret_send = Some(trimmed);
        InteractiveInputRoute::SecretWithheld
    } else if input_tx.send(trimmed).is_ok() {
        *pending_secret_send = None;
        InteractiveInputRoute::Submitted
    } else {
        InteractiveInputRoute::Dropped
    }
}

pub(crate) fn text_is_potential_local_secret(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains("accessToken") || trimmed.contains("access_token") {
        return true;
    }
    if slash_login_contains_secret(trimmed) {
        return true;
    }
    if contains_secretish_assignment(trimmed) {
        return true;
    }
    if let Some(token) = strip_bearer_token(trimmed) {
        return token.chars().count() >= 8;
    }
    if contains_known_secret_token(trimmed) {
        return true;
    }
    if trimmed.starts_with('/') || trimmed.starts_with('{') {
        return false;
    }
    if looks_like_public_clipboard_reference(trimmed) {
        return false;
    }
    false
}

fn contains_secretish_assignment(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || matches!(c, '&' | '?' | ';' | ','))
        .chain(text.lines())
        .any(secretish_assignment_has_value)
}

fn secretish_assignment_has_value(segment: &str) -> bool {
    let trimmed = segment.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '{' | '}' | '[' | ']' | '(' | ')')
    });
    let Some((key, value)) = trimmed.split_once('=').or_else(|| trimmed.split_once(':')) else {
        return false;
    };
    let key = key
        .trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .rsplit(['/', '.'])
        .next()
        .unwrap_or(key);
    let value = value.trim().trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '{' | '}' | '[' | ']' | '(' | ')')
    });
    let value_len = value.chars().count();
    (secretish_key_name(key) && value_len >= 6) || (secretish_code_key(key) && value_len >= 12)
}

fn compact_key_name(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn secretish_key_name(key: &str) -> bool {
    let compact = compact_key_name(key);
    compact == "auth"
        || compact == "authorization"
        || compact == "passwd"
        || compact == "pwd"
        || compact == "privatekey"
        || compact.ends_with("apikey")
        || compact.ends_with("token")
        || compact.contains("password")
        || compact.contains("secret")
}

fn secretish_code_key(key: &str) -> bool {
    matches!(
        compact_key_name(key).as_str(),
        "code" | "oauthcode" | "authorizationcode"
    )
}

fn strip_bearer_token(text: &str) -> Option<&str> {
    let prefix_len = "bearer".len();
    let prefix = text.get(..prefix_len)?;
    let rest = text.get(prefix_len..)?;
    if prefix.eq_ignore_ascii_case("bearer") && rest.chars().next().is_some_and(char::is_whitespace)
    {
        Some(rest.trim_start()).filter(|token| !token.is_empty())
    } else {
        None
    }
}

fn contains_known_secret_token(text: &str) -> bool {
    text.split(|c: char| {
        c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '&' | '=' | ':' | ';' | ',' | '#')
    })
    .any(|raw| {
        let token = raw.trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
                )
        });
        let len = token.chars().count();
        if len < 8 {
            return false;
        }
        let lower = token.to_ascii_lowercase();
        lower.starts_with("sk-")
            || lower.starts_with("sk_")
            || lower.starts_with("xoxb-")
            || lower.starts_with("xoxp-")
            || lower.starts_with("ghp_")
            || lower.starts_with("github_pat_")
            || lower.starts_with("glpat-")
            || lower.starts_with("ya29.")
            || (lower.starts_with("ac_") && len >= 16)
            || (token.starts_with("AIza") && len >= 20)
    })
}

fn looks_like_public_clipboard_reference(text: &str) -> bool {
    let mut saw_reference = false;
    for token in text.split_whitespace().map(|token| {
        token.trim_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>'))
    }) {
        if token.is_empty() {
            continue;
        }
        if !(looks_like_url_reference(token)
            || looks_like_social_handle(token)
            || looks_like_git_sha(token))
        {
            return false;
        }
        saw_reference = true;
    }
    saw_reference
}

fn looks_like_url_reference(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if lower.contains("://") {
        return !url_authority_has_userinfo(token);
    }
    if lower.starts_with("www.") {
        return true;
    }
    token.split_once('/').is_some_and(|(host, rest)| {
        if host.contains('@') {
            return false;
        }
        let host = host.split(':').next().unwrap_or(host);
        !rest.is_empty() && looks_like_domain_name(host)
    })
}

fn url_authority_has_userinfo(token: &str) -> bool {
    let Some((_, rest)) = token.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.contains('@')
}

fn looks_like_domain_name(host: &str) -> bool {
    if !host.contains('.') || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    let Some(tld) = host.rsplit('.').next() else {
        return false;
    };
    (2..=24).contains(&tld.len())
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
}

fn looks_like_social_handle(token: &str) -> bool {
    let Some(handle) = token.strip_prefix('@') else {
        return (3..=15).contains(&token.len())
            && token.contains('_')
            && token.chars().any(|c| c.is_ascii_digit())
            && token.chars().any(|c| c.is_ascii_lowercase())
            && token
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    };
    !handle.is_empty()
        && handle.len() <= 15
        && handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn looks_like_git_sha(token: &str) -> bool {
    let len = token.len();
    matches!(len, 7..=12 | 40) && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn slash_login_contains_secret(trimmed: &str) -> bool {
    let mut parts = trimmed.split_whitespace();
    let Some(cmd) = parts.next() else {
        return false;
    };
    if cmd != "/login" {
        return false;
    }
    let args: Vec<&str> = parts.collect();
    if args.len() < 2 {
        return false;
    }
    let secret = args[1..].join(" ");
    let lowered = secret.trim().to_ascii_lowercase();
    if provider::login_arg_requests_web_flow(&lowered)
        || provider::login_arg_requests_import(&lowered)
    {
        return false;
    }
    true
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
    ToolOutputDelta {
        call_id: String,
        name: String,
        stream: String,
        text: String,
    },
    LocalAuthPrompt {
        tool: String,
        message: String,
    },
    LoginInputMode {
        provider: Option<String>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
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
    RuntimeControl(String),
    RuntimeControlApplied {
        commands: usize,
        model_changed: bool,
        effort_changed: bool,
        stream_aborted: bool,
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
        failed: bool,
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
    fn local_auth_prompt(&mut self, tool: &str, message: &str);
    fn live_output_sender(&self) -> Option<tokio::sync::mpsc::Sender<AgentEvent>> {
        None
    }
    fn request_local_auth_secret(&mut self, tool: &str, message: &str) -> LocalAuthSecret {
        self.local_auth_prompt(tool, message);
        LocalAuthSecret::Unavailable
    }
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
                        print!("dext> …");
                        let _ = io::stdout().flush();
                        self.printed_prefix = true;
                    }
                    self.text_accum.push_str(&t);
                } else {
                    if !self.printed_prefix {
                        print!("dext> ");
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
            AgentEvent::ToolOutputDelta { .. } => {}
            AgentEvent::LocalAuthPrompt { .. } => {}
            AgentEvent::LoginInputMode { .. } => {}
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
            AgentEvent::RuntimeControl(s) => println!("{s}"),
            AgentEvent::RuntimeControlApplied { stream_aborted, .. } => {
                if stream_aborted {
                    self.printed_any_text_this_block = false;
                    self.printed_prefix = false;
                    self.text_accum.clear();
                }
            }
            AgentEvent::Info(s) => println!("{s}"),
            AgentEvent::Warn(s) => eprintln!("{s}"),
            AgentEvent::Error(s) => eprintln!("{s}"),
            AgentEvent::Slash(s) => println!("{s}"),
            AgentEvent::WorkMap { text, .. } => println!("{text}"),
            AgentEvent::TurnEnd { usage, .. } => {
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

    fn local_auth_prompt(&mut self, _tool: &str, message: &str) {
        eprintln!("{message}");
    }

    fn request_local_auth_secret(&mut self, _tool: &str, message: &str) -> LocalAuthSecret {
        read_local_auth_secret_from_tty(message)
            .map(LocalAuthSecret::Secret)
            .unwrap_or(LocalAuthSecret::Unavailable)
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
                    AgentEvent::RuntimeControlApplied {
                        stream_aborted: true,
                        ..
                    } => self.stream.text.clear(),
                    _ => {}
                }
                if matches!(&event, AgentEvent::ToolOutputDelta { .. }) {
                    return;
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
                AgentEvent::RuntimeControlApplied {
                    stream_aborted: true,
                    ..
                } => self.stream.text.clear(),
                AgentEvent::TurnEnd { usage, failed } => {
                    Self::emit_json_line(&json!({
                        "event": "final",
                        "data": {
                            "text": self.stream.text,
                            "usage": usage,
                            "failed": failed,
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

    fn local_auth_prompt(&mut self, tool: &str, message: &str) {
        if self.mode == OutputMode::StreamJson {
            if let Ok(value) = serde_json::to_value(AgentEvent::LocalAuthPrompt {
                tool: tool.to_string(),
                message: message.to_string(),
            }) {
                Self::emit_json_line(&value);
            }
        } else {
            self.inner.local_auth_prompt(tool, message);
        }
    }

    fn request_local_auth_secret(&mut self, tool: &str, message: &str) -> LocalAuthSecret {
        if self.mode == OutputMode::StreamJson
            && let Ok(value) = serde_json::to_value(AgentEvent::LocalAuthPrompt {
                tool: tool.to_string(),
                message: message.to_string(),
            })
        {
            Self::emit_json_line(&value);
        }
        self.inner.request_local_auth_secret(tool, message)
    }
}

#[cfg(test)]
struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

#[cfg(test)]
impl EventSink for ChannelSink {
    fn emit(&mut self, event: AgentEvent) {
        record_crash_event(&event);
        let _ = self.tx.send(event);
    }
    fn request_permission(&mut self, _name: &str, _input: &Value) -> Choice {
        Choice::Deny
    }

    fn local_auth_prompt(&mut self, tool: &str, message: &str) {
        let _ = self.tx.send(AgentEvent::LocalAuthPrompt {
            tool: tool.to_string(),
            message: message.to_string(),
        });
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

struct SilentSink;

impl EventSink for SilentSink {
    fn emit(&mut self, _event: AgentEvent) {}

    fn request_permission(&mut self, _name: &str, _input: &Value) -> Choice {
        Choice::Deny
    }

    fn local_auth_prompt(&mut self, _tool: &str, _message: &str) {}
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

    fn local_auth_prompt(&mut self, _tool: &str, _message: &str) {}
}

#[cfg(unix)]
struct TerminalEchoGuard {
    fd: i32,
    original: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl TerminalEchoGuard {
    fn disable(fd: i32) -> io::Result<Self> {
        // Password input must never be echoed into the TUI/terminal scrollback.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut original) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut hidden = original;
            hidden.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(fd, libc::TCSAFLUSH, &hidden) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                fd,
                original,
                active: true,
            })
        }
    }
}

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            }
            self.active = false;
        }
    }
}

fn trim_local_auth_secret_line(secret: &str) -> String {
    secret.trim_end_matches(['\r', '\n']).to_string()
}

pub(crate) fn clear_secret_string(secret: &mut String) {
    unsafe {
        secret.as_mut_vec().fill(0);
    }
    secret.clear();
}

#[cfg(unix)]
fn read_local_auth_secret_from_tty(message: &str) -> Option<String> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    writeln!(tty, "{message}").ok()?;
    write!(tty, "Password: ").ok()?;
    tty.flush().ok()?;

    let _echo_guard = TerminalEchoGuard::disable(tty.as_raw_fd()).ok()?;
    let mut line = String::new();
    let mut reader = io::BufReader::new(tty.try_clone().ok()?);
    let bytes = reader.read_line(&mut line).ok()?;
    drop(_echo_guard);
    writeln!(tty).ok()?;
    if bytes == 0 {
        None
    } else {
        Some(trim_local_auth_secret_line(&line))
    }
}

#[cfg(not(unix))]
fn read_local_auth_secret_from_tty(_message: &str) -> Option<String> {
    None
}

fn shell_single_quote(raw: &str) -> String {
    if raw.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        text: String,
    },
    Thinking {
        #[serde(rename = "thinking", alias = "text", default)]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
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

/// A user message that represents a fresh prompt (not tool results and not a
/// runtime-injected note). Used as the current-turn boundary when deciding
/// which thinking blocks must still be sent back to the provider.
fn is_fresh_user_prompt_message(msg: &Message) -> bool {
    if msg.role != "user"
        || msg
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { .. }))
    {
        return false;
    }
    msg.content.iter().any(|block| match block {
        Block::Text { text } | Block::PartialStream { text } => {
            !text.starts_with("[runtime-note]")
                && !text.starts_with("[queued-update]")
                && !text.starts_with("[queued-user-update]")
                && !text.starts_with("[prior conversation,")
        }
        _ => false,
    })
}

fn sanitize_anthropic_messages(messages: &[Message], preserve_thinking: bool) -> Vec<Message> {
    // Providers only require thinking blocks for the tool loop of the current
    // turn. Older turns' thinking is dead weight in every subsequent request,
    // so it is dropped at serialization time (history keeps the full record).
    // The one-time prefix change this causes at a turn boundary costs a single
    // partial cache re-write; carrying the blocks forever costs more.
    let current_turn_start = messages
        .iter()
        .rposition(is_fresh_user_prompt_message)
        .unwrap_or(0);
    messages
        .iter()
        .enumerate()
        .map(|(idx, message)| {
            let preserve_thinking = preserve_thinking && idx >= current_turn_start;
            Message {
                role: message.role.clone(),
                content: message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Block::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
                        } => Some(Block::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: *is_error,
                            metadata: ToolResultMetadata::default(),
                        }),
                        Block::Thinking { text, signature } => {
                            let signature = signature.as_ref().filter(|sig| !sig.is_empty())?;
                            preserve_thinking.then_some(Block::Thinking {
                                text: text.clone(),
                                signature: Some(signature.clone()),
                            })
                        }
                        Block::RedactedThinking { data } => (preserve_thinking && !data.is_empty())
                            .then_some(Block::RedactedThinking { data: data.clone() }),
                        other => Some(other.clone()),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Serialize sanitized history to Anthropic wire JSON. With prompt caching
/// enabled, a sliding breakpoint is set on the final content block of the last
/// message so the whole conversation prefix (tools → system → history) is
/// reused across tool rounds instead of being re-billed as fresh input on
/// every request. Tools and the stable system block hold the other two
/// breakpoints (3 of the 4 allowed).
fn anthropic_wire_messages(messages: &[Message], cache_enabled: bool) -> Result<Vec<Value>> {
    let mut wire: Vec<Value> = messages
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("serialize messages: {e}"))?;
    // Sanitization can empty a message out entirely — e.g. an assistant
    // message that carried only thinking blocks once prior-turn thinking is
    // stripped, or a session resumed with thinking disabled. The API rejects
    // empty content arrays, so drop such messages from the wire (they carry no
    // tool pairing, history keeps the full record).
    wire.retain(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_none_or(|blocks| !blocks.is_empty())
    });
    if cache_enabled {
        set_sliding_message_cache_breakpoint(&mut wire);
    }
    Ok(wire)
}

fn set_sliding_message_cache_breakpoint(wire: &mut [Value]) {
    let Ok(cache_control) = serde_json::to_value(CacheControl::for_prompt()) else {
        return;
    };
    for message in wire.iter_mut().rev() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks.iter_mut().rev() {
            let Some(obj) = block.as_object_mut() else {
                continue;
            };
            // Thinking blocks cannot carry cache_control breakpoints.
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(kind, "thinking" | "redacted_thinking") {
                continue;
            }
            obj.insert("cache_control".to_string(), cache_control);
            return;
        }
    }
}

/// Volatile runtime state (work ledger, todos, provider health) is delivered
/// as a transient block at the tail of the message list — after the sliding
/// cache breakpoint and never persisted to history — so the conversation
/// prefix stays byte-stable for prompt caching even while this state changes
/// between tool rounds.
fn runtime_env_wire_text(env: &str) -> String {
    format!("[dext runtime status — auto-refreshed each request; not user input]\n{env}")
}

fn append_runtime_env_block(wire: &mut Vec<Value>, env: &str) {
    let env = env.trim();
    if env.is_empty() {
        return;
    }
    let text_block = json!({"type": "text", "text": runtime_env_wire_text(env)});
    // Tool results must lead a user message, so the status text rides at the
    // end of the existing trailing user message when there is one.
    if let Some(last) = wire.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        blocks.push(text_block);
        return;
    }
    wire.push(json!({"role": "user", "content": [text_block]}));
}

fn push_runtime_env_oai_message(msgs: &mut Vec<OaiMessage>, env: &str) {
    let env = env.trim();
    if env.is_empty() {
        return;
    }
    msgs.push(OaiMessage {
        role: "user".to_string(),
        content: Some(runtime_env_wire_text(env)),
        tool_calls: None,
        tool_call_id: None,
    });
}

fn append_runtime_env_chatgpt_item(items: &mut Vec<Value>, env: &str) {
    let env = env.trim();
    if env.is_empty() {
        return;
    }
    // Message items carry an id like the rest of history_to_chatgpt_input;
    // a fixed one is fine because the item is rebuilt fresh every request.
    items.push(json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": runtime_env_wire_text(env),
        }],
        "id": "msg_runtime_env",
    }));
}

fn blocks_approx_tokens(blocks: &[Block]) -> u64 {
    let chars = blocks
        .iter()
        .map(|block| match block {
            Block::Text { text } | Block::PartialStream { text } => text.len(),
            Block::Thinking { text, .. } => text.len(),
            Block::RedactedThinking { data } => data.len(),
            Block::ToolUse { input, .. } => json_byte_len(input),
            Block::ToolResult { content, .. } => content.len(),
        })
        .sum::<usize>() as u64;
    if chars == 0 {
        0
    } else {
        ((chars.saturating_add(3)) / 4).max(1)
    }
}

fn message_approx_tokens(message: &Message) -> u64 {
    blocks_approx_tokens(&message.content).max(1)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
struct TrackOrigin {
    source_session: String,
    source_waypoint: String,
    mode: String,
    packet_hash: String,
    created_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
struct WorkMapFocusState {
    source_session: String,
    selection: String,
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
    anchor: String,
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

    fn carries(&self, item: &str) -> bool {
        match self {
            Self::Exact => false,
            Self::Carry(items) => items.iter().any(|i| i.eq_ignore_ascii_case(item)),
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
        if let Ok(v) = std::env::var("DEXT_PRIVACY") {
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
    sum > 0 && sum.is_multiple_of(10)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}

impl CacheControl {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const EPHEMERAL: Self = Self {
        kind: "ephemeral",
        ttl: None,
    };

    /// Cache control for prompt breakpoints. Honors DEXT_PROMPT_CACHE_TTL=1h
    /// for the extended-TTL cache (useful when the user pauses >5min between
    /// turns); the default 5-minute TTL omits the field entirely.
    pub(crate) fn for_prompt() -> Self {
        Self {
            kind: "ephemeral",
            ttl: extended_prompt_cache_ttl(),
        }
    }
}

pub(crate) fn extended_prompt_cache_ttl() -> Option<&'static str> {
    match std::env::var("DEXT_PROMPT_CACHE_TTL")
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1h" | "1hr" | "hour" | "60m" => Some("1h"),
        _ => None,
    }
}

#[derive(Serialize, Clone, Copy)]
struct SystemBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize, Clone)]
pub(crate) struct WireTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

fn openai_reasoning_effort(effort: ThinkingEffort) -> Option<&'static str> {
    match effort {
        ThinkingEffort::Off => None,
        ThinkingEffort::Low => Some("low"),
        ThinkingEffort::Medium => Some("medium"),
        ThinkingEffort::High | ThinkingEffort::XHigh | ThinkingEffort::Max => Some("high"),
    }
}

const LLAMA_TOOL_GRAMMAR_ENV: &str = "DEXT_LLAMA_TOOL_GRAMMAR";

/// Build a llama.cpp GBNF grammar that constrains the completion to a single
/// well-formed tool call: an object with `name` (one of the exposed tools) and
/// a NON-empty `arguments` object. This targets the local llama.cpp failure
/// mode where small models emit a tool call with empty/dropped arguments — the
/// empty-tool-call loop the orchestrator otherwise only breaks after the fact.
fn llama_tool_call_grammar(tool_names: &[&str]) -> Option<String> {
    if tool_names.is_empty() {
        return None;
    }
    let names = tool_names
        .iter()
        .map(|n| {
            format!(
                "\"\\\"{}\\\"\"",
                n.replace('\\', "\\\\").replace('"', "\\\"")
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let grammar = r#"root   ::= "{" ws "\"name\"" ws ":" ws name ws "," ws "\"arguments\"" ws ":" ws object ws "}"
name   ::= __NAMES__
object ::= "{" ws string ws ":" ws value (ws "," ws string ws ":" ws value)* ws "}"
value  ::= object | array | string | number | "true" | "false" | "null"
array  ::= "[" ws (value (ws "," ws value)*)? ws "]"
string ::= "\"" ([^"\\] | "\\" .)* "\""
number ::= "-"? [0-9]+ ("." [0-9]+)? ([eE] [-+]? [0-9]+)?
ws     ::= [ \t\n]*
"#;
    Some(grammar.replace("__NAMES__", &names))
}

/// Decide whether to attach the tool-call grammar to a request. Gated to the
/// local llama.cpp provider (cloud OpenAI rejects unknown fields) and opt-in
/// via DEXT_LLAMA_TOOL_GRAMMAR, because forcing a tool call means the model
/// cannot answer in plain text, and the grammar's effect depends on the
/// llama.cpp server build. Off by default so it never changes the default
/// local experience; enable it to escape an empty-tool-call loop.
fn llama_tool_grammar_for(
    provider_id: &str,
    api_provider: provider::ApiProvider,
    base_url: &str,
    tool_names: &[&str],
    enabled: bool,
) -> Option<String> {
    if !enabled || !provider::is_local_llama_provider(provider_id, api_provider, base_url) {
        return None;
    }
    llama_tool_call_grammar(tool_names)
}

fn anthropic_thinking_budget_tokens(effort: ThinkingEffort) -> Option<u32> {
    match effort {
        ThinkingEffort::Off => None,
        ThinkingEffort::Low => Some(1_024),
        ThinkingEffort::Medium => Some(2_048),
        ThinkingEffort::High => Some(4_096),
        ThinkingEffort::XHigh | ThinkingEffort::Max => Some(8_192),
    }
}

fn provider_model_effort_levels(provider_id: &str, model: &str) -> Option<Vec<String>> {
    let catalog = load_provider_catalog().ok()?;
    let profile = find_provider_profile(&catalog, provider_id)?;
    let normalized = normalize_provider_model_value(&profile, model).to_ascii_lowercase();
    profile.model_effort_levels.get(&normalized).cloned()
}

fn map_effort_to_provider_levels(levels: &[String], effort: ThinkingEffort) -> Option<String> {
    if effort == ThinkingEffort::Off || levels.is_empty() {
        return None;
    }
    let has = |needle: &str| levels.iter().any(|level| level == needle);
    let pick = |candidates: &[&str]| {
        candidates
            .iter()
            .find(|candidate| has(candidate))
            .map(|candidate| (*candidate).to_string())
    };
    match effort {
        ThinkingEffort::Off => None,
        ThinkingEffort::Low | ThinkingEffort::Medium | ThinkingEffort::High => {
            pick(&["high", effort.as_str(), "medium", "low"]).or_else(|| levels.first().cloned())
        }
        ThinkingEffort::XHigh | ThinkingEffort::Max => {
            pick(&["max", "xhigh", "high"]).or_else(|| levels.last().cloned())
        }
    }
}

fn provider_model_output_config_effort(
    provider_id: &str,
    model: &str,
    effort: ThinkingEffort,
) -> Option<String> {
    provider_model_effort_levels(provider_id, model)
        .and_then(|levels| map_effort_to_provider_levels(&levels, effort))
}

fn anthropic_output_config_effort(model: &str, effort: ThinkingEffort) -> Option<String> {
    let effort = match effort {
        ThinkingEffort::Off => return None,
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::High => "high",
        ThinkingEffort::XHigh => {
            if anthropic_model_supports_extended_effort(model) {
                "xhigh"
            } else {
                "high"
            }
        }
        ThinkingEffort::Max => {
            if anthropic_model_supports_extended_effort(model) {
                "max"
            } else {
                "high"
            }
        }
    };
    Some(effort.to_string())
}

fn anthropic_model_is_always_adaptive(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model.contains("opus-4-7")
        || model.contains("opus-4.7")
        || model.contains("opus-4-8")
        || model.contains("opus-4.8")
        || model.contains("fable-5")
        || model.contains("fable5")
        || model.contains("mythos")
}

fn anthropic_model_supports_extended_effort(model: &str) -> bool {
    anthropic_model_is_always_adaptive(model)
}

fn anthropic_model_supports_adaptive_thinking(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model.contains("opus-4-6")
        || model.contains("opus-4.6")
        || model.contains("opus-4-7")
        || model.contains("opus-4.7")
        || model.contains("opus-4-8")
        || model.contains("opus-4.8")
        || model.contains("sonnet-4-6")
        || model.contains("sonnet-4.6")
        || model.contains("fable-5")
        || model.contains("fable5")
        || model.contains("mythos")
}

fn uses_anthropic_adaptive_thinking(provider_id: &str, model: &str) -> bool {
    let provider_id = canonical_provider_id(provider_id);
    if provider_id == "glm" {
        return false;
    }
    let is_anthropic =
        provider_id == "anthropic" || model.trim().to_ascii_lowercase().starts_with("claude-");
    is_anthropic && anthropic_model_supports_adaptive_thinking(model)
}

fn prompt_cache_env_override() -> Option<bool> {
    match std::env::var("DEXT_PROMPT_CACHE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "0" | "false" | "no" => Some(false),
        "on" | "1" | "true" | "all" | "yes" => Some(true),
        _ => None,
    }
}

fn anthropic_prompt_cache_supported(provider_id: &str, model: &str) -> bool {
    // Explicit env configuration overrides catalog capabilities. Auto mode uses
    // metadata first and this legacy family gate only when metadata is absent.
    if let Some(enabled) = prompt_cache_env_override() {
        return enabled;
    }
    let provider_id = canonical_provider_id(provider_id);
    provider_id == "anthropic" || model.trim().to_ascii_lowercase().starts_with("claude-")
}

fn system_blocks_with_cache_control<'a>(
    blocks: &[SystemBlock<'a>],
    enabled: bool,
) -> Vec<SystemBlock<'a>> {
    blocks
        .iter()
        .map(|block| SystemBlock {
            kind: block.kind,
            text: block.text,
            cache_control: enabled.then_some(block.cache_control).flatten(),
        })
        .collect()
}

fn wire_tools_with_cache_control(tools: &[WireTool], enabled: bool) -> Vec<WireTool> {
    let mut tools = tools.to_vec();
    if !enabled {
        for tool in &mut tools {
            tool.cache_control = None;
        }
    }
    tools
}

/// Anthropic rejects a request unless `max_tokens > thinking.budget_tokens`.
/// Clamp the requested thinking budget so it always leaves output headroom (at
/// least a quarter of `max_tokens`), preventing an HTTP 400 when a high-effort
/// budget meets a smaller output cap.
fn clamp_thinking_budget_below_max(budget_tokens: u32, max_tokens: u32) -> Option<u32> {
    let strict_max = max_tokens.checked_sub(1).filter(|value| *value > 0)?;
    let ceiling = max_tokens.saturating_mul(3) / 4;
    Some(budget_tokens.min(ceiling.max(1)).min(strict_max))
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a [SystemBlock<'a>],
    // Pre-serialized message JSON: cache breakpoints and the transient runtime
    // env block are injected at wire level so they never touch stored history.
    messages: &'a [Value],
    tools: &'a [WireTool],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
}

#[derive(Serialize, Clone, Copy)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<&'static str>,
}

#[derive(Serialize, Clone)]
struct AnthropicOutputConfig {
    effort: String,
}

#[derive(Serialize)]
struct OaiStreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct OaiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<OaiMessage>,
    tools: Vec<OaiTool>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<OaiStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    /// llama.cpp GBNF extension. Only ever set for the local llama.cpp
    /// provider (cloud OpenAI rejects unknown fields), and only when the
    /// user opts in — see `llama_tool_call_grammar`.
    #[serde(skip_serializing_if = "Option::is_none")]
    grammar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<OaiChatTemplateKwargs>,
}

#[derive(Serialize)]
struct OaiChatTemplateKwargs {
    enable_thinking: bool,
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
    // Provider usage is normalized into disjoint input buckets:
    // - Anthropic: input_tokens plus cache_creation/read_input_tokens.
    // - OpenAI/ChatGPT: prompt/input tokens minus cached_tokens, with cached_tokens as cache_read.
    // - Z.ai/GLM: Anthropic-compatible fields when present; otherwise no cache buckets.
    // - local llama.cpp: timings.prompt_n as new prompt input and timings.cache_n as cache_read.
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
}

impl Usage {
    fn add(&mut self, o: Usage) {
        let lhs_cost = self.cost_usd;
        let rhs_cost = o.cost_usd;
        let lhs_tokens = self.total_tokens();
        let rhs_tokens = o.total_tokens();
        self.input += o.input;
        self.output += o.output;
        self.cache_create += o.cache_create;
        self.cache_read += o.cache_read;
        self.cost_usd = match (lhs_cost, rhs_cost) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) if rhs_tokens == 0 => Some(a),
            (None, Some(b)) if lhs_tokens == 0 => Some(b),
            (None, None) if lhs_tokens == 0 && rhs_tokens == 0 => None,
            _ => None,
        };
    }

    fn actual_input_tokens(&self) -> u64 {
        self.input
    }

    fn cached_input_tokens(&self) -> u64 {
        self.cache_create.saturating_add(self.cache_read)
    }

    fn total_input_tokens(&self) -> u64 {
        self.input.saturating_add(self.cached_input_tokens())
    }

    fn billed_tokens(&self) -> u64 {
        self.total_input_tokens().saturating_add(self.output)
    }

    fn context_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    fn total_tokens(&self) -> u64 {
        self.billed_tokens()
    }

    fn estimated_cost_usd(&self) -> f64 {
        if let Some(cost) = self.cost_usd {
            return cost;
        }
        let per_mtok = 1_000_000.0;
        (self.input as f64 / per_mtok) * DEFAULT_INPUT_USD_PER_MTOK
            + (self.output as f64 / per_mtok) * DEFAULT_OUTPUT_USD_PER_MTOK
            + (self.cache_read as f64 / per_mtok) * DEFAULT_CACHE_READ_USD_PER_MTOK
            + (self.cache_create as f64 / per_mtok) * DEFAULT_CACHE_CREATE_USD_PER_MTOK
    }

    fn parse(v: &Value) -> Self {
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

    fn parse_openai(v: &Value) -> Self {
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

    fn parse_openai_timings(v: &Value) -> Option<Self> {
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

    fn line(&self) -> String {
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

fn parse_usage_cost(v: &Value) -> Option<f64> {
    v["cost"]
        .as_f64()
        .or_else(|| v["cost_usd"].as_f64())
        .or_else(|| v["total_cost"].as_f64())
        .or_else(|| v["total_cost_usd"].as_f64())
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

#[derive(Clone, Copy)]
struct UsagePricing {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_create: f64,
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

    fn estimate(self, usage: Usage) -> f64 {
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

fn provider_cost_estimate_overrides_wire_cost(
    provider_id: &str,
    api_provider: ApiProvider,
    model: &str,
) -> bool {
    api_provider == ApiProvider::Anthropic && anthropic_prompt_cache_supported(provider_id, model)
}

fn usage_pricing_for(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
    model: &str,
) -> UsagePricing {
    let base = if provider::is_local_llama_provider(provider_id, api_provider, base_url) {
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
    };
    usage_pricing_from_env(base)
}

fn usage_with_current_pricing(
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
        usage.cost_usd = Some(pricing.estimate(usage));
    }
    usage
}

fn normalize_price_model(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn openai_pricing(model: &str) -> Option<UsagePricing> {
    if model.starts_with("gpt-5.4-mini") {
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

fn anthropic_pricing(model: &str) -> Option<UsagePricing> {
    if model.starts_with("glm-") {
        return Some(UsagePricing::default());
    }
    if model.contains("fable") {
        // Inferred from Anthropic Console billing for claude-fable-5 until public rates are listed.
        Some(UsagePricing::new(
            11.721718363700392,
            58.60859181850196,
            1.1721718363700393,
            14.65214795462549,
        ))
    } else if model.contains("opus") {
        Some(UsagePricing::new(15.0, 75.0, 1.5, 18.75))
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

fn deepseek_pricing(model: &str) -> Option<UsagePricing> {
    if model.contains("reasoner") {
        Some(UsagePricing::new(0.55, 2.19, 0.14, 0.55))
    } else if model.contains("chat") {
        Some(UsagePricing::new(0.27, 1.1, 0.07, 0.27))
    } else {
        None
    }
}

fn usage_pricing_from_env(default: UsagePricing) -> UsagePricing {
    UsagePricing {
        input: env_f64("DEXT_INPUT_USD_PER_MTOK").unwrap_or(default.input),
        output: env_f64("DEXT_OUTPUT_USD_PER_MTOK").unwrap_or(default.output),
        cache_read: env_f64("DEXT_CACHE_READ_USD_PER_MTOK").unwrap_or(default.cache_read),
        cache_create: env_f64("DEXT_CACHE_CREATE_USD_PER_MTOK").unwrap_or(default.cache_create),
    }
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
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
        std::env::var("DEXT_BUDGET_CAP")
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
    redacted_data: String,
}

impl PartialBlock {
    fn finalize(self) -> Option<Block> {
        match self.kind.as_str() {
            "text" => Some(Block::Text { text: self.text }),
            "thinking" => {
                let signature = self.thinking_signature.filter(|sig| !sig.is_empty());
                (!self.text.is_empty() || signature.is_some()).then_some(Block::Thinking {
                    text: self.text,
                    signature,
                })
            }
            "redacted_thinking" => {
                (!self.redacted_data.is_empty()).then_some(Block::RedactedThinking {
                    data: self.redacted_data,
                })
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

pub(crate) fn normalize_reasoning_summary_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<!--") {
        normalized.push_str(&remaining[..start]);
        let comment = &remaining[start + "<!--".len()..];
        let paragraph_separator = normalized
            .chars()
            .rev()
            .take_while(|ch| ch.is_whitespace())
            .filter(|ch| *ch == '\n')
            .count()
            >= 2;
        let in_markdown_code = markdown_code_span_open(&normalized);
        let Some(end) = comment.find("-->") else {
            let heading_separator = ends_with_reasoning_heading(&normalized);
            if !in_markdown_code
                && (paragraph_separator || heading_separator)
                && comment.trim().is_empty()
            {
                normalized.truncate(normalized.trim_end().len());
            } else {
                normalized.push_str(&remaining[start..]);
            }
            return separate_adjacent_reasoning_headings(&normalized);
        };
        let after = &comment[end + "-->".len()..];
        let empty_heading_separator = !in_markdown_code
            && comment[..end].trim().is_empty()
            && ends_with_reasoning_heading(&normalized)
            && (after.trim().is_empty() || after.trim_start().starts_with("**"));
        if !in_markdown_code
            && (paragraph_separator || empty_heading_separator)
            && comment[..end].trim().is_empty()
        {
            normalized.truncate(normalized.trim_end().len());
            if after.trim().is_empty() {
                return separate_adjacent_reasoning_headings(&normalized);
            }
            normalized.push_str("\n\n");
            remaining = if empty_heading_separator {
                after.trim_start()
            } else {
                after
            };
            continue;
        } else {
            normalized.push_str("<!--");
            normalized.push_str(&comment[..end]);
            normalized.push_str("-->");
        }
        remaining = after;
    }
    normalized.push_str(remaining);
    separate_adjacent_reasoning_headings(&normalized)
}

fn ends_with_reasoning_heading(text: &str) -> bool {
    let line = text
        .trim_end()
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .trim();
    line.len() > 4
        && line.starts_with("**")
        && line.ends_with("**")
        && !line[2..line.len() - 2].contains("**")
}

fn advance_markdown_code_delimiter(delimiter: &mut Option<usize>, text: &str) {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index] == b'`' {
            index += 1;
        }
        let run = index - start;
        match *delimiter {
            None => *delimiter = Some(run),
            Some(open) if open == run => *delimiter = None,
            Some(_) => {}
        }
    }
}

fn markdown_code_span_open(text: &str) -> bool {
    let mut delimiter = None;
    advance_markdown_code_delimiter(&mut delimiter, text);
    delimiter.is_some()
}

fn separate_adjacent_reasoning_headings(text: &str) -> String {
    let mut separated = String::with_capacity(text.len());
    let mut remaining = text;
    let mut in_bold = false;
    let mut bold_started_as_heading = false;
    let mut code_delimiter = None;
    while let Some(marker) = remaining.find("**") {
        let before_marker = &remaining[..marker];
        separated.push_str(before_marker);
        advance_markdown_code_delimiter(&mut code_delimiter, before_marker);
        if code_delimiter.is_none() && !in_bold {
            bold_started_as_heading = separated
                .rsplit('\n')
                .next()
                .is_none_or(|line| line.trim().is_empty());
        }
        separated.push_str("**");
        remaining = &remaining[marker + 2..];
        if code_delimiter.is_some() {
            continue;
        }
        in_bold = !in_bold;
        if !in_bold && bold_started_as_heading {
            let whitespace_len = remaining
                .find(|ch: char| !ch.is_whitespace())
                .unwrap_or(remaining.len());
            let whitespace = &remaining[..whitespace_len];
            if remaining[whitespace_len..].starts_with("**")
                && (whitespace.is_empty() || whitespace.contains('\n'))
            {
                let newline_count = whitespace.chars().filter(|ch| *ch == '\n').count();
                for _ in 0..newline_count.max(2) {
                    separated.push('\n');
                }
                remaining = &remaining[whitespace_len..];
            }
        }
    }
    separated.push_str(remaining);
    separated
}

fn normalize_restored_chatgpt_reasoning(history: &mut [Message]) {
    for message in history {
        for block in &mut message.content {
            if let Block::Thinking { text, .. } = block {
                *text = normalize_reasoning_summary_text(text);
            }
        }
    }
}

fn reasoning_summary_stream_delta(raw: &str, emitted: &mut String) -> Option<String> {
    let mut visible = normalize_reasoning_summary_text(raw);
    for partial_comment_start in ["<!-", "<!", "<"] {
        if raw.ends_with(partial_comment_start) && visible.ends_with(partial_comment_start) {
            visible.truncate(visible.len() - partial_comment_start.len());
            break;
        }
    }
    while visible.ends_with([' ', '\t']) {
        visible.pop();
    }
    if let Some(before_star) = visible.strip_suffix('*') {
        let stable = before_star.trim_end();
        if stable.ends_with("**") {
            visible.truncate(stable.len());
        }
    }
    let delta = visible.strip_prefix(emitted.as_str())?.to_string();
    *emitted = visible;
    (!delta.is_empty()).then_some(delta)
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
            "[dext note] command exited 0 but stderr contains failure-looking text; verify before trusting this result.\n".to_string(),
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

#[derive(Clone)]
struct LiveToolOutput {
    call_id: String,
    name: String,
    tx: tokio::sync::mpsc::Sender<AgentEvent>,
}

impl LiveToolOutput {
    fn emit(&self, stream: &'static str, text: String) {
        if text.is_empty() {
            return;
        }
        let _ = self.tx.try_send(AgentEvent::ToolOutputDelta {
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            stream: stream.to_string(),
            text,
        });
    }
}

struct LiveUtf8Emitter {
    target: Option<LiveToolOutput>,
    stream: &'static str,
    pending: Vec<u8>,
}

impl LiveUtf8Emitter {
    fn new(target: Option<LiveToolOutput>, stream: &'static str) -> Self {
        Self {
            target,
            stream,
            pending: Vec::new(),
        }
    }

    fn emit_text(&self, text: String) {
        if let Some(target) = &self.target {
            target.emit(self.stream, text);
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.target.is_none() || bytes.is_empty() {
            return;
        }
        self.pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    let text = text.to_string();
                    self.pending.clear();
                    self.emit_text(text);
                    break;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if valid > 0 {
                        let text = String::from_utf8_lossy(&self.pending[..valid]).to_string();
                        self.pending.drain(..valid);
                        self.emit_text(text);
                        continue;
                    }
                    if let Some(len) = err.error_len() {
                        let take = len.min(self.pending.len());
                        let text = String::from_utf8_lossy(&self.pending[..take]).to_string();
                        self.pending.drain(..take);
                        self.emit_text(text);
                        continue;
                    }
                    break;
                }
            }
        }
        if self.pending.len() > 4 {
            let emit_len = self.pending.len() - 4;
            let text = String::from_utf8_lossy(&self.pending[..emit_len]).to_string();
            self.pending.drain(..emit_len);
            self.emit_text(text);
        }
    }

    fn finish(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.pending).to_string();
        self.pending.clear();
        self.emit_text(text);
    }
}

async fn collect_async_limited<R>(reader: R, cap: usize) -> LimitedByteCapture
where
    R: tokio::io::AsyncRead + Unpin,
{
    collect_async_limited_live(reader, cap, None, "stdout").await
}

async fn collect_async_limited_live<R>(
    mut reader: R,
    cap: usize,
    live_output: Option<LiveToolOutput>,
    stream: &'static str,
) -> LimitedByteCapture
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut capture = LimitedByteCapture::new(cap);
    let mut live = LiveUtf8Emitter::new(live_output, stream);
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                capture.push(&buf[..n]);
                live.push(&buf[..n]);
            }
            Err(_) => break,
        }
    }
    live.finish();
    capture
}

// Children run in a new *session*, not merely a new process group: setsid()
// also detaches them from Dext's controlling terminal, so nothing they spawn
// can read from or paint over the TUI via /dev/tty (git credential prompts
// did exactly that — the prompt text garbled the input box while git hung on
// a terminal read that could never be answered). setsid() implies a fresh
// process group with pgid == pid, so the pgid-based cleanup in
// terminate_process_group_after_exit keeps working unchanged; setpgid is the
// fallback if setsid is ever refused.
#[cfg(unix)]
fn detach_session_pre_exec() -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    || {
        unsafe {
            if libc::setsid() == -1 {
                let _ = libc::setpgid(0, 0);
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn configure_std_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(detach_session_pre_exec());
    }
}

#[cfg(not(unix))]
fn configure_std_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn configure_tokio_process_group(cmd: &mut tokio::process::Command) {
    unsafe {
        cmd.pre_exec(detach_session_pre_exec());
    }
}

#[cfg(not(unix))]
fn configure_tokio_process_group(_cmd: &mut tokio::process::Command) {}

/// Forbid interactive credential prompting in tool children. They have no
/// usable terminal (see detach_session_pre_exec), so a git/ssh prompt could
/// only hang the call until timeout; with these set, git instead fails in
/// milliseconds with an explicit "terminal prompts disabled" error that the
/// model can surface and Dext can react to with a local credential prompt.
fn deny_interactive_prompt_env(cmd: &mut tokio::process::Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GCM_INTERACTIVE", "never");
}

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
            std::env::var("DEXT_EXTERNAL_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|secs| *secs > 0)
        })
        .unwrap_or(fallback_secs);
    std::time::Duration::from_secs(secs)
}

fn external_tool_timeout() -> std::time::Duration {
    timeout_from_env("DEXT_EXTERNAL_TIMEOUT_SECS", EXTERNAL_TOOL_TIMEOUT_SECS)
}

fn bash_tool_timeout() -> std::time::Duration {
    timeout_from_env("DEXT_BASH_TIMEOUT_SECS", EXTERNAL_TOOL_TIMEOUT_SECS)
}

fn timeout_from_tool_input(input: &Value, fallback: std::time::Duration) -> std::time::Duration {
    input["timeout"]
        .as_u64()
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(fallback)
}

fn hook_timeout() -> std::time::Duration {
    timeout_from_env("DEXT_HOOK_TIMEOUT_SECS", EXTERNAL_TOOL_TIMEOUT_SECS)
}

const CHECKPOINT_DEBOUNCE_MS_DEFAULT: u64 = 500;
const MAX_CONCURRENT_BUILTINS_DEFAULT: usize = 8;

fn max_concurrent_builtins() -> usize {
    std::env::var("DEXT_MAX_CONCURRENT_BUILTINS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_CONCURRENT_BUILTINS_DEFAULT)
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "dext-{label}-{}-{}",
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
    result_with_diff(summary, path, root, before, after)
}

fn write_file_result_with_diff(
    path: &Path,
    root: &Path,
    before: Option<&str>,
    after: &str,
) -> String {
    let summary = format!("wrote {} bytes to {}", after.len(), path.display());
    result_with_diff(summary, path, root, before.unwrap_or(""), after)
}

fn result_with_diff(
    summary: String,
    path: &Path,
    root: &Path,
    before: &str,
    after: &str,
) -> String {
    match git_unified_diff(before, after, path, root) {
        Some(diff) if !diff.is_empty() => format!("{diff}{summary}"),
        _ => summary,
    }
}

// "Full jitter" backoff: returns a wait in [base/2, base). Prevents thundering-herd when multiple
// dext processes see the same 429/503 at the same second and would otherwise retry in lockstep.
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
    let ms = std::env::var("DEXT_CHECKPOINT_DEBOUNCE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(CHECKPOINT_DEBOUNCE_MS_DEFAULT);
    std::time::Duration::from_millis(ms)
}

fn find_binary_on_path(name: &str) -> Option<PathBuf> {
    let Ok(path_var) = std::env::var("PATH") else {
        return None;
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
                return Some(p);
            }
        }
        #[cfg(windows)]
        {
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn binary_on_path(name: &str) -> bool {
    find_binary_on_path(name).is_some()
}

#[cfg(test)]
fn current_dext_executable_from(exe: PathBuf) -> Result<PathBuf> {
    if exe.is_file() {
        return Ok(exe);
    }
    if let Some(path_exe) = find_binary_on_path("dext") {
        return Ok(path_exe);
    }
    anyhow::bail!(
        "resolved current executable {} is not a file, and `dext` was not found on PATH",
        exe.display()
    )
}

fn default_discovery_excludes_enabled(extra: &[String]) -> bool {
    !extra.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--no-ignore" | "--no-ignore-vcs" | "--unrestricted" | "-u" | "-uu" | "-uuu"
        )
    })
}

fn add_default_fd_excludes(extra: &mut Vec<String>) {
    if !default_discovery_excludes_enabled(extra) {
        return;
    }
    for dir in DEFAULT_DISCOVERY_EXCLUDES {
        extra.push("--exclude".to_string());
        extra.push((*dir).to_string());
    }
}

fn add_default_rg_excludes(args: &mut Vec<String>, extra: &[String]) {
    if !default_discovery_excludes_enabled(extra) {
        return;
    }
    for dir in DEFAULT_DISCOVERY_EXCLUDES {
        args.push("--glob".to_string());
        args.push(format!("!**/{dir}/**"));
    }
}

fn add_default_grep_excludes(args: &mut Vec<String>, extra: &[String]) {
    if !default_discovery_excludes_enabled(extra) {
        return;
    }
    for dir in DEFAULT_DISCOVERY_EXCLUDES {
        args.push(format!("--exclude-dir={dir}"));
    }
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

fn find_supports_bsd_extended_regex_flag() -> bool {
    cfg!(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))
}

fn regex_ends_with_unescaped_dollar(pattern: &str) -> bool {
    if !pattern.ends_with('$') {
        return false;
    }
    let backslashes = pattern[..pattern.len().saturating_sub(1)]
        .bytes()
        .rev()
        .take_while(|b| *b == b'\\')
        .count();
    backslashes % 2 == 0
}

fn fd_find_fallback_regex(pattern: &str) -> String {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return ".*".to_string();
    }

    let anchored_start = trimmed.starts_with('^');
    let anchored_end = regex_ends_with_unescaped_dollar(trimmed);
    let body = trimmed.strip_prefix('^').unwrap_or(trimmed);
    match (anchored_start, anchored_end) {
        (true, true) => format!("(.*/)?{body}"),
        (true, false) => format!("(.*/)?({body}).*"),
        (false, true) => format!(".*({body})"),
        (false, false) => format!(".*({body}).*"),
    }
}

fn build_fd_find_fallback_args(search_root: &Path, pattern: &str, extra: &[String]) -> Vec<String> {
    let mut find_type = "f".to_string();
    let mut name_globs: Vec<String> = Vec::new();
    let mut exclude_globs: Vec<String> = if default_discovery_excludes_enabled(extra) {
        DEFAULT_DISCOVERY_EXCLUDES
            .iter()
            .flat_map(|dir| fd_exclude_path_patterns(dir))
            .collect()
    } else {
        Vec::new()
    };
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

    let mut args: Vec<String> = Vec::new();
    let bsd_find = find_supports_bsd_extended_regex_flag();
    if bsd_find {
        args.push("-E".into());
    }
    args.extend([
        search_root.to_string_lossy().to_string(),
        "-type".into(),
        find_type,
    ]);
    if !bsd_find {
        args.extend(["-regextype".into(), "posix-extended".into()]);
    }
    args.extend(["-regex".into(), fd_find_fallback_regex(pattern)]);
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
            let search_root = canonical_read_path(root, user_path)?;
            let extra = str_array(&input["extra_args"]);
            if binary_on_path("fd") {
                let mut args: Vec<String> = extra;
                add_default_fd_excludes(&mut args);
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
            let search_root = canonical_read_path(root, user_path)?;
            let extra = str_array(&input["extra_args"]);
            if binary_on_path("rg") {
                let mut args: Vec<String> = vec!["--line-number".into(), "--no-heading".into()];
                add_default_rg_excludes(&mut args, &extra);
                args.extend(translate_exclude_globs_for_rg(extra));
                args.push(pattern.to_string());
                args.push(search_root.to_string_lossy().to_string());
                Ok(("rg".to_string(), args, None))
            } else {
                let mut args: Vec<String> = vec!["-rn".into(), "-E".into(), "--color=never".into()];
                add_default_grep_excludes(&mut args, &extra);
                args.extend(translate_extra_args_for_grep(&extra));
                args.push(pattern.to_string());
                args.push(search_root.to_string_lossy().to_string());
                Ok(("grep".to_string(), args, None))
            }
        }
        "jq" => {
            let filter = input["filter"].as_str().ok_or("missing filter")?;
            if let Some(user_path) = input["path"].as_str() {
                let path = canonical_read_path(root, user_path)?;
                let _metadata = regular_file_metadata(&path)?;
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
        "browser" => {
            let args = str_array(&input["args"]);
            let stdin = input["stdin"].as_str().map(String::from);
            Ok(("agent-browser".to_string(), args, stdin))
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
                canonical_read_path(root, user_path).unwrap_or_else(|_| root.to_path_buf());
            let extra = str_array(&input["extra_args"]);
            let mut args: Vec<String> = vec!["-rn".into(), "-E".into(), "--color=never".into()];
            add_default_grep_excludes(&mut args, &extra);
            args.extend(translate_extra_args_for_grep(&extra));
            args.push(pattern.to_string());
            args.push(search_root.to_string_lossy().to_string());
            ("grep".to_string(), args, None)
        }
        "fd" => {
            let pattern = input["pattern"].as_str().unwrap_or("");
            let user_path = input["path"].as_str().unwrap_or(".");
            let search_root =
                canonical_read_path(root, user_path).unwrap_or_else(|_| root.to_path_buf());
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

fn http_tool_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > HTTP_TOOL_REDIRECT_LIMIT {
            return attempt.error("too many redirects");
        }
        if let Err(reason) = validate_http_tool_destination(attempt.url()) {
            let url = attempt.url().to_string();
            return attempt.error(format!("blocked http redirect to {url}: {reason}"));
        }
        attempt.follow()
    })
}

fn http_tool_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .dns_resolver(Arc::new(HttpToolResolver))
            .redirect(http_tool_redirect_policy())
            .build()
            .expect("build http tool client")
    })
}

struct HttpToolResolver;

impl reqwest::dns::Resolve for HttpToolResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            if !http_tool_allow_link_local() && http_tool_metadata_host(&host) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("blocked http DNS resolution for host '{host}': cloud metadata alias"),
                )
                .into());
            }

            if let Some(ip) = http_tool_host_ip_literal(&host) {
                if !http_tool_allow_link_local()
                    && let Some(reason) = http_tool_blocked_ip_reason(ip)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("host '{host}' is {ip} ({reason})"),
                    )
                    .into());
                }
                let addrs = vec![SocketAddr::new(ip, 0)];
                return Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs);
            }

            let host_for_lookup = host.clone();
            let addrs = tokio::task::spawn_blocking(move || {
                (host_for_lookup.as_str(), 0)
                    .to_socket_addrs()
                    .map(|iter| iter.collect::<Vec<SocketAddr>>())
            })
            .await
            .map_err(|e| io::Error::other(format!("HTTP DNS resolver task failed: {e}")))?
            .map_err(|e| io::Error::other(format!("HTTP DNS lookup for {host} failed: {e}")))?;

            if !http_tool_allow_link_local()
                && let Some(reason) = http_tool_blocked_addrs_reason(&host, &addrs)
            {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason).into());
            }

            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn http_tool_allow_link_local() -> bool {
    env_flag_default(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV, false)
}

fn validate_http_tool_destination(url: &reqwest::Url) -> std::result::Result<(), String> {
    if http_tool_allow_link_local() {
        return Ok(());
    }
    if let Some(reason) = http_tool_blocked_destination_reason(url) {
        Err(format!(
            "{reason}; set {HTTP_TOOL_ALLOW_LINK_LOCAL_ENV}=1 to allow link-local/metadata HTTP targets"
        ))
    } else {
        Ok(())
    }
}

fn http_tool_blocked_destination_reason(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str()?;
    if http_tool_metadata_host(host) {
        return Some(format!("host '{host}' is a cloud metadata alias"));
    }
    if let Some(ip) = http_tool_host_ip_literal(host) {
        return http_tool_blocked_ip_reason(ip).map(str::to_string);
    }
    None
}

fn http_tool_host_ip_literal(host: &str) -> Option<IpAddr> {
    let ip_host = host.trim_start_matches('[').trim_end_matches(']');
    let ip_literal = ip_host.split('%').next().unwrap_or(ip_host);
    ip_literal.parse::<IpAddr>().ok()
}

fn http_tool_blocked_addrs_reason(host: &str, addrs: &[SocketAddr]) -> Option<String> {
    for addr in addrs {
        let ip = addr.ip();
        if let Some(reason) = http_tool_blocked_ip_reason(ip) {
            return Some(format!("host '{host}' resolves to {ip} ({reason})"));
        }
    }
    None
}

fn http_tool_metadata_host(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(normalized.as_str(), "metadata" | "metadata.google.internal")
}

fn http_tool_blocked_ip_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => http_tool_blocked_ipv4_reason(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = http_tool_ipv6_embedded_ipv4(v6)
                && http_tool_blocked_ipv4_reason(v4).is_some()
            {
                return Some("IPv4-embedded IPv6 metadata/link-local address");
            }
            let first = v6.segments()[0];
            if first & 0xffc0 == 0xfe80 {
                Some("IPv6 link-local address")
            } else if v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254) {
                Some("AWS IPv6 metadata address")
            } else {
                None
            }
        }
    }
}

fn http_tool_blocked_ipv4_reason(v4: Ipv4Addr) -> Option<&'static str> {
    if v4.is_link_local() {
        Some("IPv4 link-local address")
    } else if v4 == Ipv4Addr::new(100, 100, 100, 200) {
        Some("cloud metadata address")
    } else {
        None
    }
}

fn http_tool_ipv6_embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = v6.segments();
    if segments[..5] != [0, 0, 0, 0, 0] || !matches!(segments[5], 0 | 0xffff) {
        return None;
    }
    Some(Ipv4Addr::new(
        (segments[6] >> 8) as u8,
        segments[6] as u8,
        (segments[7] >> 8) as u8,
        segments[7] as u8,
    ))
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
    let mut i = 0usize;
    while let Some(ch) = s[i..].chars().next() {
        if ch != '&' {
            out.push(ch);
            i += ch.len_utf8();
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

fn http_status_label(status: reqwest::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("{} {reason}", status.as_u16()),
        None => status.as_u16().to_string(),
    }
}

fn http_status_error(status: reqwest::StatusCode, text: &str) -> String {
    let label = http_status_label(status);
    let detail = text.trim();
    if detail.is_empty() || detail.eq_ignore_ascii_case(&format!("error code: {}", status.as_u16()))
    {
        format!("HTTP {label}")
    } else {
        format!("HTTP {label}: {detail}")
    }
}

fn format_http_request_error(err: reqwest::Error) -> String {
    let mut message = format!("HTTP request failed: {err}");
    let mut source = std::error::Error::source(&err);
    while let Some(err) = source {
        let detail = err.to_string();
        if !detail.is_empty() && !message.contains(&detail) {
            message.push_str(": ");
            message.push_str(&detail);
        }
        source = err.source();
    }
    message
}

async fn execute_http_tool_async(
    input: &Value,
    interrupt: Arc<AtomicBool>,
    default_timeout: std::time::Duration,
) -> std::result::Result<String, String> {
    let request = prepare_http_tool_request(input, default_timeout)?;
    validate_http_tool_destination(&request.url)?;
    if interrupt.load(Ordering::SeqCst) {
        return Err("killed by interrupt (^C)".to_string());
    }

    let mut req = http_tool_client()
        .request(request.method, request.url)
        .timeout(request.timeout)
        .header(reqwest::header::USER_AGENT, "dext/http");
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

    let resp = req.send().await.map_err(format_http_request_error)?;
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
            Ok(format!("HTTP {}", http_status_label(status)))
        } else {
            Ok(body)
        }
    } else if body.trim().is_empty() {
        Err(format!("HTTP {}", http_status_label(status)))
    } else {
        Err(format!("HTTP {}\n{body}", http_status_label(status)))
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

struct VerificationArtifactSpec<'a> {
    name: &'a str,
    command: &'a str,
    output: &'a str,
    exit_code: Option<i32>,
    duration: std::time::Duration,
    status: &'a str,
}

fn write_verification_artifact(
    root: &Path,
    session_id: &str,
    spec: VerificationArtifactSpec<'_>,
) -> Option<PathBuf> {
    let hash = sha256_hex_str(&format!("{}\n{}", spec.command, spec.output));
    let short_hash = &hash[..12.min(hash.len())];
    let dir = session_artifacts_dir(root, session_id);
    let path = dir.join(format!(
        "verify-{}-{}-{short_hash}.json",
        unix_timestamp_secs(),
        artifact_safe_name(spec.name)
    ));
    let body = json!({
        "type": "verification",
        "name": spec.name,
        "command": spec.command,
        "cwd": root.display().to_string(),
        "status": spec.status,
        "exit_code": spec.exit_code,
        "duration_ms": millis_u64(spec.duration),
        "output_tail": byte_suffix_at_char_boundary(spec.output, VERIFICATION_ARTIFACT_TAIL_CAP),
        "output": spec.output,
        "git": git_summary(root),
    });
    let bytes = serde_json::to_vec_pretty(&body).ok()?;
    atomic_write_bytes(&path, &bytes).ok()?;
    Some(path)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorkflowDiagnostic {
    file: String,
    line: Option<u64>,
    character: Option<u64>,
    severity: String,
    code: Option<String>,
    message: String,
}

#[derive(Clone, Debug, Default)]
struct WorkflowDiagnosticsReport {
    source: String,
    status: String,
    diagnostics: Vec<WorkflowDiagnostic>,
    raw_output: String,
    duration: std::time::Duration,
}

fn lsp_severity_label(value: Option<u64>) -> &'static str {
    match value {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "unknown",
    }
}

fn lsp_code_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn file_uri_to_display_path(uri: &str, root: &Path) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    let decoded = percent_decode_uri_path(rest);
    let path = PathBuf::from(decoded);
    path.strip_prefix(root)
        .unwrap_or(path.as_path())
        .display()
        .to_string()
}

fn percent_decode_uri_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn collect_lsp_diagnostics(value: &Value, root: &Path, out: &mut Vec<WorkflowDiagnostic>) {
    match value {
        Value::Object(map) => {
            if let Some(method) = map.get("method").and_then(Value::as_str)
                && method == "textDocument/publishDiagnostics"
                && let Some(params) = map.get("params")
            {
                let uri = params["uri"].as_str().unwrap_or("");
                let file = file_uri_to_display_path(uri, root);
                if let Some(items) = params["diagnostics"].as_array() {
                    for item in items {
                        let range = &item["range"]["start"];
                        let line = range["line"].as_u64().map(|v| v + 1);
                        let character = range["character"].as_u64().map(|v| v + 1);
                        let severity = lsp_severity_label(item["severity"].as_u64()).to_string();
                        let code = lsp_code_to_string(&item["code"]);
                        let message = item["message"]
                            .as_str()
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if !message.is_empty() {
                            out.push(WorkflowDiagnostic {
                                file: file.clone(),
                                line,
                                character,
                                severity,
                                code,
                                message,
                            });
                        }
                    }
                }
            }
            for child in map.values() {
                collect_lsp_diagnostics(child, root, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_lsp_diagnostics(child, root, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
fn parse_lsp_diagnostics_from_json_lines(output: &str, root: &Path) -> Vec<WorkflowDiagnostic> {
    let mut out = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
    {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            collect_lsp_diagnostics(&value, root, &mut out);
        }
    }
    out
}

fn render_workflow_diagnostics(report: &WorkflowDiagnosticsReport, cap: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let (errors, warnings) = workflow_diagnostic_counts(&report.diagnostics);
    let _ = writeln!(
        out,
        "diagnostics: {} via {} (errors={errors}, warnings={warnings}, total={}, {}ms)",
        report.status,
        report.source,
        report.diagnostics.len(),
        millis_u64(report.duration)
    );
    append_middle_truncated_diagnostics(&mut out, &report.diagnostics, 20);
    if report.diagnostics.is_empty() && !report.raw_output.trim().is_empty() {
        out.push_str("raw output:\n");
        out.push_str(&cap_bytes_with_hint(
            report.raw_output.clone(),
            cap / 2,
            "raw diagnostics output trimmed.",
        ));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    cap_bytes_with_hint(out, cap, "diagnostics output trimmed.")
}

fn workflow_diagnostic_summary(report: &WorkflowDiagnosticsReport) -> String {
    if let Some(first) = report.diagnostics.first() {
        format!(
            "{}{} {}",
            first.file,
            first
                .line
                .map(|line| format!(":{line}"))
                .unwrap_or_default(),
            summarize_inline(&first.message, 100)
        )
    } else if report.raw_output.trim().is_empty() {
        "no diagnostics".to_string()
    } else {
        summarize_inline(report.raw_output.trim(), 120)
    }
}

fn percent_encode_uri_path(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn file_uri_from_path(path: &Path) -> String {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let display = absolute.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        format!(
            "file:///{}",
            percent_encode_uri_path(display.trim_start_matches('/'))
        )
    } else {
        format!("file://{}", percent_encode_uri_path(&display))
    }
}

fn write_lsp_message<W: Write>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let body = value.to_string();
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()
}

fn read_lsp_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(raw) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = raw.trim().parse::<usize>().ok();
        }
    }
    let Some(len) = content_length else {
        return Ok(None);
    };
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8_lossy(&body).to_string()))
}

fn rust_analyzer_unavailable_output(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.contains("unknown binary 'rust-analyzer'")
        || lower.contains("unknown binary `rust-analyzer`")
        || lower.contains("unrecognized subcommand")
        || lower.contains("failed to spawn rust-analyzer")
}

fn rust_analyzer_command() -> Option<Command> {
    find_binary_on_path("rust-analyzer").map(Command::new)
}

fn collect_rust_files_for_diagnostics(dir: &Path, out: &mut Vec<PathBuf>, max_files: usize) {
    if out.len() >= max_files {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if out.len() >= max_files {
            return;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(name, "target" | ".git" | ".dext") {
            continue;
        }
        if path.is_dir() {
            collect_rust_files_for_diagnostics(&path, out, max_files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn rust_files_for_diagnostics(root: &Path, max_files: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_rust_files_for_diagnostics(&root.join("src"), &mut out, max_files);
    if out.is_empty() {
        collect_rust_files_for_diagnostics(root, &mut out, max_files);
    }
    out
}

fn run_rust_analyzer_diagnostics(root: &Path) -> Option<WorkflowDiagnosticsReport> {
    if !root.join("Cargo.toml").exists() {
        return None;
    }

    let started = std::time::Instant::now();
    let mut cmd = rust_analyzer_command()?;
    cmd.current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_std_process_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return None,
    };
    let child_pid = child.id();

    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let mut stdin = child.stdin.take()?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stdout_handle = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        while let Ok(Some(body)) = read_lsp_message(&mut reader) {
            if tx.send(body).is_err() {
                break;
            }
        }
    });
    let stderr_handle =
        std::thread::spawn(move || collect_sync_limited(stderr, PROCESS_STREAM_CAPTURE_CAP));

    let root_uri = file_uri_from_path(root);
    let workspace_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": workspace_name}],
            "capabilities": {}
        }
    });
    if write_lsp_message(&mut stdin, &init).is_err()
        || write_lsp_message(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        )
        .is_err()
    {
        terminate_std_child(&mut child);
        return None;
    }
    for path in rust_files_for_diagnostics(root, 64) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let uri = file_uri_from_path(&path);
            let _ = write_lsp_message(
                &mut stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didOpen",
                    "params": {
                        "textDocument": {
                            "uri": uri,
                            "languageId": "rust",
                            "version": 1,
                            "text": text
                        }
                    }
                }),
            );
        }
    }

    let timeout = timeout_from_env("DEXT_LSP_DIAGNOSTICS_TIMEOUT_SECS", 20);
    let deadline = std::time::Instant::now() + timeout;
    let mut diagnostics = Vec::new();
    let mut capture = LimitedTextCapture::new(PROCESS_STREAM_CAPTURE_CAP);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(body) => {
                let _ = capture.try_push_unit(&body);
                let _ = capture.try_push_unit("\n");
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    collect_lsp_diagnostics(&value, root, &mut diagnostics);
                }
                if !diagnostics.is_empty() && started.elapsed() > std::time::Duration::from_secs(2)
                {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = write_lsp_message(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
    );
    let _ = write_lsp_message(&mut stdin, &json!({"jsonrpc": "2.0", "method": "exit"}));
    drop(stdin);
    terminate_process_group_after_exit(child_pid);
    let _ = child.kill();
    let _ = child.wait();
    let _ = stdout_handle.join();
    let stderr = stderr_handle
        .join()
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
        .render("rust-analyzer stderr");
    let mut raw_output = capture.finish("raw LSP output trimmed.");
    if !stderr.trim().is_empty() {
        raw_output.push_str("--- stderr ---\n");
        raw_output.push_str(&stderr);
    }
    if rust_analyzer_unavailable_output(&raw_output) {
        return None;
    }
    let diagnostics = rank_dedupe_workflow_diagnostics(diagnostics);
    let status = if diagnostics.iter().any(|d| d.severity == "error") {
        "failed"
    } else {
        "passed"
    };
    Some(WorkflowDiagnosticsReport {
        source: "rust-analyzer-lsp".to_string(),
        status: status.to_string(),
        diagnostics,
        raw_output,
        duration: started.elapsed(),
    })
}

fn run_cargo_check_diagnostics(root: &Path) -> WorkflowDiagnosticsReport {
    let started = std::time::Instant::now();
    let mut cmd = Command::new("cargo");
    cmd.args(["check", "--message-format=json", "--quiet"])
        .current_dir(root);
    let result = run_sync_command_limited(
        cmd,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "cargo check diagnostics",
        timeout_from_env("DEXT_DIAGNOSTICS_TIMEOUT_SECS", 120),
    );
    match result {
        Ok((stdout, stderr, code)) => {
            let raw = merge_process_output_with_status(
                stdout.render("cargo check stdout"),
                stderr.render("cargo check stderr"),
                code,
            );
            WorkflowDiagnosticsReport {
                source: "cargo check".to_string(),
                status: if code == 0 { "passed" } else { "failed" }.to_string(),
                diagnostics: parse_cargo_json_diagnostics(&raw, root),
                raw_output: raw,
                duration: started.elapsed(),
            }
        }
        Err(err) => WorkflowDiagnosticsReport {
            source: "cargo check".to_string(),
            status: "failed".to_string(),
            diagnostics: parse_cargo_json_diagnostics(&err, root),
            raw_output: err,
            duration: started.elapsed(),
        },
    }
}

fn workflow_diagnostic_location(diagnostic: &WorkflowDiagnostic) -> String {
    match (diagnostic.line, diagnostic.character) {
        (Some(line), Some(character)) => format!("{}:{line}:{character}", diagnostic.file),
        (Some(line), None) => format!("{}:{line}", diagnostic.file),
        _ => diagnostic.file.clone(),
    }
}

fn workflow_diagnostic_rank(diagnostic: &WorkflowDiagnostic) -> usize {
    match diagnostic.severity.as_str() {
        "error" => 0,
        "warning" => 1,
        "info" => 2,
        "hint" => 3,
        _ => 4,
    }
}

fn rank_dedupe_workflow_diagnostics(
    diagnostics: Vec<WorkflowDiagnostic>,
) -> Vec<WorkflowDiagnostic> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for (idx, diagnostic) in diagnostics.into_iter().enumerate() {
        let key = (
            diagnostic.file.clone(),
            diagnostic.line,
            diagnostic.character,
            diagnostic.severity.clone(),
            diagnostic.code.clone(),
            diagnostic.message.clone(),
        );
        if seen.insert(key) {
            items.push((idx, diagnostic));
        }
    }
    items.sort_by(|(idx_a, a), (idx_b, b)| {
        workflow_diagnostic_rank(a)
            .cmp(&workflow_diagnostic_rank(b))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.character.cmp(&b.character))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| idx_a.cmp(idx_b))
    });
    items
        .into_iter()
        .map(|(_, diagnostic)| diagnostic)
        .collect()
}

fn workflow_diagnostic_counts(diagnostics: &[WorkflowDiagnostic]) -> (usize, usize) {
    let errors = diagnostics.iter().filter(|d| d.severity == "error").count();
    let warnings = diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();
    (errors, warnings)
}

fn render_workflow_diagnostic_line(diagnostic: &WorkflowDiagnostic) -> String {
    let code = diagnostic
        .code
        .as_ref()
        .map(|code| format!(" [{code}]"))
        .unwrap_or_default();
    format!(
        "- {}{}: {} — {}\n",
        diagnostic.severity,
        code,
        workflow_diagnostic_location(diagnostic),
        summarize_inline(&diagnostic.message, 180)
    )
}

fn append_middle_truncated_diagnostics(
    out: &mut String,
    diagnostics: &[WorkflowDiagnostic],
    limit: usize,
) {
    use std::fmt::Write as _;

    if limit == 0 || diagnostics.is_empty() {
        return;
    }
    if diagnostics.len() <= limit {
        for diagnostic in diagnostics {
            out.push_str(&render_workflow_diagnostic_line(diagnostic));
        }
        return;
    }
    let head = limit.div_ceil(2);
    let tail = limit.saturating_sub(head);
    for diagnostic in diagnostics.iter().take(head) {
        out.push_str(&render_workflow_diagnostic_line(diagnostic));
    }
    let omitted = diagnostics.len().saturating_sub(head + tail);
    let _ = writeln!(out, "- … {omitted} diagnostics omitted from middle");
    if tail > 0 {
        for diagnostic in diagnostics
            .iter()
            .skip(diagnostics.len().saturating_sub(tail))
        {
            out.push_str(&render_workflow_diagnostic_line(diagnostic));
        }
    }
}

fn parse_cargo_json_diagnostics(output: &str, root: &Path) -> Vec<WorkflowDiagnostic> {
    let mut out = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
    {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["reason"].as_str() != Some("compiler-message") {
            continue;
        }
        let msg = &value["message"];
        let level = msg["level"].as_str().unwrap_or("unknown");
        if !matches!(level, "error" | "warning") {
            continue;
        }
        let code = msg["code"]["code"].as_str().map(String::from);
        let message = msg["message"].as_str().unwrap_or("").trim().to_string();
        let span = msg["spans"].as_array().and_then(|spans| {
            spans
                .iter()
                .find(|span| span["is_primary"].as_bool().unwrap_or(false))
                .or_else(|| spans.first())
        });
        let (file, line, character) = if let Some(span) = span {
            let path = span["file_name"].as_str().unwrap_or("");
            let display = if path.is_empty() {
                "?".to_string()
            } else {
                let path = Path::new(path);
                if path.is_absolute() {
                    display_path_relative(path, root)
                } else {
                    path.display().to_string()
                }
            };
            (
                display,
                span["line_start"].as_u64(),
                span["column_start"].as_u64(),
            )
        } else {
            ("?".to_string(), None, None)
        };
        if !message.is_empty() {
            out.push(WorkflowDiagnostic {
                file,
                line,
                character,
                severity: level.to_string(),
                code,
                message,
            });
        }
    }
    rank_dedupe_workflow_diagnostics(out)
}

fn render_cargo_json_diagnostics_summary(output: &str, root: &Path) -> Option<String> {
    use std::fmt::Write as _;

    let diagnostics = parse_cargo_json_diagnostics(output, root);
    if diagnostics.is_empty() {
        return None;
    }
    let (errors, warnings) = workflow_diagnostic_counts(&diagnostics);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "cargo diagnostics summary (--message-format=json, ranked/deduped): errors={errors}, warnings={warnings}, total={}",
        diagnostics.len()
    );
    append_middle_truncated_diagnostics(&mut out, &diagnostics, CARGO_DIAGNOSTIC_SUMMARY_LIMIT);
    Some(out)
}

fn prepend_cargo_json_diagnostics_summary(output: String, root: &Path) -> String {
    match render_cargo_json_diagnostics_summary(&output, root) {
        Some(summary) => format!("{summary}\n{output}"),
        None => output,
    }
}

fn run_workflow_diagnostics(root: &Path) -> WorkflowDiagnosticsReport {
    run_rust_analyzer_diagnostics(root).unwrap_or_else(|| run_cargo_check_diagnostics(root))
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

fn display_path_relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn source_line_col_for_byte(content: &str, starts: &[usize], byte_idx: usize) -> (usize, usize) {
    if starts.is_empty() {
        return (1, 1);
    }
    let byte_idx = byte_idx.min(content.len());
    let line_idx = starts
        .partition_point(|start| *start <= byte_idx)
        .saturating_sub(1)
        .min(starts.len().saturating_sub(1));
    let column = content[starts[line_idx]..byte_idx].chars().count() + 1;
    (line_idx + 1, column)
}

fn render_old_string_match_locations(
    path: &Path,
    root: &Path,
    content: &str,
    old: &str,
    count: usize,
) -> String {
    use std::fmt::Write as _;

    let display = display_path_relative(path, root);
    let starts = source_line_starts(content);
    let mut capture = LimitedTextCapture::new(EDIT_MATCH_CONTEXT_CAP);
    capture.try_push_unit(&format!(
        "old_string appears {count} times in {} — must be unique\n",
        path.display()
    ));
    if starts.is_empty() {
        return capture.finish("Retry with a non-empty unique old_string.");
    }

    let mut shown = 0usize;
    for (match_idx, (byte_idx, _)) in content
        .match_indices(old)
        .take(EDIT_MATCH_DISPLAY_LIMIT)
        .enumerate()
    {
        let (line_no, column) = source_line_col_for_byte(content, &starts, byte_idx);
        let line_idx = line_no.saturating_sub(1);
        let start = line_idx.saturating_sub(EDIT_MATCH_CONTEXT_LINES);
        let end = line_idx
            .saturating_add(EDIT_MATCH_CONTEXT_LINES)
            .min(starts.len().saturating_sub(1));
        let mut block = String::new();
        let _ = writeln!(
            block,
            "match {}: {display}:{line_no}:{column}",
            match_idx + 1
        );
        for idx in start..=end {
            if let Some(line) = source_line_at(content, &starts, idx) {
                let marker = if idx == line_idx { '>' } else { ' ' };
                let _ = writeln!(block, "{marker} {}\t{line}", idx + 1);
            }
        }
        if !capture.try_push_unit(&block) {
            break;
        }
        shown += 1;
    }
    if count > shown {
        let _ = capture.try_push_unit(&format!("… {} more matches not shown\n", count - shown));
    }
    capture.finish(
        "Use read_file around a listed location, then retry with a larger unique old_string.",
    )
}

fn identifier_prefix(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("r#") {
        let mut end = 0usize;
        for (idx, ch) in rest.char_indices() {
            if is_ident_continue(ch) {
                end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        return (end > 0).then(|| &trimmed[..2 + end]);
    }

    let mut end = 0usize;
    for (idx, ch) in trimmed.char_indices() {
        if is_ident_continue(ch) {
            end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    (end > 0).then(|| &trimmed[..end])
}

fn symbol_candidates_for_line(line: &str) -> Vec<(String, &'static str)> {
    let trimmed = strip_decl_qualifiers(line);
    let mut out = Vec::new();
    if let Some(rest) = keyword_rest(trimmed, "const") {
        if let Some(fn_rest) = keyword_rest(rest, "fn") {
            if let Some(name) = identifier_prefix(fn_rest) {
                out.push((name.to_string(), "fn"));
            }
            return out;
        }
        if let Some(name) = identifier_prefix(rest) {
            out.push((name.to_string(), "const"));
        }
        return out;
    }
    if let Some(rest) = keyword_rest(trimmed, "static") {
        if let Some(name) = identifier_prefix(rest) {
            out.push((name.to_string(), "static"));
        }
        return out;
    }
    for (keyword, kind) in [
        ("fn", "fn"),
        ("struct", "struct"),
        ("enum", "enum"),
        ("trait", "trait"),
        ("type", "type"),
        ("mod", "mod"),
        ("class", "class"),
        ("def", "def"),
        ("function", "function"),
        ("interface", "interface"),
    ] {
        if let Some(rest) = keyword_rest(trimmed, keyword) {
            if let Some(name) = identifier_prefix(rest) {
                out.push((name.to_string(), kind));
            }
            return out;
        }
    }
    out
}

#[derive(Debug)]
struct SymbolSuggestion {
    name: String,
    kind: &'static str,
    line: usize,
    preview: String,
    score: usize,
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    let mut chars = candidate.chars();
    query.chars().all(|q| chars.by_ref().any(|c| c == q))
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(a, b)| a == b).count()
}

fn fuzzy_symbol_score(query: &str, candidate: &str) -> Option<usize> {
    let query = query.trim().to_ascii_lowercase();
    let candidate = candidate.trim().to_ascii_lowercase();
    if query.is_empty() || candidate.is_empty() {
        return None;
    }
    if query == candidate {
        return Some(0);
    }
    if candidate.starts_with(&query) {
        return Some(10 + candidate.len().saturating_sub(query.len()));
    }
    if candidate.contains(&query) {
        return Some(30 + candidate.len().saturating_sub(query.len()));
    }
    let distance = levenshtein_distance(&query, &candidate);
    let max_len = query.chars().count().max(candidate.chars().count());
    if distance <= (max_len / 3).max(2) {
        return Some(50 + distance * 4 + candidate.len().abs_diff(query.len()));
    }
    if is_subsequence(&query, &candidate) {
        return Some(90 + candidate.len().saturating_sub(query.len()));
    }
    let prefix = common_prefix_len(&query, &candidate);
    if prefix >= 3 || prefix * 2 >= query.chars().count().min(candidate.chars().count()) {
        return Some(
            120usize
                .saturating_sub(prefix)
                .saturating_add(candidate.len().abs_diff(query.len())),
        );
    }
    None
}

fn render_symbol_not_found_suggestions(
    path: &Path,
    root: &Path,
    content: &str,
    starts: &[usize],
    symbol: &str,
) -> Option<String> {
    use std::fmt::Write as _;

    let display = display_path_relative(path, root);
    let mut suggestions = Vec::new();
    for idx in 0..starts.len() {
        let line = source_line_at(content, starts, idx)?;
        for (name, kind) in symbol_candidates_for_line(line) {
            if let Some(score) = fuzzy_symbol_score(symbol, &name) {
                suggestions.push(SymbolSuggestion {
                    name,
                    kind,
                    line: idx + 1,
                    preview: summarize_inline(line.trim(), 100),
                    score,
                });
            }
        }
    }
    suggestions.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut seen = HashSet::new();
    suggestions.retain(|s| seen.insert((s.name.clone(), s.line)));
    if suggestions.is_empty() {
        return None;
    }

    let mut out = String::from("Did you mean:\n");
    for suggestion in suggestions.into_iter().take(READ_SYMBOL_SUGGESTION_LIMIT) {
        let _ = writeln!(
            out,
            "- {} @ {display}:{} ({}) — {}",
            suggestion.name, suggestion.line, suggestion.kind, suggestion.preview
        );
    }
    Some(out)
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

fn tool_result_context_cap_with_window(
    name: &str,
    input: &Value,
    usage: &Usage,
    model: &str,
    context_window: Option<u64>,
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
    if let Some(window) = context_window.filter(|tokens| *tokens > 0) {
        orchestrator::adaptive_tool_result_cap_for_window(usage, window, base_cap)
    } else {
        orchestrator::adaptive_tool_result_cap(usage, model, base_cap)
    }
}

#[cfg(test)]
fn tool_result_context_cap(
    name: &str,
    input: &Value,
    usage: &Usage,
    model: &str,
    context_mode: ContextMode,
) -> usize {
    tool_result_context_cap_with_window(name, input, usage, model, None, context_mode)
}

#[cfg(test)]
fn execute_tool(name: &str, input: &Value, root: &Path) -> std::result::Result<String, String> {
    execute_tool_with_cache(name, input, root, None, None)
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
    session_id: Option<&str>,
) -> std::result::Result<String, String> {
    match name {
        "read_file" => {
            let path = input["path"].as_str().ok_or("missing path")?;
            let path = canonical_read_path(root, path)?;
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let limit = input["limit"].as_u64().map(|v| v as usize);
            let explicit_window = input["offset"].is_u64() && limit.is_some();
            let cap = if explicit_window {
                effective_read_file_explicit_capture_cap()
            } else {
                effective_text_tool_capture_cap()
            };
            let metadata = regular_file_metadata(&path)?;
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
            let path = canonical_read_path(root, path_str)?;
            let _metadata = regular_file_metadata(&path)?;
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
                    let hint = tool_policy::tool_input_advisory("read_symbol", input)
                        .unwrap_or_else(|| format!("Search first with rg -n '{symbol}' {}, then retry read_symbol with an exact symbol or line.", path.display()));
                    let suggestions =
                        render_symbol_not_found_suggestions(&path, root, &content, &starts, symbol)
                            .unwrap_or_default();
                    return Err(format!(
                        "symbol '{symbol}' not found in {}\n{suggestions}{hint}",
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
            let before = match std::fs::read_to_string(&path) {
                Ok(existing) => Some(existing),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("failed to read existing file for preview: {e}")),
            };
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
            }
            std::fs::write(&path, content).map_err(|e| format!("{e}"))?;
            Ok(write_file_result_with_diff(
                &path,
                root,
                before.as_deref(),
                content,
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
                return Err(render_old_string_match_locations(
                    &path, root, &content, old, count,
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
                            "edit[{i}]: {}",
                            render_old_string_match_locations(&path, root, &content, old, count)
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
            let project_todo_path = root.join("DEXT.todo.json");
            let session_todo_path = session_id.map(|id| session_todo_path(root, id));
            let todo_path = session_todo_path
                .as_ref()
                .filter(|path| path.exists())
                .unwrap_or(&project_todo_path);
            if !todo_path.exists() {
                return Ok("(no todos — use todo_write to create a task list)".to_string());
            }
            let content =
                std::fs::read_to_string(todo_path).map_err(|e| format!("read todo: {e}"))?;
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
            let project_todo_path = root.join("DEXT.todo.json");
            let todo_path = session_id
                .map(|id| session_todo_path(root, id))
                .unwrap_or_else(|| project_todo_path.clone());
            let before =
                read_todo_counts(&todo_path).or_else(|| read_todo_counts(&project_todo_path));
            let content = serde_json::to_string_pretty(&json!(validated))
                .map_err(|e| format!("serialize todo: {e}"))?;
            atomic_write_bytes(&todo_path, content.as_bytes())
                .map_err(|e| format!("write todo: {e}"))?;

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

struct LocalSudoAuth {
    askpass: Option<PathBuf>,
    sudo_path: PathBuf,
    sudo_shim_dir: PathBuf,
    password_fifo: Option<PathBuf>,
    password: Option<String>,
    preauth_required: bool,
}

async fn prepare_local_sudo_auth(
    root: &Path,
    session_id: &str,
) -> std::result::Result<Option<LocalSudoAuth>, String> {
    let Some(sudo_path) = find_binary_on_path("sudo") else {
        return Ok(None);
    };
    let sudo_path = std::fs::canonicalize(&sudo_path).unwrap_or(sudo_path);
    let sudo_shim_dir = write_sudo_command_shim(root, session_id, &sudo_path)?;
    let askpass = write_sudo_askpass_script(root, session_id)?;
    Ok(Some(LocalSudoAuth {
        askpass: Some(askpass),
        sudo_path,
        sudo_shim_dir,
        password_fifo: None,
        password: None,
        preauth_required: true,
    }))
}

#[cfg(unix)]
fn write_sudo_askpass_script(
    root: &Path,
    session_id: &str,
) -> std::result::Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = session_sudo_dir(root, session_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("prepare local sudo prompt dir: {e}"))?;
    let mut dir_perms = std::fs::metadata(&dir)
        .map_err(|e| format!("metadata sudo prompt dir: {e}"))?
        .permissions();
    dir_perms.set_mode(0o700);
    std::fs::set_permissions(&dir, dir_perms).map_err(|e| format!("chmod sudo prompt dir: {e}"))?;
    let path = dir.join("askpass.sh");
    let content = sudo_askpass_script_content();
    atomic_write_bytes(&path, content.as_bytes())
        .map_err(|e| format!("write sudo askpass script: {e}"))?;
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| format!("metadata sudo askpass script: {e}"))?
        .permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&path, perms)
        .map_err(|e| format!("chmod sudo askpass script: {e}"))?;
    Ok(path)
}

#[cfg(unix)]
fn sudo_askpass_script_content_with_paths(zenity: &str, kdialog: &str, osascript: &str) -> String {
    let zenity = shell_single_quote(zenity);
    let kdialog = shell_single_quote(kdialog);
    let osascript = shell_single_quote(osascript);
    format!(
        r#"#!/bin/sh
set -eu
PROMPT=${{1:-'sudo password:'}}
if [ -n "${{DEXT_SUDO_PASSWORD_FIFO:-}}" ] && [ -p "$DEXT_SUDO_PASSWORD_FIFO" ]; then
  IFS= read -r password < "$DEXT_SUDO_PASSWORD_FIFO" || exit 1
  printf '%s\n' "$password"
  exit 0
fi
if command -v zenity >/dev/null 2>&1; then
  exec {zenity} --password --title='Dext local sudo prompt' --text="$PROMPT"
fi
if command -v kdialog >/dev/null 2>&1; then
  exec {kdialog} --password "$PROMPT"
fi
if [ -x {osascript} ] || command -v osascript >/dev/null 2>&1; then
  exec {osascript} \
    -e 'on run argv' \
    -e 'set promptText to item 1 of argv' \
    -e 'display dialog promptText default answer "" with hidden answer with title "Dext local sudo prompt" buttons {{"OK"}} default button "OK"' \
    -e 'text returned of result' \
    -e 'end run' \
    "$PROMPT"
fi
printf '%s\n' 'Dext local sudo prompt requires Dext local auth, osascript, zenity, or kdialog.' >&2
exit 1
"#
    )
}

#[cfg(unix)]
fn sudo_askpass_script_content() -> String {
    let zenity = find_binary_on_path("zenity")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "zenity".to_string());
    let kdialog = find_binary_on_path("kdialog")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "kdialog".to_string());
    let osascript = find_binary_on_path("osascript")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "osascript".to_string());
    sudo_askpass_script_content_with_paths(&zenity, &kdialog, &osascript)
}

#[cfg(not(unix))]
fn write_sudo_askpass_script(
    _root: &Path,
    _session_id: &str,
) -> std::result::Result<PathBuf, String> {
    Err("local sudo prompts are only supported on Unix".to_string())
}

#[cfg(unix)]
fn write_sudo_command_shim(
    root: &Path,
    session_id: &str,
    sudo_path: &Path,
) -> std::result::Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    let sudo_dir = session_sudo_dir(root, session_id);
    let bin_dir = sudo_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).map_err(|e| format!("prepare sudo command shim dir: {e}"))?;
    for dir in [&sudo_dir, &bin_dir] {
        let mut perms = std::fs::metadata(dir)
            .map_err(|e| format!("metadata sudo command shim dir: {e}"))?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(dir, perms)
            .map_err(|e| format!("chmod sudo command shim dir: {e}"))?;
    }

    let path = bin_dir.join("sudo");
    let real_sudo = shell_single_quote(&sudo_path.display().to_string());
    let content = format!("#!/bin/sh\nexec {real_sudo} -n \"$@\"\n");
    atomic_write_bytes(&path, content.as_bytes())
        .map_err(|e| format!("write sudo command shim: {e}"))?;
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| format!("metadata sudo command shim: {e}"))?
        .permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&path, perms).map_err(|e| format!("chmod sudo command shim: {e}"))?;
    Ok(bin_dir)
}

#[cfg(not(unix))]
fn write_sudo_command_shim(
    _root: &Path,
    _session_id: &str,
    _sudo_path: &Path,
) -> std::result::Result<PathBuf, String> {
    Err("local sudo command shims are only supported on Unix".to_string())
}

#[cfg(unix)]
fn sudo_secret_random_suffix() -> std::result::Result<String, String> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(|e| format!("random sudo pipe name: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn create_secret_fifo_in_dir(dir: &Path, label: &str) -> std::result::Result<PathBuf, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).map_err(|e| format!("prepare {label} prompt dir: {e}"))?;
    let mut dir_perms = std::fs::metadata(dir)
        .map_err(|e| format!("metadata {label} prompt dir: {e}"))?
        .permissions();
    dir_perms.set_mode(0o700);
    std::fs::set_permissions(dir, dir_perms)
        .map_err(|e| format!("chmod {label} prompt dir: {e}"))?;

    for _ in 0..16 {
        let path = dir.join(format!("password-{}.fifo", sudo_secret_random_suffix()?));
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("{label} pipe path contains NUL"))?;
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        if rc == 0 {
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| format!("metadata {label} pipe: {e}"))?
                .permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)
                .map_err(|e| format!("chmod {label} pipe: {e}"))?;
            return Ok(path);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::AlreadyExists {
            continue;
        }
        return Err(format!("create {label} pipe: {err}"));
    }
    Err(format!("create {label} pipe: exhausted unique names"))
}

#[cfg(unix)]
fn create_sudo_password_fifo(
    root: &Path,
    session_id: &str,
) -> std::result::Result<PathBuf, String> {
    create_secret_fifo_in_dir(&session_sudo_dir(root, session_id), "sudo password")
}

#[cfg(not(unix))]
fn create_sudo_password_fifo(
    _root: &Path,
    _session_id: &str,
) -> std::result::Result<PathBuf, String> {
    Err("local sudo password prompts are only supported on Unix".to_string())
}

struct SudoPasswordPipeRuntime {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}

#[cfg(unix)]
fn start_sudo_password_pipe_writer(path: PathBuf, password: String) -> SudoPasswordPipeRuntime {
    use std::os::unix::fs::OpenOptionsExt;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread_path = path.clone();
    let handle = std::thread::spawn(move || {
        let mut password = password;
        while !thread_stop.load(Ordering::SeqCst) {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&thread_path)
            {
                Ok(mut fifo) => {
                    let _ = fifo.write_all(password.as_bytes());
                    let _ = fifo.write_all(b"\n");
                    let _ = fifo.flush();
                    break;
                }
                Err(err)
                    if matches!(err.raw_os_error(), Some(code) if code == libc::ENXIO)
                        || err.kind() == io::ErrorKind::WouldBlock =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        clear_secret_string(&mut password);
    });
    SudoPasswordPipeRuntime {
        stop,
        handle: Some(handle),
        path,
    }
}

#[cfg(not(unix))]
fn start_sudo_password_pipe_writer(path: PathBuf, _password: String) -> SudoPasswordPipeRuntime {
    SudoPasswordPipeRuntime {
        stop: Arc::new(AtomicBool::new(true)),
        handle: None,
        path,
    }
}

impl Drop for SudoPasswordPipeRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A git HTTPS credential the user entered through the masked local prompt.
/// It lives only in Dext's memory for the rest of the session and is handed
/// to git through a FIFO-fed credential helper — it must never appear in the
/// model transcript, session logs, argv, or on disk.
#[derive(Clone)]
struct LocalGitCredential {
    username: String,
    secret: String,
    hosts: Vec<String>,
}

impl LocalGitCredential {
    fn covers_any_host(&self, hosts: &[String]) -> bool {
        !self.hosts.is_empty() && hosts.iter().any(|host| self.hosts.contains(host))
    }

    fn host_scope_label(&self) -> String {
        format_git_hosts(&self.hosts)
    }
}

impl Drop for LocalGitCredential {
    fn drop(&mut self) {
        clear_secret_string(&mut self.username);
        clear_secret_string(&mut self.secret);
        self.hosts.clear();
    }
}

/// Interpret masked-prompt input as either `token` or `username:token`.
/// The username split is deliberately conservative: bare secrets that happen
/// to contain a colon (URLs, generic passwords) stay intact.
fn parse_git_credential_input(raw: &str) -> LocalGitCredential {
    let trimmed = raw.trim();
    if let Some((user, pass)) = trimmed.split_once(':')
        && !user.is_empty()
        && !pass.is_empty()
        && !pass.starts_with("//")
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+'))
    {
        return LocalGitCredential {
            username: user.to_string(),
            secret: pass.to_string(),
            hosts: Vec::new(),
        };
    }
    LocalGitCredential {
        // Any non-empty username works for GitHub/GitLab token auth; this is
        // the conventional placeholder.
        username: "x-access-token".to_string(),
        secret: trimmed.to_string(),
        hosts: Vec::new(),
    }
}

/// Match the HTTPS credential failures git emits with GIT_TERMINAL_PROMPT=0.
/// Keep this git-specific so unrelated bash output mentioning disabled
/// terminal prompts doesn't trigger Dext's local git credential prompt.
fn output_indicates_git_credential_failure(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("could not read username for 'https://")
        || lower.contains("could not read password for 'https://")
        || lower.contains("authentication failed for 'https://")
        || ((lower.contains("invalid username or token")
            || lower.contains("invalid username or password"))
            && (lower.contains("fatal: authentication failed for 'https://")
                || (lower.contains("remote:") && lower.contains("https://"))))
}

fn normalize_git_credential_host(host: &str) -> Option<String> {
    let host = host
        .trim()
        .trim_matches(|c| matches!(c, '\'' | '"' | '[' | ']' | '(' | ')' | ',' | ';'))
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('.');
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn extract_https_git_hosts(text: &str) -> Vec<String> {
    text.split(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']')
    })
    .filter_map(|raw| {
        let token = raw.trim_matches(|c| matches!(c, ',' | ';' | '.' | ')' | ']' | '}'));
        let lower = token.to_ascii_lowercase();
        let rest = lower.strip_prefix("https://")?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        normalize_git_credential_host(authority)
    })
    .collect()
}

fn dedupe_git_hosts<I>(hosts: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for host in hosts {
        if let Some(host) = normalize_git_credential_host(&host)
            && seen.insert(host.clone())
        {
            out.push(host);
        }
    }
    out
}

fn git_credential_hosts_for_failure(planned_hosts: &[String], output: &str) -> Vec<String> {
    dedupe_git_hosts(
        planned_hosts
            .iter()
            .cloned()
            .chain(extract_https_git_hosts(output)),
    )
}

fn format_git_hosts(hosts: &[String]) -> String {
    if hosts.is_empty() {
        "the HTTPS remote".to_string()
    } else if hosts.len() == 1 {
        hosts[0].clone()
    } else {
        format!("{} +{} more", hosts[0], hosts.len() - 1)
    }
}

fn simple_shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn split_bash_credential_segments(command: &str) -> Vec<&str> {
    command
        .split([';', '\n', '&', '|'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn word_is_env_assignment(word: &str) -> bool {
    let Some((key, value)) = word.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && !value.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

fn env_assignment_is_safe_for_git_credential(word: &str) -> bool {
    let Some((key, _value)) = word.split_once('=') else {
        return false;
    };
    let upper = key.to_ascii_uppercase();
    !(upper.starts_with("GIT_CONFIG")
        || upper == "GIT_ASKPASS"
        || upper == "GIT_CURL_VERBOSE"
        || upper.starts_with("GIT_TRACE")
        || upper == "SSH_ASKPASS"
        || upper == "SSH_ASKPASS_REQUIRE")
}

fn git_config_word_is_safe_for_credential(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    !(lower.starts_with("credential.")
        || lower.starts_with("core.askpass")
        || lower.starts_with("core.hookspath")
        || lower.starts_with("http.extraheader")
        || lower.starts_with("url."))
}

fn git_subcommand_for_credential_segment(words: &[String], git_idx: usize) -> Option<&str> {
    let mut idx = git_idx.saturating_add(1);
    while idx < words.len() {
        let word = words[idx].as_str();
        if matches!(word, "-C" | "--git-dir" | "--work-tree" | "--namespace") {
            idx = idx.saturating_add(2);
            continue;
        }
        if word == "-c" {
            if !words
                .get(idx.saturating_add(1))
                .is_some_and(|config| git_config_word_is_safe_for_credential(config))
            {
                return None;
            }
            idx = idx.saturating_add(2);
            continue;
        }
        if word.starts_with("-c") && word.len() > 2 {
            if !git_config_word_is_safe_for_credential(&word[2..]) {
                return None;
            }
            idx = idx.saturating_add(1);
            continue;
        }
        if word.starts_with("-C")
            || word.starts_with("--git-dir=")
            || word.starts_with("--work-tree=")
            || word.starts_with("--namespace=")
            || matches!(word, "--no-pager" | "--literal-pathspecs")
        {
            idx = idx.saturating_add(1);
            continue;
        }
        if word.starts_with('-') {
            idx = idx.saturating_add(1);
            continue;
        }
        return Some(word);
    }
    None
}

fn git_subcommand_can_use_https_credential(words: &[String], git_idx: usize) -> bool {
    let Some(subcommand) = git_subcommand_for_credential_segment(words, git_idx) else {
        return false;
    };
    match subcommand {
        "clone" | "fetch" | "pull" | "push" | "ls-remote" => true,
        "remote" => words.iter().any(|word| word == "update"),
        "submodule" => words
            .iter()
            .any(|word| matches!(word.as_str(), "update" | "sync")),
        _ => false,
    }
}

fn git_credential_segment_is_safe(segment: &str) -> bool {
    let trimmed = segment.trim();
    if trimmed.is_empty() || trimmed == "set -euo pipefail" || trimmed == "set -eo pipefail" {
        return true;
    }
    if trimmed.contains("$(")
        || trimmed.contains('`')
        || trimmed.contains('<')
        || trimmed.contains('>')
        || trimmed.contains("DEXT_GIT_CRED")
    {
        return false;
    }
    let words = simple_shell_words(trimmed);
    let mut idx = 0;
    while idx < words.len() && word_is_env_assignment(&words[idx]) {
        if !env_assignment_is_safe_for_git_credential(&words[idx]) {
            return false;
        }
        idx += 1;
    }
    if matches!(
        words.get(idx).map(String::as_str),
        Some("command" | "builtin")
    ) {
        idx += 1;
    }
    let Some(command) = words.get(idx).map(String::as_str) else {
        return false;
    };
    if !(command == "git" || command.ends_with("/git")) {
        return false;
    }
    git_subcommand_can_use_https_credential(&words, idx)
}

fn bash_command_can_receive_git_credential(command: &str) -> bool {
    let segments = split_bash_credential_segments(command);
    !segments.is_empty()
        && segments
            .iter()
            .all(|segment| git_credential_segment_is_safe(segment))
}

fn bash_command_should_install_git_credential(command: &str, cred: &LocalGitCredential) -> bool {
    if cred.hosts.is_empty()
        || command.to_ascii_lowercase().contains("http://")
        || !bash_command_can_receive_git_credential(command)
    {
        return false;
    }
    let command_hosts = dedupe_git_hosts(extract_https_git_hosts(command));
    command_hosts.is_empty() || cred.covers_any_host(&command_hosts)
}

fn stored_git_credential_for_bash_call(
    name: &str,
    input: &Value,
    cred: Option<&LocalGitCredential>,
) -> Option<LocalGitCredential> {
    let cred = cred?;
    if name != "bash" {
        return None;
    }
    let cmd = input["command"].as_str()?;
    bash_command_should_install_git_credential(cmd, cred).then(|| cred.clone())
}

fn git_auth_guidance_for_hosts(hosts: &[String]) -> String {
    let host_label = format_git_hosts(hosts);
    let tail = GIT_AUTH_GUIDANCE
        .strip_prefix("git needs credentials for an HTTPS remote. ")
        .unwrap_or(GIT_AUTH_GUIDANCE);
    format!("git needs credentials for {host_label}. {tail}")
}

#[cfg(unix)]
fn git_credential_helper_script_content() -> &'static str {
    // Answers `get` with the credential the user typed into Dext's masked
    // prompt, delivered through a private FIFO so the secret never lands on
    // disk, in argv, or in the transcript. `store`/`erase` are no-ops.
    r#"#!/bin/sh
set -eu
op=${1:-}
if [ "$op" != get ]; then exit 0; fi
protocol=
host=
while IFS= read -r line; do
  [ -n "$line" ] || break
  case "$line" in
    protocol=*) protocol=${line#protocol=} ;;
    host=*) host=${line#host=} ;;
  esac
done
[ "$protocol" = https ] || exit 1
host=${host%%:*}
host=$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')
[ -n "${DEXT_GIT_CRED_HOSTS:-}" ] || exit 1
case " $DEXT_GIT_CRED_HOSTS " in
  *" $host "*) ;;
  *) exit 1 ;;
esac
if [ -z "${DEXT_GIT_CRED_FIFO:-}" ] || [ ! -p "$DEXT_GIT_CRED_FIFO" ]; then exit 1; fi
{ IFS= read -r u && IFS= read -r p; } < "$DEXT_GIT_CRED_FIFO" || exit 1
printf 'username=%s\npassword=%s\n' "$u" "$p"
"#
}

#[cfg(unix)]
fn write_git_credential_helper_script(
    root: &Path,
    session_id: &str,
) -> std::result::Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::session::session_git_auth_dir(root, session_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("prepare git auth dir: {e}"))?;
    let mut dir_perms = std::fs::metadata(&dir)
        .map_err(|e| format!("metadata git auth dir: {e}"))?
        .permissions();
    dir_perms.set_mode(0o700);
    std::fs::set_permissions(&dir, dir_perms).map_err(|e| format!("chmod git auth dir: {e}"))?;
    let path = dir.join("credential-helper.sh");
    atomic_write_bytes(&path, git_credential_helper_script_content().as_bytes())
        .map_err(|e| format!("write git credential helper: {e}"))?;
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| format!("metadata git credential helper: {e}"))?
        .permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&path, perms)
        .map_err(|e| format!("chmod git credential helper: {e}"))?;
    Ok(path)
}

/// Like start_sudo_password_pipe_writer, but keeps serving after the first
/// read: one git command may invoke the credential helper several times
/// (fetch + push, redirects, submodules), and each helper run opens the FIFO
/// once.
///
/// The writer holds the FIFO open O_RDWR for its whole lifetime. That
/// guarantees liveness: a reader's blocking open always completes while the
/// runtime is alive, and once the FIFO is unlinked, new readers fail with
/// ENOENT instead of blocking in open() on an orphaned inode forever (which
/// would wedge git and the tool call behind it).
#[cfg(unix)]
fn start_git_credential_pipe_writer(path: PathBuf, payload: String) -> SudoPasswordPipeRuntime {
    use std::os::unix::fs::OpenOptionsExt;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread_path = path.clone();
    let handle = std::thread::spawn(move || {
        let mut payload = payload;
        let fifo = loop {
            if thread_stop.load(Ordering::SeqCst) {
                clear_secret_string(&mut payload);
                return;
            }
            // O_RDWR on a FIFO never blocks regardless of readers.
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&thread_path)
            {
                Ok(f) => break f,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        // Keep the pipe buffer topped up with whole payloads. Writes are
        // atomic (payload << PIPE_BUF), each helper run consumes exactly one
        // payload's lines, so readers always stay line-aligned; a full buffer
        // (EAGAIN) just means plenty of unread copies are queued.
        while !thread_stop.load(Ordering::SeqCst) {
            let _ = (&fifo).write_all(payload.as_bytes());
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        clear_secret_string(&mut payload);
    });
    SudoPasswordPipeRuntime {
        stop,
        handle: Some(handle),
        path,
    }
}

/// Per-bash-call runtime for supplying the session git credential. `env` is
/// injected into the child; dropping this stops the FIFO writer and removes
/// the FIFO, so it must outlive the tool call.
struct GitCredentialHelperRuntime {
    env: Vec<(String, String)>,
    _pipe: SudoPasswordPipeRuntime,
}

#[cfg(unix)]
fn prepare_git_credential_helper(
    root: &Path,
    session_id: &str,
    cred: LocalGitCredential,
) -> std::result::Result<GitCredentialHelperRuntime, String> {
    if cred.hosts.is_empty() {
        return Err("git credential host scope is empty".to_string());
    }
    // Absolute paths only: git may run the helper from any working directory
    // (git -C, submodules), and the session state dir can be relative.
    let script = write_git_credential_helper_script(root, session_id)?;
    let script = std::fs::canonicalize(&script).unwrap_or(script);
    let fifo = create_secret_fifo_in_dir(
        &crate::session::session_git_auth_dir(root, session_id),
        "git credential",
    )?;
    let fifo = std::fs::canonicalize(&fifo).unwrap_or(fifo);
    let payload = format!("{}\n{}\n", cred.username, cred.secret);
    let pipe = start_git_credential_pipe_writer(fifo.clone(), payload);
    // GIT_CONFIG_* appends the helper for every git invocation in the
    // command, including nested ones, without touching any config file. The
    // `!` form runs the script through the shell with the operation appended.
    let helper_value = format!("!{}", shell_single_quote(&script.display().to_string()));
    let env = vec![
        ("DEXT_GIT_CRED_FIFO".to_string(), fifo.display().to_string()),
        ("DEXT_GIT_CRED_HOSTS".to_string(), cred.hosts.join(" ")),
        ("GIT_CONFIG_COUNT".to_string(), "5".to_string()),
        (
            "GIT_CONFIG_KEY_0".to_string(),
            "credential.helper".to_string(),
        ),
        ("GIT_CONFIG_VALUE_0".to_string(), String::new()),
        (
            "GIT_CONFIG_KEY_1".to_string(),
            "credential.helper".to_string(),
        ),
        ("GIT_CONFIG_VALUE_1".to_string(), helper_value),
        ("GIT_CONFIG_KEY_2".to_string(), "core.hooksPath".to_string()),
        ("GIT_CONFIG_VALUE_2".to_string(), "/dev/null".to_string()),
        ("GIT_CONFIG_KEY_3".to_string(), "protocol.allow".to_string()),
        ("GIT_CONFIG_VALUE_3".to_string(), "never".to_string()),
        (
            "GIT_CONFIG_KEY_4".to_string(),
            "protocol.https.allow".to_string(),
        ),
        ("GIT_CONFIG_VALUE_4".to_string(), "always".to_string()),
    ];
    Ok(GitCredentialHelperRuntime { env, _pipe: pipe })
}

#[cfg(not(unix))]
fn prepare_git_credential_helper(
    _root: &Path,
    _session_id: &str,
    _cred: LocalGitCredential,
) -> std::result::Result<GitCredentialHelperRuntime, String> {
    Err("local git credential prompts are only supported on Unix".to_string())
}

fn sudo_wrapper_prefix(auth: &LocalSudoAuth) -> String {
    let real_sudo = shell_single_quote(&auth.sudo_path.display().to_string());
    let mut prefix = String::new();
    if auth.preauth_required {
        if let Some(askpass) = auth.askpass.as_ref() {
            let askpass_path = std::fs::canonicalize(askpass).unwrap_or_else(|_| askpass.clone());
            let askpass = shell_single_quote(&askpass_path.display().to_string());
            let fifo_env = auth
                .password_fifo
                .as_ref()
                .map(|fifo| {
                    let fifo = std::fs::canonicalize(fifo).unwrap_or_else(|_| fifo.clone());
                    format!(
                        " {SUDO_PASSWORD_FIFO_ENV}={}",
                        shell_single_quote(&fifo.display().to_string())
                    )
                })
                .unwrap_or_default();
            prefix.push_str(&format!(
                "if ! builtin command {real_sudo} -n -v 2>/dev/null; then\n  SUDO_ASKPASS={askpass} {SUDO_ASKPASS_ENV}={askpass}{fifo_env} SUDO_PROMPT='[dext local sudo] password for %u to run %p: ' builtin command {real_sudo} -A -v || exit $?\nfi\nunset SUDO_ASKPASS {SUDO_ASKPASS_ENV} {SUDO_PASSWORD_FIFO_ENV} SUDO_PROMPT\n"
            ));
        } else {
            prefix.push_str("printf '%s\\n' 'sudo auth prompt unavailable' >&2\nexit 1\n");
        }
    }
    prefix.push_str(&format!(
        "sudo() {{ builtin command {real_sudo} -n \"$@\"; }}\n"
    ));
    let mut seen = HashSet::new();
    for path in std::iter::once(auth.sudo_path.display().to_string()).chain(
        sudo_wrapper_command_paths()
            .iter()
            .map(|path| path.to_string()),
    ) {
        if !seen.insert(path.clone()) || !sudo_shell_function_name_is_safe(&path) {
            continue;
        }
        prefix.push_str(&format!(
            "function {path} {{ builtin command {} -n \"$@\"; }}\n",
            shell_single_quote(&path)
        ));
    }
    prefix
}

fn sudo_wrapper_command_paths() -> &'static [&'static str] {
    &["/usr/bin/sudo", "/bin/sudo"]
}

fn sudo_shell_function_name_is_safe(path: &str) -> bool {
    !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
}

#[cfg(test)]
async fn execute_bash_async_with_timeout(
    cmd: &str,
    root: &Path,
    interrupt: Arc<AtomicBool>,
    timeout: std::time::Duration,
    sandbox_profile: SandboxProfile,
) -> std::result::Result<String, String> {
    execute_bash_async_prepared(
        cmd,
        root,
        interrupt,
        timeout,
        None,
        sandbox_profile,
        None,
        &[],
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_bash_async_prepared(
    cmd: &str,
    root: &Path,
    interrupt: Arc<AtomicBool>,
    timeout: std::time::Duration,
    local_sudo_auth: Option<LocalSudoAuth>,
    sandbox_profile: SandboxProfile,
    live_output: Option<LiveToolOutput>,
    extra_env: &[(String, String)],
) -> std::result::Result<String, String> {
    let mut local_sudo_auth = local_sudo_auth;
    let _sudo_password_pipe = local_sudo_auth.as_mut().and_then(|auth| {
        let fifo = auth.password_fifo.clone()?;
        let password = auth.password.take()?;
        Some(start_sudo_password_pipe_writer(fifo, password))
    });
    let bash_cmd = if let Some(auth) = local_sudo_auth.as_ref() {
        format!("{}{cmd}", sudo_wrapper_prefix(auth))
    } else {
        cmd.to_string()
    };
    // A sudo command is an intentional privilege escalation the user approved;
    // OS sandboxing it is incoherent, so it runs unconfined.
    let effective_profile = if local_sudo_auth.is_some() {
        SandboxProfile::DangerFullAccess
    } else {
        sandbox_profile
    };
    let mut command = sandbox::tokio_command("bash", effective_profile, root);
    command
        .arg("-c")
        .arg(&bash_cmd)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    deny_interactive_prompt_env(&mut command);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    if let Some(auth) = local_sudo_auth.as_ref() {
        let mut paths = vec![auth.sudo_shim_dir.clone()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        let path_env =
            std::env::join_paths(paths).map_err(|e| format!("prepare sudo PATH: {e}"))?;
        command
            .env("PATH", path_env)
            .env_remove("SUDO_ASKPASS")
            .env_remove(SUDO_ASKPASS_ENV)
            .env_remove(SUDO_PASSWORD_FIFO_ENV);
    }
    configure_tokio_process_group(&mut command);
    let mut child = command.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let child_pid = child.id();

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    let out_task = tokio::spawn(collect_async_limited_live(
        stdout,
        PROCESS_STREAM_CAPTURE_CAP,
        live_output.clone(),
        "stdout",
    ));
    let err_task = tokio::spawn(collect_async_limited_live(
        stderr,
        PROCESS_STREAM_CAPTURE_CAP,
        live_output,
        "stderr",
    ));
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
    Ok(prepend_cargo_json_diagnostics_summary(body, root))
}

async fn execute_external_async(
    bin: &str,
    args: &[String],
    stdin_data: Option<&str>,
    cwd: &Path,
    interrupt: Arc<AtomicBool>,
    timeout: std::time::Duration,
    sandbox_profile: SandboxProfile,
) -> std::result::Result<String, String> {
    use tokio::io::AsyncWriteExt;

    let mut cmd = sandbox::tokio_command(bin, sandbox_profile, cwd);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    deny_interactive_prompt_env(&mut cmd);
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
    if name == "browser" || bin == "grep" || bin == "find" {
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

fn live_output_for_tool(
    sink: &dyn EventSink,
    call_id: &str,
    name: &str,
    _input: &Value,
) -> Option<LiveToolOutput> {
    if name != "bash" {
        return None;
    }
    sink.live_output_sender().map(|tx| LiveToolOutput {
        call_id: call_id.to_string(),
        name: name.to_string(),
        tx,
    })
}

// Per-call execution context; args are distinct types passed straight through
// from the agent loop, so a struct adds indirection without preventing misuse.
#[allow(clippy::too_many_arguments)]
async fn execute_builtin_call(
    name: String,
    input: Value,
    root: PathBuf,
    interrupt: Arc<AtomicBool>,
    read_cache: Option<Arc<Mutex<ReadFileCache>>>,
    session_id: Option<String>,
    local_sudo_auth: Option<LocalSudoAuth>,
    git_credential: Option<LocalGitCredential>,
    sandbox_profile: SandboxProfile,
    live_output: Option<LiveToolOutput>,
    pack_env: Vec<(String, String)>,
) -> std::result::Result<String, String> {
    if name == "bash" {
        let cmd = input["command"].as_str().unwrap_or("").to_string();
        let guarded = tool_policy::apply_bash_guardrails(&cmd)?;
        let timeout = timeout_from_tool_input(&input, bash_tool_timeout());
        // Keep the credential-helper runtime (FIFO + writer thread) alive for
        // the duration of the call; its Drop stops the writer and removes the
        // FIFO. Setup failures are explicit failures: silently running without
        // the helper would make an auth failure look like a rejected token.
        let git_cred_runtime = match git_credential {
            Some(cred) if bash_command_should_install_git_credential(&cmd, &cred) => {
                let session_id = session_id
                    .as_deref()
                    .ok_or_else(|| "git credential helper needs a session id".to_string())?;
                Some(prepare_git_credential_helper(&root, session_id, cred)?)
            }
            _ => None,
        };
        let mut extra_env = pack_env;
        if let Some(runtime) = git_cred_runtime.as_ref() {
            extra_env.extend(runtime.env.clone());
        }
        execute_bash_async_prepared(
            &guarded,
            &root,
            interrupt,
            timeout,
            local_sudo_auth,
            sandbox_profile,
            live_output,
            &extra_env,
        )
        .await
    } else if name == "http" {
        execute_http_tool_async(&input, interrupt, external_tool_timeout()).await
    } else if is_external_process_tool(&name) {
        // Run unconfined: the browser recipe manages its own profile/cache dirs
        // and needs network + broad access; git_diff/git_log are read-only
        // inspection but git writes its index (.git/index) as bookkeeping, which
        // would intermittently fail under the read-only profile for a repo
        // outside the writable roots. Neither mutates the working tree, so this
        // is security-neutral. The write-capable externals (awk, csvkit) and the
        // rest stay confined.
        let ext_profile = if matches!(name.as_str(), "browser" | "git_diff" | "git_log") {
            SandboxProfile::DangerFullAccess
        } else {
            sandbox_profile
        };
        let (bin, args, stdin) = prepare_external_tool(&name, &input, &root)?;
        let result = execute_external_async(
            &bin,
            &args,
            stdin.as_deref(),
            &root,
            interrupt.clone(),
            external_tool_timeout(),
            ext_profile,
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
                    ext_profile,
                )
                .await
            }
            other => other,
        }
    } else {
        tokio::task::spawn_blocking(move || {
            execute_tool_with_cache(
                &name,
                &input,
                &root,
                read_cache.as_ref(),
                session_id.as_deref(),
            )
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

pub(crate) enum LocalAuthSecret {
    Secret(String),
    Canceled,
    Unavailable,
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

const SHELL_PRELUDE_LINES: &[&str] = &[
    "set -euo pipefail",
    "set -eo pipefail",
    "set -o pipefail",
    "set -eu",
    "set -e",
];

fn strip_shell_prelude_prefix(mut line: &str) -> Option<&str> {
    let mut stripped = false;
    loop {
        let trimmed = line.trim_start();
        let mut matched = false;
        for prelude in SHELL_PRELUDE_LINES {
            let Some(suffix) = trimmed.strip_prefix(prelude) else {
                continue;
            };
            if !suffix.is_empty()
                && !suffix.chars().next().is_some_and(char::is_whitespace)
                && !suffix.starts_with("&&")
                && !suffix.starts_with(';')
            {
                continue;
            }
            let suffix = suffix.trim_start();
            if suffix.starts_with('#') {
                line = "";
                stripped = true;
                matched = true;
                break;
            }
            let rest = suffix
                .strip_prefix("&&")
                .or_else(|| suffix.strip_prefix(';'))
                .unwrap_or(suffix);
            line = rest;
            stripped = true;
            matched = true;
            break;
        }
        if !matched {
            break;
        }
    }
    stripped.then(|| line.trim_start())
}

fn summarize_bash_command(command: &str, max_chars: usize) -> String {
    let raw_lines: Vec<&str> = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let mut lines = raw_lines.clone();
    while let Some(first) = lines.first().copied() {
        let Some(rest) = strip_shell_prelude_prefix(first) else {
            break;
        };
        lines.remove(0);
        if !rest.is_empty() {
            lines.insert(0, rest);
            break;
        }
    }
    if lines.is_empty() {
        lines = raw_lines;
    }
    let collapsed_full = lines.join(" ");
    if collapsed_full.is_empty() {
        return "?".to_string();
    }
    if collapsed_full.chars().count() <= max_chars {
        return collapsed_full;
    }

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

const DEFAULT_SYSTEM: &str = "You are dext, a terse coding assistant running as a CLI agent on the user's machine.

Use only tools exposed in the current API tool list. Do not assume unavailable tools exist.
Tool protocol: invoke tools only through actual provider tool calls; never print raw `to=functions.*`, `tool_use`, function-call JSON, or bash command envelopes as assistant text.
Runtime: privileged ops are auto-approved; if approval is denied, ask the user. Do not use the unsafe pip flag unless requested. Avoid mutating external state stores directly.
Runtime state: check the auto-refreshed Context State before each tool call; if a strategy shows PIVOT REQUIRED or a pattern line, stop repeating and pivot or ask.
Project state: use todo_read/todo_write for nontrivial work. Treat DEXT.md/recall.md as guidance; update recall.md only for durable decisions.
Tool hierarchy: use exposed native Dext tools before bash. Use fd/rg/read_file/read_symbol/git_diff/todo/edit tools, and http when exposed, for their domains; bash is last resort for shell-only orchestration, build/test/install, or catalog gaps.
Discovery: prefer fd for files, rg for content. Use rg first for symbols, then read_symbol/focused read_file. Read-only tools may inspect absolute paths outside the sandbox; writes stay confined. Avoid broad reads; paginate. Use read-only tools in parallel. Do not use bash for ordinary file reads, recursive search, file discovery, git diff, or HTTP when an exposed native tool fits.
Editing: always read before editing. Use edit_file for small changes, multi_edit for batches, write_file for new files. Checkpoint before large edits.
Git: inspect status/diff before editing tracked files. Use git_diff for diffs and git_commit (not raw git) for commits. Use bash git log only when history is needed and no git_log tool is exposed.
Shell: preserve pipefail in bash. Treat bash calls as atomic: Dext cleans the tool process group after each call, so do not use shell backgrounding, nohup, or disown for persistence; setsid-style detaches are unsupported because they escape Dext cleanup. For requested persistent local services, use an OS supervisor when available (Linux: systemd-run --user with dext- unit, inspect/stop via systemctl --user). Exposed Dext tools like rg/fd/http/git_diff are API tools, not shell binaries. Prefer arrays/heredocs for quoting. Inspect stderr even on exit 0. Obey [runtime-note] advisories in tool results before choosing the next tool. Validate external sources before scaling. On auth failures, ask for credentials.
Browser: if browser_recipe=agent-browser, use the browser tool only when useful; start with browser args ['skills','get','core','--full'].
Verification: narrowest checks first, realistic timeouts. Prefer stdlib/existing test runners. Compare structured outputs semantically. Rerun suites only if code changed.
Context: keep tool output small. Preserve exact paths/commands/decisions. Avoid rereading just-written files; prefer compile/test checks. Summarize large logs, share partial results early.
Communication: be terse. Report what changed, verification results, gaps. No narrative unless checkpointing.
Tables: single well-formed tables render best. When several small related tables share a theme/schema, consolidate them into one grouped table with one header row; use grouping columns/rows, one physical line per row, compact cell delimiters like ` · ` or `;`, and plain short cells. Avoid stacked heading+table blocks and fragile cell content: nested markdown/bold, emoji verdict icons, unescaped `|` characters, or multi-line cells. If separate tables are truly needed, separate them with a full prose sentence.
Packs: when creating or installing a reusable pack or shelf, default to Dext's user-global scope (`~/.dext/packs` or `~/.dext/shelves/<shelf>/packs`) unless the user explicitly asks for project-local placement.";

const TINY_SYSTEM: &str = "You are dext tiny: terse CLI agent. Use exposed tools only. Tool protocol: real tool calls only; never print raw to=functions/tool_use/function-call JSON/bash envelopes or prefill the TUI input. Check Context State; pivot at PIVOT REQUIRED or repeated-action pattern. For nontrivial work, define small steps by required input and observable output; run independent reads in parallel, reuse verified results, and repair only the failed step. Native tools before bash: prefer rg/fd/read_file/read_symbol/git_diff/edit/http; read-only absolute paths ok, writes confined; bash only for shell orchestration, build/test/install, or gaps. Inspect before edits. Keep output small. Use todo for nontrivial work. Bash is atomic; supervised dext- services only for requested persistence. Obey [runtime-note] advisories. Reusable packs default user-global unless asked otherwise. Tables: related data -> one grouped table; one row/line; plain cells, no emoji/bold/unescaped `|`/linebreaks; prose between unrelated tables. Verify narrowly. Final: changed, tests, gaps.";

const FRUGAL_TOOL_PROTOCOL_NOTE: &str = "Frugal workflow: never try to prefill the TUI input/composer. For nontrivial work, define small steps by required input and observable output; run independent reads in parallel, reuse verified results, and repair only the failed step.";

fn prompt_context_files(root: &Path, filename: &str) -> Vec<(String, PathBuf, String)> {
    scan_prompt_context_files(root, filename).sections
}

/// One ancestor-walk scan for a prompt context file, with a stat signature for
/// every candidate path it checked. The signature lets per-request callers
/// revalidate the scan with a handful of stats instead of repeating the walk
/// and re-reading the files, while still catching mid-turn writes (the agent
/// itself updates recall.md) and newly created files at any ancestor level.
#[derive(Clone, Default)]
struct PromptContextScan {
    sections: Vec<(String, PathBuf, String)>,
    signature: Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>,
}

fn prompt_file_signature(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some((meta.modified().ok()?, meta.len()))
}

fn scan_prompt_context_files(root: &Path, filename: &str) -> PromptContextScan {
    let mut scan = PromptContextScan::default();
    let mut dir = root;
    loop {
        let candidate = dir.join(filename);
        scan.signature
            .push((candidate.clone(), prompt_file_signature(&candidate)));
        if candidate.exists()
            && let Ok(content) = std::fs::read_to_string(&candidate)
        {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                let display = dir.strip_prefix(root).unwrap_or(dir).display();
                let label = if display.to_string().is_empty() {
                    ".".to_string()
                } else {
                    format!("/{display}")
                };
                scan.sections.push((label, candidate, trimmed.to_string()));
            }
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => break,
        }
    }
    scan.sections.reverse();
    scan
}

fn prompt_context_scan_is_current(scan: &PromptContextScan) -> bool {
    scan.signature
        .iter()
        .all(|(path, sig)| prompt_file_signature(path) == *sig)
}

/// Labeled prompt context sections: (ancestor label, file path, content).
type PromptContextSections = Vec<(String, PathBuf, String)>;

/// Cached prompt filesystem scans (DEXT.md/recall.md ancestor walks and pack
/// discovery), shared across the many provider requests of a single turn.
/// Refreshed when the epoch (user turn) changes or a stat signature drifts.
struct PromptScanCache {
    epoch: u64,
    dext_md: PromptContextScan,
    recall: PromptContextScan,
    pack_summary: Option<String>,
}

#[derive(Debug, Clone)]
struct SystemParts {
    stable: String,
    env: String,
    prompt_sources: Vec<PathBuf>,
}

const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "read_symbol",
    "fd",
    "rg",
    "git_diff",
    "todo_read",
];

fn todo_summary_from_path(path: &Path, max_items: usize) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let todos = serde_json::from_str::<Value>(&content).ok()?;
    let items = todos.as_array()?;
    if items.is_empty() {
        return None;
    }

    let (pending, in_progress, completed) = todo_status_counts(items);
    let mut out = format!(
        "todo_status: {pending} pending, {in_progress} in_progress, {completed} completed\n"
    );
    let mut shown = 0usize;
    for status in ["in_progress", "pending"] {
        for item in items
            .iter()
            .filter(|item| item["status"].as_str().unwrap_or("pending") == status)
        {
            if shown >= max_items {
                break;
            }
            let text = item["text"].as_str().unwrap_or("?").trim();
            if text.is_empty() {
                continue;
            }
            out.push_str(&format!("- {status}: {}\n", summarize_inline(text, 140)));
            shown += 1;
        }
    }
    if items.len() > shown {
        out.push_str(&format!(
            "- … {} more todo item(s) not shown\n",
            items.len() - shown
        ));
    }
    Some(out)
}

fn read_project_todo_summary(root: &Path, max_items: usize) -> Option<String> {
    todo_summary_from_path(&root.join("DEXT.todo.json"), max_items)
}

fn read_session_todo_summary(root: &Path, session_id: &str, max_items: usize) -> Option<String> {
    todo_summary_from_path(&session_todo_path(root, session_id), max_items)
        .or_else(|| read_project_todo_summary(root, max_items))
}

const PLAN_SYSTEM: &str = "\
You are a planning agent. You have READ-ONLY tools: read_file, read_symbol, fd, rg, \
git_diff, todo_read. Explore the codebase and produce a concrete implementation plan.

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

// Output cap for the one-shot compaction/summary request (kept small: the
// summary is intentionally terse).
const COMPACT_SUMMARY_MAX_TOKENS: u32 = 2_048;
const COMPACT_SUMMARY_MAX_TOKENS_THINKING: u32 = 8_192;

fn compact_summary_max_tokens(thinking_effort: ThinkingEffort) -> u32 {
    if thinking_effort == ThinkingEffort::Off {
        COMPACT_SUMMARY_MAX_TOKENS
    } else {
        COMPACT_SUMMARY_MAX_TOKENS_THINKING
    }
}

const HISTORY_CHAR_BUDGET_MIN: usize = 24_000;
pub(crate) const HISTORY_CHAR_BUDGET_END_TURN_PERCENT: u8 = 90;
const HISTORY_CHAR_BUDGET_ACTIVE_PERCENT: u8 = 80;
const FRUGAL_HISTORY_CHAR_BUDGET_PERCENT: u8 = 80;
const FRUGAL_HISTORY_CHAR_BUDGET_MIN: usize = 8_000;
const FRUGAL_HISTORY_CHAR_BUDGET_MAX: usize = 32_000;
// Fixed history budget for frugal (non-tiny) context mode.
const FRUGAL_HISTORY_CHAR_BUDGET: usize = 60_000;
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
        let path = std::env::var("DEXT_HOOKS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("hooks.json"));
        Self::load_file(&path)
    }

    fn load_file(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("[hooks] parse error in {}: {e}", path.display());
                Hooks::default()
            }),
            Err(_) => Hooks::default(),
        }
    }

    fn extend(&mut self, other: Self) {
        self.pre_tool.extend(other.pre_tool);
        self.post_tool.extend(other.post_tool);
        self.user_prompt.extend(other.user_prompt);
    }

    fn fire(
        &self,
        phase: &str,
        tool: &str,
        env: &[(&str, &str)],
        extra_env: &[(String, String)],
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
            for (k, v) in extra_env {
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

fn pack_auto_invocation_disabled_by_env(pack: &packs::PackInfo) -> bool {
    let Ok(raw) = std::env::var("DEXT_NO_PACK") else {
        return false;
    };
    let raw = raw.trim();
    if raw.is_empty() || matches!(raw, "0" | "false" | "off" | "no") {
        return false;
    }
    let tokens: Vec<&str> = raw
        .split(|c: char| c == ',' || c == ';' || c.is_ascii_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // Glob tokens disable all packs. Check before normalization since
    // normalize_pack_disable_token strips non-alphanumeric chars like '*'.
    if tokens
        .iter()
        .any(|t| matches!(*t, "*" | "all" | "true" | "1"))
    {
        return true;
    }
    let name = normalize_pack_disable_token(&pack.name);
    let env_name = normalize_pack_disable_token(&pack.env_var_name());
    let shelf = pack.shelf.as_deref().map(normalize_pack_disable_token);
    tokens
        .iter()
        .map(|t| normalize_pack_disable_token(t))
        .any(|t| t == name || t == env_name || shelf.as_deref() == Some(t.as_str()))
}

fn normalize_pack_disable_token(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if matches!(c, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn load_prompt_env_value(value: String) -> Result<String> {
    if let Some(path) = value.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("reading prompt env file {path}"))
    } else {
        Ok(value)
    }
}

const LATEST_SESSION_NAME: &str = "_latest";
const SESSION_FORMAT_VERSION: u32 = 3;

fn default_context_mode_for_provider(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
) -> ContextMode {
    if provider::is_local_llama_provider(provider_id, api_provider, base_url) {
        ContextMode::Frugal
    } else {
        ContextMode::Standard
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) enum ContextMode {
    #[default]
    Standard,
    Frugal,
    Tiny,
}

impl ContextMode {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "standard" | "default" | "full" | "normal" | "off" => Some(Self::Standard),
            "frugal" | "lean" | "slim" | "minimal" | "min" => Some(Self::Frugal),
            "tiny" | "skinny" | "micro" | "lite" => Some(Self::Tiny),
            _ => None,
        }
    }

    fn from_env() -> Self {
        std::env::var("DEXT_CONTEXT_MODE")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Frugal => "frugal",
            Self::Tiny => "tiny",
        }
    }

    fn is_frugal(self) -> bool {
        matches!(self, Self::Frugal | Self::Tiny)
    }

    fn is_tiny(self) -> bool {
        self == Self::Tiny
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum ToolContextProfile {
    #[default]
    Default,
    Frugal,
    Full,
}

impl ToolContextProfile {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "default" | "standard" | "core" => Some(Self::Default),
            "frugal" | "slim" | "minimal" | "tiny" => Some(Self::Frugal),
            "full" | "all" => Some(Self::Full),
            _ => None,
        }
    }

    fn parse_selectable(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "default" | "standard" | "core" => Some(Self::Default),
            "full" | "all" => Some(Self::Full),
            _ => None,
        }
    }

    fn from_env() -> Self {
        std::env::var("DEXT_TOOLSET")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }

    fn effective(self, _context_mode: ContextMode) -> Self {
        if self == Self::Frugal {
            Self::Default
        } else {
            self
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Frugal => "frugal",
            Self::Full => "full",
        }
    }
}

const DEFAULT_TOOL_NAMES: &[&str] = &[
    "read_file",
    "read_symbol",
    "write_file",
    "edit_file",
    "multi_edit",
    "bash",
    "fd",
    "rg",
    "http",
    "browser",
    "git_diff",
    "git_commit",
    "todo_read",
    "todo_write",
];

const SPECIALIZED_TOOL_NAMES: &[&str] = &["jq", "fzf", "awk", "git_log", "csvkit"];

fn tool_name_allowed_in_profile(name: &str, profile: ToolContextProfile) -> bool {
    match profile {
        ToolContextProfile::Full => true,
        ToolContextProfile::Default | ToolContextProfile::Frugal => {
            DEFAULT_TOOL_NAMES.contains(&name)
        }
    }
}

struct ToolsCommandResult {
    output: String,
}

fn render_tools_status(agent: &Agent) -> String {
    use std::fmt::Write as _;

    let header = agent.session_header();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "tools: {} (schemas {}, browser {})",
        agent.tool_context_profile().as_str(),
        agent.wire_tool_profile().as_str(),
        agent.browser_recipe().as_str()
    );
    let _ = writeln!(out, "usage: /tools [status|default|full]");
    let _ = writeln!(
        out,
        "exposed ({}): {}",
        header.exposed_tools.len(),
        render_limited_csv(&header.exposed_tools, SLASH_LIST_LIMIT, "(none)", "tools")
    );
    let _ = writeln!(
        out,
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
        out,
        "auto-approved now ({}): {}",
        header.auto_approved_tools.len(),
        render_limited_csv(
            &header.auto_approved_tools,
            SLASH_LIST_LIMIT,
            "(none)",
            "tools"
        )
    );

    let hidden_specialized: Vec<String> = SPECIALIZED_TOOL_NAMES
        .iter()
        .filter(|name| !agent.tools.iter().any(|tool| tool.name == **name))
        .map(|name| (*name).to_string())
        .collect();
    if !hidden_specialized.is_empty() {
        let _ = writeln!(
            out,
            "hidden until /tools full: {}",
            hidden_specialized.join(", ")
        );
    }
    if agent.browser_recipe() == BrowserRecipe::Disabled {
        let _ = writeln!(out, "browser: off (separate /browser agent-browser opt-in)");
    }
    out.trim_end().to_string()
}

fn handle_tools_command(agent: &mut Agent, arg: &str) -> ToolsCommandResult {
    let raw = arg.trim();
    let normalized = raw.to_ascii_lowercase();
    match normalized.as_str() {
        "" | "status" | "list" | "ls" => ToolsCommandResult {
            output: render_tools_status(agent),
        },
        "default" | "standard" | "core" | "full" | "all" => {
            let profile =
                ToolContextProfile::parse_selectable(raw).unwrap_or(ToolContextProfile::Default);
            agent.tool_context_profile = profile.effective(agent.context_mode);
            agent.refresh_tools_for_context();
            let browser_note = if agent.browser_recipe() == BrowserRecipe::Disabled {
                "; browser off"
            } else {
                ""
            };
            ToolsCommandResult {
                output: format!(
                    "tools -> {} ({} exposed; schemas {}{})",
                    agent.tool_context_profile().as_str(),
                    agent.tools.len(),
                    agent.wire_tool_profile().as_str(),
                    browser_note
                ),
            }
        }
        _ => ToolsCommandResult {
            output: "usage: /tools [status|default|full]".to_string(),
        },
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
    diagnostics: Vec<WorkflowDiagnosticRecord>,
    next_actions: Vec<String>,
    active_focus: Option<WorkMapFocusState>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct WorkflowDiagnosticRecord {
    source: String,
    status: String,
    summary: String,
    errors: usize,
    warnings: usize,
    duration_ms: u64,
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
    consecutive_server_errors: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct SessionProvenance {
    dext_version: String,
    git: Option<String>,
    provider: String,
    api_provider: ApiProvider,
    model: String,
    thinking_effort: ThinkingEffort,
    approval_profile: ApprovalProfile,
    sandbox_profile: SandboxProfile,
    system_prompt_hash: String,
    dext_md_hash: Option<String>,
    #[serde(default, alias = "dext_memory_hash")]
    recall_hash: Option<String>,
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
    // Full prompt actually sent/shown; `system` remains the restore-safe base prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    composed_system: Option<String>,
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
    context_mode_explicit: bool,
    #[serde(default)]
    tool_context_profile: ToolContextProfile,
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
            composed_system: None,
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
            context_mode_explicit: false,
            tool_context_profile: ToolContextProfile::default(),
            tool_profile: ToolProfile::default(),
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
    if !ledger.current_phase.trim().is_empty() {
        out.push_str(&format!("current_phase: {}\n", ledger.current_phase.trim()));
    }
    // Only real, observable state is surfaced here. The objective/done/pending/
    // in_progress/blocked/next_actions fields are excluded from the runtime
    // status block: they were previously seeded from ObjectiveTracker's
    // synthesized checkpoints and the latest user prompt, which produced
    // placeholder noise ("produce execution plan", "deliver requested outcome
    // …") and echoed raw user text. The ObjectiveTracker remains a turn-local
    // mechanism for runtime nudges; it no longer pollutes displayed state.
    for (label, items) in [
        ("constraints", &ledger.constraints),
        ("decisions", &ledger.decisions),
        ("queued_user_updates", &ledger.steering),
        ("files_changed", &ledger.files_changed),
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
    if !ledger.diagnostics.is_empty() {
        out.push_str("diagnostics:\n");
        for record in ledger.diagnostics.iter().rev().take(4).rev() {
            out.push_str(&format!(
                "- {}: {} errors={} warnings={} ({}ms) {}\n",
                record.source,
                record.status,
                record.errors,
                record.warnings,
                record.duration_ms,
                record.summary.trim()
            ));
        }
    }
    out
}

fn normalize_http_status_noise(text: &str) -> String {
    let normalized = text.replace(" <unknown status code>", "");
    let Some(http) = normalized.strip_prefix("HTTP ") else {
        return normalized;
    };
    let code = http
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if code.is_empty() {
        return normalized;
    }
    let redundant = format!(": error code: {code}");
    normalized
        .strip_suffix(&redundant)
        .unwrap_or(&normalized)
        .to_string()
}

fn normalize_provider_health_errors(health: &mut ProviderHealthLedger) {
    for state in health.providers.values_mut() {
        if let Some(error) = &mut state.last_error {
            *error = normalize_http_status_noise(error);
        }
    }
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
            out.push_str(&format!(
                "last_error={} ",
                summarize_inline(&normalize_http_status_noise(err), 160)
            ));
        }
        out.push('\n');
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ContextStrategy {
    GitStatus,
    HttpUrlHunt,
    BinaryHunt,
}

impl ContextStrategy {
    fn label(self) -> &'static str {
        match self {
            Self::GitStatus => "git_status",
            Self::HttpUrlHunt => "http_url_hunt",
            Self::BinaryHunt => "binary_hunt",
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::GitStatus | Self::BinaryHunt => 1,
            Self::HttpUrlHunt => 2,
        }
    }

    fn counts_successes(self) -> bool {
        matches!(self, Self::GitStatus)
    }
}

#[derive(Clone, Debug)]
struct ContextToolUseSummary {
    summary: String,
    action_key: String,
    strategy: Option<ContextStrategy>,
    mutates_worktree: bool,
}

#[derive(Clone, Debug)]
struct ContextActionSummary {
    summary: String,
    action_key: String,
    strategy: Option<ContextStrategy>,
    mutates_worktree: bool,
    ok: bool,
    detail: String,
}

#[derive(Default)]
struct ContextStrategyBudget {
    used: usize,
}

fn command_looks_like_git_status(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("git status")
        || lower.contains("git diff --stat")
        || lower.contains("git diff --shortstat")
}

fn command_looks_like_http_probe(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("curl ")
        || lower.contains("wget ")
        || lower.contains("httpie ")
        || lower.contains("python -m requests")
}

fn command_looks_like_binary_hunt(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let names_browser = [
        "agent-browser",
        "lightpanda",
        "chromium",
        "google-chrome",
        "chrome",
        "firefox",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    names_browser
        && (lower.contains("which ")
            || lower.contains("command -v")
            || lower.contains("type -p")
            || lower.contains("/usr/bin/")
            || lower.contains("/snap/bin/")
            || lower.contains("/bin/"))
}

fn context_strategy_for_tool(name: &str, input: &Value) -> Option<ContextStrategy> {
    match name {
        "git_diff" if input["stat"].as_bool().unwrap_or(false) => Some(ContextStrategy::GitStatus),
        "http" => Some(ContextStrategy::HttpUrlHunt),
        "browser" => Some(ContextStrategy::HttpUrlHunt),
        "bash" => {
            let command = input["command"].as_str().unwrap_or("");
            if command_looks_like_git_status(command) {
                Some(ContextStrategy::GitStatus)
            } else if command_looks_like_binary_hunt(command) {
                Some(ContextStrategy::BinaryHunt)
            } else if command_looks_like_http_probe(command) {
                Some(ContextStrategy::HttpUrlHunt)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn context_action_key(name: &str, input: &Value) -> String {
    if name == "bash" {
        format!(
            "bash:{}",
            summarize_bash_command(
                input["command"].as_str().unwrap_or(""),
                TOOL_SUMMARY_CHAR_CAP
            )
        )
    } else {
        format!("{name}:{}", input)
    }
}

fn bash_command_may_mutate_worktree(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "rm ",
        "mv ",
        "cp ",
        "touch ",
        "mkdir ",
        "rmdir ",
        "git commit",
        "git add",
        "git rm",
        "git mv",
        ">",
        "sed -i",
        "python - <<",
        "cat <<",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn context_action_mutates_worktree(name: &str, input: &Value) -> bool {
    match name {
        "write_file" | "edit_file" | "multi_edit" | "git_commit" => true,
        "bash" => bash_command_may_mutate_worktree(input["command"].as_str().unwrap_or("")),
        _ => false,
    }
}

fn output_has_blocked_source_marker(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "captcha",
        "cloudflare",
        "verify you are human",
        "access denied",
        "attention required",
        "enable javascript",
        "just a moment",
        "unrecognized http arg",
        "decode error",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn context_strategy_semantic_failure(strategy: Option<ContextStrategy>, content: &str) -> bool {
    matches!(strategy, Some(ContextStrategy::HttpUrlHunt))
        && (tool_policy::output_has_auth_failure_markers(content)
            || output_has_blocked_source_marker(content))
}

fn collect_context_actions(history: &[Message]) -> Vec<ContextActionSummary> {
    let start = history
        .iter()
        .rposition(is_fresh_user_prompt_message)
        .unwrap_or(0);
    let mut uses: HashMap<String, ContextToolUseSummary> = HashMap::new();
    let mut actions = Vec::new();
    for message in &history[start..] {
        match message.role.as_str() {
            "assistant" => {
                for block in &message.content {
                    if let Block::ToolUse { id, name, input } = block {
                        uses.insert(
                            id.clone(),
                            ContextToolUseSummary {
                                summary: summarize_call(name, input),
                                action_key: context_action_key(name, input),
                                strategy: context_strategy_for_tool(name, input),
                                mutates_worktree: context_action_mutates_worktree(name, input),
                            },
                        );
                    }
                }
            }
            "user" => {
                for block in &message.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = block
                        && let Some(call) = uses.get(tool_use_id)
                    {
                        let failed = is_error.unwrap_or(false)
                            || context_strategy_semantic_failure(call.strategy, content);
                        actions.push(ContextActionSummary {
                            summary: call.summary.clone(),
                            action_key: call.action_key.clone(),
                            strategy: call.strategy,
                            mutates_worktree: call.mutates_worktree,
                            ok: !failed,
                            detail: summarize_inline(content, 160),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    actions
}

fn context_strategy_budget_usage(
    actions: &[ContextActionSummary],
) -> BTreeMap<ContextStrategy, ContextStrategyBudget> {
    let mut budgets: BTreeMap<ContextStrategy, ContextStrategyBudget> = BTreeMap::new();
    for action in actions {
        if action.ok && action.mutates_worktree {
            budgets.remove(&ContextStrategy::GitStatus);
        }
        let Some(strategy) = action.strategy else {
            continue;
        };
        if strategy.counts_successes() || !action.ok {
            let budget = budgets.entry(strategy).or_default();
            budget.used = budget.used.saturating_add(1);
        }
    }
    budgets
}

fn context_strategy_budget_status(strategy: ContextStrategy, used: usize) -> &'static str {
    let limit = strategy.limit();
    if used >= limit {
        "PIVOT REQUIRED"
    } else if used > 0 && used + 1 >= limit {
        "WARNING"
    } else {
        "OK"
    }
}

fn context_repetition_pattern(actions: &[ContextActionSummary]) -> Option<String> {
    let mut best_key = "";
    let mut best_summary = "";
    let mut best_run = 0usize;
    let mut idx = 0usize;
    while idx < actions.len() {
        let action = &actions[idx];
        let mut end = idx + 1;
        while end < actions.len() && actions[end].action_key == action.action_key {
            end += 1;
        }
        let run = end - idx;
        if run >= 2 && run >= best_run {
            best_key = &action.action_key;
            best_summary = &action.summary;
            best_run = run;
        }
        idx = end;
    }
    if best_run >= 2 && !best_key.is_empty() {
        Some(format!(
            "→ PATTERN: same action repeated {best_run}x ({best_summary}). Stop repeating; trust the latest result or pivot."
        ))
    } else {
        None
    }
}

fn active_checkpoint_lines(ledger: &WorkLedger) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    for (status, items) in [
        ("in_progress", &ledger.in_progress),
        ("unresolved", &ledger.pending),
        ("blocked", &ledger.blocked),
    ] {
        for item in items {
            let item = item.trim();
            if item.is_empty() || !seen.insert(item.to_string()) {
                continue;
            }
            lines.push(format!("- [{status}] {}", summarize_inline(item, 180)));
            if lines.len() >= 6 {
                return lines;
            }
        }
    }
    lines
}

fn render_context_state_prompt(history: &[Message], ledger: &WorkLedger) -> String {
    let actions = collect_context_actions(history);
    let checkpoints = active_checkpoint_lines(ledger);
    if actions.is_empty() && checkpoints.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    if !actions.is_empty() {
        out.push_str("Recent actions (last 5):\n");
        let recent_start = actions.len().saturating_sub(5);
        let recent = &actions[recent_start..];
        for (idx, action) in recent.iter().enumerate() {
            let offset = recent.len() - idx;
            out.push_str(&format!(
                "- T-{offset}: {} · {} · {}\n",
                if action.ok { "ok" } else { "err" },
                action.summary,
                action.detail
            ));
        }
        if let Some(pattern) = context_repetition_pattern(recent) {
            out.push_str(&pattern);
            out.push('\n');
        }
    }

    let budgets = context_strategy_budget_usage(&actions);
    out.push_str("Strategy budget:\n");
    for strategy in [
        ContextStrategy::HttpUrlHunt,
        ContextStrategy::BinaryHunt,
        ContextStrategy::GitStatus,
    ] {
        let used = budgets
            .get(&strategy)
            .map(|budget| budget.used)
            .unwrap_or(0);
        out.push_str(&format!(
            "- {}: {}/{} used · {}\n",
            strategy.label(),
            used,
            strategy.limit(),
            context_strategy_budget_status(strategy, used)
        ));
    }

    if !checkpoints.is_empty() {
        out.push_str("Active checkpoints:\n");
        for line in checkpoints {
            out.push_str(&line);
            out.push('\n');
        }
    }

    let errors: Vec<&ContextActionSummary> = actions.iter().filter(|action| !action.ok).collect();
    if !errors.is_empty() {
        out.push_str("Last errors:\n");
        let start = errors.len().saturating_sub(3);
        for action in &errors[start..] {
            let label = action
                .strategy
                .map(ContextStrategy::label)
                .unwrap_or("tool");
            out.push_str(&format!(
                "- {label}: {} => {}\n",
                action.summary, action.detail
            ));
        }
    }
    out
}

fn compact_summary_chat_template_kwargs(
    provider_id: &str,
    api_provider: ApiProvider,
    base_url: &str,
) -> Option<OaiChatTemplateKwargs> {
    provider::is_local_llama_provider(provider_id, api_provider, base_url).then_some(
        OaiChatTemplateKwargs {
            enable_thinking: false,
        },
    )
}

fn summary_task_marker_pos(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let leading_ws = line.len().saturating_sub(trimmed.len());
    let stripped = trimmed.trim_start_matches('#').trim_start();
    let stripped = stripped.trim_matches('*').trim();
    let lower = stripped.to_ascii_lowercase();
    (lower == "task" || lower.starts_with("task:")).then_some(leading_ws)
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.trim().to_string();
    }
    s.chars()
        .skip(count.saturating_sub(max_chars))
        .collect::<String>()
        .trim()
        .to_string()
}

fn extract_summary_from_reasoning(reasoning: &str) -> String {
    let mut best_pos = None;
    let mut cursor = 0usize;
    for line in reasoning.split_inclusive('\n') {
        let line_no_newline = line.trim_end_matches(['\r', '\n']);
        if let Some(offset) = summary_task_marker_pos(line_no_newline) {
            best_pos = Some(cursor + offset);
        }
        cursor += line.len();
    }
    if cursor < reasoning.len() {
        let line = &reasoning[cursor..];
        if let Some(offset) = summary_task_marker_pos(line) {
            best_pos = Some(cursor + offset);
        }
    }
    best_pos
        .map(|pos| reasoning[pos..].trim().to_string())
        .unwrap_or_else(|| tail_chars(reasoning, 2_000))
}

fn openai_summary_text_from_response(json: &Value) -> Result<String> {
    let choice = json["choices"].as_array().and_then(|arr| arr.first());
    let mut text = choice
        .and_then(|c| c["message"]["content"].as_str())
        .unwrap_or_default()
        .to_string();
    let finish_reason = choice
        .and_then(|c| c["finish_reason"].as_str())
        .unwrap_or_default();
    if text.trim().is_empty()
        && finish_reason == "length"
        && let Some(reasoning) = choice.and_then(|c| c["message"]["reasoning_content"].as_str())
    {
        text = extract_summary_from_reasoning(reasoning);
    }
    if text.trim().is_empty() {
        anyhow::bail!("summary response had no text: {json}");
    }
    Ok(text)
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

fn parse_runtime_control_command(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    runtime_control_command_accepts(cmd).then_some((cmd, arg))
}

fn runtime_control_command_accepts(cmd: &str) -> bool {
    matches!(cmd, "model" | "effort" | "think" | "thinking")
}

pub(crate) fn parse_active_runtime_control_sequence(text: &str) -> Option<Vec<String>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut commands = Vec::new();
    for part in trimmed
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        parse_runtime_control_command(part)?;
        commands.push(part.to_string());
    }
    (!commands.is_empty()).then_some(commands)
}

fn is_active_runtime_control_command(text: &str) -> bool {
    parse_active_runtime_control_sequence(text).is_some()
}

fn is_slash_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

fn unsupported_busy_slash_message(text: &str) -> String {
    let cmd = text
        .trim_start()
        .strip_prefix('/')
        .and_then(|rest| rest.split_whitespace().next())
        .filter(|cmd| !cmd.is_empty())
        .unwrap_or("command");
    format!(
        "queued slash command /{cmd} not run while agent is busy; only /model and /effort (/think) are active runtime controls"
    )
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
    let detected_window = agent.refresh_context_window();
    agent.pin_model_for_provider(&target_provider, &target_model);
    let context_note = detected_window
        .map(|tokens| format!("; detected llama.cpp context {tokens} tokens"))
        .unwrap_or_default();
    match set_provider_default_model_in_catalog(&target_provider, &target_model) {
        Ok(()) if provider_changed => Ok(format!(
            "model -> {} (provider -> {}; saved as default; applies immediately to the next model request{context_note})",
            agent.model, agent.provider_id
        )),
        Ok(()) => Ok(format!(
            "model -> {} (saved as default for provider {}; applies immediately to the next model request{context_note})",
            agent.model, agent.provider_id
        )),
        Err(e) if provider_changed => Ok(format!(
            "model -> {} (provider -> {}; applies immediately; session-only model change; failed to persist default: {e:#}{context_note})",
            agent.model, agent.provider_id
        )),
        Err(e) => Ok(format!(
            "model -> {} (applies immediately; session-only; failed to persist default: {e:#}{context_note})",
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

    let Some((cmd, arg)) = parse_runtime_control_command(trimmed) else {
        return false;
    };
    if cmd == "model" {
        match apply_runtime_model_command(agent, arg) {
            Ok(msg) => emit(msg),
            Err(e) => emit(format!("[err] {e:#}")),
        }
        return true;
    }

    let effort_arg = matches!(cmd, "effort" | "think" | "thinking").then_some(arg);
    if let Some(arg) = effort_arg {
        match arg.to_ascii_lowercase().as_str() {
            "" | "status" => {
                let effort = agent.thinking_effort();
                emit(format!(
                    "thinking effort: {} (model reasoning depth/tool persistence)",
                    effort.as_str()
                ));
            }
            "next" | "+" => {
                let effort = agent.cycle_thinking_effort(1);
                emit(format!(
                    "thinking effort -> {} (applies immediately to the next model request in this run)",
                    effort.as_str()
                ));
            }
            "prev" | "previous" | "-" => {
                let effort = agent.cycle_thinking_effort(-1);
                emit(format!(
                    "thinking effort -> {} (applies immediately to the next model request in this run)",
                    effort.as_str()
                ));
            }
            _ => match ThinkingEffort::parse(arg) {
                Some(level) => {
                    let changed = agent.set_thinking_effort(level);
                    let effort = agent.thinking_effort();
                    if changed {
                        emit(format!(
                            "thinking effort -> {} (applies immediately to the next model request in this run)",
                            effort.as_str()
                        ));
                    } else {
                        emit(format!(
                            "thinking effort: {} (already active)",
                            effort.as_str()
                        ));
                    }
                }
                None => emit(
                    "usage: /effort [off|low|medium|high|xhigh|max|next|prev|status]".to_string(),
                ),
            },
        }
        return true;
    }

    false
}

#[derive(Debug, Default)]
struct AppliedRuntimeControls {
    commands: usize,
    changed_model: bool,
    changed_effort: bool,
    aborted_stream: bool,
}

fn finish_active_runtime_controls(
    agent: &mut Agent,
    messages: Vec<String>,
    abort_stream: bool,
) -> AppliedRuntimeControls {
    let mut applied = AppliedRuntimeControls::default();
    for message in messages {
        let before_model = (agent.provider_id.clone(), agent.model.clone());
        let before_effort = agent.thinking_effort();
        let mut notes = Vec::new();
        let handled = apply_runtime_control_command(agent, &message, |msg| notes.push(msg));
        for note in notes {
            agent.sink.emit(AgentEvent::RuntimeControl(note));
        }
        if handled {
            applied.commands += 1;
            if (agent.provider_id.as_str(), agent.model.as_str())
                != (before_model.0.as_str(), before_model.1.as_str())
            {
                applied.changed_model = true;
            }
            if agent.thinking_effort() != before_effort {
                applied.changed_effort = true;
            }
        } else {
            agent.sink.emit(AgentEvent::Warn(format!(
                "unsupported active runtime control: {}",
                summarize_inline(&message, 120)
            )));
        }
    }
    if applied.commands > 0 {
        agent.sink.emit(AgentEvent::ThinkingEffortChanged {
            effort: agent.thinking_effort(),
        });
        if applied.changed_model {
            agent.emit_runtime_provider_state();
        }
        if abort_stream && (applied.changed_model || applied.changed_effort) {
            applied.aborted_stream = true;
            agent.sink.emit(AgentEvent::Warn(
                "[runtime control] current provider stream stopped; continuing immediately with updated runtime"
                    .to_string(),
            ));
            agent.append_latest_log(
                "runtime_control_abort_stream",
                &format!(
                    "commands={} model_changed={} effort_changed={}",
                    applied.commands, applied.changed_model, applied.changed_effort
                ),
            );
        } else {
            agent.append_latest_log(
                "runtime_control_applied",
                &format!(
                    "commands={} model_changed={} effort_changed={}",
                    applied.commands, applied.changed_model, applied.changed_effort
                ),
            );
        }
        agent.sink.emit(AgentEvent::RuntimeControlApplied {
            commands: applied.commands,
            model_changed: applied.changed_model,
            effort_changed: applied.changed_effort,
            stream_aborted: applied.aborted_stream,
        });
        agent.checkpoint_latest_session("after_runtime_control");
    }
    applied
}

fn apply_queued_runtime_controls(agent: &mut Agent) -> AppliedRuntimeControls {
    let messages = agent.drain_runtime_controls();
    finish_active_runtime_controls(agent, messages, false)
}

async fn queued_runtime_control_waiter(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
) -> Option<String> {
    if let Some(rx) = rx {
        rx.recv().await
    } else {
        std::future::pending::<Option<String>>().await
    }
}

fn apply_runtime_control_for_stream(
    agent: &mut Agent,
    first: Option<String>,
) -> AppliedRuntimeControls {
    let mut messages = Vec::new();
    if let Some(first) = first {
        messages.push(first);
    }
    messages.extend(agent.drain_runtime_controls());
    if messages.is_empty() {
        AppliedRuntimeControls::default()
    } else {
        finish_active_runtime_controls(agent, messages, true)
    }
}

fn try_apply_runtime_controls_for_stream(agent: &mut Agent) -> AppliedRuntimeControls {
    apply_runtime_control_for_stream(agent, None)
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
        "Summarize the following transcript so a future assistant can resume the work.\n\nUse the exact section headings below and keep each section concise:\n- Task\n- Decisions\n- Files\n- Open work\n- Recent state\n\nPreserve concrete state over prose: latest user intent, active work ledger, verification results, file paths/line refs, provider/runtime errors, unresolved blockers, and cited tool/event IDs. If the transcript conflicts with the deterministic evidence packet, trust the evidence packet.\n{evidence_section}\n\n---\n{transcript}---"
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
    let ledger_text = render_work_ledger_prompt(ledger);
    if !ledger_text.trim().is_empty() {
        out.push_str("[ledger:active]\n");
        out.push_str(&ledger_text);
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
                Block::Thinking { .. } | Block::RedactedThinking { .. } => {}
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

// Last-resort context window when env, name hints, catalog, and family
// heuristics all miss. New model families should be added to the provider
// catalog (providers.json) rather than hardcoded here.
const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;

fn configured_context_window_override() -> Option<u64> {
    std::env::var("DEXT_CONTEXT_WINDOW")
        .or_else(|_| std::env::var("DEXT_CONTEXT_WINDOW_TOKENS"))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|tokens| *tokens > 0)
}

pub(crate) fn model_context_window(model: &str) -> u64 {
    if let Some(tokens) = configured_context_window_override() {
        return tokens;
    }

    if let Some(tokens) = provider_catalog_context_window(model, true) {
        return tokens;
    }
    if let Some(tokens) = builtin_profile_context_window(model, true) {
        return tokens;
    }
    if let Some(hint) = parse_model_context_hint_tokens(model) {
        return hint;
    }

    // Resolution order (first match wins):
    //   1. DEXT_CONTEXT_WINDOW / DEXT_CONTEXT_WINDOW_TOKENS env var (handled above).
    //   2. Explicit per-model provider-catalog metadata.
    //   3. Model-name suffix hint ("-128k", "-1m", etc).
    //   4. Provider-catalog default.
    //   5. Built-in family heuristics.
    //   6. Hard fallback (200_000).
    if let Some(tokens) = provider_catalog_context_window(model, false) {
        return tokens;
    }
    if let Some(tokens) = builtin_family_context_window(model) {
        return tokens;
    }
    if let Some(tokens) = builtin_profile_context_window(model, false) {
        return tokens;
    }
    DEFAULT_CONTEXT_WINDOW_TOKENS
}

fn model_context_window_for_profile(profile: Option<&ProviderProfile>, model: &str) -> u64 {
    if let Some(tokens) = configured_context_window_override() {
        return tokens;
    }
    if let Some((profile, normalized)) = profile.map(|profile| {
        let normalized = normalize_provider_model_value(profile, model).to_ascii_lowercase();
        (profile, normalized)
    }) {
        if let Some(tokens) = profile
            .model_specs
            .get(&normalized)
            .and_then(|spec| spec.context_window)
            .or_else(|| profile.model_context_windows.get(&normalized).copied())
            .filter(|tokens| *tokens > 0)
        {
            return tokens;
        }
        if let Some(tokens) = parse_model_context_hint_tokens(model) {
            return tokens;
        }
        if let Some(tokens) = profile
            .model_defaults
            .context_window
            .or(profile.context_window)
            .filter(|tokens| *tokens > 0)
        {
            return tokens;
        }
        return builtin_family_context_window(model).unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
    }
    model_context_window(model)
}

fn runtime_context_window_for_profile(
    profile: Option<&ProviderProfile>,
    model: &str,
    detected: Option<u64>,
) -> u64 {
    configured_context_window_override()
        .or_else(|| detected.filter(|tokens| *tokens > 0))
        .unwrap_or_else(|| model_context_window_for_profile(profile, model))
}

// Default per-request output token cap for streaming completions. Override with
// DEXT_MAX_OUTPUT_TOKENS. Kept provider-agnostic; oversized values are safely
// capped server-side by local llama.cpp, and the Anthropic path additionally
// clamps the thinking budget below this via clamp_thinking_budget_below_max.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

fn max_output_tokens_for(spec: Option<&ResolvedModelSpec>) -> u32 {
    if let Ok(raw) = std::env::var("DEXT_MAX_OUTPUT_TOKENS")
        && let Ok(v) = raw.trim().parse::<u32>()
        && v > 0
    {
        return v;
    }
    spec.and_then(|spec| spec.max_output_tokens)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

// True for ChatGPT/Codex "codex" implementation models (gpt-5.3-codex,
// gpt-5-codex, future *-codex variants). Substring match so new codex releases
// are covered without editing this gate.
fn model_is_codex_implementation_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("codex")
}

fn provider_catalog_context_window(model: &str, per_model_only: bool) -> Option<u64> {
    let normalized_model = model.trim().to_ascii_lowercase();
    let catalog = load_provider_catalog().ok()?;
    context_window_from_profiles(&catalog.providers, &normalized_model, per_model_only)
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
                .eq_ignore_ascii_case(normalized_model)
            || profile.model_specs.contains_key(normalized_model)
            || profile.model_context_windows.contains_key(normalized_model);
        if !profile_knows_model {
            continue;
        }
        if let Some(per_model) = profile
            .model_specs
            .get(normalized_model)
            .and_then(|spec| spec.context_window)
            .or_else(|| profile.model_context_windows.get(normalized_model).copied())
            .filter(|window| *window > 0)
        {
            return Some(per_model);
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

fn builtin_profile_context_window(model: &str, per_model_only: bool) -> Option<u64> {
    let normalized_model = model.trim().to_ascii_lowercase();
    let profiles = built_in_provider_profiles();
    context_window_from_profiles(&profiles, &normalized_model, per_model_only)
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
        return Some(DEFAULT_CONTEXT_WINDOW_TOKENS);
    }
    None
}

fn compact_threshold_chars_for_window(window_tokens: u64, percent: u8) -> usize {
    let window_tokens = usize::try_from(window_tokens).unwrap_or(usize::MAX / 4);
    window_tokens
        .saturating_mul(4)
        .saturating_mul(percent.clamp(1, 100) as usize)
        .saturating_div(100)
        .max(HISTORY_CHAR_BUDGET_MIN)
}

#[cfg(test)]
fn compact_threshold_chars_for_percent(model: &str, percent: u8) -> usize {
    compact_threshold_chars_for_window(model_context_window(model), percent)
}

pub(crate) fn history_char_budget_with_window(
    window_tokens: u64,
    override_chars: Option<usize>,
    context_mode: ContextMode,
    percent: u8,
) -> usize {
    if let Some(v) = override_chars.filter(|v| *v > 0) {
        return v;
    }
    if let Ok(raw) = std::env::var("DEXT_MAX_HISTORY_CHARS")
        && let Ok(v) = raw.trim().parse::<usize>()
        && v > 0
    {
        return v;
    }
    if context_mode.is_tiny() {
        let window_chars = usize::try_from(window_tokens)
            .unwrap_or(usize::MAX / 4)
            .saturating_mul(4);
        return window_chars
            .saturating_mul(FRUGAL_HISTORY_CHAR_BUDGET_PERCENT as usize)
            .saturating_div(100)
            .clamp(
                FRUGAL_HISTORY_CHAR_BUDGET_MIN,
                FRUGAL_HISTORY_CHAR_BUDGET_MAX,
            );
    }
    if context_mode.is_frugal() {
        return FRUGAL_HISTORY_CHAR_BUDGET;
    }

    compact_threshold_chars_for_window(window_tokens, percent)
}

#[cfg(test)]
fn history_char_budget_with_percent(
    model: &str,
    override_chars: Option<usize>,
    context_mode: ContextMode,
    percent: u8,
) -> usize {
    history_char_budget_with_window(
        model_context_window(model),
        override_chars,
        context_mode,
        percent,
    )
}

#[cfg(test)]
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

fn active_history_char_budget_with_window(
    window_tokens: u64,
    override_chars: Option<usize>,
    context_mode: ContextMode,
) -> usize {
    history_char_budget_with_window(
        window_tokens,
        override_chars,
        context_mode,
        HISTORY_CHAR_BUDGET_ACTIVE_PERCENT,
    )
}

#[cfg(test)]
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
    dext_state_dir().join("settings.json")
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
    provider_profile: Option<ProviderProfile>,
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
    // Usage of the most recent provider request. This is the context-pressure
    // signal for adaptive tool-result caps: session_usage is a running bill
    // across requests (it re-counts the prompt every round), so comparing it
    // against the context window would saturate within a few rounds.
    last_request_usage: Usage,
    interrupt: Arc<AtomicBool>,
    shelf_registry: shelves::ShelfRegistry,
    hooks: Hooks,
    pack_hook_env: Vec<(String, String)>,
    active_pack_hook_paths: HashSet<PathBuf>,
    suppress_pack_activation: bool,
    state_lock: Option<Arc<SessionStateLock>>,
    session_enabled: bool,
    session_id: String,
    latest_session_path: PathBuf,
    latest_log_path: PathBuf,
    pending_login_provider: Option<String>,
    // non-critical checkpoints skip rewriting an unchanged transcript.
    suppress_checkpoints: bool,
    last_checkpoint_at: Option<std::time::Instant>,
    session_model_pins: HashMap<String, String>,
    partial_stream_text: Option<String>,
    compact_threshold_chars: Option<usize>,
    compact_threshold_percent: Option<u8>,
    context_window_tokens: u64,
    approval_profile: ApprovalProfile,
    sandbox_profile: SandboxProfile,
    browser_recipe: BrowserRecipe,
    context_mode: ContextMode,
    context_mode_explicit: bool,
    tool_context_profile: ToolContextProfile,
    tool_profile: ToolProfile,
    preview_mode: MutationPreviewMode,
    budget_cap: Option<BudgetCap>,
    budget_exhausted: bool,
    // Caps concurrent builtin tool execution. Model may request 20 read_files at once; without
    // a cap, the runtime spawns 20 tasks that all contend for disk/CPU and open-file limits.
    // Default 8; override via DEXT_MAX_CONCURRENT_BUILTINS=N.
    builtin_semaphore: Arc<tokio::sync::Semaphore>,
    sink: Box<dyn EventSink>,
    runtime_control_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    runtime_control_tx: tokio::sync::mpsc::UnboundedSender<String>,
    steering_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    steering_tx: tokio::sync::mpsc::UnboundedSender<String>,
    read_cache: Arc<Mutex<ReadFileCache>>,
    work_ledger: WorkLedger,
    provider_health: ProviderHealthLedger,
    track_origin: Option<TrackOrigin>,
    privacy: PrivacyPolicy,
    // Session-scoped git HTTPS credential from the masked local prompt.
    // Never serialized, logged, or shown to the model.
    git_credential: Option<LocalGitCredential>,
    checkpoint_cache: git_checkpoints::RepoRootCache,
    checkpoint_ordinal: usize,
    prompt_scan_cache: Mutex<Option<PromptScanCache>>,
    prompt_scan_epoch: u64,
    // (history len, history chars) at the last session autosave; lets
    // non-critical checkpoints skip rewriting an unchanged transcript.
    last_checkpoint_signature: Option<(usize, usize)>,
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
        let api_provider = request_contract_for_profile(&resolved.profile).api_provider();
        let base_url = resolved.base_url;
        let model = resolved.model;
        let detected_context_window =
            refresh_local_llama_context_window(&provider_id, api_provider, &base_url, &model);
        let context_window_tokens = runtime_context_window_for_profile(
            Some(&resolved.profile),
            &model,
            detected_context_window,
        );

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

        let thinking_effort = std::env::var("DEXT_THINKING_EFFORT")
            .ok()
            .and_then(|v| ThinkingEffort::parse(&v))
            .unwrap_or_default();
        let configured_context_mode = std::env::var("DEXT_CONTEXT_MODE")
            .ok()
            .and_then(|value| ContextMode::parse(&value));
        let context_mode_explicit = configured_context_mode.is_some();
        let context_mode = configured_context_mode.unwrap_or_else(|| {
            default_context_mode_for_provider(&provider_id, api_provider, &base_url)
        });
        let base_system = std::env::var("DEXT_SYSTEM")
            .ok()
            .and_then(|v| {
                // Support `@path/to/file` to load system prompt from disk
                if let Some(path) = v.strip_prefix('@') {
                    std::fs::read_to_string(path).ok()
                } else {
                    Some(v)
                }
            })
            .unwrap_or_else(|| {
                if context_mode.is_tiny() {
                    TINY_SYSTEM.to_string()
                } else {
                    DEFAULT_SYSTEM.to_string()
                }
            });
        let system = match std::env::var("DEXT_SYSTEM_APPEND")
            .ok()
            .and_then(|v| load_prompt_env_value(v).ok())
        {
            Some(extra) if !extra.trim().is_empty() => format!("{base_system}\n\n{extra}"),
            _ => base_system,
        };
        let sandbox_root = std::fs::canonicalize(sandbox.unwrap_or_else(|| {
            PathBuf::from(std::env::var("DEXT_SANDBOX").unwrap_or_else(|_| ".".to_string()))
        }))
        .context("could not canonicalize sandbox root")?;
        let session_id = new_session_id();
        let latest_session = if session_enabled {
            session_latest_session_path(&sandbox_root, &session_id)
        } else {
            project_latest_session_path(&sandbox_root)
        };
        let latest_log = session_latest_log_path(&sandbox_root, &session_id);
        record_crash_session_id(&latest_session);
        let state_lock = if session_enabled {
            Some(Arc::new(SessionStateLock::acquire(
                &sandbox_root,
                &session_id,
            )?))
        } else {
            None
        };

        let pretty = io::stdout().is_terminal();
        let git_context = git_summary(&sandbox_root);
        let browser_recipe = std::env::var("DEXT_BROWSER_RECIPE")
            .ok()
            .and_then(|v| BrowserRecipe::parse(&v))
            .unwrap_or_default();
        let tool_profile = ToolProfile::from_env();
        let budget_cap = BudgetCap::from_env();
        let tool_context_profile = ToolContextProfile::from_env().effective(context_mode);
        let tools: Vec<Tool> = provider_tool_definitions()
            .into_iter()
            .filter(|t| browser_recipe == BrowserRecipe::AgentBrowser || t.name != "browser")
            .filter(|t| tool_name_allowed_in_profile(t.name, tool_context_profile))
            .collect();
        let compact_threshold_percent = load_compact_threshold_percent_setting();
        Ok(Self {
            client: Arc::new(OnceLock::new()),
            provider_id,
            provider_profile: Some(resolved.profile),
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
            allowed: HashSet::new(),
            deny_tools: HashSet::new(),
            shelf_registry: shelves::ShelfRegistry::discover(&sandbox_root),
            hooks: Hooks::load(&sandbox_root),
            pack_hook_env: Vec::new(),
            active_pack_hook_paths: HashSet::new(),
            suppress_pack_activation: false,
            sandbox_root,
            git_context,
            silent: false,
            pretty,
            max_iterations: None,
            session_usage: Usage::default(),
            last_request_usage: Usage::default(),
            interrupt: Arc::new(AtomicBool::new(false)),
            state_lock,
            session_enabled,
            session_id,
            latest_session_path: latest_session,
            latest_log_path: latest_log,
            pending_login_provider: None,
            suppress_checkpoints: false,
            last_checkpoint_at: None,
            session_model_pins: HashMap::new(),
            partial_stream_text: None,
            compact_threshold_chars: compact_threshold_percent
                .map(|percent| compact_threshold_chars_for_window(context_window_tokens, percent)),
            compact_threshold_percent,
            context_window_tokens,
            approval_profile: ApprovalProfile::default(),
            sandbox_profile: SandboxProfile::default(),
            browser_recipe,
            context_mode,
            context_mode_explicit,
            tool_context_profile,
            tool_profile,
            preview_mode: MutationPreviewMode::from_env(),
            budget_cap,
            budget_exhausted: false,
            builtin_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent_builtins())),
            sink: Box::new(ConsoleSink::new(pretty, false)),
            runtime_control_rx: None,
            runtime_control_tx: Self::noop_text_tx(),
            steering_rx: None,
            steering_tx: Self::noop_text_tx(),
            read_cache: Arc::new(Mutex::new(ReadFileCache::default())),
            work_ledger: WorkLedger::default(),
            provider_health: ProviderHealthLedger::default(),
            track_origin: None,
            privacy: PrivacyPolicy::from_env(),
            git_credential: None,
            checkpoint_cache: git_checkpoints::RepoRootCache::new(),
            checkpoint_ordinal: 0,
            prompt_scan_cache: Mutex::new(None),
            prompt_scan_epoch: 0,
            last_checkpoint_signature: None,
        })
    }

    fn noop_text_tx() -> tokio::sync::mpsc::UnboundedSender<String> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tx
    }

    fn prompt_for_git_credential(&mut self, hosts: &[String]) -> String {
        let hosts = dedupe_git_hosts(hosts.iter().cloned());
        if hosts.is_empty() {
            return "\n\n[dext] git asked for credentials, but Dext could not determine the \
                    HTTPS host to scope a local credential safely. Ask the user to configure a git \
                    credential helper or re-run with an explicit HTTPS remote URL; never ask for \
                    tokens in chat."
                .to_string();
        }
        let message = git_auth_guidance_for_hosts(&hosts);
        match self.sink.request_local_auth_secret("git", &message) {
            LocalAuthSecret::Secret(mut raw) => {
                let mut cred = parse_git_credential_input(&raw);
                clear_secret_string(&mut raw);
                cred.hosts = hosts;
                let host_label = cred.host_scope_label();
                self.git_credential = Some(cred);
                format!(
                    "\n\n[dext] git asked for credentials for {host_label}; the user entered them \
                     in a masked local prompt (they are never shown in chat or to you). They will \
                     be supplied automatically through a credential helper for matching HTTPS git \
                     remotes for the rest of this session — re-run the failed command now."
                )
            }
            LocalAuthSecret::Canceled => "\n\n[dext] git asked for credentials and the user \
                 dismissed the local prompt. Do not retry the command; ask the user how they \
                 want to authenticate."
                .to_string(),
            LocalAuthSecret::Unavailable => {
                self.sink.local_auth_prompt("git", &message);
                "\n\n[dext] git needs credentials but this frontend has no local secret \
                 prompt. Ask the user to configure a credential helper (for example `gh auth \
                 login` or `git config credential.helper`) and then re-run the command. Never \
                 ask for tokens in chat."
                    .to_string()
            }
        }
    }

    /// A bash tool call failed because git could not obtain HTTPS credentials.
    /// Collect them through the masked local prompt (never chat input) and
    /// return a note for the model describing what happened and whether a
    /// retry will now succeed. `ran_with_credential` is whether the failing
    /// call had the stored credential helper installed; only if the failure's
    /// host matches that credential does it mean the credential was rejected.
    fn handle_git_credential_failure(
        &mut self,
        ran_with_credential: bool,
        hosts: Vec<String>,
    ) -> String {
        let hosts = dedupe_git_hosts(hosts);
        if let Some(stored) = self.git_credential.as_ref() {
            let matches_stored = !hosts.is_empty() && stored.covers_any_host(&hosts);
            if ran_with_credential && matches_stored {
                let host_label = stored.host_scope_label();
                self.git_credential = None;
                return format!(
                    "\n\n[dext] The stored git credential for {host_label} was rejected by the \
                     remote and has been discarded. Re-running the command will prompt the user for \
                     a new one; if it keeps failing, ask the user to verify the token's scopes."
                );
            }
            if matches_stored {
                if ran_with_credential {
                    return format!(
                        "\n\n[dext] git asked for credentials for {}; the user has since provided \
                         them via the masked local prompt. Re-run the failed command now.",
                        stored.host_scope_label()
                    );
                }
                return format!(
                    "\n\n[dext] A stored git credential is available for {}, but Dext did not \
                     attach it to this compound or unsafe shell command. Re-run as a direct git \
                     fetch/pull/push/ls-remote command for that HTTPS remote.",
                    stored.host_scope_label()
                );
            }
        }
        self.prompt_for_git_credential(&hosts)
    }

    #[cfg(test)]
    fn noop_steering_tx() -> tokio::sync::mpsc::UnboundedSender<String> {
        Self::noop_text_tx()
    }

    fn runtime_control_sender(&self) -> tokio::sync::mpsc::UnboundedSender<String> {
        self.runtime_control_tx.clone()
    }

    fn steering_sender(&self) -> tokio::sync::mpsc::UnboundedSender<String> {
        self.steering_tx.clone()
    }

    fn install_runtime_controls(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        self.runtime_control_rx = Some(rx);
        self.runtime_control_tx = tx;
    }

    fn install_steering(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        self.steering_rx = Some(rx);
        self.steering_tx = tx;
    }

    fn drain_runtime_controls(&mut self) -> Vec<String> {
        let mut commands = Vec::new();
        if let Some(rx) = &mut self.runtime_control_rx {
            while let Ok(cmd) = rx.try_recv() {
                commands.push(cmd);
            }
        }
        commands
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
        self.provider_id = resolved.profile.id.clone();
        self.api_provider = request_contract_for_profile(&resolved.profile).api_provider();
        self.provider_profile = Some(resolved.profile);
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
        self.refresh_context_window();
        if !self.context_mode_explicit {
            let mode = default_context_mode_for_provider(
                &self.provider_id,
                self.route_api_provider(),
                &self.base_url,
            );
            self.set_context_mode_automatic(mode);
        }
    }

    fn request_contract(&self) -> RequestContract {
        self.provider_profile
            .as_ref()
            .map(request_contract_for_profile)
            .unwrap_or_else(|| RequestContract::for_api_provider(self.api_provider))
    }

    fn route_api_provider(&self) -> ApiProvider {
        self.request_contract().api_provider()
    }

    fn resolved_model_spec(&self) -> Option<ResolvedModelSpec> {
        self.provider_profile
            .as_ref()
            .map(|profile| resolve_model_spec(profile, &self.model))
    }

    fn request_max_output_tokens(&self) -> u32 {
        max_output_tokens_for(self.resolved_model_spec().as_ref())
    }

    fn effective_thinking_effort(&self) -> ThinkingEffort {
        if self
            .resolved_model_spec()
            .is_some_and(|spec| !spec.reasoning)
        {
            ThinkingEffort::Off
        } else {
            self.thinking_effort
        }
    }

    fn model_supports_tools(&self) -> bool {
        self.resolved_model_spec().is_none_or(|spec| spec.tools)
    }

    fn model_supports_prompt_cache(&self) -> bool {
        let contract = self.request_contract();
        if contract == RequestContract::AnthropicMessages
            && let Some(enabled) = prompt_cache_env_override()
        {
            return enabled;
        }
        self.resolved_model_spec().map_or_else(
            || {
                contract == RequestContract::ChatGptResponses
                    || (contract == RequestContract::AnthropicMessages
                        && anthropic_prompt_cache_supported(&self.provider_id, &self.model))
            },
            |spec| {
                if spec.source == "legacy" {
                    contract == RequestContract::ChatGptResponses
                        || (contract == RequestContract::AnthropicMessages
                            && anthropic_prompt_cache_supported(&self.provider_id, &self.model))
                } else {
                    spec.prompt_cache
                }
            },
        )
    }

    fn model_supports_reasoning(&self, model: &str) -> bool {
        self.provider_profile
            .as_ref()
            .is_none_or(|profile| resolve_model_spec(profile, model).reasoning)
    }

    fn model_supports_image_input(&self) -> bool {
        self.resolved_model_spec()
            .is_some_and(|spec| spec.image_input)
    }

    fn model_spec_source(&self) -> &'static str {
        self.resolved_model_spec()
            .map_or("legacy", |spec| spec.source)
    }

    fn provider_health_key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            canonical_provider_id(&self.provider_id),
            self.request_contract().as_str(),
            self.base_url
                .trim()
                .trim_end_matches('/')
                .to_ascii_lowercase(),
            self.model.trim().to_ascii_lowercase()
        )
    }

    pub(crate) fn context_window_tokens(&self) -> u64 {
        if self.context_window_tokens > 0 {
            self.context_window_tokens
        } else {
            model_context_window(&self.model)
        }
    }

    fn finalize_usage_metrics(&self, usage: &mut Usage) {
        let model_spec = self.resolved_model_spec();
        *usage = usage_with_current_pricing(
            *usage,
            &self.provider_id,
            self.route_api_provider(),
            &self.base_url,
            &self.model,
            model_spec.as_ref().and_then(|spec| spec.pricing.as_ref()),
        );
    }

    fn finalize_turn_usage_metrics(&self, usage: &mut Usage, blocks: &[Block]) {
        Self::fill_missing_usage_metrics(
            usage,
            self.estimated_context_tokens_from_history().max(1),
            blocks,
        );
        self.finalize_usage_metrics(usage);
    }

    fn fill_missing_usage_metrics(usage: &mut Usage, fallback_input_tokens: u64, blocks: &[Block]) {
        if usage.input == 0 && usage.cache_create == 0 && usage.cache_read == 0 {
            usage.input = fallback_input_tokens.max(1);
        }
        if usage.output == 0 {
            usage.output = blocks_approx_tokens(blocks);
        }
    }

    fn ensure_session_usage_cost(&mut self) {
        if self.session_usage.cost_usd.is_none() && self.session_usage.total_tokens() > 0 {
            self.session_usage.cost_usd = self.priced_session_usage().cost_usd;
        }
    }

    fn priced_session_usage(&self) -> Usage {
        let model_spec = self.resolved_model_spec();
        usage_with_current_pricing(
            self.session_usage,
            &self.provider_id,
            self.route_api_provider(),
            &self.base_url,
            &self.model,
            model_spec.as_ref().and_then(|spec| spec.pricing.as_ref()),
        )
    }

    fn refresh_context_window(&mut self) -> Option<u64> {
        let updated = refresh_local_llama_context_window(
            &self.provider_id,
            self.route_api_provider(),
            &self.base_url,
            &self.model,
        );
        self.context_window_tokens = runtime_context_window_for_profile(
            self.provider_profile.as_ref(),
            &self.model,
            updated,
        );
        if let Some(percent) = self.compact_threshold_percent {
            self.compact_threshold_chars = Some(compact_threshold_chars_for_window(
                self.context_window_tokens,
                percent,
            ));
        }
        updated
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
        self.refresh_tools_for_context();
        Ok(())
    }

    fn emit_runtime_provider_state(&mut self) {
        self.sink.emit(AgentEvent::TurnDiagnostics {
            provider: self.provider_id.clone(),
            api_family: api_family_label(self.request_contract()).to_string(),
            auth_source: self.key_source.clone(),
            model: self.model.clone(),
            context_window: Some(self.context_window_tokens()),
            last_retry_reason: None,
            workaround_fired: false,
            turn_duration_ms: None,
            context_mode: Some(self.context_mode),
            tool_profile: Some(format!(
                "{}:{}",
                self.tool_context_profile().as_str(),
                self.wire_tool_profile().as_str()
            )),
            compacted: None,
        });
    }

    fn provider_status_line(&self) -> String {
        format!(
            "provider={} contract={} api={} model={} spec={} tools={} reasoning={} image_input={} prompt_cache={} auth={} base={}",
            self.provider_id,
            self.request_contract().as_str(),
            self.route_api_provider().as_str(),
            self.model,
            self.model_spec_source(),
            self.model_supports_tools(),
            self.resolved_model_spec().is_none_or(|spec| spec.reasoning),
            self.model_supports_image_input(),
            self.model_supports_prompt_cache(),
            self.key_source,
            self.base_url
        )
    }

    fn api_family_label(&self) -> &'static str {
        api_family_label(self.request_contract())
    }

    fn set_pending_login_provider(&mut self, provider: Option<String>) {
        self.pending_login_provider = provider.clone();
        self.sink.emit(AgentEvent::LoginInputMode { provider });
    }

    fn clear_pending_login(&mut self) -> Option<String> {
        let provider = self.pending_login_provider.take();
        if provider.is_some() {
            self.sink
                .emit(AgentEvent::LoginInputMode { provider: None });
        }
        provider
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
            self.clear_pending_login();
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
        self.clear_pending_login();
        self.reload_provider(None, false)?;
        Ok(Some(format!(
            "{}\nactive -> {}",
            login.message,
            self.provider_status_line()
        )))
    }

    fn set_trust_mode(&mut self, on: bool) -> usize {
        let profile = if on {
            ApprovalProfile::Always
        } else {
            ApprovalProfile::Ask
        };
        self.set_approval_profile(profile)
    }

    fn set_approval_profile(&mut self, profile: ApprovalProfile) -> usize {
        self.approval_profile = profile;
        let privileged: Vec<String> = self
            .tools
            .iter()
            .filter(|t| needs_permission(t.name))
            .map(|t| t.name.to_string())
            .collect();
        let mut changed = 0usize;
        for tool in privileged {
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

    fn note_runtime_model_change(&mut self, model: &str) {
        self.model = model.to_string();
        let provider_id = self.provider_id.clone();
        self.pin_model_for_provider(&provider_id, model);
        self.refresh_context_window();
        self.refresh_tools_for_context();
    }

    fn apply_implementation_phase_model_mitigation(&mut self) -> Option<String> {
        if self.request_contract() != RequestContract::ChatGptResponses
            || !model_is_codex_implementation_model(&self.model)
            || self.thinking_effort != ThinkingEffort::XHigh
        {
            return None;
        }
        self.thinking_effort = ThinkingEffort::Medium;
        Some(format!(
            "runtime model mitigation: {} implementation phase uses medium effort to favor concrete tool calls over analysis narration; keep xhigh for review/debug phases.",
            self.model
        ))
    }

    fn maybe_fallback_implementation_model(&mut self) -> Option<String> {
        if self.request_contract() != RequestContract::ChatGptResponses
            || !model_is_codex_implementation_model(&self.model)
        {
            return None;
        }
        let target = self.implementation_fallback_model_target()?;
        if target.eq_ignore_ascii_case(&self.model) || model_is_codex_implementation_model(&target)
        {
            return None;
        }
        self.note_runtime_model_change(&target);
        Some(format!(
            "runtime model fallback: action contract is still unresolved after repeated no-mutation turns; switched model to {target} for the next request."
        ))
    }

    /// Non-codex model to escape to when a codex implementation model stalls.
    /// Resolution: DEXT_IMPL_FALLBACK_MODEL env override, else the provider's
    /// built-in default model, else its first advertised non-codex model.
    fn implementation_fallback_model_target(&self) -> Option<String> {
        if let Ok(raw) = std::env::var("DEXT_IMPL_FALLBACK_MODEL") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        let profile = built_in_provider_profiles()
            .into_iter()
            .find(|p| p.id == self.provider_id)?;
        if !model_is_codex_implementation_model(&profile.default_model) {
            return Some(profile.default_model);
        }
        profile
            .models
            .into_iter()
            .find(|m| !model_is_codex_implementation_model(m))
    }

    fn action_contract_violation_runtime_notes(
        &mut self,
        no_mutation_turns: u32,
        fallback_emitted: &mut bool,
    ) -> Vec<String> {
        let mut notes = Vec::new();
        if no_mutation_turns >= 2
            && !*fallback_emitted
            && let Some(note) = self.maybe_fallback_implementation_model()
        {
            *fallback_emitted = true;
            notes.push(note);
        }
        notes.push(action_contract_runtime_note(no_mutation_turns));
        notes
    }

    fn push_runtime_notes(
        &mut self,
        notes: Vec<String>,
        log_event: &str,
        checkpoint_label: &str,
    ) -> bool {
        if notes.is_empty() {
            return false;
        }
        for note in notes {
            self.sink.emit(AgentEvent::Warn(note.clone()));
            self.append_latest_log(log_event, &note);
            self.history.push(Message {
                role: "user".to_string(),
                content: vec![Block::Text {
                    text: format!("[runtime-note] {note}"),
                }],
            });
        }
        self.checkpoint_latest_session(checkpoint_label);
        true
    }

    fn update_work_ledger_from_objective(&mut self, objective: &orchestrator::ObjectiveTracker) {
        self.work_ledger.objective = objective.summary.clone();
        self.work_ledger.current_phase = "probe".to_string();
        for checkpoint in &objective.checkpoints {
            self.work_ledger.done.retain(|v| v != checkpoint);
        }
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
        self.work_ledger.next_actions.retain(|v| v != item);
    }

    fn sync_work_ledger_with_objective_coverage(
        &mut self,
        coverage: &orchestrator::ObjectiveCoverage,
    ) {
        for item in &coverage.unresolved {
            self.work_ledger.done.retain(|v| v != item);
        }
        for item in &coverage.satisfied {
            self.mark_work_done(item);
        }
        for item in &coverage.unresolved {
            if !self.work_ledger.pending.iter().any(|v| v == item) {
                self.work_ledger.pending.push(item.clone());
            }
            if !self.work_ledger.next_actions.iter().any(|v| v == item) {
                self.work_ledger.next_actions.push(item.clone());
            }
        }
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

    pub(crate) fn trust_mode_active(&self) -> bool {
        self.approval_profile == ApprovalProfile::Always
    }

    pub(crate) fn auto_approved_privileged_tool_count(&self) -> usize {
        self.tools
            .iter()
            .filter(|tool| {
                needs_permission(tool.name) && self.tool_auto_approved(tool.name, &Value::Null)
            })
            .count()
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
        history_char_budget_with_window(
            self.context_window_tokens(),
            self.compact_threshold_chars,
            self.context_mode,
            HISTORY_CHAR_BUDGET_END_TURN_PERCENT,
        )
    }

    fn active_compact_threshold_chars(&self) -> usize {
        active_history_char_budget_with_window(
            self.context_window_tokens(),
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
        self.compact_threshold_chars = Some(compact_threshold_chars_for_window(
            self.context_window_tokens(),
            percent,
        ));
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

    /// Pre-warm the TCP+TLS connection to the provider API by sending a
    /// lightweight request. The actual API call will reuse the warm connection.
    fn prewarm_connection(&self) {
        let client = self.client.get_or_init(reqwest::Client::new).clone();
        let url = provider_request_url(&self.base_url, self.request_contract());
        // Fire-and-forget HEAD request to warm TLS
        std::mem::drop(tokio::spawn(async move {
            let _ = client
                .head(&url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;
        }));
    }

    fn http_client(&self) -> &reqwest::Client {
        self.client.get_or_init(reqwest::Client::new)
    }

    async fn interrupt_aware_sleep(&mut self, secs: u64) -> AppliedRuntimeControls {
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(secs));
        tokio::pin!(sleep);
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(25));
        loop {
            if self.interrupt.load(Ordering::SeqCst) {
                return AppliedRuntimeControls::default();
            }
            tokio::select! {
                _ = &mut sleep => return AppliedRuntimeControls::default(),
                msg = queued_runtime_control_waiter(&mut self.runtime_control_rx) => {
                    return apply_runtime_control_for_stream(self, msg);
                }
                _ = ticker.tick() => {}
            }
        }
    }

    fn refresh_state_paths(&mut self) {
        if self.session_enabled {
            self.latest_session_path =
                session_latest_session_path(&self.sandbox_root, &self.session_id);
            self.latest_log_path = session_latest_log_path(&self.sandbox_root, &self.session_id);
        } else {
            self.latest_session_path = project_latest_session_path(&self.sandbox_root);
            self.latest_log_path = session_latest_log_path(&self.sandbox_root, &self.session_id);
        }
    }

    fn set_sandbox_root(&mut self, root: PathBuf) -> Result<()> {
        self.pack_hook_env.clear();
        self.active_pack_hook_paths.clear();
        self.suppress_pack_activation = false;
        let next_lock_path = session_state_lock_path(&root, &self.session_id);
        let same_session = self
            .state_lock
            .as_ref()
            .is_some_and(|lock| lock.path == next_lock_path);
        let next_lock = if self.session_enabled && !same_session {
            Some(Arc::new(SessionStateLock::acquire(
                &root,
                &self.session_id,
            )?))
        } else {
            None
        };

        self.sandbox_root = root;
        self.shelf_registry = shelves::ShelfRegistry::discover(&self.sandbox_root);
        self.hooks = Hooks::load(&self.sandbox_root);
        self.git_context = git_summary(&self.sandbox_root);
        self.checkpoint_cache = git_checkpoints::RepoRootCache::new();
        self.checkpoint_ordinal = 0;
        self.refresh_state_paths();
        record_crash_session_id(&self.latest_session_path);
        if let Some(lock) = next_lock {
            self.state_lock = Some(lock);
        }
        Ok(())
    }

    fn activate_pack_hooks(&mut self, pack: &packs::PackInfo) {
        let path = pack.path.display().to_string();
        let env_name = pack.env_var_name();
        self.pack_hook_env
            .retain(|(key, _)| key != &env_name && key != "DEXT_PACK_DIR");
        self.pack_hook_env
            .push(("DEXT_PACK_DIR".to_string(), path.clone()));
        self.pack_hook_env.push((env_name, path));
        if let Some(phooks) = &pack.phooks_path {
            let key = std::fs::canonicalize(phooks).unwrap_or_else(|_| phooks.clone());
            if self.active_pack_hook_paths.insert(key) {
                self.hooks.extend(Hooks::load_file(phooks));
            }
        }
    }

    async fn run_pack(&mut self, selector: &str, task: &str) -> Result<()> {
        let pack = packs::find_pack(&self.sandbox_root, selector)?;
        self.activate_pack_hooks(&pack);
        self.sink.emit(AgentEvent::Slash(format!(
            "▶ pack: {} · {}\nworkflow: {}",
            pack.name,
            if task.trim().is_empty() {
                "run"
            } else {
                task.trim()
            },
            pack.pack_md_path.display()
        )));
        let prompt = packs::pack_prompt(&pack, task)?;
        self.chat(prompt).await
    }

    fn browser_recipe_hint(&self) -> Option<String> {
        if self.browser_recipe != BrowserRecipe::AgentBrowser {
            return None;
        }
        if binary_on_path("agent-browser") {
            Some("browser recipe enabled: use the browser tool with args like ['skills','get','core','--full'] or ['open','https://example.com'] when useful.".to_string())
        } else {
            Some("browser recipe requested, but agent-browser is not on PATH; install it or disable with /browser off.".to_string())
        }
    }

    fn maybe_create_tool_checkpoint(&mut self, name: &str, input: &Value) {
        if !git_checkpoints::tool_needs_checkpoint(name, input) {
            return;
        }
        let Some(git_root) = self.checkpoint_cache.get(&self.sandbox_root).ok().flatten() else {
            return;
        };
        let paths_hint: Vec<String> = input["path"]
            .as_str()
            .map(|p| vec![p.to_string()])
            .unwrap_or_default();
        self.checkpoint_ordinal += 1;
        match git_checkpoints::create_checkpoint_in_repo(
            &self.sandbox_root,
            &git_root,
            name,
            &paths_hint,
            self.checkpoint_ordinal,
        ) {
            Ok(Some(cp)) => self.append_latest_log("checkpoint", &format!("created {}", cp.id)),
            Ok(None) => self.append_latest_log("checkpoint", "skipped unborn HEAD"),
            Err(e) => self.append_latest_log("checkpoint", &format!("warning: {e}")),
        }
    }

    fn compute_mutation_preview(&self, name: &str, input: &Value) -> Option<String> {
        let root = &self.sandbox_root;
        match name {
            "write_file" => {
                let path_str = input["path"].as_str()?;
                let content = input["content"].as_str()?;
                match mutation_preview::preview_write_file(root, path_str, content) {
                    Ok(p) => Some(format_preview(&p)),
                    Err(_) => None,
                }
            }
            "edit_file" => {
                let path_str = input["path"].as_str()?;
                let old = input["old_string"].as_str()?;
                let new = input["new_string"].as_str()?;
                match mutation_preview::preview_edit_file(root, path_str, old, new) {
                    Ok(p) => Some(format_preview(&p)),
                    Err(e) => Some(format!("preview error: {e}")),
                }
            }
            "multi_edit" => {
                let path_str = input["path"].as_str()?;
                let edits_arr = input["edits"].as_array()?;
                let edits: Vec<_> = edits_arr
                    .iter()
                    .filter_map(|e| {
                        Some(mutation_preview::MultiEdit {
                            old_string: e["old_string"].as_str()?.to_string(),
                            new_string: e["new_string"].as_str()?.to_string(),
                            replace_all: e["replace_all"].as_bool().unwrap_or(false),
                        })
                    })
                    .collect();
                match mutation_preview::preview_multi_edit(root, path_str, &edits) {
                    Ok(p) => Some(format_preview(&p)),
                    Err(e) => Some(format!("preview error: {e}")),
                }
            }
            _ => None,
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
                return Some("browser tool is disabled. Enable with /browser agent-browser or --browser agent-browser before using browser automation.".to_string());
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

    fn shelf_frame(&self) -> shelves::ShelfFrame {
        let mut frame = shelves::ShelfFrame::new(self.sandbox_root.clone());
        frame.session_id = Some(self.session_id.clone());
        frame
    }

    /// Prompt context contributed by the typed shelf signal→effect loop
    /// (Context abilities of shelves that opt in via a load-signal Hook).
    fn shelf_context_section(&self) -> Option<String> {
        self.shelf_registry
            .collect_context(&shelves::Signal::Load, &self.shelf_frame(), 1_200)
    }

    /// A shelf veto for a tool call, if any behavioral shelf opts into tool
    /// signals and returns a Block effect. No-op for manifest-only shelves.
    fn shelf_tool_denial(&self, name: &str, input: &Value) -> Option<String> {
        self.shelf_registry
            .tool_block_reason(&self.shelf_frame(), name, input)
    }

    fn budget_cap_denial(&mut self) -> Option<String> {
        self.ensure_session_usage_cost();
        let cap = self.budget_cap?;
        let priced = self.priced_session_usage();
        let msg = cap.exceeded(priced)?;
        self.budget_exhausted = true;
        Some(format!(
            "{msg}; refusing another model request. Raise/clear with /budget <cap|off> or restart with --budget. Current usage: {}",
            priced.line()
        ))
    }

    fn update_budget_state_after_usage(&mut self) -> Option<String> {
        let cap = self.budget_cap?;
        let priced = self.priced_session_usage();
        let msg = cap.exceeded(priced)?;
        if self.budget_exhausted {
            return None;
        }
        self.budget_exhausted = true;
        Some(format!(
            "{msg}; this turn will stop before another model request. Current usage: {}",
            priced.line()
        ))
    }

    #[cfg(test)]
    fn session_dir(&self) -> PathBuf {
        crate::session::session_state_dir(&self.sandbox_root, &self.session_id)
    }

    fn append_latest_log(&self, event: &str, detail: &str) {
        if !self.session_enabled {
            return;
        }
        let detail = self.privacy.redact_log_detail(detail);
        append_log_event(&self.latest_log_path, event, &detail);
    }

    /// Filesystem scans behind the stable system prompt, cached per user turn
    /// and revalidated with cheap stats between tool rounds. compose runs once
    /// per provider request; without the cache it would repeat the ancestor
    /// walks and pack-directory reads on every round of a turn.
    fn prompt_scans(&self) -> (PromptContextSections, PromptContextSections, Option<String>) {
        let Ok(mut guard) = self.prompt_scan_cache.lock() else {
            return (
                prompt_context_files(&self.sandbox_root, "DEXT.md"),
                prompt_context_files(&self.sandbox_root, "recall.md"),
                packs::pack_summary_for_prompt(&self.sandbox_root),
            );
        };
        if let Some(cache) = guard.as_ref()
            && cache.epoch == self.prompt_scan_epoch
            && prompt_context_scan_is_current(&cache.dext_md)
            && prompt_context_scan_is_current(&cache.recall)
        {
            return (
                cache.dext_md.sections.clone(),
                cache.recall.sections.clone(),
                cache.pack_summary.clone(),
            );
        }
        let dext_md = scan_prompt_context_files(&self.sandbox_root, "DEXT.md");
        let recall = scan_prompt_context_files(&self.sandbox_root, "recall.md");
        let pack_summary = packs::pack_summary_for_prompt(&self.sandbox_root);
        let result = (
            dext_md.sections.clone(),
            recall.sections.clone(),
            pack_summary.clone(),
        );
        *guard = Some(PromptScanCache {
            epoch: self.prompt_scan_epoch,
            dext_md,
            recall,
            pack_summary,
        });
        result
    }

    fn compose_system_details(&self) -> SystemParts {
        let mut stable = self.system.clone();
        if self.context_mode.is_frugal()
            && !self.context_mode.is_tiny()
            && !stable.contains(FRUGAL_TOOL_PROTOCOL_NOTE)
        {
            stable.push('\n');
            stable.push_str(FRUGAL_TOOL_PROTOCOL_NOTE);
        }
        let mut context_budget = if self.context_mode.is_tiny() {
            1_300
        } else if self.context_mode.is_frugal() {
            FRUGAL_PROJECT_CONTEXT_CAP
        } else {
            PROJECT_CONTEXT_CAP
        };
        let mut prompt_sources = Vec::new();
        let (dext_md_sections, recall_sections, cached_pack_summary) = self.prompt_scans();
        for (label, path, content) in &dext_md_sections {
            if context_budget == 0 {
                break;
            }
            prompt_sources.push(path.clone());
            let section = format!("\n\n## Project context (DEXT.md from {label})\n{}", content);
            if section.len() <= context_budget {
                stable.push_str(&section);
                context_budget -= section.len();
            } else {
                let remaining = cap_bytes_with_hint(
                    content.clone(),
                    context_budget.saturating_sub(60),
                    "DEXT.md truncated; keep only the most important project guidance here.",
                );
                stable.push_str(&format!(
                    "\n\n## Project context (DEXT.md from {label})\n{remaining}"
                ));
                context_budget = 0;
                break;
            }
        }

        for (label, path, content) in &recall_sections {
            if context_budget == 0 {
                break;
            }
            prompt_sources.push(path.clone());
            let section = format!("\n\n## Recall (recall.md from {label})\n{}", content);
            if section.len() <= context_budget {
                stable.push_str(&section);
                context_budget -= section.len();
            } else {
                let remaining = cap_bytes_with_hint(
                    content.clone(),
                    context_budget.saturating_sub(60),
                    "recall.md truncated; keep durable facts concise.",
                );
                stable.push_str(&format!(
                    "\n\n## Recall (recall.md from {label})\n{remaining}"
                ));
                break;
            }
        }

        let mut env = String::from("## Environment\n");
        if self.context_mode.is_tiny() {
            if let Some(git) = &self.git_context {
                env.push_str(&format!(
                    "cwd={} os={} git={} provider={} model={} effort={} context={} schemas={} approval={} sandbox={}\n",
                    self.sandbox_root.display(),
                    std::env::consts::OS,
                    git,
                    self.provider_id,
                    self.model,
                    self.thinking_effort.as_str(),
                    self.context_mode.as_str(),
                    self.wire_tool_profile().as_str(),
                    self.approval_profile.as_str(),
                    self.sandbox_profile.as_str()
                ));
            } else {
                env.push_str(&format!(
                    "cwd={} os={} provider={} model={} effort={} context={} schemas={} approval={} sandbox={}\n",
                    self.sandbox_root.display(),
                    std::env::consts::OS,
                    self.provider_id,
                    self.model,
                    self.thinking_effort.as_str(),
                    self.context_mode.as_str(),
                    self.wire_tool_profile().as_str(),
                    self.approval_profile.as_str(),
                    self.sandbox_profile.as_str()
                ));
            }
            env.push_str(&format!(
                "compact={} active={}\n",
                self.compact_threshold_chars(),
                self.active_compact_threshold_chars()
            ));
            let ledger = self.work_ledger_prompt();
            if !ledger.trim().is_empty() {
                env.push_str("\n## Work ledger\n");
                env.push_str(&cap_bytes_with_hint(
                    ledger,
                    600,
                    "work ledger trimmed for tiny context.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
            let context_state = self.context_state_prompt();
            if !context_state.trim().is_empty() {
                env.push_str("\n## Context State\n");
                env.push_str(&cap_bytes_with_hint(
                    context_state,
                    800,
                    "context state trimmed for tiny context.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
            env.push_str(&self.privacy.prompt_status_line());
            env.push('\n');
            return SystemParts {
                stable,
                env,
                prompt_sources,
            };
        }
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
            if let Some(todo) = read_session_todo_summary(&self.sandbox_root, &self.session_id, 3) {
                env.push_str("\n## Project todos\n");
                env.push_str(&cap_bytes_with_hint(
                    todo,
                    600,
                    "project todo summary trimmed for frugal context.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
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
            let context_state = self.context_state_prompt();
            if !context_state.trim().is_empty() {
                env.push_str("\n## Context State\n");
                env.push_str(&cap_bytes_with_hint(
                    context_state,
                    1_200,
                    "context state trimmed for frugal context.",
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
            if let Some(pack_summary) = cached_pack_summary.clone() {
                env.push_str("\n## Dext packs\n");
                env.push_str(&cap_bytes_with_hint(
                    pack_summary,
                    600,
                    "pack summary trimmed for frugal context.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
            if let Some(shelf_summary) = shelves::registry_summary_for_prompt(&self.shelf_registry)
            {
                env.push_str("\n## Dext shelves\n");
                env.push_str(&cap_bytes_with_hint(
                    shelf_summary,
                    700,
                    "shelf registry summary trimmed for frugal context.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
            if let Some(shelf_context) = self.shelf_context_section() {
                env.push_str("\n## Shelf context\n");
                env.push_str(&cap_bytes_with_hint(
                    shelf_context,
                    600,
                    "shelf context trimmed for frugal budget.",
                ));
                if !env.ends_with('\n') {
                    env.push('\n');
                }
            }
            env.push_str(&self.privacy.prompt_status_line());
            env.push('\n');
            return SystemParts {
                stable,
                env,
                prompt_sources,
            };
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
        if let Some(todo) = read_session_todo_summary(&self.sandbox_root, &self.session_id, 5) {
            env.push_str("\n## Project todos\n");
            env.push_str(&cap_bytes_with_hint(
                todo,
                900,
                "project todo summary trimmed for prompt budget.",
            ));
            if !env.ends_with('\n') {
                env.push('\n');
            }
        }
        let ledger = self.work_ledger_prompt();
        if !ledger.trim().is_empty() {
            env.push_str("\n## Work ledger\n");
            env.push_str(&cap_bytes_with_hint(
                ledger,
                2_000,
                "work ledger trimmed for prompt budget.",
            ));
            if !env.ends_with('\n') {
                env.push('\n');
            }
        }
        let context_state = self.context_state_prompt();
        if !context_state.trim().is_empty() {
            env.push_str("\n## Context State\n");
            env.push_str(&cap_bytes_with_hint(
                context_state,
                1_800,
                "context state trimmed for prompt budget.",
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
        if let Some(pack_summary) = cached_pack_summary {
            env.push_str("\n## Dext packs\n");
            env.push_str(&cap_bytes_with_hint(
                pack_summary,
                600,
                "pack summary trimmed for prompt budget.",
            ));
            if !env.ends_with('\n') {
                env.push('\n');
            }
        }
        if let Some(shelf_summary) = shelves::registry_summary_for_prompt(&self.shelf_registry) {
            env.push_str("\n## Dext shelves\n");
            env.push_str(&cap_bytes_with_hint(
                shelf_summary,
                700,
                "shelf registry summary trimmed for prompt budget.",
            ));
            if !env.ends_with('\n') {
                env.push('\n');
            }
        }
        if let Some(shelf_context) = self.shelf_context_section() {
            env.push_str("\n## Shelf context\n");
            env.push_str(&cap_bytes_with_hint(
                shelf_context,
                1_200,
                "shelf context trimmed for prompt budget.",
            ));
            if !env.ends_with('\n') {
                env.push('\n');
            }
        }
        env.push_str(&self.privacy.prompt_status_line());
        env.push('\n');
        SystemParts {
            stable,
            env,
            prompt_sources,
        }
    }

    fn compose_system_parts(&self) -> (String, String) {
        let parts = self.compose_system_details();
        (parts.stable, parts.env)
    }

    fn work_ledger_prompt(&self) -> String {
        render_work_ledger_prompt(&self.work_ledger)
    }

    fn context_state_prompt(&self) -> String {
        render_context_state_prompt(&self.history, &self.work_ledger)
    }

    fn provider_health_prompt(&self) -> String {
        render_provider_health_prompt(&self.provider_health)
    }

    fn cleaned_work_ledger(&self) -> WorkLedger {
        let mut ledger = self.work_ledger.clone();
        ledger.files_changed.retain(|path| {
            let p = Path::new(path);
            !p.is_absolute() && !path.starts_with(".dext/")
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
        if p.is_absolute() || path.starts_with(".dext/") {
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
        self.work_ledger
            .steering
            .retain(|item| !steering_item_acknowledged(item, &self.history));
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
        self.work_ledger
            .done
            .retain(|item| item != "respond to queued user update");
        if !self
            .work_ledger
            .pending
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
            if text_is_potential_local_secret(&message) {
                self.sink.emit(AgentEvent::Warn(
                    "queued input withheld: use the local auth prompt or run /login again when the agent is idle".to_string(),
                ));
                continue;
            }
            if is_slash_command(&message) {
                self.sink
                    .emit(AgentEvent::Warn(unsupported_busy_slash_message(&message)));
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
            "queued_update",
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

    fn begin_provider_turn(&mut self) {
        for state in self.provider_health.providers.values_mut() {
            state.retry_after = None;
            state.disabled_for_turn = false;
        }
    }

    fn record_provider_success(&mut self) {
        let health_key = self.provider_health_key();
        self.provider_health.providers.remove(&health_key);
    }

    fn record_provider_http_failure(
        &mut self,
        status: reqwest::StatusCode,
        text: &str,
        retry_after: Option<u64>,
    ) {
        let health_key = self.provider_health_key();
        let mode = api_family_label(self.request_contract()).to_string();
        let state = self
            .provider_health
            .providers
            .entry(health_key)
            .or_default();
        state.auth = if matches!(status.as_u16(), 401 | 403 | 407) {
            "failed".to_string()
        } else {
            "present".to_string()
        };
        state.last_error = Some(http_status_error(status, &summarize_inline(text, 220)));
        state.retry_after = retry_after;
        state.mode = Some(mode);
        let is_server_error = matches!(status.as_u16(), 500 | 502..=504 | 520..=525 | 527);
        if is_server_error {
            state.consecutive_server_errors = state.consecutive_server_errors.saturating_add(1);
        } else {
            state.consecutive_server_errors = 0;
        }
        state.disabled_for_turn = matches!(status.as_u16(), 401 | 403 | 407 | 429)
            || (is_server_error
                && state.consecutive_server_errors >= MAX_CONSECUTIVE_SERVER_ERRORS);
    }

    fn record_provider_stream_failure(&mut self, text: &str) {
        let health_key = self.provider_health_key();
        let mode = api_family_label(self.request_contract()).to_string();
        let state = self
            .provider_health
            .providers
            .entry(health_key)
            .or_default();
        state.auth = "present".to_string();
        state.last_error = Some(summarize_inline(text, 260));
        state.mode = Some(mode);
        state.disabled_for_turn = tool_policy::output_has_auth_failure_markers(text);
    }

    fn set_context_mode(&mut self, mode: ContextMode) {
        self.context_mode_explicit = true;
        self.set_context_mode_automatic(mode);
    }

    fn set_context_mode_automatic(&mut self, mode: ContextMode) {
        let switching_to_tiny = mode.is_tiny();
        self.context_mode = mode;
        self.tool_context_profile = self.tool_context_profile.effective(mode);
        if switching_to_tiny {
            if self.system == DEFAULT_SYSTEM {
                self.system = TINY_SYSTEM.to_string();
            }
        } else if self.system == TINY_SYSTEM {
            self.system = DEFAULT_SYSTEM.to_string();
        }
    }

    fn tool_context_profile(&self) -> ToolContextProfile {
        self.tool_context_profile.effective(self.context_mode)
    }

    fn refresh_tools_for_context(&mut self) {
        let profile = self.tool_context_profile();
        self.tools = provider_tool_definitions()
            .into_iter()
            .filter(|t| self.browser_recipe == BrowserRecipe::AgentBrowser || t.name != "browser")
            .filter(|t| tool_name_allowed_in_profile(t.name, profile))
            .collect();
        let exposed: HashSet<&str> = self.tools.iter().map(|t| t.name).collect();
        self.allowed.retain(|name| exposed.contains(name.as_str()));
        self.deny_tools
            .retain(|name| exposed.contains(name.as_str()));
    }

    fn wire_tool_profile(&self) -> ToolProfile {
        self.tool_profile
    }

    fn wire_tools(&self) -> Vec<WireTool> {
        if !self.model_supports_tools() {
            return Vec::new();
        }
        tools::wire_tools(&self.tools, self.wire_tool_profile())
    }

    fn wire_tools_oai(&self) -> Vec<OaiTool> {
        if !self.model_supports_tools() {
            return Vec::new();
        }
        tools::wire_tools_oai(&self.tools, self.wire_tool_profile())
    }

    fn tool_use_ids_in_messages(messages: &[Message]) -> HashSet<String> {
        let mut ids = HashSet::new();
        for m in messages {
            for b in &m.content {
                if let Block::ToolUse { id, .. } = b {
                    ids.insert(id.clone());
                }
            }
        }
        ids
    }

    fn provider_context_history(&self) -> &[Message] {
        if self.work_ledger.active_focus.is_none() {
            return &self.history;
        }
        let Some(start) = self.history.iter().rposition(|m| {
            m.role == "user"
                && m.content.iter().any(|b| match b {
                    Block::Text { text } | Block::PartialStream { text } => {
                        text.starts_with("[dext focus packet loaded]")
                    }
                    _ => false,
                })
        }) else {
            return &self.history;
        };
        &self.history[start..]
    }

    fn history_to_oai_messages(&self, system_text: &str) -> Vec<OaiMessage> {
        let history = self.provider_context_history();
        let valid_ids = Self::tool_use_ids_in_messages(history);
        let mut msgs = vec![OaiMessage {
            role: "system".to_string(),
            content: Some(system_text.to_string()),
            tool_calls: None,
            tool_call_id: None,
        }];
        for m in history {
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
                    // An assistant message with neither text nor tool calls
                    // (e.g. one that only carried thinking blocks, which this
                    // path never sends) would serialize as content:null and
                    // trip strict providers — skip it.
                    if content.is_none() && oai_tool_calls.is_none() {
                        continue;
                    }
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
        if !self.model_supports_tools() {
            return Vec::new();
        }
        tools::wire_tools_chatgpt(&self.tools, self.wire_tool_profile())
    }

    fn history_to_chatgpt_input(&self) -> Vec<Value> {
        let history = self.provider_context_history();
        let valid_ids = Self::tool_use_ids_in_messages(history);
        let mut items = Vec::new();
        let mut msg_counter = 0usize;

        for msg in history {
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
                        // Dext stores the server-provided call_id (starts with "call_") in
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
        let contract = self.request_contract();
        let effort = self.effective_thinking_effort();
        let max_output_tokens = self.request_max_output_tokens();
        let url = provider_request_url(&self.base_url, contract);
        match contract {
            RequestContract::ChatGptResponses => {
                // Instructions carry only the stable system text; volatile env
                // state rides as a transient trailing input item so the prefix
                // stays byte-stable for the Responses API's implicit caching.
                let mut input = self.history_to_chatgpt_input();
                append_runtime_env_chatgpt_item(&mut input, sys_env);
                let mut body = build_chatgpt_request(
                    &self.model,
                    effort,
                    sys_stable,
                    chatgpt_session_id,
                    input,
                    self.wire_tools_chatgpt(),
                );
                if !self.model_supports_prompt_cache()
                    && let Some(object) = body.as_object_mut()
                {
                    object.remove("prompt_cache_key");
                }
                let bytes = serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                Ok((url, bytes))
            }
            RequestContract::OpenAiChatCompletions => {
                // Same split for OpenAI-compatible providers (DeepSeek, local
                // llama.cpp, OpenAI): a changing system message would invalidate
                // their implicit prefix caches on every tool round.
                let mut oai_msgs = self.history_to_oai_messages(sys_stable);
                push_runtime_env_oai_message(&mut oai_msgs, sys_env);
                let oai_tools = self.wire_tools_oai();
                let reasoning_effort = openai_reasoning_effort(effort);
                let stream_options = (!provider::is_local_llama_provider(
                    &self.provider_id,
                    self.route_api_provider(),
                    &self.base_url,
                ))
                .then_some(OaiStreamOptions {
                    include_usage: true,
                });
                let tool_names: Vec<&str> =
                    oai_tools.iter().map(|t| t.function.name.as_str()).collect();
                let grammar = llama_tool_grammar_for(
                    &self.provider_id,
                    self.route_api_provider(),
                    &self.base_url,
                    &tool_names,
                    env_flag_default(LLAMA_TOOL_GRAMMAR_ENV, false),
                );
                let body = OaiRequest {
                    model: &self.model,
                    max_tokens: max_output_tokens,
                    messages: oai_msgs,
                    tools: oai_tools,
                    stream: true,
                    stream_options,
                    reasoning_effort,
                    grammar,
                    chat_template_kwargs: None,
                };
                let bytes = serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                Ok((url, bytes))
            }
            RequestContract::AnthropicMessages => {
                let max_tokens = max_output_tokens;
                let resolved_spec = self.resolved_model_spec();
                let configured_effort = resolved_spec
                    .as_ref()
                    .filter(|spec| !spec.effort_levels.is_empty())
                    .and_then(|spec| map_effort_to_provider_levels(&spec.effort_levels, effort))
                    .or_else(|| {
                        (resolved_spec
                            .as_ref()
                            .is_none_or(|spec| spec.source == "legacy"))
                        .then(|| {
                            provider_model_output_config_effort(
                                &self.provider_id,
                                &self.model,
                                effort,
                            )
                        })
                        .flatten()
                    });
                let (thinking, output_config) = if let Some(effort) = configured_effort {
                    (
                        Some(AnthropicThinking {
                            kind: "enabled",
                            budget_tokens: None,
                            display: None,
                        }),
                        Some(AnthropicOutputConfig { effort }),
                    )
                } else if uses_anthropic_adaptive_thinking(&self.provider_id, &self.model) {
                    let effort = anthropic_output_config_effort(&self.model, effort);
                    let always_adaptive = anthropic_model_is_always_adaptive(&self.model);
                    let thinking = effort.as_ref().map(|_| AnthropicThinking {
                        kind: "adaptive",
                        budget_tokens: None,
                        display: always_adaptive.then_some("omitted"),
                    });
                    (
                        thinking,
                        effort.map(|effort| AnthropicOutputConfig { effort }),
                    )
                } else {
                    (
                        anthropic_thinking_budget_tokens(effort)
                            .and_then(|budget_tokens| {
                                clamp_thinking_budget_below_max(budget_tokens, max_tokens)
                            })
                            .map(|budget_tokens| AnthropicThinking {
                                kind: "enabled",
                                budget_tokens: Some(budget_tokens),
                                display: None,
                            }),
                        None,
                    )
                };
                let history = self.provider_context_history();
                let messages = sanitize_anthropic_messages(history, thinking.is_some());
                let prompt_cache_enabled = self.model_supports_prompt_cache();
                let mut messages = anthropic_wire_messages(&messages, prompt_cache_enabled)?;
                append_runtime_env_block(&mut messages, sys_env);
                let system = system_blocks_with_cache_control(sys_blocks, prompt_cache_enabled);
                let tools = wire_tools_with_cache_control(wire_tools, prompt_cache_enabled);
                let body = Request {
                    model: &self.model,
                    max_tokens,
                    system: &system,
                    messages: &messages,
                    tools: &tools,
                    stream: true,
                    thinking,
                    output_config,
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
        match self.request_contract() {
            RequestContract::OpenAiChatCompletions => self.read_stream_oai(resp).await,
            RequestContract::ChatGptResponses => self.read_stream_chatgpt(resp).await,
            RequestContract::AnthropicMessages => self.read_stream(resp).await,
        }
    }

    fn session_header(&self) -> SessionHeader {
        self.session_header_with_origin(self.track_origin.clone())
    }

    fn session_header_with_origin(&self, track_origin: Option<TrackOrigin>) -> SessionHeader {
        let system_details = self.compose_system_details();
        let composed_system = format!("{}\n\n{}", system_details.stable, system_details.env);
        let provenance = self.session_provenance_from(&system_details, &composed_system);
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
            composed_system: Some(composed_system),
            allowed,
            exposed_tools,
            approval_required_tools,
            auto_approved_tools,
            sandbox: Some(self.sandbox_root.display().to_string()),
            usage: self.priced_session_usage(),
            thinking_effort: self.thinking_effort,
            compact_threshold_chars: self.compact_threshold_override(),
            compact_threshold_percent: self.compact_threshold_override_percent(),
            approval_profile: self.approval_profile,
            sandbox_profile: self.sandbox_profile,
            budget_cap: self.budget_cap,
            browser_recipe: self.browser_recipe,
            context_mode: self.context_mode,
            context_mode_explicit: self.context_mode_explicit,
            tool_context_profile: self.tool_context_profile(),
            tool_profile: self.tool_profile,
            provenance,
            work_ledger: self.cleaned_work_ledger(),
            provider_health: self.provider_health.clone(),
            track_origin,
            privacy: self.privacy.clone(),
        }
    }

    fn composed_system_prompt(&self) -> String {
        let (sys_stable, sys_env) = self.compose_system_parts();
        format!("{sys_stable}\n\n{sys_env}")
    }

    fn session_provenance_from(
        &self,
        details: &SystemParts,
        system_prompt: &str,
    ) -> SessionProvenance {
        let mut prompt_sources = Vec::new();
        let dext_md_root = self.sandbox_root.join("DEXT.md");
        let recall_root = self.sandbox_root.join("recall.md");
        let dext_md_hash = std::fs::read(&dext_md_root).ok().map(|bytes| {
            if !details
                .prompt_sources
                .iter()
                .any(|path| path == &dext_md_root)
            {
                prompt_sources.push(dext_md_root.display().to_string());
            }
            sha256_hex_bytes(&bytes)
        });
        let recall_hash = std::fs::read(&recall_root).ok().map(|bytes| {
            if !details
                .prompt_sources
                .iter()
                .any(|path| path == &recall_root)
            {
                prompt_sources.push(recall_root.display().to_string());
            }
            sha256_hex_bytes(&bytes)
        });
        prompt_sources.extend(
            details
                .prompt_sources
                .iter()
                .map(|path| path.display().to_string()),
        );
        SessionProvenance {
            dext_version: env!("CARGO_PKG_VERSION").to_string(),
            git: self.git_context.clone(),
            provider: self.provider_id.clone(),
            api_provider: self.route_api_provider(),
            model: self.model.clone(),
            thinking_effort: self.thinking_effort,
            approval_profile: self.approval_profile,
            sandbox_profile: self.sandbox_profile,
            system_prompt_hash: sha256_hex_str(system_prompt),
            dext_md_hash,
            recall_hash,
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
        // Transcripts carry whatever the user typed and every tool output;
        // treat them as secrets (0600) so a leaked credential in the
        // conversation is at least not world-readable.
        crate::session::atomic_write_secret(path, &data)?;
        Ok(())
    }

    pub(crate) fn export_session_html_to_path(&self, path: &Path) -> Result<()> {
        let header = self.session_header();
        let html = render_session_html(&header, &self.history, "current session");
        crate::session::atomic_write_secret(path, html.as_bytes())?;
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
            && !matches!(reason, "after_focus" | "after_focus_clear")
            && let Some(last) = self.last_checkpoint_at
            && last.elapsed() < checkpoint_debounce()
        {
            return;
        }
        // Skip rewriting the session file when the transcript hasn't changed
        // since the last save; the full-file rewrite is the costly part. Only
        // the high-frequency transcript-driven reasons are eligible — settings
        // checkpoints (runtime control, privacy, …) must write even with an
        // unchanged transcript because they persist header state.
        let signature = (self.history.len(), self.history_chars());
        let transcript_driven = matches!(
            reason,
            "after_assistant_message" | "after_tool_results" | "after_partial_stream_preserve"
        );
        if transcript_driven && self.last_checkpoint_signature == Some(signature) {
            return;
        }
        match self.save_latest_session() {
            Ok(path) => {
                self.last_checkpoint_at = Some(std::time::Instant::now());
                self.last_checkpoint_signature = Some(signature);
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
            context_mode_explicit,
            tool_context_profile,
            tool_profile,
            work_ledger,
            mut provider_health,
            track_origin,
            privacy,
            provenance,
            ..
        } = parse_session_header(header)?;

        self.model = model;
        self.refresh_context_window();
        self.system = system;
        self.allowed = allowed.into_iter().collect();
        self.session_usage = usage;
        self.thinking_effort = thinking_effort;
        self.compact_threshold_percent =
            compact_threshold_percent.filter(|v| (1..=100).contains(v));
        self.compact_threshold_chars = self
            .compact_threshold_percent
            .map(|percent| {
                compact_threshold_chars_for_window(self.context_window_tokens(), percent)
            })
            .or_else(|| compact_threshold_chars.filter(|v| *v > 0));
        self.approval_profile = approval_profile;
        self.sandbox_profile = sandbox_profile;
        self.budget_cap = budget_cap;
        self.budget_exhausted = false;
        self.work_ledger = work_ledger;
        normalize_provider_health_errors(&mut provider_health);
        self.provider_health = provider_health;
        self.track_origin = track_origin;
        self.privacy = privacy;
        self.context_mode_explicit = context_mode_explicit;
        let restored_context_mode = if context_mode_explicit {
            context_mode
        } else {
            default_context_mode_for_provider(
                &self.provider_id,
                self.route_api_provider(),
                &self.base_url,
            )
        };
        self.set_context_mode_automatic(restored_context_mode);
        self.tool_context_profile = tool_context_profile.effective(restored_context_mode);
        self.tool_profile = tool_profile;
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
        if provenance.api_provider == ApiProvider::ChatGpt {
            normalize_restored_chatgpt_reasoning(&mut hist);
        }
        self.history = hist;
        self.clear_pending_login();
        Ok(path.to_path_buf())
    }

    fn load_session(&mut self, selector: &str) -> Result<PathBuf> {
        let path = resolve_session_selector(&self.sandbox_root, selector)?;
        self.load_session_from_path(&path)
    }

    fn load_latest_session(&mut self) -> Result<PathBuf> {
        let path = latest_session_path(&self.sandbox_root);
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
            "[dext workaround] provider rejected structured tool_result blocks; flattened results:\n",
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
        self.provider_context_history()
            .iter()
            .map(message_approx_tokens)
            .sum()
    }

    fn history_chars(&self) -> usize {
        self.history
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        Block::Text { text } | Block::PartialStream { text } => text.len(),
                        // Thinking is stripped from prior turns at serialization
                        // time, so it must not count toward the compaction
                        // trigger — otherwise stored reasoning would force
                        // compaction of context that is never actually sent.
                        Block::Thinking { .. } | Block::RedactedThinking { .. } => 0,
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
                    Block::Thinking { text, .. } => text.len(),
                    Block::RedactedThinking { data } => data.len(),
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

    /// Model used for the one-shot compaction summary. DEXT_COMPACT_MODEL
    /// points it at a cheaper slug on the same provider (e.g. claude-haiku-4-5,
    /// glm-4.6) so compaction doesn't pay flagship rates for a terse digest.
    fn compact_summary_model(&self) -> String {
        std::env::var("DEXT_COMPACT_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.model.clone())
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
                    Block::Thinking { .. } | Block::RedactedThinking { .. } => None,
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
        let transcript = render_transcript_for_summary(old, self.context_mode);
        let user_text = compaction_user_text_with_evidence(&transcript, evidence);
        let summary_model = self.compact_summary_model();
        let summary_reasoning_supported = self.model_supports_reasoning(&summary_model);
        let summary_max_tokens = compact_summary_max_tokens(self.thinking_effort);

        #[derive(PartialEq, Eq)]
        enum SummaryParse {
            Anthropic,
            OpenAi,
            ChatGptSse,
        }

        let request_contract = self.request_contract();
        let is_chatgpt_summary = request_contract == RequestContract::ChatGptResponses;
        let (mut resp, parse_mode): (reqwest::Response, SummaryParse) = if is_chatgpt_summary {
            let body = build_chatgpt_summary_request(
                &summary_model,
                COMPACT_SYSTEM,
                &user_text,
                summary_reasoning_supported,
            );
            let url = provider_request_url(&self.base_url, self.request_contract());
            let bytes = serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
            let req = apply_provider_headers(
                self.http_client()
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream")
                    .body(bytes),
                self.request_contract(),
                &self.api_key,
                None,
            )?;
            (req.send().await?, SummaryParse::ChatGptSse)
        } else if request_contract == RequestContract::OpenAiChatCompletions {
            let reasoning_effort = None;
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
                model: &summary_model,
                max_tokens: summary_max_tokens,
                messages,
                tools: Vec::new(),
                stream: false,
                stream_options: None,
                reasoning_effort,
                grammar: None,
                chat_template_kwargs: compact_summary_chat_template_kwargs(
                    &self.provider_id,
                    self.route_api_provider(),
                    &self.base_url,
                ),
            };
            let mut req = self
                .http_client()
                .post(provider_request_url(
                    &self.base_url,
                    self.request_contract(),
                ))
                .header("content-type", "application/json")
                .json(&body);
            if !self.api_key.trim().is_empty() {
                req = req.header("authorization", format!("Bearer {}", self.api_key));
            }
            (req.send().await?, SummaryParse::OpenAi)
        } else {
            let messages = vec![json!({
                "role": "user",
                "content": [{"type": "text", "text": user_text.clone()}],
            })];
            let sys_blocks = [SystemBlock {
                kind: "text",
                text: COMPACT_SYSTEM,
                cache_control: None,
            }];
            let body = Request {
                model: &summary_model,
                max_tokens: summary_max_tokens,
                system: &sys_blocks,
                messages: &messages,
                tools: &[],
                stream: false,
                thinking: None,
                output_config: None,
            };
            let mut req = self
                .http_client()
                .post(provider_request_url(
                    &self.base_url,
                    self.request_contract(),
                ))
                .header("anthropic-version", ANTHROPIC_API_VERSION)
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
            anyhow::bail!("summary {}", http_status_error(status, &text));
        }

        if parse_mode == SummaryParse::ChatGptSse {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match self.read_stream_chatgpt(resp).await {
                    Ok((blocks, _finish_reason, mut usage)) => {
                        let fallback_input =
                            ((user_text.len() as u64).saturating_add(3) / 4).max(1);
                        Self::fill_missing_usage_metrics(&mut usage, fallback_input, &blocks);
                        self.finalize_usage_metrics(&mut usage);
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
                            let _ = self.interrupt_aware_sleep(wait).await;
                            let body = build_chatgpt_summary_request(
                                &summary_model,
                                COMPACT_SYSTEM,
                                &user_text,
                                summary_reasoning_supported,
                            );
                            let url = provider_request_url(&self.base_url, self.request_contract());
                            let bytes =
                                serde_json::to_vec(&body).map_err(|e| anyhow::anyhow!(e))?;
                            let req = apply_provider_headers(
                                self.http_client()
                                    .post(&url)
                                    .header("content-type", "application/json")
                                    .header("accept", "text/event-stream")
                                    .body(bytes),
                                self.request_contract(),
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
                                anyhow::bail!("summary {}", http_status_error(retry_status, &text));
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
                let text = openai_summary_text_from_response(&json)?;
                let mut usage = Usage::parse_openai(&json["usage"]);
                self.finalize_usage_metrics(&mut usage);
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
                let mut usage = Usage::parse(&json["usage"]);
                self.finalize_usage_metrics(&mut usage);
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
        self.ensure_session_usage_cost();

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
        let plan = self.generate_read_only_plan(&task).await?;
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

    async fn generate_read_only_plan(&mut self, task: &str) -> Result<String> {
        let saved_system = std::mem::replace(&mut self.system, PLAN_SYSTEM.to_string());
        let saved_tools = std::mem::replace(
            &mut self.tools,
            provider_tool_definitions()
                .into_iter()
                .filter(|tool| READ_ONLY_TOOLS.contains(&tool.name))
                .collect(),
        );
        let saved_max_iterations = self.max_iterations.replace(15);
        let saved_history = std::mem::take(&mut self.history);
        let saved_silent = self.silent;
        let saved_pretty = self.pretty;
        let saved_sink = std::mem::replace(&mut self.sink, Box::new(SilentSink));
        let saved_suppress_checkpoints = self.suppress_checkpoints;
        let saved_hooks = self.hooks.clone();
        let saved_pack_hook_env = self.pack_hook_env.clone();
        let saved_active_pack_hook_paths = self.active_pack_hook_paths.clone();
        let saved_suppress_pack_activation = self.suppress_pack_activation;
        let saved_work_ledger = self.work_ledger.clone();
        let saved_budget_exhausted = self.budget_exhausted;
        self.silent = true;
        self.pretty = false;
        self.suppress_checkpoints = true;
        self.suppress_pack_activation = true;
        self.hooks = Hooks::default();
        self.pack_hook_env.clear();
        self.active_pack_hook_paths.clear();
        self.budget_exhausted = false;

        let prompt = format!("Produce a read-only implementation plan for this task:\n\n{task}");
        let chat_result = self.chat(prompt).await;

        let plan = self
            .history
            .iter()
            .rev()
            .find_map(|message| {
                if message.role != "assistant" {
                    return None;
                }
                let text: String = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }
            })
            .unwrap_or_else(|| "(planner returned no text)".to_string());

        self.history = saved_history;
        self.system = saved_system;
        self.tools = saved_tools;
        self.max_iterations = saved_max_iterations;
        self.silent = saved_silent;
        self.pretty = saved_pretty;
        self.suppress_checkpoints = saved_suppress_checkpoints;
        self.hooks = saved_hooks;
        self.pack_hook_env = saved_pack_hook_env;
        self.active_pack_hook_paths = saved_active_pack_hook_paths;
        self.suppress_pack_activation = saved_suppress_pack_activation;
        self.work_ledger = saved_work_ledger;
        self.budget_exhausted = saved_budget_exhausted;
        self.sink = saved_sink;

        chat_result?;
        Ok(plan)
    }

    async fn chat(&mut self, user_input: String) -> Result<()> {
        self.interrupt.store(false, Ordering::SeqCst);
        self.begin_provider_turn();
        self.sink.emit(AgentEvent::TurnStart);
        self.append_latest_log("chat_start", &format!("chars={}", user_input.len()));
        let result = self.chat_inner(user_input).await;
        if result.is_err() {
            let interrupted = self.interrupt.load(Ordering::SeqCst);
            if interrupted {
                self.sink.emit(AgentEvent::Interrupted);
            }
            self.sink.emit(AgentEvent::TurnEnd {
                usage: Usage::default(),
                failed: !interrupted,
            });
        }
        result
    }

    async fn chat_inner(&mut self, mut user_input: String) -> Result<()> {
        let mut compacted_this_turn = false;
        // New user turn: force a fresh prompt filesystem scan (DEXT.md/recall.md
        // walks, pack discovery) on the first request of the turn.
        self.prompt_scan_epoch = self.prompt_scan_epoch.wrapping_add(1);
        self.git_context = git_summary(&self.sandbox_root);
        if !self.suppress_pack_activation
            && let Some(invocation) = packs::infer_pack_invocation(&self.sandbox_root, &user_input)
        {
            if pack_auto_invocation_disabled_by_env(&invocation.pack) {
                self.sink.emit(AgentEvent::Info(format!(
                    "[pack:{}] auto-invocation disabled by DEXT_NO_PACK",
                    invocation.pack.name
                )));
            } else {
                self.activate_pack_hooks(&invocation.pack);
                self.sink.emit(AgentEvent::Info(format!(
                    "[pack:{}] inferred conversational invocation",
                    invocation.pack.name
                )));
                user_input = packs::pack_prompt(&invocation.pack, &invocation.task)?;
            }
        }
        let hook_env = [("DEXT_USER_INPUT", user_input.as_str())];
        for (out, _code) in self.hooks.fire(
            "user_prompt",
            "",
            &hook_env,
            &self.pack_hook_env,
            &self.sandbox_root,
        ) {
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
        let mut action_contract_must_mutate = false;
        let mut action_contract_no_mutation_turns: u32 = 0;
        let mut implementation_fallback_emitted = false;
        let mut last_retry_reason: Option<String> = None;
        let mut workaround_fired_this_turn = false;

        self.set_work_phase(turn_state.phase().label());
        self.sink.emit(AgentEvent::Info(format!(
            "[phase:{}] validate one representative source item before scaling",
            turn_state.phase().label()
        )));
        if objective.apply_fixes_allowed()
            && let Some((_, msg)) = turn_state.advance_phase(orchestrator::PhaseTrigger::Fix)
        {
            self.set_work_phase(turn_state.phase().label());
            self.sink.emit(AgentEvent::Info(format!(
                "[phase:{}] {msg}",
                turn_state.phase().label()
            )));
        }
        if objective.apply_fixes_allowed()
            && let Some(note) = self.apply_implementation_phase_model_mitigation()
        {
            self.sink.emit(AgentEvent::Warn(note.clone()));
            self.append_latest_log("model_mitigation", &note);
        }

        if self.provider_requires_api_key && self.api_key.trim().is_empty() {
            anyhow::bail!(
                "missing credentials for provider '{}'. run `/login {}` in REPL or `dext auth login {}`.",
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
            let runtime_controls = apply_queued_runtime_controls(self);
            if runtime_controls.aborted_stream {
                continue;
            }

            let chatgpt_session_id = (self.request_contract() == RequestContract::ChatGptResponses)
                .then(|| {
                    format!(
                        "dext-{}-{}",
                        self.provider_id,
                        project_key(&self.sandbox_root)
                    )
                });
            self.partial_stream_text = None;
            let (sys_stable, sys_env) = self.compose_system_parts();
            // Only the stable text lives in the system prompt (with a cache
            // breakpoint); the volatile env section is appended per request at
            // the tail of the message list so it never invalidates the cached
            // tools → system → history prefix.
            let sys_blocks = vec![SystemBlock {
                kind: "text",
                text: &sys_stable,
                cache_control: Some(CacheControl::for_prompt()),
            }];
            let mut stream_attempt: u32 = 0;
            let (blocks, stop_reason, mut usage) = 'stream_retry: loop {
                let wire_tools = self.wire_tools();
                let (url, req_body) = self.build_streaming_request(
                    &sys_stable,
                    &sys_env,
                    &sys_blocks,
                    &wire_tools,
                    chatgpt_session_id.as_deref().unwrap_or("dext"),
                )?;
                stream_attempt += 1;
                let mut attempt: u32 = 0;
                let mut provider_workaround_used = false;
                let resp = loop {
                    attempt += 1;
                    let mut builder = self
                        .http_client()
                        .post(&url)
                        .header("content-type", "application/json")
                        .header("accept", "text/event-stream");
                    if self.request_contract() == RequestContract::AnthropicMessages
                        && extended_prompt_cache_ttl().is_some()
                        && self.model_supports_prompt_cache()
                    {
                        builder = builder.header("anthropic-beta", "extended-cache-ttl-2025-04-11");
                    }
                    let req = apply_provider_headers(
                        builder.body(req_body.clone()),
                        self.request_contract(),
                        &self.api_key,
                        chatgpt_session_id.as_deref(),
                    )?;
                    let applied = try_apply_runtime_controls_for_stream(self);
                    if applied.aborted_stream {
                        break 'stream_retry (
                            Vec::new(),
                            Some("runtime_control".to_string()),
                            Usage::default(),
                        );
                    }
                    let mut interrupt_ticker =
                        tokio::time::interval(std::time::Duration::from_millis(25));
                    let send = req.send();
                    tokio::pin!(send);
                    let res = loop {
                        tokio::select! {
                            _ = async {
                                loop {
                                    if self.interrupt.load(Ordering::SeqCst) {
                                        break;
                                    }
                                    interrupt_ticker.tick().await;
                                }
                            } => anyhow::bail!("interrupted by user before provider response"),
                            msg = queued_runtime_control_waiter(&mut self.runtime_control_rx) => {
                                let runtime_control = apply_runtime_control_for_stream(self, msg);
                                if runtime_control.aborted_stream {
                                    break 'stream_retry (
                                        Vec::new(),
                                        Some("runtime_control".to_string()),
                                        Usage::default(),
                                    );
                                }
                                continue;
                            },
                            res = &mut send => break res,
                        }
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

                            if !plan.retry || attempt >= MAX_HTTP_ATTEMPTS {
                                let error = http_status_error(status, &text);
                                self.append_latest_log(
                                    "http_error",
                                    &format!("kind={} {error}", plan.label()),
                                );
                                anyhow::bail!(error);
                            }
                            let wait = retry_after
                                .unwrap_or_else(|| jittered_backoff_secs(1u64 << (attempt - 1)));
                            self.append_latest_log(
                                "http_retry",
                                &format!(
                                    "attempt={attempt} kind={} status={} wait={wait}s",
                                    plan.label(),
                                    http_status_label(status)
                                ),
                            );
                            let retry_reason =
                                format!("{} HTTP {}", plan.label(), http_status_label(status));
                            last_retry_reason = Some(retry_reason.clone());
                            self.sink.emit(AgentEvent::HttpRetry {
                                attempt,
                                wait_secs: wait,
                                reason: retry_reason,
                            });
                            turn_state.record_http_retry();
                            emit_external_telemetry(self.sink.as_mut(), &turn_state);
                            let runtime_controls = self.interrupt_aware_sleep(wait).await;
                            if runtime_controls.aborted_stream {
                                break 'stream_retry (
                                    Vec::new(),
                                    Some("runtime_control".to_string()),
                                    Usage::default(),
                                );
                            }
                        }
                        Err(e) => {
                            let plan = orchestrator::classify_transport_failure(
                                e.is_connect(),
                                e.is_timeout(),
                            );
                            if plan.retry && attempt < MAX_HTTP_ATTEMPTS {
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
                                let runtime_controls = self.interrupt_aware_sleep(wait).await;
                                if runtime_controls.aborted_stream {
                                    break 'stream_retry (
                                        Vec::new(),
                                        Some("runtime_control".to_string()),
                                        Usage::default(),
                                    );
                                }
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

                let runtime_control = try_apply_runtime_controls_for_stream(self);
                if runtime_control.aborted_stream {
                    break 'stream_retry (
                        Vec::new(),
                        Some("runtime_control".to_string()),
                        Usage::default(),
                    );
                }
                match self.parse_stream_response(resp).await {
                    Ok(result) => {
                        self.record_provider_success();
                        break 'stream_retry result;
                    }
                    Err(e)
                        if stream_error_body(&e)
                            .contains("runtime control changed active stream") =>
                    {
                        break 'stream_retry (
                            Vec::new(),
                            Some("runtime_control".to_string()),
                            Usage::default(),
                        );
                    }
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
                            let runtime_controls = self.interrupt_aware_sleep(wait).await;
                            if runtime_controls.aborted_stream {
                                break 'stream_retry (
                                    Vec::new(),
                                    Some("runtime_control".to_string()),
                                    Usage::default(),
                                );
                            }
                            continue 'stream_retry;
                        }
                        if self.request_contract() == RequestContract::ChatGptResponses {
                            let partial_blocks = self.partial_chatgpt_stream_blocks();
                            if maybe_preserve_partial_stream(
                                &partial_blocks,
                                &mut self.history,
                                self.context_mode,
                            ) {
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
                        if plan.retry {
                            return Err(e.context(format!(
                                "{} stream error after {stream_attempt} attempts; provider/upstream kept returning retryable SSE errors",
                                plan.label()
                            )));
                        }
                        return Err(e);
                    }
                }
            };

            if stop_reason.as_deref() == Some("runtime_control") {
                self.partial_stream_text = None;
                continue;
            }

            self.finalize_turn_usage_metrics(&mut usage, &blocks);

            self.last_request_usage = usage;
            self.session_usage.add(usage);
            turn_usage.add(usage);
            self.ensure_session_usage_cost();
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

            let assistant_response_text = assistant_blocks_text(&blocks);
            let response_has_pseudo_tool =
                blocks_contain_pseudo_tool_syntax_for_context(&blocks, self.context_mode);
            if objective.apply_fixes_allowed()
                && assistant_text_has_implementation_commitment(&assistant_response_text)
            {
                action_contract_must_mutate = true;
            }

            if maybe_preserve_partial_stream(&blocks, &mut self.history, self.context_mode) {
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
                        content: assistant_blocks_for_context(&blocks, self.context_mode),
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

            let empty_call_count = tool_calls
                .iter()
                .filter(|(_, _, input)| {
                    input.as_object().is_some_and(|m| m.is_empty()) || input.is_null()
                })
                .count();
            turn_state.record_empty_tool_calls(empty_call_count);
            let empty_tool_call_loop_note = turn_state.empty_tool_call_loop_note();

            if tool_calls.is_empty() {
                let coverage = objective.assess_history(&self.history);
                self.sync_work_ledger_with_objective_coverage(&coverage);
                if objective.apply_fixes_allowed()
                    && objective_warning_emitted
                    && coverage
                        .unresolved
                        .iter()
                        .any(|item| item == "implement requested changes")
                    && !orchestrator::assistant_text_has_blocked_reason(&assistant_response_text)
                {
                    action_contract_must_mutate = true;
                }
                if response_has_pseudo_tool {
                    let note = pseudo_tool_runtime_note();
                    self.sink.emit(AgentEvent::Warn(note.clone()));
                    self.append_latest_log("pseudo_tool_text", &note);
                    self.history.push(Message {
                        role: "user".to_string(),
                        content: vec![Block::Text {
                            text: format!("[runtime-note] {note}"),
                        }],
                    });
                    self.checkpoint_latest_session("after_pseudo_tool_warning");
                    continue;
                }
                if action_contract_must_mutate {
                    action_contract_no_mutation_turns =
                        action_contract_no_mutation_turns.saturating_add(1);
                    let notes = self.action_contract_violation_runtime_notes(
                        action_contract_no_mutation_turns,
                        &mut implementation_fallback_emitted,
                    );
                    if self.push_runtime_notes(
                        notes,
                        "action_contract_violation",
                        "after_action_contract_warning",
                    ) {
                        continue;
                    }
                }
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
                let coverage = objective.assess_history(&self.history);
                self.sync_work_ledger_with_objective_coverage(&coverage);
                if !objective_warning_emitted && !coverage.unresolved.is_empty() {
                    let reminder =
                        orchestrator::objective_runtime_reminder_from_coverage(&coverage);
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
                local_sudo_auth_needed: bool,
                cache_key: Option<String>,
                bash_similarity_key: Option<String>,
                plan: Plan,
            }

            let mut plans: Vec<PlannedCall> = Vec::new();
            for (ordinal, (id, name, input)) in tool_calls.into_iter().enumerate() {
                let event_call_id = normalize_tool_call_id(&id, 0, ordinal);
                let input_str = input.to_string();
                let summary = summarize_call(&name, &input);
                let call_sig = format!("{name}\n{input_str}");
                let hosts = tool_policy::hosts_for_tool_call(&name, &input);
                let bulk_network = tool_policy::looks_like_bulk_network_call(&name, &input);
                let cache_key = orchestrator::network_cache_key(&name, &input);
                let bash_similarity_key = if name == "bash" {
                    Some(orchestrator::normalize_bash_similarity_key(
                        input["command"].as_str().unwrap_or(""),
                    ))
                } else if matches!(name.as_str(), "write_file" | "edit_file") {
                    input["path"].as_str().map(|p| format!("{name}:{p}"))
                } else {
                    None
                };

                let mut plan: Option<Plan> = None;
                let mut local_sudo_auth_needed = false;

                if let Err(msg) = tool_policy::validate_tool_input(&name, &input) {
                    if let Some(budget_msg) = turn_state.tool_retry_guard(&name, &msg) {
                        emit_external_telemetry(self.sink.as_mut(), &turn_state);
                        plan = Some(Plan::Immediate {
                            content: budget_msg,
                            is_error: Some(true),
                        });
                    } else {
                        plan = Some(Plan::Immediate {
                            content: msg,
                            is_error: Some(true),
                        });
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
                    && let Some(reason) = self.shelf_tool_denial(&name, &input)
                {
                    plan = Some(Plan::Immediate {
                        content: format!("shelf policy blocked {name}: {reason}"),
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

                if plan.is_none() {
                    let approved = if self.deny_tools.contains(&name)
                        || denied_signatures.contains(&call_sig)
                        || (needs_permission(&name)
                            && self.approval_profile == ApprovalProfile::Never)
                    {
                        false
                    } else if needs_permission(&name) && !self.tool_auto_approved(&name, &input) {
                        // Show mutation preview for direct file tools before asking permission
                        if self.preview_mode != MutationPreviewMode::Off
                            && matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit")
                            && let Some(preview) = self.compute_mutation_preview(&name, &input)
                        {
                            self.sink.emit(AgentEvent::Info(preview));
                        }
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
                            ("DEXT_TOOL_NAME", name.as_str()),
                            ("DEXT_TOOL_INPUT", input_str.as_str()),
                        ];
                        let mut blocked: Option<String> = None;
                        for (out, code) in self.hooks.fire(
                            "pre_tool",
                            &name,
                            &pre_env,
                            &self.pack_hook_env,
                            &self.sandbox_root,
                        ) {
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
                            None => {
                                local_sudo_auth_needed = name == "bash"
                                    && tool_policy::command_invokes_sudo(
                                        input["command"].as_str().unwrap_or(""),
                                    );
                                Plan::Builtin
                            }
                        });
                    }
                }

                let plan = plan.expect("plan must be set");

                if matches!(plan, Plan::Builtin)
                    && matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit")
                {
                    self.work_ledger_note_file_change(&input);
                }

                // Create recovery checkpoint before write-risk mutations.
                if matches!(plan, Plan::Builtin) {
                    self.maybe_create_tool_checkpoint(&name, &input);
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
                        turn_state.advance_phase(if objective.apply_fixes_allowed() {
                            orchestrator::PhaseTrigger::Fix
                        } else {
                            orchestrator::PhaseTrigger::DeliverableWrite
                        })
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
                    local_sudo_auth_needed,
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
            // Calls that executed while a git credential was installed: only
            // their auth failures mean the credential itself was rejected.
            let mut builtin_git_cred_used: HashSet<usize> = HashSet::new();
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
                        let session_id = self.session_id.clone();
                        let sandbox_profile = self.sandbox_profile;
                        let pack_env = self.pack_hook_env.clone();
                        let git_credential = stored_git_credential_for_bash_call(
                            &n,
                            &inp,
                            self.git_credential.as_ref(),
                        );
                        if git_credential.is_some() {
                            builtin_git_cred_used.insert(*idx);
                        }
                        let handle = tokio::spawn(async move {
                            let _permit = match sem.acquire_owned().await {
                                Ok(p) => p,
                                Err(e) => return Err(format!("builtin semaphore closed: {e}")),
                            };
                            execute_builtin_call(
                                n,
                                inp,
                                root,
                                interrupt,
                                Some(read_cache),
                                Some(session_id),
                                None,
                                git_credential,
                                sandbox_profile,
                                None,
                                pack_env,
                            )
                            .await
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
                    let session_id = self.session_id.clone();
                    let local_sudo_auth_needed = plans[idx].local_sudo_auth_needed;
                    self.sink.emit(AgentEvent::ToolCallStart {
                        call_id: plans[idx].event_call_id.clone(),
                        name: n.clone(),
                        summary: summary.clone(),
                    });
                    self.append_latest_log("tool_start", &summary);
                    builtin_started_at.insert(idx, std::time::Instant::now());
                    let mut local_sudo_auth = if local_sudo_auth_needed {
                        match prepare_local_sudo_auth(&root, &session_id).await {
                            Ok(auth) => auth,
                            Err(e) => {
                                builtin_outputs.insert(idx, Err(e));
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    if let Some(auth) = local_sudo_auth.as_mut()
                        && auth.preauth_required
                    {
                        let message = SUDO_AUTH_GUIDANCE.to_string();
                        match self.sink.request_local_auth_secret("bash", &message) {
                            LocalAuthSecret::Secret(password) => {
                                match create_sudo_password_fifo(&root, &session_id) {
                                    Ok(fifo) => {
                                        auth.password_fifo = Some(fifo);
                                        auth.password = Some(password);
                                    }
                                    Err(e) => {
                                        let mut password = password;
                                        clear_secret_string(&mut password);
                                        builtin_outputs.insert(idx, Err(e));
                                        continue;
                                    }
                                }
                            }
                            LocalAuthSecret::Canceled => {
                                builtin_outputs.insert(
                                    idx,
                                    Err("sudo authentication canceled by user".to_string()),
                                );
                                continue;
                            }
                            LocalAuthSecret::Unavailable => {
                                self.sink.local_auth_prompt("bash", SUDO_AUTH_GUIDANCE);
                            }
                        }
                    }
                    let live_output = live_output_for_tool(
                        self.sink.as_ref(),
                        &plans[idx].event_call_id,
                        &n,
                        &inp,
                    );
                    let git_credential_for_call =
                        stored_git_credential_for_bash_call(&n, &inp, self.git_credential.as_ref());
                    if git_credential_for_call.is_some() {
                        builtin_git_cred_used.insert(idx);
                    }
                    let r = execute_builtin_call(
                        n,
                        inp,
                        root,
                        self.interrupt.clone(),
                        Some(read_cache.clone()),
                        Some(session_id),
                        local_sudo_auth,
                        git_credential_for_call,
                        self.sandbox_profile,
                        live_output,
                        self.pack_hook_env.clone(),
                    )
                    .await;
                    builtin_outputs.insert(idx, r);
                }
            }

            let mut batch_failed = 0usize;
            let mut batch_call_ids: Vec<String> = Vec::new();
            let mut batch_labels: Vec<String> = Vec::new();
            let mut results = Vec::new();
            let mut round_external_failures: usize = 0;
            let mut mutation_succeeded = false;
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
                    local_sudo_auth_needed: _local_sudo_auth_needed,
                    cache_key,
                    bash_similarity_key,
                    plan,
                } = p;

                let started_at = builtin_started_at.remove(&idx);
                let ran_builtin = matches!(plan, Plan::Builtin);
                let ui_summary = summary.clone();
                let mut followup_warnings: Vec<String> = Vec::new();
                let mut provider_runtime_notes: Vec<String> = Vec::new();
                if let Some(advisory) = tool_policy::tool_input_advisory(&name, &input) {
                    followup_warnings.push(advisory.clone());
                    provider_runtime_notes.push(advisory);
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

                if name == "bash" && output_indicates_git_credential_failure(&content) {
                    let ran_with_credential = builtin_git_cred_used.contains(&idx);
                    let failure_hosts = git_credential_hosts_for_failure(&hosts, &content);
                    content.push_str(
                        &self.handle_git_credential_failure(ran_with_credential, failure_hosts),
                    );
                }

                let post_env = [
                    ("DEXT_TOOL_NAME", name.as_str()),
                    ("DEXT_TOOL_INPUT", input_str.as_str()),
                    ("DEXT_TOOL_RESULT", content.as_str()),
                ];
                for (out, _code) in self.hooks.fire(
                    "post_tool",
                    &name,
                    &post_env,
                    &self.pack_hook_env,
                    &self.sandbox_root,
                ) {
                    let t = out.trim();
                    if !t.is_empty() {
                        content.push_str(&format!("\n\n[hook:post_tool]\n{t}"));
                    }
                }

                let ok = !is_error.unwrap_or(false);
                if ok
                    && ran_builtin
                    && (ACTION_CONTRACT_MUTATING_TOOL_NAMES.contains(&name.as_str())
                        || name == "bash"
                            && input["command"]
                                .as_str()
                                .is_some_and(orchestrator::bash_command_likely_mutates_files))
                {
                    mutation_succeeded = true;
                    turn_state.mark_mutation_succeeded();
                }
                let privacy_redaction = self.privacy.apply_tool_output(&name, &input, content);
                let redacted_count = privacy_redaction.counts.total();
                content = privacy_redaction.text;
                if redacted_count > 0 {
                    followup_warnings.push(format!(
                        "privacy redacted {} from {name} output before model context",
                        privacy_redaction.counts.summary()
                    ));
                }
                let observation =
                    turn_state.record_external_outcome(orchestrator::ExternalOutcomeInput {
                        tool_name: &name,
                        hosts: &hosts,
                        cache_key: cache_key.as_deref(),
                        bash_similarity_key: bash_similarity_key.as_deref(),
                        command: input["command"].as_str(),
                        content: &mut content,
                        is_error,
                    });
                emit_external_telemetry(self.sink.as_mut(), &turn_state);
                round_external_failures =
                    round_external_failures.saturating_add(observation.round_external_failures);
                followup_warnings.extend(observation.followup_warnings);
                insert_runtime_notes(&mut content, &provider_runtime_notes);

                if runnable_set.contains(&idx) {
                    if !ok {
                        batch_failed = batch_failed.saturating_add(1);
                    }
                    batch_call_ids.push(event_call_id.clone());
                    batch_labels.push(ui_summary.clone());
                }

                let ui_cap = orchestrator::adaptive_tool_ui_cap_for_window(
                    &self.last_request_usage,
                    self.context_window_tokens(),
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
                        &self.session_id,
                        VerificationArtifactSpec {
                            name: &ui_summary,
                            command: &command,
                            output: &content,
                            exit_code,
                            duration,
                            status,
                        },
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
                let dynamic_result_cap = tool_result_context_cap_with_window(
                    &name,
                    &input,
                    &self.last_request_usage,
                    &self.model,
                    Some(self.context_window_tokens()),
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
                        cap_bytes_head_tail_with_hint(content, dynamic_result_cap, &result_hint)
                    } else if matches!(name.as_str(), "bash" | "awk" | "csvkit") {
                        cap_bytes_head_tail_with_hint(
                            content,
                            dynamic_result_cap,
                            TOOL_OUTPUT_NARROW_HINT,
                        )
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

            let squashed_results = squash_identical_error_result_content(results);
            self.history.push(Message {
                role: "user".to_string(),
                content: squashed_results,
            });
            self.checkpoint_latest_session("after_tool_results");

            if let Some(halt) = turn_state.empty_tool_call_halt_message() {
                self.sink.emit(AgentEvent::Warn(halt.clone()));
                self.append_latest_log("empty_tool_call_halt", &halt);
                self.history.push(Message {
                    role: "assistant".to_string(),
                    content: vec![Block::Text { text: halt }],
                });
                self.checkpoint_latest_session("after_empty_tool_call_halt");
                break;
            }

            if let Some(note) = empty_tool_call_loop_note {
                self.sink.emit(AgentEvent::Warn(note.clone()));
                self.append_latest_log("empty_tool_call_loop", &note);
                self.history.push(Message {
                    role: "user".to_string(),
                    content: vec![Block::Text {
                        text: format!("[runtime-note] {note}"),
                    }],
                });
                self.checkpoint_latest_session("after_empty_tool_call_hint");
                continue;
            }

            if mutation_succeeded {
                action_contract_must_mutate = false;
                action_contract_no_mutation_turns = 0;
            } else if action_contract_must_mutate {
                action_contract_no_mutation_turns =
                    action_contract_no_mutation_turns.saturating_add(1);
                let notes = self.action_contract_violation_runtime_notes(
                    action_contract_no_mutation_turns,
                    &mut implementation_fallback_emitted,
                );
                if self.push_runtime_notes(
                    notes,
                    "action_contract_violation",
                    "after_action_contract_warning",
                ) {
                    continue;
                }
            }

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
        // Queued user updates were surfaced as a [queued-user-update] history
        // message this turn. Retire the ledger summaries now so they appear at
        // most once and never echo into later turns' runtime status block.
        self.work_ledger.steering.clear();
        let coverage = objective.assess_history(&self.history);
        self.sync_work_ledger_with_objective_coverage(&coverage);
        if !coverage.unresolved.is_empty() && objective_warning_emitted {
            let reminder = final_objective_warning_from_coverage(&coverage);
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
            api_family: api_family_label(self.request_contract()).to_string(),
            auth_source: self.key_source.clone(),
            model: self.model.clone(),
            context_window: Some(self.context_window_tokens()),
            last_retry_reason,
            workaround_fired: workaround_fired_this_turn,
            turn_duration_ms: Some(millis_u64(turn_started_at.elapsed())),
            context_mode: Some(self.context_mode),
            tool_profile: Some(format!(
                "{}:{}",
                self.tool_context_profile().as_str(),
                self.wire_tool_profile().as_str()
            )),
            compacted: Some(compacted_this_turn),
        });
        self.sink.emit(AgentEvent::TurnEnd {
            usage: turn_usage,
            failed: false,
        });
        Ok(())
    }

    async fn read_stream_next_chunk(
        &mut self,
        stream: &mut (
                 impl futures_util::Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>>
                 + Unpin
             ),
        interrupted_msg: &str,
    ) -> Result<Option<bytes::Bytes>> {
        use futures_util::StreamExt;

        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(25));
        loop {
            if self.interrupt.load(Ordering::SeqCst) {
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
                msg = queued_runtime_control_waiter(&mut self.runtime_control_rx) => {
                    let applied = apply_runtime_control_for_stream(self, msg);
                    if applied.aborted_stream {
                        anyhow::bail!("runtime control changed active stream");
                    }
                }
                _ = ticker.tick() => {}
            }
        }
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

        while let Some(chunk) = self
            .read_stream_next_chunk(&mut stream, "interrupted by user")
            .await?
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
                        } else if kind == "thinking" {
                            if let Some(t) = cb["thinking"].as_str() {
                                pb.text.push_str(t);
                            }
                            if let Some(sig) = cb["signature"].as_str() {
                                pb.thinking_signature = Some(sig.to_string());
                            }
                        } else if kind == "redacted_thinking"
                            && let Some(data) = cb["data"].as_str()
                        {
                            pb.redacted_data.push_str(data);
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
                                        pb.thinking_signature = Some(sig.to_string());
                                    }
                                }
                                "redacted_thinking_delta" | "data_delta" => {
                                    if let Some(data) = delta["data"].as_str() {
                                        pb.redacted_data.push_str(data);
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
                                let privileged =
                                    needs_permission(&pb.name) && !self.allowed.contains(&pb.name);
                                if !privileged {
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
                        if let Some(u) = data.get("usage") {
                            let parsed = Usage::parse(u);
                            if parsed.output > 0 {
                                usage.output = parsed.output;
                            }
                            if parsed.input > 0 || parsed.cache_create > 0 || parsed.cache_read > 0
                            {
                                usage.input = parsed.input;
                                usage.cache_create = parsed.cache_create;
                                usage.cache_read = parsed.cache_read;
                            }
                            if parsed.cost_usd.is_some() {
                                usage.cost_usd = parsed.cost_usd;
                            }
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

        while let Some(chunk) = self
            .read_stream_next_chunk(&mut stream, "interrupted by user")
            .await?
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

                if let Some(timings_usage) =
                    data.get("timings").and_then(Usage::parse_openai_timings)
                {
                    usage = timings_usage;
                } else if let Some(u) = data.get("usage") {
                    let parsed = Usage::parse_openai(u);
                    let keep_local_timings = provider::is_local_llama_provider(
                        &self.provider_id,
                        self.route_api_provider(),
                        &self.base_url,
                    ) && usage.cache_read > 0
                        && parsed.cache_read == 0;
                    if !keep_local_timings {
                        usage = parsed;
                    }
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
        let mut reasoning_emitted = String::new();
        let mut usage = Usage::default();
        let mut finish_reason: Option<String> = None;
        // Keyed by item_id (fc_...). Value: (name, arguments_json, call_id).
        // item_id is what SSE delta/done events carry; call_id is what we store on Block
        // so tool results can pair back to the call.
        let mut tool_calls: std::collections::BTreeMap<String, (String, String, String)> =
            std::collections::BTreeMap::new();
        let mut tool_call_order: Vec<String> = Vec::new();

        while let Some(chunk) = self
            .read_stream_next_chunk(&mut stream, "interrupted by user")
            .await?
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
                            reasoning_buf.push_str(delta);
                            if let Some(visible) = reasoning_summary_stream_delta(
                                &reasoning_buf,
                                &mut reasoning_emitted,
                            ) {
                                self.sink.emit(AgentEvent::ThinkingDelta(visible));
                            }
                        }
                    }
                    "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                        if reasoning_buf.is_empty()
                            && let Some(text) = data["text"].as_str()
                            && !text.is_empty()
                        {
                            reasoning_buf.push_str(text);
                            if let Some(visible) = reasoning_summary_stream_delta(
                                &reasoning_buf,
                                &mut reasoning_emitted,
                            ) {
                                self.sink.emit(AgentEvent::ThinkingDelta(visible));
                            }
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
                        } else if let Some(timings_usage) =
                            data.get("timings").and_then(Usage::parse_openai_timings)
                        {
                            usage = timings_usage;
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
            let reasoning = normalize_reasoning_summary_text(&reasoning_buf);
            if !reasoning.is_empty() {
                self.sink
                    .emit(AgentEvent::ThinkingBlockComplete(reasoning.clone()));
                blocks.push(Block::Thinking {
                    text: reasoning,
                    signature: None,
                });
            }
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
    format!("dext-session-{}.{}", unix_timestamp_secs(), format.ext())
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

fn render_session_entry(
    path: &Path,
    name: &str,
    modified: Option<std::time::SystemTime>,
    opts: &list_render::ListOptions,
    root: &Path,
) -> String {
    let updated = modified
        .and_then(system_time_unix_secs)
        .map(|secs| format!("updated {secs}"))
        .unwrap_or_else(|| "updated unknown".to_string());
    let (messages, model) = match read_session_jsonl(path) {
        Ok((header, history)) => (history.len(), header.model),
        Err(e) => {
            let meta = vec![("path", list_render::display_path(path, opts, root))];
            return list_render::render_entry(name, &format!("unreadable ({e:#})"), &meta, opts);
        }
    };
    let meta = vec![
        ("msgs", messages.to_string()),
        ("model", model),
        ("updated", updated),
        ("path", list_render::display_path(path, opts, root)),
    ];
    list_render::render_entry(name, "", &meta, opts)
}

fn render_session_listing(root: &Path) -> String {
    use std::fmt::Write as _;
    let opts = list_render::ListOptions::detect(false);

    let latest_path = latest_session_path(root);
    let sessions_root = named_sessions_dir_for_root(root);

    let mut autosaved_sessions: Vec<(String, PathBuf, Option<std::time::SystemTime>)> =
        std::fs::read_dir(&sessions_root)
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(|e| e.ok()))
            .filter_map(|entry| {
                let path = entry.path().join(format!("{LATEST_SESSION_NAME}.jsonl"));
                if !path.exists() {
                    return None;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let modified = path.metadata().ok().and_then(|m| m.modified().ok());
                Some((name, path, modified))
            })
            .collect();
    autosaved_sessions.sort_by_key(|(_, _, modified)| std::cmp::Reverse(*modified));

    let named_records = list_session_records_for_root(root);

    let latest_exists = latest_path.exists();
    let total = (if latest_exists { 1 } else { 0 })
        + autosaved_sessions.len()
        + named_records.as_ref().map(|r| r.len()).unwrap_or(0);
    let mut out = String::new();
    let _ = write!(
        out,
        "{}",
        list_render::render_header("Sessions", total, &opts)
    );

    let _ = writeln!(out, "{}", list_render::bold("Latest", opts.color));
    if latest_exists {
        let modified = latest_path.metadata().ok().and_then(|m| m.modified().ok());
        out.push_str(&render_session_entry(
            &latest_path,
            "latest",
            modified,
            &opts,
            root,
        ));
    } else {
        let _ = writeln!(
            out,
            "    (none yet; send a message to create {})",
            list_render::display_path(&latest_path, &opts, root)
        );
    }
    out.push('\n');

    let _ = writeln!(out, "{}", list_render::bold("Autosaved", opts.color));
    if autosaved_sessions.is_empty() {
        let _ = writeln!(
            out,
            "    (none in {})",
            list_render::display_path(&sessions_root, &opts, root)
        );
    } else {
        for (name, path, modified) in autosaved_sessions.iter().take(SLASH_LIST_LIMIT) {
            out.push_str(&render_session_entry(path, name, *modified, &opts, root));
        }
        if autosaved_sessions.len() > SLASH_LIST_LIMIT {
            let _ = writeln!(
                out,
                "  … [{} more session dirs omitted]",
                autosaved_sessions.len() - SLASH_LIST_LIMIT
            );
        }
    }
    out.push('\n');

    let _ = writeln!(out, "{}", list_render::bold("Named", opts.color));
    match &named_records {
        Ok(records) if records.is_empty() => {
            let _ = writeln!(
                out,
                "    (none in {}; use /save <name>)",
                list_render::display_path(&named_sessions_dir_for_root(root), &opts, root)
            );
        }
        Ok(records) => {
            for record in records.iter().take(SLASH_LIST_LIMIT) {
                out.push_str(&render_session_entry(
                    &record.path,
                    &record.name,
                    record.modified,
                    &opts,
                    root,
                ));
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
        "{}",
        list_render::render_footer(
            &[
                "/resume [name]",
                "/save <name>",
                "/map",
                "/focus @wNN",
                "/focus @wNN --branch",
                "/branches",
                "/export html [path]",
            ],
            &opts,
        )
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
        Block::Thinking { text, signature } => {
            let label = if signature.is_some() {
                "thinking (encrypted)"
            } else {
                "thinking"
            };
            let _ = write!(
                out,
                "<details class=\"thinking\"><summary>{}</summary><pre>{}</pre></details>",
                label,
                html_escape(text)
            );
        }
        Block::RedactedThinking { data } => {
            let _ = write!(
                out,
                "<details class=\"thinking\"><summary>redacted thinking</summary><pre>{}</pre></details>",
                html_escape(&summarize_inline(data, 160))
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
        html_escape(header.composed_system.as_deref().unwrap_or(&header.system))
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
        "<div class=\"footer\">Exported by Dext at unix {}</div></body></html>",
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
    provider: String,
    model: String,
    usage: Usage,
}

fn analyze_session_history(header: &SessionHeader, history: &[Message]) -> SessionAnalysis {
    let provider_id = if header.provenance.provider.is_empty() {
        if matches!(header.provenance.api_provider, ApiProvider::Anthropic)
            && header
                .model
                .trim()
                .to_ascii_lowercase()
                .starts_with("claude-")
        {
            "anthropic"
        } else {
            ""
        }
    } else {
        &header.provenance.provider
    };
    let base_url = match header.provenance.api_provider {
        ApiProvider::Anthropic => "https://api.anthropic.com",
        ApiProvider::OpenAi | ApiProvider::ChatGpt => "",
    };
    let model = if header.provenance.model.is_empty() {
        &header.model
    } else {
        &header.provenance.model
    };
    let usage = usage_with_current_pricing(
        header.usage,
        provider_id,
        header.provenance.api_provider,
        base_url,
        model,
        None,
    );
    let mut analysis = SessionAnalysis {
        messages: history.len(),
        provider: header.provenance.provider.clone(),
        model: header.model.clone(),
        usage,
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
                Block::Thinking { .. } | Block::RedactedThinking { .. } => {}
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
        anchor: String::new(),
        kind,
        message_start,
        message_end,
        summary,
        files: files_out,
        commands: commands_out,
        status,
    });
}

fn work_map_anchor_for(waypoint: &WorkMapWaypoint) -> String {
    let raw = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        waypoint.kind.as_str(),
        waypoint.message_start,
        waypoint.message_end,
        waypoint.summary,
        waypoint.files.join("\u{1f}"),
        waypoint.commands.join("\u{1f}"),
        waypoint.status.as_deref().unwrap_or_default()
    );
    format!("@{}", &sha256_hex_str(&raw)[..8])
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
        waypoint.anchor = work_map_anchor_for(waypoint);
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

    // The objective and blocked fields are intentionally NOT turned into
    // waypoints here: objective is synthesized verbatim from the latest user
    // prompt (redundant with the Intent waypoint every real user message
    // already produces via text_work_map_kind), and blocked only ever holds
    // synthesized objective-warning reminders. Real decisions, file changes,
    // verification results, and history-derived waypoints are sufficient.
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
                Block::Thinking { .. } | Block::RedactedThinking { .. } => {}
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

fn resolve_work_map_token(raw: &str, map: &WorkMap) -> Option<usize> {
    if let Some(idx) = parse_waypoint_number(raw) {
        return Some(idx);
    }
    let needle = raw.trim().trim_start_matches('@');
    if needle.is_empty() {
        return None;
    }
    map.waypoints.iter().position(|wp| {
        wp.anchor.trim_start_matches('@') == needle
            || wp.anchor == raw.trim()
            || wp.id == raw.trim()
    })
}

fn parse_work_map_selection(raw: &str, map: &WorkMap) -> Result<WorkMapSelection> {
    let raw = raw.trim();
    if raw.is_empty() {
        anyhow::bail!("missing waypoint id (example: @w03, @a1b2c3d4, or @w03..@w08)");
    }
    let (start_raw, end_raw) = raw.split_once("..").unwrap_or((raw, raw));
    let start = resolve_work_map_token(start_raw, map)
        .ok_or_else(|| anyhow::anyhow!("invalid waypoint id '{start_raw}'"))?;
    let end = resolve_work_map_token(end_raw, map)
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

fn work_map_waypoint_ids_for_view(map: &WorkMap, filters: &[WorkMapFilter]) -> Vec<String> {
    map.waypoints
        .iter()
        .filter(|wp| work_map_filter_matches(map, wp, filters))
        .map(|wp| wp.id.clone())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkMapFilter {
    Kind(WorkMapKind),
    File(String),
    Query(String),
}

fn parse_work_map_filter_args_with_default(
    args: &[String],
    default_selector: &str,
) -> Result<(String, Vec<WorkMapFilter>)> {
    let mut selector: Option<String> = None;
    let mut filters = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        match arg {
            "current" | "this" | "memory" | "latest" if selector.is_none() => {
                selector = Some(arg.to_string());
                idx += 1;
            }
            "all" => {
                idx += 1;
            }
            "intent" | "intents" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Intent));
                idx += 1;
            }
            "evidence" | "read" | "reads" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Evidence));
                idx += 1;
            }
            "change" | "changes" | "write" | "writes" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Change));
                idx += 1;
            }
            "failure" | "failures" | "blocked" | "errors" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Failure));
                idx += 1;
            }
            "verify" | "verification" | "tests" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Verify));
                idx += 1;
            }
            "decision" | "decisions" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Decision));
                idx += 1;
            }
            "compact" | "compactions" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Compact));
                idx += 1;
            }
            "result" | "results" => {
                filters.push(WorkMapFilter::Kind(WorkMapKind::Result));
                idx += 1;
            }
            "file" => {
                let Some(path) = args.get(idx + 1) else {
                    anyhow::bail!("usage: /map file <path-fragment>");
                };
                filters.push(WorkMapFilter::File(path.clone()));
                idx += 2;
            }
            "query" | "search" | "grep" => {
                let rest = args[idx + 1..].join(" ");
                if rest.trim().is_empty() {
                    anyhow::bail!("usage: /map query <text>");
                }
                filters.push(WorkMapFilter::Query(rest));
                break;
            }
            other if selector.is_none() => {
                selector = Some(other.to_string());
                idx += 1;
            }
            other => {
                filters.push(WorkMapFilter::Query(other.to_string()));
                idx += 1;
            }
        }
    }
    Ok((
        selector.unwrap_or_else(|| default_selector.to_string()),
        filters,
    ))
}

fn parse_work_map_filter_args(args: &[String]) -> Result<(String, Vec<WorkMapFilter>)> {
    parse_work_map_filter_args_with_default(args, "current")
}

fn work_map_filter_matches(
    _map: &WorkMap,
    waypoint: &WorkMapWaypoint,
    filters: &[WorkMapFilter],
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let kind_filters = filters
        .iter()
        .filter_map(|filter| match filter {
            WorkMapFilter::Kind(kind) => Some(*kind),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !kind_filters.is_empty() && !kind_filters.contains(&waypoint.kind) {
        return false;
    }
    filters.iter().all(|filter| match filter {
        WorkMapFilter::Kind(_) => true,
        WorkMapFilter::File(needle) => {
            let needle = needle.to_ascii_lowercase();
            waypoint
                .files
                .iter()
                .any(|file| file.to_ascii_lowercase().contains(&needle))
                || waypoint.summary.to_ascii_lowercase().contains(&needle)
        }
        WorkMapFilter::Query(needle) => {
            let needle = needle.to_ascii_lowercase();
            waypoint.id.to_ascii_lowercase().contains(&needle)
                || waypoint.anchor.to_ascii_lowercase().contains(&needle)
                || waypoint.kind.as_str().contains(&needle)
                || waypoint.summary.to_ascii_lowercase().contains(&needle)
                || waypoint
                    .files
                    .iter()
                    .any(|file| file.to_ascii_lowercase().contains(&needle))
                || waypoint
                    .commands
                    .iter()
                    .any(|cmd| cmd.to_ascii_lowercase().contains(&needle))
        }
    })
}

fn work_map_filter_label(filters: &[WorkMapFilter]) -> Option<String> {
    if filters.is_empty() {
        return None;
    }
    Some(
        filters
            .iter()
            .map(|filter| match filter {
                WorkMapFilter::Kind(kind) => kind.as_str().to_string(),
                WorkMapFilter::File(path) => format!("file={path}"),
                WorkMapFilter::Query(query) => format!("query={query}"),
            })
            .collect::<Vec<_>>()
            .join(","),
    )
}

fn render_work_map(map: &WorkMap, filters: &[WorkMapFilter]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let provider = if map.header.provenance.provider.is_empty() {
        "unknown"
    } else {
        &map.header.provenance.provider
    };
    let _ = writeln!(out, "Session map — {}", map.source);
    let _ = writeln!(
        out,
        "model {} · provider {} · messages {} · moments {}",
        map.header.model,
        provider,
        map.messages,
        map.waypoints.len()
    );
    if let Some(origin) = &map.header.track_origin {
        let _ = writeln!(
            out,
            "branched from: {} moment {} ({})",
            origin.source_session, origin.source_waypoint, origin.mode,
        );
    }
    let visible = map
        .waypoints
        .iter()
        .filter(|wp| work_map_filter_matches(map, wp, filters))
        .collect::<Vec<_>>();
    if let Some(label) = work_map_filter_label(filters) {
        let _ = writeln!(
            out,
            "filter {label} · showing {}/{}",
            visible.len(),
            map.waypoints.len()
        );
    }
    if let Some(focus) = &map.header.work_ledger.active_focus {
        let _ = writeln!(
            out,
            "focus active: moment {} of {} ({})",
            focus.selection, focus.source_session, focus.mode,
        );
    }
    if map.waypoints.is_empty() {
        let _ = writeln!(out, "(no waypoints found yet)");
    } else if visible.is_empty() {
        let _ = writeln!(out, "(no waypoints match filter)");
    } else {
        for wp in visible {
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
            extra.push(format!("anchor {}", wp.anchor));
            let extra = format!(" · {}", extra.join(" · "));
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
    }
    let _ = write!(
        out,
        "commands: ↑/↓ or PgUp/PgDn navigate · Enter inspect · f edit · b branch · z filter (/map failures|changes|verify|file <path>|query <text>)"
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
    render_work_map_packet_with_mode(map, selection, None)
}

fn render_work_map_packet_with_mode(
    map: &WorkMap,
    selection: &WorkMapSelection,
    mode: Option<&FocusMode>,
) -> String {
    use std::fmt::Write as _;
    let waypoints = selected_waypoints(map, selection);
    let mut out = String::new();
    let _ = writeln!(out, "[dext packet {}]", work_map_selection_label(waypoints));
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

    let broad_packet = mode.is_none();
    let include_decisions =
        broad_packet || mode.is_some_and(|m| m.carries("decisions") || m.carries("decision"));
    let include_files =
        broad_packet || mode.is_some_and(|m| m.carries("files") || m.carries("file"));
    let include_commands =
        broad_packet || mode.is_some_and(|m| m.carries("commands") || m.carries("cmd"));
    let include_verification = broad_packet
        || mode.is_some_and(|m| {
            m.carries("verify") || m.carries("verification") || m.carries("tests")
        });
    let include_constraints =
        broad_packet || mode.is_some_and(|m| m.carries("constraints") || m.carries("constraint"));

    // Intent is no longer synthesized from the objective field (which echoes
    // raw user text); real user-message Intent waypoints appear below.
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
                "- {} {} {}{} anchor={}: {}",
                wp.id,
                wp.kind.as_str(),
                wp.display_range(),
                status,
                wp.anchor,
                wp.summary
            );
        }
    }

    let mut files = collect_waypoint_items(waypoints, |wp| &wp.files);
    if include_files {
        for file in &map.header.work_ledger.files_changed {
            push_unique_limited(&mut files, file.clone(), 24);
        }
    }
    let mut commands = collect_waypoint_items(waypoints, |wp| &wp.commands);
    if include_commands {
        for record in &map.header.work_ledger.verification {
            push_unique_limited(&mut commands, record.command.clone(), 24);
        }
    }
    let decisions = if include_decisions {
        map.header
            .work_ledger
            .decisions
            .iter()
            .map(|s| summarize_inline(s, 220))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let failures = waypoints
        .iter()
        .filter(|wp| wp.kind == WorkMapKind::Failure)
        .map(|wp| wp.summary.clone())
        .collect::<Vec<_>>();
    let mut verification = waypoints
        .iter()
        .filter(|wp| wp.kind == WorkMapKind::Verify)
        .map(|wp| {
            let status = wp.status.as_deref().unwrap_or("unknown");
            format!("{}: {status}", wp.summary)
        })
        .collect::<Vec<_>>();
    if include_verification {
        for v in &map.header.work_ledger.verification {
            push_unique_limited(
                &mut verification,
                format!(
                    "{}: {} exit={:?} artifact={}",
                    v.name,
                    v.status,
                    v.exit_code,
                    v.artifact.clone().unwrap_or_else(|| "(none)".to_string())
                ),
                24,
            );
        }
    }
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
    if include_constraints && !map.header.work_ledger.constraints.is_empty() {
        render_bullets(
            &mut out,
            "Session constraints",
            &map.header.work_ledger.constraints,
            12,
        );
    }
    let _ = writeln!(out, "Constraints:");
    let _ = writeln!(
        out,
        "- Focus changes model context only; it does not rewind files, git state, or later logs."
    );
    if matches!(mode, Some(FocusMode::Exact)) {
        let _ = writeln!(
            out,
            "- Exact focus withholds prior session history from model context after activation; only this packet and later messages are sent."
        );
    } else if matches!(mode, Some(FocusMode::Carry(_))) {
        let _ = writeln!(
            out,
            "- Carry focus withholds prior raw history and carries only the requested ledger categories plus selected waypoints."
        );
    }
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
        if arg == "--exact" || arg == "exact" || arg == "--isolate" || arg == "isolate" {
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
        "[dext focus {} mode={}]",
        work_map_selection_label(selected),
        mode.label()
    );
    let _ = writeln!(
        out,
        "Note: this does not rewind files, git, or your current session."
    );
    match mode {
        FocusMode::Exact => {
            let _ = writeln!(
                out,
                "Exact: narrows the model's live context to this summary onward."
            );
        }
        FocusMode::Carry(items) => {
            let carry = if items.is_empty() {
                "(none)".to_string()
            } else {
                items.join(",")
            };
            let _ = writeln!(
                out,
                "Carry: keeps only these facts in live context: {carry}"
            );
        }
    }
    out.push('\n');
    out.push_str(&render_work_map_packet_with_mode(
        map,
        selection,
        Some(mode),
    ));
    out
}

fn activate_work_map_focus(
    agent: &mut Agent,
    map: &WorkMap,
    selection: &WorkMapSelection,
    mode: &FocusMode,
) -> String {
    let text = render_work_map_focus(map, selection, mode);
    let selected = selected_waypoints(map, selection);
    let selection_label = work_map_selection_label(selected);
    agent.work_ledger.active_focus = Some(WorkMapFocusState {
        source_session: map.source.clone(),
        selection: selection_label,
        mode: mode.label().to_string(),
        packet_hash: sha256_hex_str(&text),
        created_at: unix_timestamp_secs(),
    });
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: format!("[dext focus packet loaded]\n{text}"),
        }],
    });
    agent.checkpoint_latest_session("after_focus");
    text
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
        || raw
            .trim()
            .strip_prefix('@')
            .is_some_and(|rest| rest.len() >= 6 && rest.chars().all(|c| c.is_ascii_hexdigit()))
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
    let mut selector = selector;
    let selector_from_before = id_pos > 0;
    let mut mode_args = Vec::new();
    let mut prev_was_branch = false;
    for arg in &args[id_pos + 1..] {
        if arg.starts_with("--") || matches!(arg.as_str(), "exact" | "isolate") {
            mode_args.push(arg.clone());
            prev_was_branch = *arg == "--branch";
        } else if prev_was_branch {
            mode_args.push(arg.clone());
            prev_was_branch = false;
        } else if !selector_from_before && selector == default_selector {
            selector = arg.as_str();
        } else {
            anyhow::bail!("unexpected argument after waypoint id: {arg}");
        }
    }
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
        if arg.starts_with("--") || matches!(arg.as_str(), "exact" | "isolate") {
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
    let _ = writeln!(out, "Branches (sessions continued from past moments):");
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
                    "  · {} — from moment {} ({})",
                    record.name, origin.source_waypoint, origin.mode,
                );
                if shown >= SLASH_LIST_LIMIT {
                    break;
                }
            }
            if shown == 0 {
                let _ = writeln!(out, "  (none yet; use /focus @wNN --branch [name])");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "[err] {e:#}");
        }
    }
    let _ = write!(out, "resume a branch with: /resume <name>");
    let _ = root;
    out
}

fn default_track_name(waypoint_id: &str) -> String {
    let clean = waypoint_id
        .trim_start_matches('@')
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    format!("branch-{clean}-{}", unix_timestamp_secs())
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
    header.work_ledger.objective = format!("branched from {}", work_map_selection_label(selected));
    let path = named_session_path_for_root(root, &track_name)?;
    let history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: format!(
                    "Continued from moment {} in a previous session. Here is the summary of work so far:\n\n{}",
                    work_map_selection_label(selected),
                    packet
                ),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text:
                    "Branch ready. Your files and git state are unchanged — verify current repo state before editing."
                        .to_string(),
            }],
        },
    ];
    let mut data = Vec::new();
    writeln!(&mut data, "{}", serde_json::to_string(&header)?)?;
    for message in history {
        writeln!(&mut data, "{}", serde_json::to_string(&message)?)?;
    }
    crate::session::atomic_write_secret(&path, &data)?;
    Ok(path)
}

fn create_branch_text(
    agent: &Agent,
    map: &WorkMap,
    selection: &WorkMapSelection,
    name: Option<&str>,
) -> Result<String> {
    let mode = FocusMode::Carry(vec!["failures".into(), "decisions".into(), "files".into()]);
    let label = work_map_selection_label(selected_waypoints(map, selection));
    let path = create_track_from_work_map(agent, map, selection, name, &mode)?;
    let branch_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("branch");
    Ok(format!(
        "Created branch \"{branch_name}\" — continues from moment {label}.\n\
         Your current session is unchanged.\n\
         Resume the branch later with: /resume {branch_name}",
    ))
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
        let _ = writeln!(out, "- dext_version: {}", header.provenance.dext_version);
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
    let _ = writeln!(out, "compactions: {}", analysis.compactions);
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
                | Block::Thinking { text, .. }
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
                Block::RedactedThinking { data } => {
                    if data.to_ascii_lowercase().contains(&needle_lower) {
                        hits.push(format!(
                            "#{} {} redacted_thinking: {}",
                            idx + 1,
                            msg.role,
                            summarize_inline(data, 220)
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
            crate::session::atomic_write_secret(target, &bytes)?;
        }
        SessionExportFormat::Html => {
            let (header, history) = read_session_jsonl(source)?;
            let title = source
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("dext session");
            let html = render_session_html(&header, &history, title);
            crate::session::atomic_write_secret(target, html.as_bytes())?;
        }
    }
    Ok(())
}

fn normalize_session_selector(selector: &str) -> String {
    let mut trimmed = selector.trim();
    loop {
        let stripped = if trimmed.len() >= 2 {
            trimmed
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                .or_else(|| {
                    trimmed
                        .strip_prefix('\'')
                        .and_then(|s| s.strip_suffix('\''))
                })
        } else {
            None
        };
        let Some(next) = stripped.map(str::trim).filter(|s| !s.is_empty()) else {
            break;
        };
        trimmed = next;
    }
    trimmed.to_string()
}

fn resolve_session_selector(root: &Path, selector: &str) -> Result<PathBuf> {
    let trimmed = normalize_session_selector(selector);
    let trimmed = trimmed.as_str();
    if trimmed.is_empty() || trimmed == "latest" || trimmed == LATEST_SESSION_NAME {
        return Ok(latest_session_path(root));
    }
    let path = expand_user_path(trimmed);
    if path.is_dir() {
        let latest = path.join(format!("{LATEST_SESSION_NAME}.jsonl"));
        if latest.exists() {
            return Ok(latest);
        }
    }
    if path.exists() || path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        return Ok(path);
    }
    let named_path = named_session_path_for_root(root, trimmed)?;
    if named_path.exists() {
        return Ok(named_path);
    }
    let session_latest = session_latest_session_path(root, trimmed);
    if session_latest.exists() {
        return Ok(session_latest);
    }
    Ok(named_path)
}

fn format_preview(p: &mutation_preview::MutationPreview) -> String {
    let status = if p.is_new_file {
        "new file".to_string()
    } else {
        format!("{}+ {}-", p.added, p.removed)
    };
    let mut out = format!(
        "preview: {} ({}){}",
        p.path.display(),
        status,
        if p.truncated { " [truncated]" } else { "" }
    );
    if !p.diff.is_empty() && p.diff != "(no changes)" {
        out.push('\n');
        out.push_str(&p.diff);
    }
    out
}

fn handle_undo_cli(args: &[String], root: &Path) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "--list" || a == "list") {
        match git_checkpoints::list_checkpoints(root, 20) {
            Ok(cps) if cps.is_empty() => println!("no checkpoints"),
            Ok(cps) => {
                for cp in &cps {
                    println!("{}  {}  {}", cp.id, cp.tool_name, cp.paths_hint.join(","));
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        }
        return 0;
    }
    if args.iter().any(|a| a == "--prune" || a == "prune") {
        match git_checkpoints::prune(root, None, None) {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        }
        return 0;
    }
    let apply = args.iter().any(|a| a == "--apply");
    let reset = args.iter().any(|a| a == "--reset-head");
    let id_arg = args.iter().find(|a| !a.starts_with('-'));
    let target = if let Some(id) = id_arg {
        match git_checkpoints::find_checkpoint(root, id) {
            Ok(Some(cp)) => cp,
            Ok(None) => {
                eprintln!("checkpoint not found: {id}");
                return 1;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        }
    } else {
        match git_checkpoints::latest_checkpoint(root) {
            Ok(Some(cp)) => cp,
            Ok(None) => {
                eprintln!("no checkpoints");
                return 1;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        }
    };
    let mode = if reset {
        git_checkpoints::RestoreMode::ResetHead
    } else if apply {
        git_checkpoints::RestoreMode::Worktree
    } else {
        git_checkpoints::RestoreMode::Preview
    };
    match git_checkpoints::restore_worktree(root, &target, mode) {
        Ok(msg) => println!("{msg}"),
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    }
    0
}

fn handle_memory_cli(args: &[String], root: &Path) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("check");
    match sub {
        "check" => {
            match memory_merge::check(root) {
                Ok(status) => {
                    println!(
                        "memory merge: {}",
                        if status.memory_registered {
                            "registered"
                        } else {
                            "not registered"
                        }
                    );
                    println!(
                        "recall merge: {}",
                        if status.recall_registered {
                            "registered"
                        } else {
                            "not registered"
                        }
                    );
                    println!(
                        "local attributes: {}",
                        if status.gitattributes_local {
                            "yes"
                        } else {
                            "no"
                        }
                    );
                    println!(
                        "versioned attributes: {}",
                        if status.gitattributes_versioned {
                            "yes"
                        } else {
                            "no"
                        }
                    );
                    if !status.memory_registered {
                        eprintln!("run 'dext memory register' to enable section-aware merging");
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return 1;
                }
            }
            0
        }
        "register" => {
            let versioned = args.iter().any(|a| a == "--versioned-attributes");
            let modes = if args.iter().any(|a| a == "--recall") {
                vec![memory_merge::RegisterMode::Recall]
            } else if args.iter().any(|a| a == "--memory") {
                vec![memory_merge::RegisterMode::Memory]
            } else {
                vec![
                    memory_merge::RegisterMode::Memory,
                    memory_merge::RegisterMode::Recall,
                ]
            };
            for mode in modes {
                if let Err(e) = memory_merge::register(root, mode, versioned) {
                    eprintln!("error: {e}");
                    return 1;
                }
            }
            println!("registered memory merge driver(s)");
            0
        }
        "unregister" => {
            if let Err(e) = memory_merge::unregister(root) {
                eprintln!("error: {e}");
                return 1;
            }
            println!("unregistered memory merge drivers");
            0
        }
        "merge" => {
            let is_recall = args.iter().any(|a| a == "--recall");
            // Git merge driver protocol: %O %A %B %L %P
            // %O=base %A=ours %B=theirs %L=marker-size %P=path
            let positional: Vec<&String> = args
                .iter()
                .skip(1)
                .filter(|a| !a.starts_with('-'))
                .collect();
            if positional.len() < 3 {
                eprintln!(
                    "usage: dext memory merge [--recall] <base> <ours> <theirs> [marker-size] [path]"
                );
                return 2;
            }
            let base_content = std::fs::read_to_string(positional[0]).unwrap_or_default();
            let ours_content = std::fs::read_to_string(positional[1]).unwrap_or_default();
            let theirs_content = std::fs::read_to_string(positional[2]).unwrap_or_default();

            let outcome = if is_recall {
                memory_merge::merge_recall(&base_content, &ours_content, &theirs_content)
            } else {
                memory_merge::merge_memory(&base_content, &ours_content, &theirs_content)
            };

            // Write result to ours file (Git merge driver protocol)
            if let Err(e) = std::fs::write(positional[1], &outcome.content) {
                eprintln!("error writing merge result: {e}");
                return 1;
            }
            for w in &outcome.warnings {
                eprintln!("warning: {w}");
            }
            if outcome.clean { 0 } else { 1 }
        }
        "distill" => {
            let memory_path = root.join("MEMORY.md");
            let recall_path = root.join("recall.md");
            let recall = match std::fs::read_to_string(&recall_path) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!("error reading {}: {e}", recall_path.display());
                    return 1;
                }
            };
            let memory = std::fs::read_to_string(&memory_path).unwrap_or_default();
            let distill = memory_merge::distill_recall(&memory, &recall);

            println!(
                "recall.md: {} bullet(s) -> {} after dedupe ({} exact duplicate(s) removed)",
                distill.original_bullets,
                distill.kept_bullets,
                distill.removed_duplicates.len()
            );
            for dup in &distill.removed_duplicates {
                println!("  - duplicate: {dup}");
            }
            if !distill.near_duplicates.is_empty() {
                println!("near-duplicate bullets (kept; review manually):");
                for item in &distill.near_duplicates {
                    println!("  ~ {item}");
                }
            }
            if !distill.unbacked.is_empty() {
                println!(
                    "bullets not reflected in MEMORY.md (possibly stale, or promote to MEMORY.md):"
                );
                for item in &distill.unbacked {
                    println!("  ? {item}");
                }
            }

            let apply = args.iter().any(|a| a == "--apply");
            if apply {
                if distill.content == recall {
                    println!("recall.md already distilled; nothing to write");
                } else if let Err(e) =
                    crate::session::atomic_write_bytes(&recall_path, distill.content.as_bytes())
                {
                    eprintln!("error writing {}: {e}", recall_path.display());
                    return 1;
                } else {
                    println!("applied: rewrote {}", recall_path.display());
                }
            } else if distill.content != recall {
                println!("\n(dry run; re-run with --apply to rewrite recall.md)");
            } else {
                println!("\nrecall.md already distilled; no changes needed");
            }
            0
        }
        _ => {
            eprintln!("usage: dext memory [check|register|unregister|merge|distill]");
            2
        }
    }
}

fn handle_doctor_cli(root: &Path) -> i32 {
    let (report, _warnings) = doctor_report(root);
    print!("{report}");
    0
}

/// Build the `dext doctor` report text and the count of warnings it contains.
/// Kept separate from printing so it can be asserted in tests.
fn doctor_report(root: &Path) -> (String, usize) {
    let out = std::cell::RefCell::new(String::new());
    let warnings = std::cell::Cell::new(0usize);
    let line = |ok: bool, label: &str, detail: String| {
        let mark = if ok { "ok  " } else { "warn" };
        if !ok {
            warnings.set(warnings.get() + 1);
        }
        out.borrow_mut()
            .push_str(&format!("[{mark}] {label}: {detail}\n"));
    };

    out.borrow_mut()
        .push_str("dext doctor — environment and capability check\n\n");

    line(
        true,
        "version",
        format!(
            "dext {} ({} {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    );
    line(true, "cwd", root.display().to_string());

    // OS-level sandbox enforcement.
    line(
        sandbox::is_enforced(),
        "sandbox",
        if sandbox::is_enforced() {
            sandbox::describe()
        } else {
            format!("{} (path-validation still applies)", sandbox::describe())
        },
    );

    // Terminal.
    line(
        true,
        "terminal",
        if io::stdout().is_terminal() {
            "interactive tty".to_string()
        } else {
            "non-tty (piped/redirected)".to_string()
        },
    );

    // Git repo.
    let in_git = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    line(
        true,
        "git repo",
        if in_git {
            "yes (checkpoints/undo available)".to_string()
        } else {
            "no (Git checkpoints disabled)".to_string()
        },
    );

    // Tool binaries. bash/git are required; the rest are optional native tools.
    for (name, required) in [
        ("bash", true),
        ("git", true),
        ("rg", false),
        ("fd", false),
        ("jq", false),
        ("awk", false),
        ("fzf", false),
        ("in2csv", false),
    ] {
        let present = binary_on_path(name);
        let detail = if present {
            "on PATH".to_string()
        } else if required {
            "MISSING (required)".to_string()
        } else {
            "not found (native tool falls back to bash where possible)".to_string()
        };
        line(present || !required, &format!("tool {name}"), detail);
    }

    // Providers and auth.
    match (
        provider::load_provider_catalog(),
        provider::load_auth_store(),
    ) {
        (Ok(catalog), Ok(store)) => {
            let active = provider::resolve_active_provider_id(&catalog);
            line(true, "active provider", active.clone());
            let mut any_auth = false;
            for profile in &catalog.providers {
                let status = provider::provider_auth_status(profile, &store);
                let authed = status.starts_with("auth")
                    || status.starts_with("env:")
                    || status == "not-required";
                if authed {
                    any_auth = true;
                }
                let marker = if provider::canonical_provider_id(&profile.id)
                    == provider::canonical_provider_id(&active)
                {
                    "* "
                } else {
                    "  "
                };
                out.borrow_mut()
                    .push_str(&format!("       {marker}{:<10} {}\n", profile.id, status));
            }
            if !any_auth {
                warnings.set(warnings.get() + 1);
                out.borrow_mut().push_str(
                    "[warn] no provider has resolvable credentials; run `dext auth login <provider>`\n",
                );
            }

            // Local llama reachability probe for any local provider.
            if let Some(local) = catalog.providers.iter().find(|p| {
                provider::is_local_llama_provider(
                    &p.id,
                    p.api_provider,
                    &provider::resolve_provider_base_url(p),
                )
            }) {
                let base = provider::resolve_provider_base_url(local);
                let model = provider::resolve_provider_model(local);
                match provider::refresh_local_llama_context_window(
                    &local.id,
                    local.api_provider,
                    &base,
                    &model,
                ) {
                    Some(ctx) => line(
                        true,
                        "local llama",
                        format!("reachable at {base} (context {ctx} tokens)"),
                    ),
                    None => line(
                        false,
                        "local llama",
                        format!(
                            "configured ({base}) but not reachable; start llama-server if you use the local provider"
                        ),
                    ),
                }
            }
        }
        _ => {
            line(
                false,
                "providers",
                "could not load provider catalog/auth store".to_string(),
            );
        }
    }

    // Active session locks under this project.
    let lock_count = std::fs::read_dir(session::latest_sessions_dir(root))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().join(SESSION_STATE_LOCK_NAME).exists())
                .count()
        })
        .unwrap_or(0);
    line(
        true,
        "session locks",
        if lock_count == 0 {
            "none".to_string()
        } else {
            format!("{lock_count} held (stale ones clear on next clean start)")
        },
    );

    let warnings = warnings.get();
    let summary = if warnings == 0 {
        "\nall checks passed\n".to_string()
    } else {
        format!("\n{warnings} warning(s); see [warn] lines above\n")
    };
    out.borrow_mut().push_str(&summary);
    (out.into_inner(), warnings)
}

/// A compact, safe continuation packet for handing work to another agent or a
/// fresh session. Built from the curated work ledger (header) plus distilled
/// analysis facts — deliberately not the raw prompts/tool output, which the
/// full jsonl carries and which may contain sensitive content.
fn render_session_brief(
    source: &Path,
    header: &SessionHeader,
    analysis: &SessionAnalysis,
) -> String {
    let mut out = String::from("# Dext session brief\n");
    out.push_str(&format!("source: {}\n", source.display()));
    let provider = if analysis.provider.is_empty() {
        header.model.clone()
    } else {
        format!("{}/{}", analysis.provider, analysis.model)
    };
    out.push_str(&format!(
        "model: {}   messages: {}   usage: {}\n",
        provider,
        analysis.messages,
        analysis.usage.line()
    ));
    if analysis.compactions > 0 {
        out.push_str(&format!("compactions: {}\n", analysis.compactions));
    }

    let ledger = render_work_ledger_prompt(&header.work_ledger)
        .trim()
        .to_string();
    out.push_str("\n## Work ledger\n");
    if ledger.is_empty() {
        out.push_str("(no work ledger recorded for this session)\n");
    } else {
        out.push_str(&ledger);
        out.push('\n');
    }

    // Supplement with distilled facts the ledger may not carry.
    if header.work_ledger.files_changed.is_empty() && !analysis.files_touched.is_empty() {
        out.push_str("\n## Files touched\n");
        let mut files: Vec<(&String, &usize)> = analysis.files_touched.iter().collect();
        files.sort_by(|a, b| b.1.cmp(a.1));
        for (path, count) in files.into_iter().take(12) {
            out.push_str(&format!("- {path} ({count} edits)\n"));
        }
    }
    if !analysis.failures.is_empty() {
        out.push_str("\n## Recent failures\n");
        for item in analysis.failures.iter().rev().take(6).rev() {
            out.push_str(&format!("- {item}\n"));
        }
    }
    if header.work_ledger.verification.is_empty() && !analysis.verification.is_empty() {
        out.push_str("\n## Verification\n");
        for v in analysis.verification.iter().rev().take(6).rev() {
            out.push_str(&format!(
                "- {}: {} exit={}\n",
                v.name,
                v.status,
                v.exit_code.map(|c| c.to_string()).unwrap_or_default()
            ));
        }
    }

    out.push_str(
        "\n## Continue\nDistilled continuation packet, not the full transcript. Resume the live session with `dext --resume`, inspect detail with `dext session analyze`, or hand this brief to another agent to continue from the work ledger, recent failures, and verification facts above.\n",
    );
    out
}

fn collect_session_lock_paths(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_session_lock_paths(&path, out)?;
        } else if path.file_name().and_then(|s| s.to_str()) == Some(SESSION_STATE_LOCK_NAME) {
            out.push(path);
        }
    }
    Ok(())
}

fn prune_project_dirs(root: &Path, dry_run: bool, max_age_days: u64) -> Result<()> {
    let projects_dir = session::dext_state_dir().join("projects");
    let current_key = project_key(root);
    let now = std::time::SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_days * 86400);

    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => {
            println!("no projects dir at {}", projects_dir.display());
            return Ok(());
        }
    };

    let mut stale_locks = Vec::new();
    collect_session_lock_paths(&projects_dir, &mut stale_locks)?;
    stale_locks.retain(|path| !session_state_lock_is_live(path));

    let mut candidates = Vec::new();
    let mut kept = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == current_key {
            kept += 1;
            continue;
        }
        let sessions_dir = path.join("sessions");
        let has_real_session = match walkdir_jsonl_count(&sessions_dir) {
            Ok(count) => count > 0,
            Err(_) => {
                kept += 1;
                continue;
            }
        };
        if has_real_session {
            kept += 1;
            continue;
        }
        let mut locks = Vec::new();
        if collect_session_lock_paths(&path, &mut locks).is_err() {
            kept += 1;
            continue;
        }
        if locks.iter().any(|lock| session_state_lock_is_live(lock)) {
            kept += 1;
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(now);
        let age = now.duration_since(modified).unwrap_or_default();
        if age <= max_age {
            kept += 1;
            continue;
        }
        candidates.push((path, name_str.to_string(), age));
    }

    if candidates.is_empty() && stale_locks.is_empty() {
        println!("nothing to prune ({kept} project dir(s) kept; sessions preserved).");
        return Ok(());
    }

    let action = if dry_run { "would remove" } else { "removing" };
    for lock in &stale_locks {
        let rel = lock.strip_prefix(&projects_dir).unwrap_or(lock);
        println!("{action} stale lock: {}", rel.display());
    }
    for (_, name, age) in &candidates {
        println!(
            "{action} empty project dir: {name} ({}d old)",
            age.as_secs() / 86400
        );
    }
    println!(
        "\n{} empty project dir candidate(s) · {} stale lock(s) · {kept} kept · sessions preserved.{}",
        candidates.len(),
        stale_locks.len(),
        if dry_run {
            " Re-run with --apply to remove."
        } else {
            ""
        }
    );

    if !dry_run {
        for lock in &stale_locks {
            let _ = remove_stale_session_state_lock(lock);
        }
        for (path, _, _) in &candidates {
            let _ = std::fs::remove_dir_all(path);
        }
    }
    Ok(())
}

fn walkdir_jsonl_count(dir: &Path) -> std::io::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    let stack = vec![dir.to_path_buf()];
    let mut current = stack;
    while let Some(d) = current.pop() {
        for entry in std::fs::read_dir(&d)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                current.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                count += 1;
            }
        }
    }
    Ok(count)
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
                eprintln!("usage: dext sessions-grep <text>");
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
                            "usage: dext session export [latest|NAME|PATH] [html|jsonl] [OUT]"
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
                        eprintln!("usage: dext session grep <text> [latest|NAME|PATH]");
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
                "brief" => {
                    let selector = argv.get(2).map(|s| s.as_str()).unwrap_or("latest");
                    let source = resolve_session_selector(&root, selector)?;
                    let (header, history) = read_session_jsonl(&source)?;
                    let analysis = analyze_session_history(&header, &history);
                    print!("{}", render_session_brief(&source, &header, &analysis));
                    Ok(Some(0))
                }
                "prune" => {
                    let dry_run = !argv.iter().skip(2).any(|a| a == "--apply" || a == "apply");
                    let max_age_days = argv
                        .iter()
                        .skip(2)
                        .find_map(|a| {
                            a.strip_prefix("--days=")
                                .and_then(|v| v.parse::<u64>().ok())
                        })
                        .unwrap_or(7);
                    prune_project_dirs(&root, dry_run, max_age_days)?;
                    Ok(Some(0))
                }
                "map" => {
                    let args = parse_work_map_command_args(&argv[2..].join(" "));
                    let (selector, filters) =
                        parse_work_map_filter_args_with_default(&args, "latest")?;
                    let (_, map) = load_work_map_for_selector(&root, &selector)?;
                    println!("{}", render_work_map(&map, &filters));
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
                "tracks" | "branches" => {
                    println!("{}", render_tracks_listing(&root));
                    Ok(Some(0))
                }
                "track" => {
                    let sub = argv.get(2).map(|s| s.as_str());
                    let args_str = if matches!(sub, Some("open")) {
                        argv[3..].join(" ")
                    } else {
                        argv.get(2..).map(|s| s.join(" ")).unwrap_or_default()
                    };
                    let args = parse_work_map_command_args(&args_str);
                    match parse_track_open_args(&args, "latest") {
                        Ok((id, selector, name, _)) => {
                            let (source, map) = load_work_map_for_selector(&root, selector)?;
                            let selection = parse_work_map_selection(id, &map)?;
                            let (header, _) = read_session_jsonl(&source)?;
                            let mode = FocusMode::Carry(vec![
                                "failures".into(),
                                "decisions".into(),
                                "files".into(),
                            ]);
                            let path = create_track_from_work_map_with_header(
                                &root, header, &map, &selection, name, &mode,
                            )?;
                            let bn = path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("branch");
                            println!(
                                "Created branch \"{}\" — resume with: dext --resume={}",
                                bn, bn
                            );
                            Ok(Some(0))
                        }
                        Err(_) => {
                            println!("{}", render_tracks_listing(&root));
                            Ok(Some(0))
                        }
                    }
                }
                _ => {
                    eprintln!(
                        "usage: dext session [list|map [session] [filter]|focus [session] @wNN [--exact]|packet [session] @wNN|tracks|track [open] @wNN [name]|export|analyze|brief|grep|failures|verify-log|decisions|prune]"
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
        "pack" | "packs" => {
            let mut pack_arg = arg;
            let mut leading_verbose = false;
            loop {
                let mut flag_parts = pack_arg.splitn(2, char::is_whitespace);
                let tok = flag_parts.next().unwrap_or("").trim();
                if !matches!(tok, "--verbose" | "-v" | "--paths") {
                    break;
                }
                leading_verbose = true;
                pack_arg = flag_parts.next().unwrap_or("").trim_start();
            }
            let mut parts = pack_arg.splitn(3, char::is_whitespace);
            let sub = parts.next().unwrap_or("").trim();
            match sub {
                "" | "list" | "ls" => {
                    let (_, inline_verbose) = list_render::take_verbose(pack_arg);
                    let verbose = leading_verbose || inline_verbose;
                    let _ = write!(
                        w,
                        "{}",
                        packs::render_pack_listing_opts(&agent.sandbox_root, verbose)
                    );
                }
                "inspect" | "info" | "show" => {
                    let selector = parts.next().unwrap_or("").trim();
                    if selector.is_empty() {
                        let _ = writeln!(w, "usage: /pack inspect <name>");
                    } else {
                        match packs::render_pack_inspect(&agent.sandbox_root, selector) {
                            Ok(text) => {
                                let _ = write!(w, "{text}");
                            }
                            Err(e) => {
                                let _ = writeln!(w, "{e:#}");
                            }
                        }
                    }
                }
                "run" | "use" | "start" => {
                    let selector = parts.next().unwrap_or("").trim();
                    let task = parts.next().unwrap_or("").trim();
                    if selector.is_empty() {
                        let _ = writeln!(w, "usage: /pack run <name> <task>");
                    } else if task.is_empty() {
                        let _ = writeln!(w, "usage: /pack run {selector} <task>");
                    } else {
                        let _ = writeln!(
                            w,
                            "pack run is async; use /pack run from the interactive loop, or `dext pack run {selector} {task}`"
                        );
                    }
                }
                _ => {
                    let _ = writeln!(w, "usage: /pack [list|inspect <name>|run <name> <task>]");
                }
            }
        }
        "shelf" | "shelves" => {
            let _ = write!(
                w,
                "{}",
                shelves::render_registry_listing(&agent.shelf_registry)
            );
        }
        "help" | "?" => {
            let _ = writeln!(w, "── Core ──");
            let _ = writeln!(w, "  /help                     show this");
            let _ = writeln!(w, "  /quit, /exit              exit dext");
            let _ = writeln!(w, "  /reset                    clear conversation history");
            let _ = writeln!(w);
            let _ = writeln!(w, "── Tools & policy ──");
            let _ = writeln!(
                w,
                "  /tools [default|full]     list or switch provider-visible tools"
            );
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
                "  /trust [on|off|status]   auto-approve all privileged tools"
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
                "  /preview [mode]          off|simple|git mutation previews"
            );
            let _ = writeln!(
                w,
                "  /sandbox-profile [profile] read-only|workspace-write|danger-full-access"
            );
            let _ = writeln!(
                w,
                "  /budget [cap|off]         show/set budget cap ($ or tokens)"
            );
            let _ = writeln!(w);
            let _ = writeln!(w, "── Packs & shelves ──");
            let _ = writeln!(
                w,
                "  /pack [list|inspect|run]  discover or invoke Dext packs"
            );
            let _ = writeln!(
                w,
                "  /shelves                  list typed shelf manifests and ability metadata"
            );
            let _ = writeln!(
                w,
                "  /browser [off|agent-browser|agentbrowser] optional browser automation recipe"
            );
            let _ = writeln!(
                w,
                "  /sandbox [path]           show or change the sandbox root"
            );
            let _ = writeln!(w);
            let _ = writeln!(w, "── Provider & auth ──");
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
            let _ = writeln!(w);
            let _ = writeln!(w, "── Context & diagnostics ──");
            let _ = writeln!(
                w,
                "  /effort [level]           set model reasoning depth/tool persistence: off|low|medium|high|xhigh|max"
            );
            let _ = writeln!(
                w,
                "  /context [standard|frugal|tiny] context/cap mode; tiny is skinny local mode"
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
                "  /diagnostics              run rust-analyzer diagnostics (fallback: cargo check)"
            );
            let _ = writeln!(w);
            let _ = writeln!(w, "── Sessions & work map ──");
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
                "  /resume [name]            load the latest autosaved or a named session"
            );
            let _ = writeln!(
                w,
                "  /map [session] [filter]    show session as moments @wNN; filters: failures|changes|verify|file <path>|query <text>"
            );
            let _ = writeln!(
                w,
                "  /focus @wNN               inspect a past moment (read-only; your session is unchanged)"
            );
            let _ = writeln!(
                w,
                "  /focus @wNN --branch [n]  branch into a new session from that moment; resume with /resume <name>"
            );
            let _ = writeln!(
                w,
                "  /focus @wNN --exact       (advanced) narrow model context to this moment onward; /focus clear restores"
            );
            let _ = writeln!(
                w,
                "  /branches                 list branches (continuations from past moments)"
            );
            let _ = writeln!(
                w,
                "    Map drawer keys: ↑/↓/PgUp/PgDn/Home/End navigate · Enter inspect · f edit · b branch · z filter · Esc close"
            );
            let _ = writeln!(
                w,
                "  /sessions                 list latest + autosaved/named sessions; /sessions analyze|brief|grep|failures|verify-log|decisions|export|prune"
            );
            let _ = writeln!(w, "  /session                  alias for /sessions");
            let _ = writeln!(
                w,
                "  /plan <task>              run a read-only planner, seed the plan into history"
            );
            let _ = writeln!(
                w,
                "  /hooks [reload]           show hook config or reload from disk"
            );
            let _ = writeln!(
                w,
                "  /undo [--apply|--list|<id>] preview or restore latest checkpoint"
            );
            let _ = writeln!(w, "  /version                  show dext version");
        }
        "version" => {
            let _ = writeln!(w, "dext {}", env!("CARGO_PKG_VERSION"));
        }
        "quit" | "exit" => {
            return Some(false);
        }
        "reset" => {
            let n = agent.history.len();
            agent.history.clear();
            agent.work_ledger.active_focus = None;
            agent.clear_pending_login();
            if agent.session_enabled {
                let _ = std::fs::remove_file(&agent.latest_session_path);
            }
            let _ = writeln!(w, "cleared {n} messages");
        }
        "tools" => {
            let result = handle_tools_command(agent, arg);
            let _ = writeln!(w, "{}", result.output);
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
                        Block::RedactedThinking { .. } => "redacted_thinking",
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
                let composed = agent.composed_system_prompt();
                let _ = writeln!(
                    w,
                    "{}",
                    cap_bytes_with_hint(
                        composed,
                        SLASH_TEXT_CAP,
                        "system prompt display truncated; use /system <text> to replace the base prompt.",
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
            let mut v: Vec<String> = agent
                .allowed
                .iter()
                .filter(|name| agent.tools.iter().any(|tool| tool.name == name.as_str()))
                .cloned()
                .collect();
            if v.is_empty() {
                let _ = writeln!(w, "(none)");
            } else {
                v.sort();
                let _ = writeln!(
                    w,
                    "{}",
                    render_limited_csv(&v, SLASH_LIST_LIMIT, "(none)", "allowed tools")
                );
            }
        }
        "trust" => match arg {
            "" | "status" => {
                let mode = if agent.trust_mode_active() {
                    "ON"
                } else {
                    "off"
                };
                let _ = writeln!(w, "trust: {mode}");
                let _ = writeln!(w, "approval profile: {}", agent.approval_profile().as_str());
                let _ = writeln!(
                    w,
                    "danger zone: privileged tools run without per-call confirmation"
                );
            }
            "on" => {
                let changed = agent.set_trust_mode(true);
                ui_update = SlashUiUpdate::ApprovalProfile;
                let _ = writeln!(
                    w,
                    "trust mode on: {changed} privileged tools now auto-approving"
                );
            }
            "off" => {
                let changed = agent.set_trust_mode(false);
                ui_update = SlashUiUpdate::ApprovalProfile;
                let _ = writeln!(
                    w,
                    "trust mode off: {changed} privileged tools returned to per-call prompts"
                );
            }
            _ => {
                let _ = writeln!(w, "usage: /trust [on|off|status]");
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
        "preview" => {
            if arg.is_empty() || arg == "status" {
                let _ = writeln!(w, "mutation preview: {}", agent.preview_mode.as_str());
                let _ = writeln!(w, "modes: off, simple, git");
            } else if let Some(mode) = MutationPreviewMode::parse(arg) {
                agent.preview_mode = mode;
                let _ = writeln!(w, "mutation preview -> {}", mode.as_str());
                if mode == MutationPreviewMode::Git {
                    let _ = writeln!(w, "git previews currently fall back to simple previews");
                }
            } else {
                let _ = writeln!(w, "usage: /preview [off|simple|git]");
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
                let _ = writeln!(w, "session usage: {}", agent.priced_session_usage().line());
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
                let _ = writeln!(w, "usage: /browser [off|agent-browser|agentbrowser]");
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
                        let _ = writeln!(
                            w,
                            "usage: /effort [off|low|medium|high|xhigh|max|next|prev|status]"
                        );
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
                agent.set_context_mode(mode);
                agent.refresh_tools_for_context();
                let _ = writeln!(
                    w,
                    "context mode -> {} (compact threshold {}, toolset {})",
                    agent.context_mode.as_str(),
                    agent.compact_threshold_chars(),
                    agent.tool_context_profile().as_str()
                );
            } else {
                let _ = writeln!(w, "usage: /context [standard|frugal|tiny|status]");
            }
        }
        "tool-profile" | "tools-profile" => {
            if arg.is_empty() || arg.eq_ignore_ascii_case("status") {
                let _ = writeln!(
                    w,
                    "tool profile: {} (toolset {})",
                    agent.wire_tool_profile().as_str(),
                    agent.tool_context_profile().as_str()
                );
            } else if let Some(profile) = ToolProfile::parse(arg) {
                agent.tool_profile = profile;
                agent.refresh_tools_for_context();
                let _ = writeln!(
                    w,
                    "tool profile -> {} (toolset {})",
                    agent.wire_tool_profile().as_str(),
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
                let base = history_char_budget_with_window(
                    agent.context_window_tokens(),
                    None,
                    agent.context_mode,
                    HISTORY_CHAR_BUDGET_END_TURN_PERCENT,
                );
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
            let _ = writeln!(w, "session: {}", agent.priced_session_usage().line());
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
            let _ = writeln!(w, "session usage: {}", agent.priced_session_usage().line());
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
            let _ = writeln!(w, "schemas: {}", agent.wire_tool_profile().as_str());
            let _ = writeln!(w, "toolset: {}", agent.tool_context_profile().as_str());
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
        "diagnostics" | "diag" => {
            let report = run_workflow_diagnostics(&agent.sandbox_root);
            let errors = report
                .diagnostics
                .iter()
                .filter(|d| d.severity == "error")
                .count();
            let warnings = report
                .diagnostics
                .iter()
                .filter(|d| d.severity == "warning")
                .count();
            agent
                .work_ledger
                .diagnostics
                .push(WorkflowDiagnosticRecord {
                    source: report.source.clone(),
                    status: report.status.clone(),
                    summary: workflow_diagnostic_summary(&report),
                    errors,
                    warnings,
                    duration_ms: millis_u64(report.duration),
                });
            if agent.work_ledger.diagnostics.len() > 12 {
                let excess = agent.work_ledger.diagnostics.len() - 12;
                agent.work_ledger.diagnostics.drain(0..excess);
            }
            let _ = write!(
                w,
                "{}",
                render_workflow_diagnostics(&report, SLASH_TEXT_CAP)
            );
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
            let args = parse_work_map_command_args(arg);
            match parse_work_map_filter_args(&args).and_then(|(selector, filters)| {
                let (_, map) = choose_work_map_source(&agent.sandbox_root, &selector, Some(agent))?;
                Ok((selector, filters, map))
            }) {
                Ok((selector, filters, map)) => emit_work_map_event(
                    agent.sink.as_mut(),
                    WorkMapEventKind::Map,
                    render_work_map(&map, &filters),
                    work_map_waypoint_ids_for_view(&map, &filters),
                    work_map_event_selector(&selector),
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
            if matches!(arg.trim(), "clear" | "off" | "reset") {
                agent.work_ledger.active_focus = None;
                agent.checkpoint_latest_session("after_focus_clear");
                let _ = writeln!(w, "focus cleared; full session history is active again");
            } else {
                let args = parse_work_map_command_args(arg);
                match parse_work_map_operation_args(&args, "current").and_then(
                    |(id, selector, mode_args)| {
                        let (_, map) =
                            choose_work_map_source(&agent.sandbox_root, selector, Some(agent))?;
                        let selection = parse_work_map_selection(id, &map)?;
                        Ok((map, selection, mode_args, selector))
                    },
                ) {
                    Ok((map, selection, mode_args, selector)) => {
                        let is_branch = mode_args.iter().any(|a| a == "--branch");
                        if is_branch {
                            let name = mode_args
                                .iter()
                                .position(|a| a == "--branch")
                                .and_then(|pos| mode_args.get(pos + 1))
                                .filter(|s| !s.starts_with("--"))
                                .map(|s| s.as_str());
                            match create_branch_text(agent, &map, &selection, name) {
                                Ok(text) => emit_work_map_event(
                                    agent.sink.as_mut(),
                                    WorkMapEventKind::Focus,
                                    text,
                                    work_map_waypoint_ids(&map),
                                    work_map_event_selector(selector),
                                ),
                                Err(e) => {
                                    let _ = writeln!(w, "[err] {e:#}");
                                }
                            }
                        } else if mode_args.is_empty() {
                            emit_work_map_event(
                                agent.sink.as_mut(),
                                WorkMapEventKind::Focus,
                                render_work_map_packet(&map, &selection),
                                work_map_waypoint_ids(&map),
                                work_map_event_selector(selector),
                            );
                        } else {
                            let mode = parse_focus_mode(&mode_args);
                            let text = activate_work_map_focus(agent, &map, &selection, &mode);
                            emit_work_map_event(
                                agent.sink.as_mut(),
                                WorkMapEventKind::Focus,
                                text,
                                work_map_waypoint_ids(&map),
                                work_map_event_selector(selector),
                            );
                        }
                    }
                    Err(e) => {
                        let _ = writeln!(
                            w,
                            "[err] {e:#}\nusage: /focus @wNN [--branch name|--exact|--carry=...] or /focus clear"
                        );
                    }
                }
            }
        }
        "tracks" | "branches" => emit_work_map_event(
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
                        |(id, selector, name, _)| {
                            let (_, map) =
                                choose_work_map_source(&agent.sandbox_root, selector, Some(agent))?;
                            let selection = parse_work_map_selection(id, &map)?;
                            Ok((map, selection, name, selector))
                        },
                    ) {
                        Ok((map, selection, name, selector)) => {
                            match create_branch_text(agent, &map, &selection, name) {
                                Ok(text) => emit_work_map_event(
                                    agent.sink.as_mut(),
                                    WorkMapEventKind::Focus,
                                    text,
                                    work_map_waypoint_ids(&map),
                                    work_map_event_selector(selector),
                                ),
                                Err(e) => {
                                    let _ = writeln!(
                                        w,
                                        "[err] {e:#}\nusage: /focus @wNN --branch [name]"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            let _ = writeln!(w, "[err] {e:#}\nusage: /focus @wNN --branch [name]");
                        }
                    }
                }
                _ => emit_work_map_event(
                    agent.sink.as_mut(),
                    WorkMapEventKind::Tracks,
                    render_tracks_listing(&agent.sandbox_root),
                    Vec::new(),
                    None,
                ),
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
                "map" | "packet" | "focus" | "tracks" | "track" | "branches" => {
                    let _ = writeln!(
                        w,
                        "use the top-level command instead: /map · /focus · /branches"
                    );
                }
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
                "brief" => {
                    let selector = if rest.is_empty() { "latest" } else { rest };
                    match resolve_session_selector(&agent.sandbox_root, selector)
                        .and_then(|source| read_session_jsonl(&source).map(|pair| (source, pair)))
                    {
                        Ok((source, (header, history))) => {
                            let analysis = analyze_session_history(&header, &history);
                            let _ =
                                write!(w, "{}", render_session_brief(&source, &header, &analysis));
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
                        "usage: /sessions [list|analyze|brief|map|packet|focus|tracks|grep|failures|verify-log|decisions]"
                    );
                }
            }
        }
        "hooks" => {
            if arg == "reload" {
                agent.hooks = Hooks::load(&agent.sandbox_root);
                agent.active_pack_hook_paths.clear();
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
        "undo" => {
            let mut parts = arg.splitn(2, char::is_whitespace);
            let first = parts.next().unwrap_or("");
            let second = parts.next().unwrap_or("").trim();
            if first == "--list" || first == "list" {
                match git_checkpoints::list_checkpoints(&agent.sandbox_root, 10) {
                    Ok(cps) if cps.is_empty() => {
                        let _ = writeln!(w, "no checkpoints");
                    }
                    Ok(cps) => {
                        for cp in &cps {
                            let _ = writeln!(
                                w,
                                "{}  {}  {}",
                                cp.id,
                                cp.tool_name,
                                cp.paths_hint.join(",")
                            );
                        }
                    }
                    Err(e) => {
                        let _ = writeln!(w, "error: {e}");
                    }
                }
            } else if first == "--prune" || first == "prune" {
                match git_checkpoints::prune(&agent.sandbox_root, None, None) {
                    Ok(msg) => {
                        let _ = writeln!(w, "{msg}");
                    }
                    Err(e) => {
                        let _ = writeln!(w, "error: {e}");
                    }
                }
            } else if first == "--apply" {
                match git_checkpoints::latest_checkpoint(&agent.sandbox_root) {
                    Ok(Some(cp)) => {
                        match git_checkpoints::restore_worktree(
                            &agent.sandbox_root,
                            &cp,
                            git_checkpoints::RestoreMode::Worktree,
                        ) {
                            Ok(msg) => {
                                let _ = writeln!(w, "{msg}");
                            }
                            Err(e) => {
                                let _ = writeln!(w, "error: {e}");
                            }
                        }
                    }
                    Ok(None) => {
                        let _ = writeln!(w, "no checkpoints");
                    }
                    Err(e) => {
                        let _ = writeln!(w, "error: {e}");
                    }
                }
            } else if !first.is_empty() && first != "--preview" {
                // Specific checkpoint id
                let apply = second == "--apply";
                match git_checkpoints::find_checkpoint(&agent.sandbox_root, first) {
                    Ok(Some(cp)) => {
                        let mode = if apply {
                            git_checkpoints::RestoreMode::Worktree
                        } else {
                            git_checkpoints::RestoreMode::Preview
                        };
                        match git_checkpoints::restore_worktree(&agent.sandbox_root, &cp, mode) {
                            Ok(msg) => {
                                let _ = writeln!(w, "{msg}");
                            }
                            Err(e) => {
                                let _ = writeln!(w, "error: {e}");
                            }
                        }
                    }
                    Ok(None) => {
                        let _ = writeln!(w, "checkpoint not found: {first}");
                    }
                    Err(e) => {
                        let _ = writeln!(w, "error: {e}");
                    }
                }
            } else {
                // Default: preview latest
                match git_checkpoints::latest_checkpoint(&agent.sandbox_root) {
                    Ok(Some(cp)) => {
                        match git_checkpoints::preview_restore(&agent.sandbox_root, &cp) {
                            Ok(msg) => {
                                let _ = writeln!(w, "{msg}");
                            }
                            Err(e) => {
                                let _ = writeln!(w, "error: {e}");
                            }
                        }
                    }
                    Ok(None) => {
                        let _ = writeln!(w, "no checkpoints");
                    }
                    Err(e) => {
                        let _ = writeln!(w, "error: {e}");
                    }
                }
            }
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
            Block::Thinking { text, .. } => text.len(),
            Block::RedactedThinking { data } => data.len(),
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
    indexed.sort_by_key(|(_, tokens)| std::cmp::Reverse(*tokens));
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
                Block::RedactedThinking { .. } => "redacted_thinking",
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
                | "give"
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

fn message_has_queued_update_marker(message: &Message) -> bool {
    message.role == "user"
        && message.content.iter().any(|block| {
            block_text(block).is_some_and(|text| {
                let text = text.trim_start();
                text.starts_with("[queued-user-update]") || text.starts_with("[queued-update]")
            })
        })
}

fn assistant_text_after_latest_queued_update(history: &[Message]) -> Option<String> {
    let start = history.iter().rposition(message_has_queued_update_marker)?;
    Some(assistant_text(&history[start + 1..]))
}

fn steering_item_acknowledged(item: &str, history: &[Message]) -> bool {
    let assistant = assistant_text_after_latest_queued_update(history)
        .unwrap_or_else(|| assistant_text(history))
        .to_ascii_lowercase();
    if assistant.trim().is_empty() {
        return false;
    }
    let keywords = steering_item_keywords(item);
    if item.to_ascii_lowercase().contains("web recipe") && assistant.contains("web recipe") {
        return true;
    }
    if keywords.is_empty() {
        return assistant.contains("steer") || assistant.contains("queued guidance");
    }
    let hits = keywords
        .iter()
        .filter(|keyword| assistant.contains(keyword.as_str()))
        .count();
    hits >= keywords.len().min(2)
}

#[cfg(test)]
fn final_objective_warning(
    objective: &orchestrator::ObjectiveTracker,
    history: &[Message],
) -> Option<String> {
    let coverage = objective.assess_history(history);
    if coverage.unresolved.is_empty() {
        None
    } else {
        Some(final_objective_warning_from_coverage(&coverage))
    }
}

const ACTION_CONTRACT_MUTATING_TOOL_NAMES: &[&str] = &["edit_file", "multi_edit", "write_file"];

fn block_text(block: &Block) -> Option<&str> {
    match block {
        Block::Text { text } | Block::PartialStream { text } => Some(text.as_str()),
        _ => None,
    }
}

fn assistant_blocks_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(block_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_text_has_implementation_commitment(text: &str) -> bool {
    let lower = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    !lower.is_empty()
        && [
            "i'll implement",
            "i will implement",
            "i’ll implement",
            "i'll apply",
            "i will apply",
            "i’ll apply",
            "applying patch",
            "apply the patch",
            "applying the patch",
            "making changes now",
            "make the patch now",
            "make changes now",
            "i'll patch",
            "i will patch",
            "i’ll patch",
            "patching now",
            "editing now",
            "i'll edit",
            "i will edit",
            "i’ll edit",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
}

pub(crate) fn text_line_looks_like_pseudo_tool_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    lower.trim_end() == "to="
        || lower.trim_end() == "to ="
        || lower.starts_with("to=functions")
        || lower.starts_with("to = functions")
        || lower.starts_with("to=multi_tool_use")
        || lower.starts_with("to = multi_tool_use")
        || lower.starts_with("functions.")
        || lower.starts_with("multi_tool_use.")
        || lower.starts_with("<|tool")
        || lower.starts_with("<tool")
        || lower.starts_with("tool_use")
        || lower.starts_with("function_call")
        || compact.starts_with("{\"recipient")
        || compact.starts_with("{\"type\":\"function")
        || compact.starts_with("{\"command")
}

pub(crate) fn text_line_looks_like_pseudo_tool_syntax(line: &str) -> bool {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    lower.starts_with("to=functions")
        || lower.starts_with("to = functions")
        || lower.starts_with("to=multi_tool_use")
        || lower.starts_with("to = multi_tool_use")
        || lower.starts_with("functions.")
        || lower.starts_with("multi_tool_use.")
        || lower.starts_with("<|tool")
        || lower.starts_with("<tool")
        || lower.starts_with("tool_use")
        || lower.starts_with("tool call:") && lower.contains("functions.")
        || lower.starts_with("function_call")
        || lower.contains("to=functions.")
        || lower.contains("to = functions.")
        || lower.contains("to=multi_tool_use.")
        || lower.contains("to = multi_tool_use.")
        || compact.contains("\"recipient_name\":\"functions.")
        || compact.contains("\"recipient_name\":\"multi_tool_use.")
        || compact.contains("\"parameters\":{\"command\"")
        || compact.contains("\"input\":{\"command\"")
        || compact.starts_with("{\"command\":")
        || compact.contains("\"type\":\"function_call\"")
        || (compact.contains("\"name\":\"bash\"")
            && compact.contains("\"arguments\"")
            && compact.contains("\"command\""))
}

pub(crate) fn text_contains_pseudo_tool_syntax(text: &str) -> bool {
    text.lines().any(text_line_looks_like_pseudo_tool_syntax)
}

fn text_contains_pseudo_tool_syntax_for_context(text: &str, context_mode: ContextMode) -> bool {
    if context_mode.is_frugal() {
        text.lines().any(|line| {
            text_line_looks_like_pseudo_tool_syntax(line)
                || text_line_looks_like_pseudo_tool_start(line)
        })
    } else {
        text_contains_pseudo_tool_syntax(text)
    }
}

fn blocks_contain_pseudo_tool_syntax(blocks: &[Block]) -> bool {
    blocks
        .iter()
        .filter_map(block_text)
        .any(text_contains_pseudo_tool_syntax)
}

fn blocks_contain_pseudo_tool_syntax_for_context(
    blocks: &[Block],
    context_mode: ContextMode,
) -> bool {
    if context_mode.is_frugal() {
        blocks
            .iter()
            .filter_map(block_text)
            .any(|text| text_contains_pseudo_tool_syntax_for_context(text, context_mode))
    } else {
        blocks_contain_pseudo_tool_syntax(blocks)
    }
}

fn action_contract_runtime_note(no_mutation_turns: u32) -> String {
    format!(
        "runtime guidance: action contract active because the assistant committed to implement/apply changes. This is invalid progress after {no_mutation_turns} assistant response(s) without a successful file mutation. Keep looping; next assistant message must use a real file-mutating tool_use ({} or a bash command that mutates files). Text-only blocked statements do not clear this contract.",
        ACTION_CONTRACT_MUTATING_TOOL_NAMES.join("|")
    )
}

fn pseudo_tool_runtime_note() -> String {
    "runtime guidance: pseudo-tool syntax emitted as plain text is invalid progress. Use an actual provider tool_use block, or state a concise blocked reason if no tool can run.".to_string()
}

fn final_objective_warning_from_coverage(coverage: &orchestrator::ObjectiveCoverage) -> String {
    format!(
        "final objective warning: unresolved checkpoint(s): {}. If these are complete, mention the evidence; otherwise say what is blocked or still pending.",
        coverage.unresolved.join("; ")
    )
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
        timeout_from_env("DEXT_EVAL_TIMEOUT_SECS", 15),
    )?;
    let duration = started_at.elapsed();
    let stdout = stdout.render("stdout");
    let stderr = stderr.render("stderr");
    let combined = format!("--- stdout ---\n{stdout}--- stderr ---\n{stderr}");
    let _ = write_verification_artifact(
        root,
        "eval",
        VerificationArtifactSpec {
            name: "eval-command",
            command,
            output: &combined,
            exit_code: Some(code),
            duration,
            status: if code == 0 { "passed" } else { "failed" },
        },
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
            task: "Edit notes.md in place: replace 'world' with 'dext'. Do not create a new file.",
            setup: |p| {
                std::fs::write(p.join("notes.md"), "hello world\n")?;
                Ok(())
            },
            configure: noop_eval_config,
            assertions: &[
                Assertion::ToolCalledAny(&["edit_file", "multi_edit"]),
                Assertion::ToolNotCalled("write_file"),
                Assertion::FileEquals("notes.md", "hello dext\n"),
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
    pub(crate) resume_selector: Option<String>,
    pub(crate) no_session: bool,
    pub(crate) no_tui: bool,
    pub(crate) trust_mode: bool,
    pub(crate) no_trust_mode: bool,
    pub(crate) output: OutputMode,
    pub(crate) cd: Option<PathBuf>,
    pub(crate) fork: bool,
    pub(crate) budget_cap: Option<BudgetCap>,
    pub(crate) approval_profile: Option<ApprovalProfile>,
    pub(crate) sandbox_profile: Option<SandboxProfile>,
    pub(crate) browser_recipe: Option<BrowserRecipe>,
    pub(crate) thinking_effort: Option<ThinkingEffort>,
    pub(crate) context_mode: Option<ContextMode>,
    pub(crate) tool_context_profile: Option<ToolContextProfile>,
    pub(crate) tool_profile: Option<ToolProfile>,
    pub(crate) preview_mode: Option<MutationPreviewMode>,
    pub(crate) pack: Option<String>,
}

pub(crate) fn parse_cli_options(argv: Vec<String>) -> Result<CliOptions> {
    let mut positional = Vec::new();
    let mut print = false;
    let mut resume_latest = false;
    let mut resume_selector: Option<String> = None;
    let mut no_session = false;
    let mut no_tui = false;
    let mut trust_mode = false;
    let mut no_trust_mode = false;
    let mut output = OutputMode::Text;
    let mut cd: Option<PathBuf> = None;
    let mut fork = false;
    let mut budget_cap: Option<BudgetCap> = None;
    let mut approval_profile: Option<ApprovalProfile> = None;
    let mut sandbox_profile: Option<SandboxProfile> = None;
    let mut browser_recipe: Option<BrowserRecipe> = None;
    let mut thinking_effort: Option<ThinkingEffort> = None;
    let mut context_mode: Option<ContextMode> = None;
    let mut tool_context_profile: Option<ToolContextProfile> = None;
    let mut tool_profile: Option<ToolProfile> = None;
    let mut preview_mode: Option<MutationPreviewMode> = None;
    let mut pack: Option<String> = None;
    let mut i = 0usize;
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "-p" | "--print" => print = true,
            "--resume" => resume_latest = true,
            "--no-session" => no_session = true,
            "--no-tui" => no_tui = true,
            "--trust" => {
                trust_mode = true;
                no_trust_mode = false;
            }
            "--no-trust" => {
                no_trust_mode = true;
                trust_mode = false;
            }
            "--fork" => fork = true,
            "--frugal" => {
                context_mode = Some(ContextMode::Frugal);
                tool_profile = Some(ToolProfile::Lean);
                if thinking_effort.is_none() {
                    thinking_effort = Some(ThinkingEffort::Medium);
                }
            }
            "--tiny" => {
                context_mode = Some(ContextMode::Tiny);
                tool_profile = Some(ToolProfile::Lean);
                if thinking_effort.is_none() {
                    thinking_effort = Some(ThinkingEffort::Medium);
                }
            }
            "--context-mode" => {
                i += 1;
                let value = argv.get(i).ok_or_else(|| {
                    anyhow::anyhow!("--context-mode requires standard|frugal|tiny")
                })?;
                context_mode = Some(ContextMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid --context-mode '{value}' (expected standard|frugal|tiny)"
                    )
                })?);
            }
            "--toolset" | "--tool-context-profile" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--toolset requires default|full"))?;
                tool_context_profile =
                    Some(ToolContextProfile::parse_selectable(value).ok_or_else(|| {
                        anyhow::anyhow!("invalid --toolset '{value}' (expected default|full)")
                    })?);
            }
            "--tool-profile" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--tool-profile requires lean|full"))?;
                tool_profile = Some(ToolProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --tool-profile '{value}' (expected lean|full)")
                })?);
            }
            "--preview" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--preview requires off|simple|git"))?;
                preview_mode = Some(MutationPreviewMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --preview '{value}' (expected off|simple|git)")
                })?);
            }
            "--pack" => {
                i += 1;
                let value = argv
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--pack requires a pack name"))?;
                pack = Some(value.clone());
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
                let value = argv.get(i).ok_or_else(|| {
                    anyhow::anyhow!("--browser requires off|agent-browser|agentbrowser")
                })?;
                browser_recipe = Some(BrowserRecipe::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid --browser '{value}' (expected off|agent-browser|agentbrowser)"
                    )
                })?);
            }
            "--agent-browser" => browser_recipe = Some(BrowserRecipe::AgentBrowser),
            "--effort" | "--thinking-effort" => {
                i += 1;
                let value = argv.get(i).ok_or_else(|| {
                    anyhow::anyhow!("--effort requires off|low|medium|high|xhigh|max")
                })?;
                thinking_effort = Some(ThinkingEffort::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid thinking effort '{value}' (expected off|low|medium|high|xhigh|max)"
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
            _ if arg.starts_with("--resume=") => {
                resume_latest = true;
                let value = arg.trim_start_matches("--resume=").trim();
                if !value.is_empty() {
                    resume_selector = Some(value.to_string());
                }
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
                        "invalid thinking effort '{value}' (expected off|low|medium|high|xhigh|max)"
                    )
                })?);
            }
            _ if arg.starts_with("--context-mode=") => {
                let value = arg.trim_start_matches("--context-mode=");
                context_mode = Some(ContextMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid context mode '{value}' (expected standard|frugal|tiny)"
                    )
                })?);
            }
            _ if arg.starts_with("--pack=") => {
                let value = arg.trim_start_matches("--pack=");
                if value.trim().is_empty() {
                    anyhow::bail!("--pack requires a pack name");
                }
                pack = Some(value.to_string());
            }
            _ if arg.starts_with("--toolset=") || arg.starts_with("--tool-context-profile=") => {
                let value = arg
                    .strip_prefix("--toolset=")
                    .or_else(|| arg.strip_prefix("--tool-context-profile="))
                    .unwrap_or_default();
                tool_context_profile =
                    Some(ToolContextProfile::parse_selectable(value).ok_or_else(|| {
                        anyhow::anyhow!("invalid toolset '{value}' (expected default|full)")
                    })?);
            }
            _ if arg.starts_with("--tool-profile=") => {
                let value = arg.trim_start_matches("--tool-profile=");
                tool_profile = Some(ToolProfile::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid tool profile '{value}' (expected lean|full)")
                })?);
            }
            _ if arg.starts_with("--preview=") => {
                let value = arg.trim_start_matches("--preview=");
                preview_mode = Some(MutationPreviewMode::parse(value).ok_or_else(|| {
                    anyhow::anyhow!("invalid --preview '{value}' (expected off|simple|git)")
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
                    anyhow::anyhow!(
                        "invalid browser recipe '{value}' (expected off|agent-browser|agentbrowser)"
                    )
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
        resume_selector,
        no_session,
        no_tui,
        trust_mode,
        no_trust_mode,
        output,
        cd,
        fork,
        budget_cap,
        approval_profile,
        sandbox_profile,
        browser_recipe,
        thinking_effort,
        context_mode,
        tool_context_profile,
        tool_profile,
        preview_mode,
        pack,
    })
}

fn env_flag_default(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            !(t.is_empty() || t == "0" || t == "false" || t == "off" || t == "no")
        }
        Err(_) => default,
    }
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
            eprintln!("[dext crash snapshot: {}]", path.display());
        }
        default_panic(info);
    }));

    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        println!("dext {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if argv.first().is_some_and(|a| a == "pack" || a == "packs") {
        let root = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        let mut sub_idx = 1usize;
        let mut leading_verbose = false;
        while argv
            .get(sub_idx)
            .is_some_and(|a| matches!(a.as_str(), "--verbose" | "-v" | "--paths"))
        {
            leading_verbose = true;
            sub_idx += 1;
        }
        let sub = argv.get(sub_idx).map(String::as_str).unwrap_or("list");
        match sub {
            "" | "list" | "ls" => {
                let verbose = leading_verbose
                    || argv
                        .iter()
                        .skip(sub_idx + 1)
                        .any(|a| a == "--verbose" || a == "-v" || a == "--paths");
                println!("{}", packs::render_pack_listing_opts(&root, verbose));
                return Ok(());
            }
            "inspect" | "info" | "show" => {
                let Some(selector) = argv.get(sub_idx + 1) else {
                    eprintln!("usage: dext pack inspect <name>");
                    release_registered_locks();
                    std::process::exit(2);
                };
                println!("{}", packs::render_pack_inspect(&root, selector)?);
                return Ok(());
            }
            "run" | "use" | "start" => {
                if argv.len() < sub_idx + 3 {
                    eprintln!("usage: dext pack run <name> <task>");
                    release_registered_locks();
                    std::process::exit(2);
                }
                let mut forwarded = argv[(sub_idx + 2)..].to_vec();
                forwarded.insert(0, "--pack".to_string());
                forwarded.insert(1, argv[sub_idx + 1].clone());
                argv = forwarded;
            }
            _ => {
                eprintln!("usage: dext pack [list|inspect <name>|run <name> <task>]");
                release_registered_locks();
                std::process::exit(2);
            }
        }
    }
    if argv.first().is_some_and(|a| a == "shelf" || a == "shelves") {
        let root = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        println!(
            "{}",
            shelves::render_registry_listing(&shelves::ShelfRegistry::discover(&root))
        );
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
    if argv.first().is_some_and(|a| a == "undo") {
        let root = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        let code = handle_undo_cli(&argv[1..], &root);
        release_registered_locks();
        std::process::exit(code);
    }
    if argv.first().is_some_and(|a| a == "memory") {
        let root = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        let code = handle_memory_cli(&argv[1..], &root);
        release_registered_locks();
        std::process::exit(code);
    }
    if argv.first().is_some_and(|a| a == "doctor") {
        let root = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        let code = handle_doctor_cli(&root);
        release_registered_locks();
        std::process::exit(code);
    }
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: dext [TASK...]        run one-shot with TASK (joined with spaces)");
        println!("       dext -p               read task from stdin, run one-shot");
        println!(
            "       dext --resume[=NAME|PATH]  resume the newest auto-saved session or selector"
        );
        println!("       dext sessions         list latest + autosaved/named sessions");
        println!("       dext session brief [latest|NAME|PATH]  distilled continuation packet");
        println!("       dext session map [latest|NAME|PATH]");
        println!("       dext session packet [latest|NAME|PATH] @wNN");
        println!("       dext session focus [latest|NAME|PATH] @wNN [--exact]");
        println!("       dext session tracks");
        println!("       dext session track open [latest|NAME|PATH] @wNN [name]");
        println!("       dext session export [latest|NAME|PATH] [html|jsonl] [OUT]");
        println!("       dext doctor           check environment, sandbox, providers, and tools");
        println!("       dext pack list|inspect|run ...  discover or invoke Dext packs");
        println!(
            "       dext shelves                      list typed shelf manifests and ability metadata"
        );
        println!("       dext --pack NAME TASK  invoke a pack in one-shot mode");
        println!("       dext session analyze|grep|failures|verify-log|decisions [session]");
        println!("       dext --no-session     run without autosaved session/log writes");
        println!("       dext --fork           resume latest into an isolated, unsaved branch");
        println!("       dext --cd DIR         use DIR as sandbox/cwd");
        println!("       dext --output json|stream-json  emit machine-readable output");
        println!(
            "       dext --budget CAP     stop before more model calls once CAP is reached ($ or tokens)"
        );
        println!("       dext --approval ask|auto-read|auto-write|never|always");
        println!("       dext --preview off|simple|git  mutation preview mode");
        println!("       dext --sandbox read-only|workspace-write|danger-full-access");
        println!("       dext --browser agent-browser    add optional browser automation recipe");
        println!(
            "       dext --effort off|low|medium|high|xhigh|max  set provider reasoning effort"
        );
        println!(
            "       dext --frugal        minimize prompt/tool/history context for lower token cost"
        );
        println!("       dext --tiny          extra-light context mode with condensed prompt");
        println!("       dext --context-mode standard|frugal|tiny");
        println!("       dext --toolset default|full  choose provider-visible tool count profile");
        println!("       dext --tool-context-profile default|full  alias for --toolset");
        println!(
            "       dext --tool-profile lean|full  choose provider tool schema verbosity (default lean)"
        );
        println!("       dext --eval [NAME]    run eval harness (optionally a single case)");
        println!("       dext --trust          auto-approve privileged tools");
        println!("       dext --no-trust       opt out of default trust mode");
        println!("       dext auth ...         provider/model/auth management commands");
        println!("       dext undo --list      list recent Dext checkpoints");
        println!("       dext undo --preview <id>  non-interactive preview");
        println!("       dext undo --apply <id>   non-interactive apply");
        println!("       dext memory check    check memory merge registration");
        println!("       dext memory register register merge drivers (local)");
        println!("       dext memory distill [--apply]  dedupe recall.md, flag stale bullets");
        println!("       dext memory merge [--recall] <base> <ours> <theirs>");
        println!("       dext                  interactive REPL (or reads stdin if piped)");
        println!(
            "env:   DEXT_PROVIDER, DEXT_PROFILE, DEXT_MODEL, DEXT_MODEL_<PROVIDER>, DEXT_MODEL_FORCE=1, DEXT_BASE_URL, DEXT_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY, CHATGPT_ACCESS_TOKEN, ZAI_API_KEY, ANTHROPIC_BASE_URL, OPENAI_BASE_URL, DEXT_SYSTEM, DEXT_SANDBOX, DEXT_EXTERNAL_TIMEOUT_SECS, DEXT_BASH_TIMEOUT_SECS, DEXT_HOOK_TIMEOUT_SECS, DEXT_SESSIONS_DIR, DEXT_LOGS_DIR, DEXT_LOG_ARCHIVES (0-16 rotated archives of latest.log; default 0 keeps truncation-only), DEXT_TRUST=0 to opt out of default trust, DEXT_NO_TUI=1, DEXT_THINKING_EFFORT=off|low|medium|high|xhigh|max, DEXT_CONTEXT_MODE=standard|frugal|tiny, DEXT_TOOLSET=default|full, DEXT_TOOL_PROFILE=lean|full, DEXT_MUTATION_PREVIEW=off|simple|git, DEXT_BUDGET_CAP, DEXT_APPROVAL, DEXT_SANDBOX_PROFILE, DEXT_PACKS_DIR, DEXT_SHELVES_DIR, DEXT_BROWSER_RECIPE"
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
    let trust_mode =
        opts.trust_mode || (!opts.no_trust_mode && env_flag_default("DEXT_TRUST", true));

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
    agent.prewarm_connection();
    if let Some(profile) = std::env::var("DEXT_APPROVAL")
        .ok()
        .and_then(|v| ApprovalProfile::parse(&v))
    {
        agent.set_approval_profile(profile);
    }
    if let Some(profile) = std::env::var("DEXT_SANDBOX_PROFILE")
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
        agent.set_context_mode(mode);
    }
    if let Some(profile) = opts.tool_context_profile {
        agent.tool_context_profile = profile.effective(agent.context_mode);
    }
    if let Some(profile) = opts.tool_profile {
        agent.tool_profile = profile;
    }
    if let Some(mode) = opts.preview_mode {
        agent.preview_mode = mode;
    }
    agent.refresh_tools_for_context();
    if opts.fork {
        agent.suppress_checkpoints = true;
        agent.session_enabled = false;
        agent.state_lock = None;
        agent.latest_session_path = project_latest_session_path(&agent.sandbox_root);
    }
    if opts.output.is_json() {
        agent.pretty = false;
        agent.set_sink(Box::new(JsonSink::new(opts.output, false, false)));
    }
    if opts.resume_latest || opts.fork {
        let loaded = if let Some(selector) = opts.resume_selector.as_deref() {
            agent.load_session(selector)
        } else {
            agent.load_latest_session()
        };
        match loaded {
            Ok(path) => {
                let configured_context_mode = opts.context_mode.or_else(|| {
                    std::env::var("DEXT_CONTEXT_MODE")
                        .ok()
                        .and_then(|value| ContextMode::parse(&value))
                });
                if let Some(mode) = configured_context_mode {
                    agent.set_context_mode(mode);
                }
                if let Some(profile) = opts.tool_context_profile {
                    agent.tool_context_profile = profile.effective(agent.context_mode);
                }
                if let Some(profile) = opts.tool_profile {
                    agent.tool_profile = profile;
                }
                agent.refresh_tools_for_context();
                if opts.fork {
                    agent.suppress_checkpoints = true;
                    agent.session_enabled = false;
                    agent.state_lock = None;
                    agent.latest_session_path = project_latest_session_path(&agent.sandbox_root);
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
                eprintln!("[error] failed to resume session: {e:#}");
                release_registered_locks();
                std::process::exit(1);
            }
        }
    }
    if trust_mode {
        let _ = agent.set_trust_mode(true);
    }
    if !opts.output.is_json() {
        if let Some(cap) = agent.budget_cap {
            eprintln!("[budget] cap {}", cap.line());
        }
        if agent.sandbox_profile() != SandboxProfile::WorkspaceWrite {
            eprintln!("[sandbox] profile {}", agent.sandbox_profile().as_str());
        }
        if agent.tool_context_profile() != ToolContextProfile::Default
            && !agent.context_mode.is_frugal()
        {
            eprintln!("[tools] toolset {}", agent.tool_context_profile().as_str());
        }
        if agent.browser_recipe() != BrowserRecipe::Disabled {
            eprintln!("[browser] recipe {}", agent.browser_recipe().as_str());
        }
    }

    if let Some(pack_name) = opts.pack.clone() {
        let task = one_shot_task.clone().unwrap_or_default();
        if task.trim().is_empty() {
            eprintln!("usage: dext --pack <name> <task>");
            release_registered_locks();
            std::process::exit(2);
        }
        let result = agent.run_pack(&pack_name, &task).await;
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

    let use_tui = !opts.no_tui
        && !opts.output.is_json()
        && std::env::var("DEXT_NO_TUI").is_err()
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

    let (runtime_control_tx, runtime_control_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let agent_busy_flag: std::sync::Arc<std::sync::atomic::AtomicBool> =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    agent.install_runtime_controls(runtime_control_rx, runtime_control_tx.clone());
    agent.install_steering(steer_rx, steer_tx.clone());
    if opts.fork {
        println!(
            "fork: autosave disabled; use /save <name> or /export [path] to keep this branch."
        );
    }
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let input_tx = input_tx.clone();
        let runtime_control_tx = runtime_control_tx.clone();
        let steering_tx = steer_tx.clone();
        let busy = agent_busy_flag.clone();
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut pending_secret_send: Option<String> = None;
            loop {
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        match route_interactive_input_line(
                            line,
                            &busy,
                            &input_tx,
                            &runtime_control_tx,
                            &steering_tx,
                            &mut pending_secret_send,
                        ) {
                            InteractiveInputRoute::RuntimeControlQueued => {
                                eprintln!("[runtime control queued]");
                            }
                            InteractiveInputRoute::SteeringQueued => {
                                eprintln!("[queued for next response]");
                            }
                            InteractiveInputRoute::UnsupportedBusySlash(text) => {
                                eprintln!("{}", unsupported_busy_slash_message(&text));
                            }
                            InteractiveInputRoute::SecretWithheld => {
                                eprintln!(
                                    "[input looks like a credential and was NOT sent; if a command needs auth, let Dext open its masked prompt. Repeat the exact line to send anyway]"
                                );
                            }
                            InteractiveInputRoute::Dropped if busy.load(Ordering::SeqCst) => {
                                eprintln!(
                                    "[input withheld: use the local auth prompt for sudo/auth secrets]"
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
    }

    let mut stdout = io::stdout();

    println!("dext — chat loop with tools. /help for commands, empty line or Ctrl+D to exit.");
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
            if is_active_runtime_control_command(&input) {
                for command in parse_active_runtime_control_sequence(&input)
                    .unwrap_or_else(|| vec![input.clone()])
                {
                    let _ = agent.runtime_control_sender().send(command);
                }
                println!("[runtime control queued]");
                autosave_latest(&mut agent);
                continue;
            }
            if text_is_potential_local_secret(&input) {
                eprintln!("[input withheld: use the local auth prompt for sudo/auth secrets]");
                autosave_latest(&mut agent);
                continue;
            }
            if is_slash_command(&input) {
                eprintln!("{}", unsupported_busy_slash_message(&input));
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
                    let base = history_char_budget_with_window(
                        agent.context_window_tokens(),
                        None,
                        agent.context_mode,
                        HISTORY_CHAR_BUDGET_END_TURN_PERCENT,
                    );
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

        if input == "/pack"
            || input.starts_with("/pack ")
            || input == "/packs"
            || input.starts_with("/packs ")
        {
            let raw = input
                .trim_start_matches("/packs")
                .trim_start_matches("/pack")
                .trim();
            let mut parts = raw.splitn(3, char::is_whitespace);
            let sub = parts.next().unwrap_or("");
            if matches!(sub, "run" | "use" | "start") {
                let selector = parts.next().unwrap_or("").trim();
                let task = parts.next().unwrap_or("").trim();
                if selector.is_empty() || task.is_empty() {
                    println!("usage: /pack run <name> <task>");
                } else {
                    agent_busy_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    if let Err(e) = agent.run_pack(selector, task).await {
                        eprintln!("[pack error] {e:#}");
                    }
                    agent_busy_flag.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                autosave_latest(&mut agent);
                continue;
            }
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
