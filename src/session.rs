use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::{
    DEFAULT_SYSTEM, LATEST_LOG_ARCHIVE_MAX, LATEST_LOG_CAP, LATEST_SESSION_NAME, LOG_DETAIL_CAP,
    PROJECT_STATE_LOCK_NAME, PROJECT_STATE_LOCK_STALE_SECS, SessionHeader, ThinkingEffort,
    byte_prefix_at_char_boundary, byte_suffix_at_char_boundary, cap_bytes_with_hint,
};

pub(crate) fn dext_state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DEXT_HOME") {
        return PathBuf::from(p);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dext")
}

pub(crate) fn named_sessions_dir_for_root(root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("DEXT_SESSIONS_DIR") {
        return PathBuf::from(p);
    }
    project_state_dir(root).join("sessions")
}

pub(crate) fn canonicalize_or_clone(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn git_toplevel(root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if top.is_empty() {
        return None;
    }
    std::fs::canonicalize(top).ok()
}

fn project_scope_root(root: &Path) -> PathBuf {
    git_toplevel(root).unwrap_or_else(|| canonicalize_or_clone(root))
}

fn slugify_project_component(s: &str) -> String {
    let mut out = String::new();
    let mut last_was_sep = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep && !out.is_empty() {
            out.push('-');
            last_was_sep = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

fn stable_hash64(s: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn stable_hash64_hex(s: &str) -> String {
    format!("{:016x}", stable_hash64(s))
}

pub(crate) fn project_key(root: &Path) -> String {
    let scoped_root = project_scope_root(root);
    let basis = if cfg!(windows) {
        scoped_root.to_string_lossy().to_lowercase()
    } else {
        scoped_root.to_string_lossy().to_string()
    };
    let label = scoped_root
        .file_name()
        .and_then(|s| s.to_str())
        .map(slugify_project_component)
        .unwrap_or_else(|| "project".to_string());
    format!("{label}-{}", stable_hash64_hex(&basis))
}

pub(crate) fn project_state_dir(root: &Path) -> PathBuf {
    dext_state_dir().join("projects").join(project_key(root))
}

pub(crate) fn latest_sessions_dir(root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("DEXT_SESSIONS_DIR") {
        return PathBuf::from(p);
    }
    project_state_dir(root).join("sessions")
}

pub(crate) fn logs_dir(root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("DEXT_LOGS_DIR") {
        return PathBuf::from(p);
    }
    project_state_dir(root).join("logs")
}

pub(crate) fn latest_log_path(root: &Path) -> PathBuf {
    logs_dir(root).join("latest.log")
}

pub(crate) fn latest_session_path(root: &Path) -> PathBuf {
    latest_sessions_dir(root).join(format!("{LATEST_SESSION_NAME}.jsonl"))
}

fn temp_swap_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("state");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(".{name}.tmp-{}-{stamp}", std::process::id()))
}

#[cfg(windows)]
fn replace_file_atomically(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let from_w = wide(from);
    let to_w = wide(to);
    let ok = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file_atomically(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

pub(crate) fn atomic_write_bytes(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_swap_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        replace_file_atomically(&tmp, path)
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

fn log_detail(s: &str) -> String {
    cap_bytes_with_hint(
        s.replace('\r', "\\r").replace('\n', "\\n"),
        LOG_DETAIL_CAP,
        "log detail truncated.",
    )
}

pub(crate) fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn cap_latest_log_buffer(content: String) -> String {
    if content.len() <= LATEST_LOG_CAP {
        return content;
    }
    let notice = format!(
        "[{}] log_truncated kept latest {} of {} bytes in latest.log",
        unix_timestamp_secs(),
        LATEST_LOG_CAP,
        content.len()
    );
    let prefix = format!("{notice}\n");
    let budget = LATEST_LOG_CAP.saturating_sub(prefix.len());
    if budget == 0 {
        return byte_prefix_at_char_boundary(&prefix, LATEST_LOG_CAP).to_string();
    }
    let mut kept = byte_suffix_at_char_boundary(&content, budget);
    if let Some(pos) = kept.find('\n') {
        kept = &kept[pos + 1..];
    }
    format!("{prefix}{kept}")
}

fn log_archive_count() -> u32 {
    std::env::var("DEXT_LOG_ARCHIVES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|n| n.min(LATEST_LOG_ARCHIVE_MAX))
        .unwrap_or(0)
}

fn rotate_log_archives(path: &Path, current: &[u8], keep: u32) -> io::Result<()> {
    if keep == 0 {
        return Ok(());
    }
    for idx in (1..keep).rev() {
        let from = path.with_extension(format!("log.{idx}"));
        if !from.exists() {
            continue;
        }
        let to = path.with_extension(format!("log.{}", idx + 1));
        let _ = std::fs::remove_file(&to);
        let _ = std::fs::rename(&from, &to);
    }
    let first_archive = path.with_extension("log.1");
    let _ = std::fs::remove_file(&first_archive);
    atomic_write_bytes(&first_archive, current)
}

pub(crate) fn append_log_line(path: &Path, line: &str) {
    let current_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as usize;
    let needed = line.len() + 1;
    let projected = current_len + needed;

    if projected <= LATEST_LOG_CAP {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let mut buf = String::with_capacity(needed);
            buf.push_str(line);
            buf.push('\n');
            if f.write_all(buf.as_bytes()).is_ok() {
                return;
            }
        }
    }

    let existing = std::fs::read(path).unwrap_or_default();
    let mut data = String::from_utf8_lossy(&existing).into_owned();
    if !data.is_empty() && !data.ends_with('\n') {
        data.push('\n');
    }
    data.push_str(line);
    data.push('\n');

    let archives = log_archive_count();
    if archives > 0 && data.len() > LATEST_LOG_CAP {
        match rotate_log_archives(path, &existing, archives) {
            Ok(()) => {
                let fresh = format!("{line}\n");
                let _ = atomic_write_bytes(path, fresh.as_bytes());
                return;
            }
            Err(e) => {
                let notice = format!(
                    "[{}] log_archive_failed {}\n",
                    unix_timestamp_secs(),
                    log_detail(&format!("{e}"))
                );
                data.insert_str(0, &notice);
            }
        }
    }

    let data = cap_latest_log_buffer(data);
    let _ = atomic_write_bytes(path, data.as_bytes());
}

pub(crate) fn append_latest_log(root: &Path, event: &str, detail: &str) {
    append_log_line(
        &latest_log_path(root),
        &format!(
            "[{}] {} {}",
            unix_timestamp_secs(),
            event,
            log_detail(detail)
        ),
    );
}

pub(crate) fn project_state_lock_path(root: &Path) -> PathBuf {
    project_state_dir(root).join(PROJECT_STATE_LOCK_NAME)
}

fn unix_timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn current_pid() -> u32 {
    std::process::id()
}

#[cfg(windows)]
pub(crate) fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    unsafe extern "system" {
        fn OpenProcess(
            desired_access: u32,
            inherit_handle: i32,
            process_id: u32,
        ) -> *mut std::ffi::c_void;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        false
    } else {
        unsafe {
            CloseHandle(handle);
        }
        true
    }
}

#[cfg(unix)]
pub(crate) fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let rc = unsafe { kill(pid as i32, 0) };
    if rc != 0 && matches!(io::Error::last_os_error().raw_os_error(), Some(3)) {
        return false;
    }

    !linux_process_is_zombie(pid)
}

#[cfg(target_os = "linux")]
fn linux_process_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.rsplit_once(") ").map(|(_, rest)| rest.to_string()))
        .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
        .is_some_and(|state| state == "Z")
}

#[cfg(all(unix, not(target_os = "linux")))]
fn linux_process_is_zombie(_pid: u32) -> bool {
    false
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn process_exists(pid: u32) -> bool {
    pid != 0
}

#[derive(Serialize, Deserialize)]
struct ProjectStateLockRecord {
    token: String,
    pid: u32,
    acquired_at: u64,
    project_key: String,
    sandbox_root: String,
}

#[derive(Debug)]
pub(crate) struct ProjectStateLock {
    pub(crate) path: PathBuf,
    token: String,
}

fn lock_cleanup_registry() -> &'static Mutex<Vec<(PathBuf, String)>> {
    static REGISTRY: OnceLock<Mutex<Vec<(PathBuf, String)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_tui_active(on: bool) {
    TUI_ACTIVE.store(on, Ordering::SeqCst);
}

pub(crate) fn restore_terminal_if_tui() {
    if TUI_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(
            out,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::SetCursorStyle::DefaultUserShape,
            crossterm::cursor::Show,
            crossterm::cursor::MoveToColumn(0)
        );
        let _ = writeln!(out);
        let _ = out.flush();
    }
}

pub(crate) fn release_registered_locks() {
    let Ok(mut entries) = lock_cleanup_registry().lock() else {
        return;
    };
    for (path, token) in entries.drain(..) {
        let Ok(existing) = ProjectStateLock::read_record(&path) else {
            continue;
        };
        if existing.token == token && existing.pid == current_pid() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn register_lock_cleanup(path: &Path, token: &str) {
    if let Ok(mut entries) = lock_cleanup_registry().lock() {
        entries.push((path.to_path_buf(), token.to_string()));
    }
}

fn unregister_lock_cleanup(path: &Path, token: &str) {
    if let Ok(mut entries) = lock_cleanup_registry().lock() {
        entries.retain(|(p, t)| !(p == path && t == token));
    }
}

impl ProjectStateLock {
    fn read_record(path: &Path) -> Result<ProjectStateLockRecord> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading project state lock {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("parsing project state lock {}", path.display()))
    }

    fn write_record(path: &Path, record: &ProjectStateLockRecord) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec(record)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("creating project state lock {}", path.display()))?;
        file.write_all(&data)?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        let path = project_state_lock_path(root);
        let record = ProjectStateLockRecord {
            token: format!("{}-{:x}", current_pid(), unix_timestamp_nanos()),
            pid: current_pid(),
            acquired_at: unix_timestamp_secs(),
            project_key: project_key(root),
            sandbox_root: root.display().to_string(),
        };

        loop {
            match Self::write_record(&path, &record) {
                Ok(()) => {
                    register_lock_cleanup(&path, &record.token);
                    return Ok(Self {
                        path,
                        token: record.token.clone(),
                    });
                }
                Err(e) => {
                    let already_exists = e
                        .downcast_ref::<io::Error>()
                        .is_some_and(|ioe| ioe.kind() == io::ErrorKind::AlreadyExists);
                    if !already_exists {
                        return Err(e);
                    }
                }
            }

            match Self::read_record(&path) {
                Ok(existing) => {
                    if !process_exists(existing.pid) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    anyhow::bail!(
                        "another dext process already owns project state for {} (pid {}, lock {}). Close that process or remove the stale lock if it is no longer running.",
                        existing.sandbox_root,
                        existing.pid,
                        path.display(),
                    );
                }
                Err(_) => {
                    let stale = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age.as_secs() >= PROJECT_STATE_LOCK_STALE_SECS);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    anyhow::bail!(
                        "project state lock exists but could not be read: {}",
                        path.display()
                    );
                }
            }
        }
    }
}

impl Drop for ProjectStateLock {
    fn drop(&mut self) {
        unregister_lock_cleanup(&self.path, &self.token);
        let Ok(existing) = Self::read_record(&self.path) else {
            return;
        };
        if existing.token == self.token && existing.pid == current_pid() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("session name cannot be empty");
    }
    if name == LATEST_SESSION_NAME {
        anyhow::bail!("'{LATEST_SESSION_NAME}' is reserved for the auto-saved latest session");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        anyhow::bail!(
            "invalid session name '{name}' (use only ASCII letters, digits, '.', '_' or '-')"
        );
    }
    Ok(())
}

pub(crate) fn named_session_path_for_root(root: &Path, name: &str) -> Result<PathBuf> {
    validate_session_name(name)?;
    Ok(named_sessions_dir_for_root(root).join(format!("{name}.jsonl")))
}

#[derive(Clone)]
pub(crate) struct SessionRecord {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) modified: Option<std::time::SystemTime>,
}

pub(crate) fn list_session_records_for_dir(dir: &Path) -> Result<Vec<SessionRecord>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut records: Vec<SessionRecord> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                return None;
            }
            let name = path.file_stem()?.to_str()?;
            if name == LATEST_SESSION_NAME {
                return None;
            }
            let modified = e.metadata().ok().and_then(|m| m.modified().ok());
            Some(SessionRecord {
                name: name.to_string(),
                path,
                modified,
            })
        })
        .collect();
    records.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(records)
}

pub(crate) fn list_session_records_for_root(root: &Path) -> Result<Vec<SessionRecord>> {
    list_session_records_for_dir(&named_sessions_dir_for_root(root))
}

pub(crate) fn render_limited_csv(
    items: &[String],
    limit: usize,
    empty: &str,
    label: &str,
) -> String {
    if items.is_empty() {
        return empty.to_string();
    }
    let shown = items.len().min(limit);
    let mut out = items[..shown].join(", ");
    if items.len() > shown {
        out.push_str(&format!(
            ", … [{} more {label}; showing {shown}/{}]",
            items.len() - shown,
            items.len()
        ));
    }
    out
}

#[cfg(test)]
pub(crate) fn render_limited_lines(
    items: &[String],
    limit: usize,
    newest: bool,
    label: &str,
) -> String {
    if items.is_empty() {
        return "(none)".to_string();
    }
    let shown = items.len().min(limit);
    let start = if newest { items.len() - shown } else { 0 };
    let slice = &items[start..start + shown];
    let mut out = slice.join("\n");
    if items.len() > shown {
        let omitted = items.len() - shown;
        let direction = if newest { "earlier" } else { "later" };
        out.push_str(&format!(
            "\n… [{omitted} {direction} {label} omitted; showing {shown}/{}]",
            items.len()
        ));
    }
    out
}

pub(crate) fn parse_session_header(line: &str) -> Result<SessionHeader> {
    if let Ok(header) = serde_json::from_str::<SessionHeader>(line) {
        return Ok(header);
    }

    let meta: serde_json::Value = serde_json::from_str(line).context("bad session header")?;
    Ok(SessionHeader {
        version: meta["version"].as_u64().unwrap_or(1) as u32,
        model: meta["model"].as_str().unwrap_or("glm-4.6").to_string(),
        system: meta["system"]
            .as_str()
            .unwrap_or(DEFAULT_SYSTEM)
            .to_string(),
        composed_system: meta["composed_system"].as_str().map(String::from),
        allowed: meta["allowed"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        exposed_tools: meta["exposed_tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        approval_required_tools: meta["approval_required_tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        auto_approved_tools: meta["auto_approved_tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        sandbox: meta["sandbox"].as_str().map(String::from),
        usage: serde_json::from_value(meta["usage"].clone()).unwrap_or_default(),
        thinking_effort: meta["thinking_effort"]
            .as_str()
            .and_then(ThinkingEffort::parse)
            .unwrap_or_default(),
        compact_threshold_chars: meta["compact_threshold_chars"]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok()),
        compact_threshold_percent: meta["compact_threshold_percent"]
            .as_u64()
            .and_then(|v| u8::try_from(v).ok()),
        approval_profile: meta["approval_profile"]
            .as_str()
            .and_then(crate::ApprovalProfile::parse)
            .unwrap_or_default(),
        sandbox_profile: meta["sandbox_profile"]
            .as_str()
            .and_then(crate::SandboxProfile::parse)
            .unwrap_or_default(),
        budget_cap: if meta["budget_cap"].is_null() {
            None
        } else {
            serde_json::from_value(meta["budget_cap"].clone()).ok()
        },
        browser_recipe: meta["browser_recipe"]
            .as_str()
            .and_then(crate::BrowserRecipe::parse)
            .unwrap_or_default(),
        context_mode: meta["context_mode"]
            .as_str()
            .and_then(crate::ContextMode::parse)
            .unwrap_or_default(),
        tool_context_profile: meta["tool_context_profile"]
            .as_str()
            .and_then(crate::ToolContextProfile::parse)
            .unwrap_or_default(),
        tool_profile: meta["tool_profile"]
            .as_str()
            .and_then(crate::ToolProfile::parse)
            .unwrap_or_default(),
        provenance: serde_json::from_value(meta["provenance"].clone()).unwrap_or_default(),
        work_ledger: serde_json::from_value(meta["work_ledger"].clone()).unwrap_or_default(),
        provider_health: serde_json::from_value(meta["provider_health"].clone())
            .unwrap_or_default(),
        track_origin: serde_json::from_value(meta["track_origin"].clone())
            .ok()
            .flatten(),
        privacy: serde_json::from_value(meta["privacy"].clone()).unwrap_or_default(),
    })
}
