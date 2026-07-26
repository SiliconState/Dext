//! Crash snapshots: a panic hook writes a redacted, owner-only JSON record of
//! the last events and the current session so a crash can be diagnosed without
//! asking the user to reproduce it.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use crate::{
    AgentEvent, LATEST_SESSION_NAME, atomic_write_secret, dext_state_dir, io, sha256_hex_str,
    unix_timestamp_secs,
};

fn panic_payload_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
}

pub(crate) fn panic_message_is_broken_pipe(message: &str) -> bool {
    (message.starts_with("failed printing to stdout: ")
        || message.starts_with("failed printing to stderr: "))
        && message.contains("Broken pipe")
}

pub(crate) fn panic_info_is_broken_pipe(info: &std::panic::PanicHookInfo<'_>) -> bool {
    panic_payload_text(info.payload()).is_some_and(panic_message_is_broken_pipe)
}

#[derive(Default)]
pub(crate) struct CrashRuntimeState {
    pub(crate) current_session_id: Option<String>,
    pub(crate) last_event_ids: Vec<String>,
}

pub(crate) fn crash_runtime_state() -> &'static Mutex<CrashRuntimeState> {
    static STATE: OnceLock<Mutex<CrashRuntimeState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(CrashRuntimeState::default()))
}

pub(crate) fn generated_session_id_from_path(path: &Path) -> Option<String> {
    if path.file_stem()?.to_str()? != LATEST_SESSION_NAME || path.extension()?.to_str()? != "jsonl"
    {
        return None;
    }
    let candidate = path.parent()?.file_name()?.to_str()?;
    let mut parts = candidate.split('-');
    let timestamp = parts.next()?;
    let pid = parts.next()?;
    let nonce = parts.next()?;
    if parts.next().is_some()
        || timestamp.parse::<u64>().is_err()
        || pid.parse::<u32>().is_err()
        || nonce.len() != 12
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(candidate.to_string())
}

pub(crate) fn record_crash_session_id(path: &Path) {
    let session_id = generated_session_id_from_path(path);
    if let Ok(mut state) = crash_runtime_state().lock() {
        state.current_session_id = session_id;
    }
}

pub(crate) fn crash_event_breadcrumb(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::TurnStart => Some("turn_start".to_string()),
        AgentEvent::ToolCallPreview { .. } => Some("tool_preview".to_string()),
        AgentEvent::ToolCallStart { .. } => Some("tool_start".to_string()),
        AgentEvent::ToolCallResult { ok, .. } => Some(format!("tool_result:ok={ok}")),
        AgentEvent::ToolOutputDelta { .. } => None,
        AgentEvent::ToolBatchStart { call_ids, .. } => {
            Some(format!("tool_batch_start:count={}", call_ids.len()))
        }
        AgentEvent::ToolBatchEnd {
            call_ids, failed, ..
        } => Some(format!(
            "tool_batch_end:count={}:failed={failed}",
            call_ids.len()
        )),
        AgentEvent::HttpRetry { attempt, .. } => Some(format!("http_retry:{attempt}")),
        AgentEvent::CompactStart => Some("compact_start".to_string()),
        AgentEvent::CompactEnd { before, after } => Some(format!("compact_end:{before}->{after}")),
        AgentEvent::CompactFailed { .. } => Some("compact_failed".to_string()),
        AgentEvent::Interrupted => Some("interrupted".to_string()),
        AgentEvent::RuntimeControl(_) => Some("runtime_control".to_string()),
        AgentEvent::RuntimeControlApplied {
            commands,
            model_changed,
            effort_changed,
            mode_changed,
            stream_aborted,
        } => Some(format!(
            "runtime_control_applied:{commands}:model={model_changed}:effort={effort_changed}:mode={mode_changed}:abort={stream_aborted}"
        )),
        AgentEvent::SteeringReceived { messages, .. } => {
            Some(format!("steering:messages={messages}"))
        }
        AgentEvent::TurnEnd { failed, .. } => Some(if *failed {
            "turn_end:failed".to_string()
        } else {
            "turn_end".to_string()
        }),
        _ => None,
    }
}

pub(crate) fn record_crash_event(event: &AgentEvent) {
    let Some(label) = crash_event_breadcrumb(event) else {
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

pub(crate) fn crash_snapshot_body(id: &str, location: Option<(&str, u32, u32)>) -> Value {
    let runtime = crash_runtime_state().try_lock().ok().map(|state| {
        json!({
            "current_session_id": state.current_session_id,
            "last_event_ids": state.last_event_ids,
            "input_buffer_state": null,
            "active_modal": null,
        })
    });
    let parse_terminal_dimension = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
    };
    let backtrace_enabled = std::env::var("RUST_BACKTRACE")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "full"));
    let location = location.map(|(file, line, column)| {
        json!({
            "file_sha256": sha256_hex_str(file),
            "line": line,
            "column": column,
        })
    });
    json!({
        "id": id,
        "panic": "panic captured; free-form payload omitted",
        "location": location,
        "terminal": {
            "columns": parse_terminal_dimension("COLUMNS"),
            "lines": parse_terminal_dimension("LINES"),
        },
        "pid": std::process::id(),
        "runtime": runtime,
        "backtrace_enabled": backtrace_enabled,
    })
}

fn ensure_private_crash_dir(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "crash directory is not a real directory: {}",
                    path.display()
                ),
            ));
        }
        #[cfg(unix)]
        Ok(metadata)
            if {
                use std::os::unix::fs::MetadataExt as _;
                metadata.uid() != unsafe { libc::geteuid() }
            } =>
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "crash directory is not owned by the current user",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder.create(path)?;
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn write_private_crash_snapshot(path: &Path, body: &Value) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "crash path has no parent"))?;
    ensure_private_crash_dir(parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "crash snapshot path is not a regular file: {}",
                    path.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let bytes = serde_json::to_vec_pretty(body).map_err(io::Error::other)?;
    atomic_write_secret(path, &bytes)
}

pub(crate) fn new_crash_id() -> Option<String> {
    let mut nonce = [0u8; 6];
    getrandom::fill(&mut nonce).ok()?;
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(format!(
        "crash-{}-{}-{nonce}",
        unix_timestamp_secs(),
        std::process::id()
    ))
}

pub(crate) fn crash_snapshot_notice(id: &str) -> String {
    format!("[dext crash snapshot id: {id}]")
}

pub(crate) fn write_crash_snapshot(info: &std::panic::PanicHookInfo<'_>) -> Option<String> {
    let id = new_crash_id()?;
    let path = dext_state_dir().join("crashes").join(format!("{id}.json"));
    let location = info
        .location()
        .map(|loc| (loc.file(), loc.line(), loc.column()));
    let body = crash_snapshot_body(&id, location);
    write_private_crash_snapshot(&path, &body).ok()?;
    Some(id)
}
