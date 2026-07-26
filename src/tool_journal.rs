use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const TOOL_JOURNAL_VERSION: u32 = 1;
const TOOL_JOURNAL_FILE: &str = "tool-journal.json";
const TOOL_JOURNAL_MAX_BYTES: u64 = 128 * 1024;
const TOOL_JOURNAL_MAX_ENTRIES: usize = 64;
const TOOL_JOURNAL_MAX_UNRESOLVED: usize = 32;
const TOOL_JOURNAL_TERMINAL_TAIL: usize = 32;
const TOOL_JOURNAL_SUMMARY_CHARS: usize = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolJournalStatus {
    Started,
    Completed,
    Failed,
    Interrupted,
}

impl ToolJournalStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        self != Self::Started
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ToolJournalEntry {
    pub(crate) record_id: String,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) batch_id: String,
    pub(crate) call_id: String,
    pub(crate) tool_name: String,
    pub(crate) summary: String,
    pub(crate) input_sha256: String,
    pub(crate) started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ended_at: Option<u64>,
    pub(crate) status: ToolJournalStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolJournal {
    version: u32,
    session_id: String,
    entries: Vec<ToolJournalEntry>,
}

pub(crate) struct StartSpec<'a> {
    pub(crate) turn_id: &'a str,
    pub(crate) batch_id: &'a str,
    pub(crate) call_id: &'a str,
    pub(crate) tool_name: &'a str,
    pub(crate) summary: &'a str,
    pub(crate) input: &'a serde_json::Value,
}

pub(crate) fn journal_path(root: &Path, session_id: &str) -> PathBuf {
    crate::session::session_state_dir(root, session_id).join(TOOL_JOURNAL_FILE)
}

pub(crate) fn start(root: &Path, session_id: &str, spec: StartSpec<'_>) -> Result<String> {
    validate_session_id(session_id)?;
    validate_identity(spec.turn_id, "turn id")?;
    validate_identity(spec.batch_id, "batch id")?;
    validate_identity(spec.call_id, "call id")?;
    validate_identity(spec.tool_name, "tool name")?;

    let path = journal_path(root, session_id);
    let mut journal = load_path(&path, session_id)?.unwrap_or_else(|| ToolJournal {
        version: TOOL_JOURNAL_VERSION,
        session_id: session_id.to_string(),
        entries: Vec::new(),
    });
    if journal.entries.len() >= TOOL_JOURNAL_MAX_ENTRIES {
        anyhow::bail!(
            "tool journal is full; persist a clean transcript checkpoint and compact it before executing another side-effect-capable call"
        );
    }
    let unresolved = journal
        .entries
        .iter()
        .filter(|entry| entry.status == ToolJournalStatus::Started)
        .count();
    if unresolved >= TOOL_JOURNAL_MAX_UNRESOLVED {
        anyhow::bail!(
            "tool journal contains {unresolved} unresolved entries; reconcile them before executing another side-effect-capable call"
        );
    }

    let started_at = unix_timestamp_millis();
    let record_id = new_record_id(
        session_id,
        spec.turn_id,
        spec.batch_id,
        spec.call_id,
        started_at,
        journal.entries.len(),
    );
    let input_sha256 = input_sha256(spec.input)?;
    journal.entries.push(ToolJournalEntry {
        record_id: record_id.clone(),
        session_id: session_id.to_string(),
        turn_id: spec.turn_id.to_string(),
        batch_id: spec.batch_id.to_string(),
        call_id: spec.call_id.to_string(),
        tool_name: spec.tool_name.to_string(),
        summary: cap_chars(spec.summary, TOOL_JOURNAL_SUMMARY_CHARS),
        input_sha256,
        started_at,
        ended_at: None,
        status: ToolJournalStatus::Started,
    });
    write_path(&path, &journal)?;
    Ok(record_id)
}

pub(crate) fn finish(
    root: &Path,
    session_id: &str,
    record_id: &str,
    status: ToolJournalStatus,
) -> Result<()> {
    if !status.is_terminal() {
        anyhow::bail!("tool journal terminal update requires a terminal status");
    }
    validate_session_id(session_id)?;
    let path = journal_path(root, session_id);
    let mut journal = load_path(&path, session_id)?
        .with_context(|| format!("tool journal start record is missing: {record_id}"))?;
    let entry = journal
        .entries
        .iter_mut()
        .find(|entry| entry.record_id == record_id)
        .with_context(|| format!("tool journal start record is missing: {record_id}"))?;
    if entry.status != ToolJournalStatus::Started {
        if entry.status == status {
            return Ok(());
        }
        anyhow::bail!(
            "tool journal record {record_id} is already terminal ({})",
            entry.status.as_str()
        );
    }
    entry.status = status;
    entry.ended_at = Some(unix_timestamp_millis());
    write_path(&path, &journal)
}

pub(crate) fn compact(root: &Path, session_id: &str) -> Result<()> {
    validate_session_id(session_id)?;
    let path = journal_path(root, session_id);
    let Some(mut journal) = load_path(&path, session_id)? else {
        return Ok(());
    };
    compact_entries(&mut journal.entries);
    write_path(&path, &journal)
}

pub(crate) fn load_for_session_file(path: &Path) -> Result<Option<Vec<ToolJournalEntry>>> {
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let Some(session_id) = parent.file_name().and_then(|value| value.to_str()) else {
        return Ok(None);
    };
    let journal_path = parent.join(TOOL_JOURNAL_FILE);
    Ok(load_path(&journal_path, session_id)?.map(|journal| journal.entries))
}

pub(crate) fn input_sha256(input: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(input).context("serializing tool input digest")?;
    Ok(sha256_hex(&bytes))
}

fn load_path(path: &Path, expected_session_id: &str) -> Result<Option<ToolJournal>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting tool journal {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "tool journal path is not a regular file: {}",
            path.display()
        );
    }
    validate_private_file(path, &metadata)?;
    if metadata.len() > TOOL_JOURNAL_MAX_BYTES {
        anyhow::bail!(
            "tool journal exceeds {} bytes: {}",
            TOOL_JOURNAL_MAX_BYTES,
            path.display()
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("reading tool journal {}", path.display()))?;
    let journal: ToolJournal = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing tool journal {}", path.display()))?;
    validate_journal(&journal, expected_session_id)?;
    Ok(Some(journal))
}

fn write_path(path: &Path, journal: &ToolJournal) -> Result<()> {
    ensure_private_session_dir(
        path.parent()
            .context("tool journal path has no session directory")?,
    )?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            anyhow::bail!(
                "tool journal path is not a regular file: {}",
                path.display()
            );
        }
        Ok(metadata) => validate_private_file(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting tool journal {}", path.display()));
        }
    }
    validate_journal(journal, &journal.session_id)?;
    let bytes = serde_json::to_vec_pretty(journal).context("serializing tool journal")?;
    if bytes.len() as u64 > TOOL_JOURNAL_MAX_BYTES {
        anyhow::bail!(
            "tool journal serialization exceeds {} bytes",
            TOOL_JOURNAL_MAX_BYTES
        );
    }
    crate::session::atomic_write_secret(path, &bytes)
        .with_context(|| format!("persisting tool journal {}", path.display()))?;
    sync_parent_dir(path)?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("verifying tool journal {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "tool journal path is not a regular file after write: {}",
            path.display()
        );
    }
    validate_private_file(path, &metadata)
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().context("tool journal path has no parent")?;
        let directory = std::fs::File::open(parent)
            .with_context(|| format!("opening tool journal directory {}", parent.display()))?;
        directory
            .sync_all()
            .with_context(|| format!("syncing tool journal directory {}", parent.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn validate_journal(journal: &ToolJournal, expected_session_id: &str) -> Result<()> {
    if journal.version != TOOL_JOURNAL_VERSION {
        anyhow::bail!(
            "unsupported tool journal version {} (supported: {})",
            journal.version,
            TOOL_JOURNAL_VERSION
        );
    }
    validate_session_id(&journal.session_id)?;
    if journal.session_id != expected_session_id {
        anyhow::bail!("tool journal session identity does not match its state directory");
    }
    if journal.entries.len() > TOOL_JOURNAL_MAX_ENTRIES {
        anyhow::bail!("tool journal contains too many entries");
    }
    let mut record_ids = std::collections::HashSet::new();
    for entry in &journal.entries {
        if entry.session_id != journal.session_id {
            anyhow::bail!("tool journal entry has a mismatched session identity");
        }
        for (value, label) in [
            (&entry.record_id, "record id"),
            (&entry.turn_id, "turn id"),
            (&entry.batch_id, "batch id"),
            (&entry.call_id, "call id"),
            (&entry.tool_name, "tool name"),
        ] {
            validate_identity(value, label)?;
        }
        if !record_ids.insert(&entry.record_id) {
            anyhow::bail!("tool journal contains a duplicate record id");
        }
        if entry.summary.chars().count() > TOOL_JOURNAL_SUMMARY_CHARS {
            anyhow::bail!("tool journal entry summary exceeds its bound");
        }
        if entry.input_sha256.len() != 64
            || !entry
                .input_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("tool journal entry has an invalid input digest");
        }
        match (entry.status, entry.ended_at) {
            (ToolJournalStatus::Started, None) => {}
            (status, Some(_)) if status.is_terminal() => {}
            _ => anyhow::bail!("tool journal entry has inconsistent status timestamps"),
        }
    }
    Ok(())
}

fn validate_session_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        anyhow::bail!("tool journal session id is not a safe path component");
    }
    Ok(())
}

fn validate_identity(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > 256 {
        anyhow::bail!("tool journal {label} is empty or too long");
    }
    Ok(())
}

fn compact_entries(entries: &mut Vec<ToolJournalEntry>) {
    if entries.len() <= TOOL_JOURNAL_TERMINAL_TAIL {
        return;
    }
    let unresolved = entries
        .iter()
        .filter(|entry| entry.status == ToolJournalStatus::Started)
        .count();
    let terminal_to_keep = entries
        .iter()
        .filter(|entry| entry.status.is_terminal())
        .count()
        .min(TOOL_JOURNAL_TERMINAL_TAIL)
        .min(TOOL_JOURNAL_MAX_ENTRIES.saturating_sub(unresolved));
    let mut remaining_terminal = terminal_to_keep;
    let mut kept_rev = Vec::with_capacity(entries.len());
    for entry in entries.drain(..).rev() {
        if entry.status == ToolJournalStatus::Started {
            kept_rev.push(entry);
        } else if remaining_terminal > 0 {
            kept_rev.push(entry);
            remaining_terminal -= 1;
        }
    }
    kept_rev.reverse();
    *entries = kept_rev;
}

fn ensure_private_session_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!(
                "tool journal session path is not a real directory: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating tool journal state parent {}", parent.display())
                })?;
            }
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder
                .create(path)
                .with_context(|| format!("creating tool journal session dir {}", path.display()))?;
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing tool journal session dir {}", path.display()))?;
    }
    Ok(())
}

fn validate_private_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!(
                "tool journal is not owned by the current user: {}",
                path.display()
            );
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "tool journal permissions are unsafe ({mode:04o}); expected owner-only access"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (path, metadata);
    Ok(())
}

fn new_record_id(
    session_id: &str,
    turn_id: &str,
    batch_id: &str,
    call_id: &str,
    started_at: u64,
    entry_count: usize,
) -> String {
    let material = format!(
        "{session_id}\0{turn_id}\0{batch_id}\0{call_id}\0{started_at}\0{entry_count}\0{}",
        std::process::id()
    );
    format!("tj-{}", &sha256_hex(material.as_bytes())[..24])
}

fn unix_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cap_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dext-tool-journal-{label}-{}-{}",
            std::process::id(),
            unix_timestamp_millis()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn persists_bounded_redacted_metadata_without_raw_input_or_output() {
        let _guard = crate::test_env_lock();
        let root = temp_dir("roundtrip");
        let session_id = "session-1";
        let input = serde_json::json!({"command": "printf super-secret-value"});
        let record_id = start(
            &root,
            session_id,
            StartSpec {
                turn_id: "turn-1",
                batch_id: "batch-1",
                call_id: "call-1",
                tool_name: "bash",
                summary: "bash: printf [REDACTED_SECRET]",
                input: &input,
            },
        )
        .unwrap();
        finish(&root, session_id, &record_id, ToolJournalStatus::Completed).unwrap();

        let path = journal_path(&root, session_id);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("super-secret-value"), "{raw}");
        assert!(!raw.contains("tool output"), "{raw}");
        let entries = load_for_session_file(&crate::session::session_latest_session_path(
            &root, session_id,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, ToolJournalStatus::Completed);
        assert_eq!(entries[0].input_sha256.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_permissive_journal_paths() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let _guard = crate::test_env_lock();

        let root = temp_dir("unsafe-path");
        let session_id = "session-1";
        let dir = crate::session::session_state_dir(&root, session_id);
        std::fs::create_dir_all(&dir).unwrap();
        let target = root.join("target.json");
        std::fs::write(&target, b"{}").unwrap();
        symlink(&target, journal_path(&root, session_id)).unwrap();
        let error = load_for_session_file(&crate::session::session_latest_session_path(
            &root, session_id,
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("not a regular file"), "{error}");
        std::fs::remove_file(journal_path(&root, session_id)).unwrap();

        std::fs::write(journal_path(&root, session_id), b"{}").unwrap();
        std::fs::set_permissions(
            journal_path(&root, session_id),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let error = load_for_session_file(&crate::session::session_latest_session_path(
            &root, session_id,
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("permissions are unsafe"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn checked_in_journal_fixtures_cover_v1_and_rejections() {
        for fixture in [
            include_str!("../tests/fixtures/state/tool-journal/v1-completed.json"),
            include_str!("../tests/fixtures/state/tool-journal/v1-unresolved.json"),
        ] {
            let journal: ToolJournal = serde_json::from_str(fixture).expect("parse valid fixture");
            validate_journal(&journal, "session-1").expect("validate v1 fixture");
        }

        let future: ToolJournal = serde_json::from_str(include_str!(
            "../tests/fixtures/state/tool-journal/future.json"
        ))
        .expect("parse future fixture");
        assert!(
            validate_journal(&future, "session-1")
                .unwrap_err()
                .to_string()
                .contains("unsupported tool journal version")
        );
        assert!(
            serde_json::from_str::<ToolJournal>(include_str!(
                "../tests/fixtures/state/tool-journal/corrupt.json"
            ))
            .is_err()
        );
        let unsafe_path: ToolJournal = serde_json::from_str(include_str!(
            "../tests/fixtures/state/tool-journal/unsafe-path.json"
        ))
        .expect("parse unsafe-path fixture");
        assert!(
            validate_journal(&unsafe_path, "../escape")
                .unwrap_err()
                .to_string()
                .contains("safe path component")
        );
    }

    #[test]
    fn rejects_corrupt_and_future_journals() {
        let _guard = crate::test_env_lock();
        let root = temp_dir("versions");
        let session_id = "session-1";
        let dir = crate::session::session_state_dir(&root, session_id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = journal_path(&root, session_id);
        crate::session::atomic_write_secret(&path, b"{").unwrap();
        assert!(
            load_for_session_file(&crate::session::session_latest_session_path(
                &root, session_id
            ))
            .unwrap_err()
            .to_string()
            .contains("parsing tool journal")
        );
        crate::session::atomic_write_secret(
            &path,
            br#"{"version":2,"session_id":"session-1","entries":[]}"#,
        )
        .unwrap();
        assert!(
            load_for_session_file(&crate::session::session_latest_session_path(
                &root, session_id
            ))
            .unwrap_err()
            .to_string()
            .contains("unsupported tool journal version")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
