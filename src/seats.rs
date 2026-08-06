use crate::{SeatRef, SessionHeader};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::{Path, PathBuf};

const SEAT_RECORD_VERSION: u32 = 1;
const SEAT_ID_MAX_BYTES: usize = 128;
const SEAT_RECORD_MAX_BYTES: u64 = 64 * 1024;
const SEAT_LABEL_MAX_CHARS: usize = 128;
pub(crate) const SEAT_SUMMARY_MAX_CHARS: usize = 4_000;
pub(crate) const SEAT_SUMMARY_MAX_BYTES: usize = SEAT_SUMMARY_MAX_CHARS * 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SeatRecord {
    pub(crate) version: u32,
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_session_id: Option<String>,
}

impl Default for SeatRecord {
    fn default() -> Self {
        Self {
            version: SEAT_RECORD_VERSION,
            id: String::new(),
            label: None,
            summary: None,
            created_at: 0,
            updated_at: 0,
            last_session_id: None,
        }
    }
}

impl SeatRecord {
    pub(crate) fn seat_ref(&self) -> SeatRef {
        SeatRef {
            id: self.id.clone(),
            label: self.label.clone(),
        }
    }
}

fn windows_reserved_component(id: &str) -> bool {
    let stem = id.split('.').next().unwrap_or(id).to_ascii_lowercase();
    let bytes = stem.as_bytes();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (bytes.len() == 4
            && matches!(&bytes[..3], b"com" | b"lpt")
            && matches!(bytes[3], b'1'..=b'9'))
}

pub(crate) fn validate_seat_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > SEAT_ID_MAX_BYTES
        || matches!(id, "." | "..")
        || id.ends_with('.')
        || windows_reserved_component(id)
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        anyhow::bail!(
            "seat id must be a portable 1..{SEAT_ID_MAX_BYTES} byte component using lowercase ASCII letters, digits, '-', '_', or '.'; it cannot end in '.', equal '.'/'..', or use a Windows device name"
        );
    }
    Ok(())
}

fn validate_label(label: Option<&str>) -> Result<()> {
    if label.is_some_and(|label| {
        label.trim().is_empty()
            || label.chars().count() > SEAT_LABEL_MAX_CHARS
            || label.chars().any(char::is_control)
    }) {
        anyhow::bail!("seat label must contain 1..{SEAT_LABEL_MAX_CHARS} non-control characters");
    }
    Ok(())
}

fn validate_summary(summary: Option<&str>) -> Result<()> {
    if summary.is_some_and(|summary| {
        summary.trim().is_empty()
            || summary.chars().count() > SEAT_SUMMARY_MAX_CHARS
            || summary
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    }) {
        anyhow::bail!(
            "seat summary must contain 1..{SEAT_SUMMARY_MAX_CHARS} characters and no unsafe controls"
        );
    }
    Ok(())
}

pub(crate) fn validate_seat_ref(seat: &SeatRef) -> Result<()> {
    validate_seat_id(&seat.id)?;
    validate_label(seat.label.as_deref())
}

fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || matches!(id, "." | "..")
        || id.ends_with('.')
        || windows_reserved_component(id)
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        anyhow::bail!("seat record contains a non-portable session id");
    }
    Ok(())
}

fn validate_record(record: &SeatRecord, expected_id: &str) -> Result<()> {
    if record.version != SEAT_RECORD_VERSION {
        anyhow::bail!(
            "unsupported seat record version {} (supported: {})",
            record.version,
            SEAT_RECORD_VERSION
        );
    }
    validate_seat_id(&record.id)?;
    validate_seat_ref(&record.seat_ref())?;
    if record.id != expected_id {
        anyhow::bail!("seat record identity does not match its directory");
    }
    if record.created_at == 0 || record.updated_at < record.created_at {
        anyhow::bail!("seat record has invalid timestamps");
    }
    validate_summary(record.summary.as_deref())?;
    if let Some(session_id) = record.last_session_id.as_deref() {
        validate_session_id(session_id)?;
    }
    Ok(())
}

fn validate_no_symlink_components(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving relative Seat state path")?
            .join(path)
    };
    for component in absolute
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
    {
        match std::fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "seat state path has a symlinked ancestor: {}",
                    component.display()
                );
            }
            Ok(metadata) if !metadata.is_dir() => {
                anyhow::bail!(
                    "seat state ancestor is not a directory: {}",
                    component.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_owner_safe_dir(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "seat state ancestor is not a real directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o022 != 0
        {
            anyhow::bail!("seat state ancestor is not owner-safe: {}", path.display());
        }
    }
    Ok(())
}

fn validate_state_root_or_parent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_owner_safe_dir(path, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut parent = path.parent().context("Seat state root has no parent")?;
            loop {
                match std::fs::symlink_metadata(parent) {
                    Ok(metadata) => {
                        validate_owner_safe_dir(parent, &metadata)?;
                        return Ok(false);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        parent = parent
                            .parent()
                            .context("Seat state root has no existing parent")?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_owner_safe_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_owner_safe_dir(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let builder = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt as _;
                    let mut builder = std::fs::DirBuilder::new();
                    builder.mode(0o700);
                    builder
                }
                #[cfg(not(unix))]
                {
                    std::fs::DirBuilder::new()
                }
            };
            builder
                .create(path)
                .with_context(|| format!("creating seat state ancestor {}", path.display()))
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_owner_safe_dir_if_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_owner_safe_dir(path, &metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn ensure_seat_ancestors(root: &Path) -> Result<()> {
    let project = crate::session::project_state_dir(root);
    validate_no_symlink_components(&project)?;
    let projects = project
        .parent()
        .context("project state directory has no parent")?;
    let state = projects
        .parent()
        .context("projects directory has no parent")?;
    ensure_owner_safe_dir(state)?;
    ensure_owner_safe_dir(projects)?;
    ensure_owner_safe_dir(&project)
}

fn validate_seat_ancestors_if_exists(root: &Path) -> Result<bool> {
    let project = crate::session::project_state_dir(root);
    validate_no_symlink_components(&project)?;
    let projects = project
        .parent()
        .context("project state directory has no parent")?;
    let state = projects
        .parent()
        .context("projects directory has no parent")?;
    let state_exists = validate_state_root_or_parent(state)?;
    if !state_exists || !validate_owner_safe_dir_if_exists(projects)? {
        return Ok(false);
    }
    validate_owner_safe_dir_if_exists(&project)
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!(
                "seat state path is not a real directory: {}",
                path.display()
            );
        }
        Ok(metadata) => {
            #[cfg(not(unix))]
            let _ = &metadata;
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                if metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o077 != 0
                {
                    anyhow::bail!(
                        "seat state directory is not owner-private: {}",
                        path.display()
                    );
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let builder = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt as _;
                    let mut builder = std::fs::DirBuilder::new();
                    builder.mode(0o700);
                    builder
                }
                #[cfg(not(unix))]
                {
                    std::fs::DirBuilder::new()
                }
            };
            builder
                .create(path)
                .with_context(|| format!("creating seat state directory {}", path.display()))?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn validate_private_dir_if_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            ensure_private_dir(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn validate_private_file(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("seat record is not a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            anyhow::bail!("seat record is not owner-private: {}", path.display());
        }
    }
    Ok(())
}

pub(crate) fn seats_dir(root: &Path) -> PathBuf {
    crate::session::project_state_dir(root).join("seats")
}

pub(crate) fn seat_record_path(root: &Path, id: &str) -> Result<PathBuf> {
    validate_seat_id(id)?;
    Ok(seats_dir(root).join(id).join("seat.json"))
}

pub(crate) fn load(root: &Path, id: &str) -> Result<Option<SeatRecord>> {
    validate_seat_id(id)?;
    if !validate_seat_ancestors_if_exists(root)? {
        return Ok(None);
    }
    let path = seat_record_path(root, id)?;
    let seat_dir = path.parent().context("seat record path has no parent")?;
    let seats = seat_dir.parent().context("seat directory has no parent")?;
    validate_private_dir_if_exists(seats)?;
    validate_private_dir_if_exists(seat_dir)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    };
    validate_private_file(&path, &metadata)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("validating open seat record {}", path.display()))?;
    validate_private_file(&path, &opened)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            anyhow::bail!("seat record changed while opening: {}", path.display());
        }
    }
    if opened.len() > SEAT_RECORD_MAX_BYTES {
        anyhow::bail!("seat record exceeds {SEAT_RECORD_MAX_BYTES} bytes");
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(SEAT_RECORD_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > SEAT_RECORD_MAX_BYTES {
        anyhow::bail!("seat record exceeds {SEAT_RECORD_MAX_BYTES} bytes");
    }
    let record: SeatRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    validate_record(&record, id)?;
    Ok(Some(record))
}

fn save(root: &Path, record: &SeatRecord) -> Result<()> {
    validate_record(record, &record.id)?;
    ensure_seat_ancestors(root)?;
    let path = seat_record_path(root, &record.id)?;
    let seat_dir = path.parent().context("seat record path has no parent")?;
    let seats = seat_dir.parent().context("seat directory has no parent")?;
    ensure_private_dir(seats)?;
    ensure_private_dir(seat_dir)?;
    let data = serde_json::to_vec_pretty(record)?;
    crate::session::atomic_write_secret(&path, &data)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn new_record(id: &str) -> SeatRecord {
    let now = crate::session::unix_timestamp_secs();
    SeatRecord {
        version: SEAT_RECORD_VERSION,
        id: id.to_string(),
        label: None,
        summary: None,
        created_at: now,
        updated_at: now,
        last_session_id: None,
    }
}

fn load_or_new_unlocked(root: &Path, id: &str) -> Result<SeatRecord> {
    validate_seat_id(id)?;
    Ok(load(root, id)?.unwrap_or_else(|| new_record(id)))
}

pub(crate) struct SeatMetadataUpdate {
    pub(crate) label: Option<Option<String>>,
    pub(crate) summary: Option<Option<String>>,
}

pub(crate) fn update_metadata(
    root: &Path,
    id: &str,
    update: SeatMetadataUpdate,
) -> Result<SeatRecord> {
    if update.label.is_none() && update.summary.is_none() {
        anyhow::bail!("seat metadata update is empty");
    }
    validate_seat_id(id)?;
    if let Some(label) = update.label.as_ref() {
        validate_label(label.as_deref())?;
    }
    if let Some(summary) = update.summary.as_ref() {
        validate_summary(summary.as_deref())?;
    }
    validate_seat_ancestors_if_exists(root)?;
    let _guard = crate::session::SessionLockOperationGuard::acquire()?;
    let existing = load(root, id)?;
    let adds_metadata = update.label.as_ref().is_some_and(Option::is_some)
        || update.summary.as_ref().is_some_and(Option::is_some);
    if existing.is_none() && !adds_metadata {
        anyhow::bail!("seat '{id}' does not exist; nothing to clear");
    }
    ensure_seat_ancestors(root)?;
    let mut record = existing.unwrap_or_else(|| new_record(id));
    if let Some(label) = update.label {
        record.label = label;
    }
    if let Some(summary) = update.summary {
        record.summary = summary;
    }
    record.updated_at = crate::session::unix_timestamp_secs()
        .max(record.updated_at)
        .max(record.created_at);
    save(root, &record)?;
    Ok(record)
}

pub(crate) fn record_session(root: &Path, seat: &SeatRef, session_id: &str) -> Result<()> {
    validate_seat_ref(seat)?;
    validate_session_id(session_id)?;
    validate_seat_ancestors_if_exists(root)?;
    let _guard = crate::session::SessionLockOperationGuard::acquire()?;
    ensure_seat_ancestors(root)?;
    let mut record = load_or_new_unlocked(root, &seat.id)?;
    let same_session = record.last_session_id.as_deref() == Some(session_id);
    let filled_label = record.label.is_none() && seat.label.is_some();
    if filled_label {
        record.label = seat.label.clone();
    }
    if same_session && !filled_label {
        return Ok(());
    }
    record.last_session_id = Some(session_id.to_string());
    record.updated_at = crate::session::unix_timestamp_secs()
        .max(record.updated_at)
        .max(record.created_at);
    save(root, &record)
}

pub(crate) fn remove_session_and_clear_if_matches(
    root: &Path,
    seat_id: &str,
    session_id: &str,
    session_path: &Path,
) -> Result<()> {
    validate_seat_id(seat_id)?;
    validate_session_id(session_id)?;
    validate_seat_ancestors_if_exists(root)?;
    let _guard = crate::session::SessionLockOperationGuard::acquire()?;
    let original = load(root, seat_id)?;
    let pointer_matches = original
        .as_ref()
        .and_then(|record| record.last_session_id.as_deref())
        == Some(session_id);
    if let Some(mut record) = original.clone()
        && pointer_matches
    {
        record.last_session_id = None;
        record.updated_at = crate::session::unix_timestamp_secs()
            .max(record.updated_at)
            .max(record.created_at);
        save(root, &record)?;
    }
    match std::fs::remove_file(session_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            if pointer_matches
                && let Some(record) = original
                && let Err(restore_error) = save(root, &record)
            {
                anyhow::bail!(
                    "removing session {} failed: {error}; restoring Seat pointer also failed: {restore_error:#}",
                    session_path.display()
                );
            }
            Err(error).with_context(|| format!("removing session {}", session_path.display()))
        }
    }
}

fn read_session_header(path: &Path) -> Result<SessionHeader> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let line = crate::session::read_session_header_line(&mut reader, path)?;
    crate::session::parse_session_header(line.trim_end())
}

pub(crate) fn latest_session_path(root: &Path, id: &str) -> Result<PathBuf> {
    let record = load(root, id)?.with_context(|| format!("seat '{id}' does not exist"))?;
    let session_id = record
        .last_session_id
        .as_deref()
        .with_context(|| format!("seat '{id}' has no saved session"))?;
    let path = crate::session::session_latest_session_path(root, session_id);
    if !path.is_file() {
        anyhow::bail!(
            "latest session for seat '{id}' is unavailable: {}",
            path.display()
        );
    }
    let header = read_session_header(&path)?;
    if header.seat.as_ref().map(|seat| seat.id.as_str()) != Some(id) {
        anyhow::bail!("latest session recorded for seat '{id}' has a different seat identity");
    }
    Ok(path)
}

pub(crate) fn list(root: &Path) -> Result<Vec<SeatRecord>> {
    if !validate_seat_ancestors_if_exists(root)? {
        return Ok(Vec::new());
    }
    let dir = seats_dir(root);
    validate_private_dir_if_exists(&dir)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if metadata.is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if validate_seat_id(&id).is_err() {
            continue;
        }
        if let Some(record) = load(root, &id)? {
            records.push(record);
        }
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(records)
}

pub(crate) fn render_list(root: &Path) -> Result<String> {
    let records = list(root)?;
    if records.is_empty() {
        return Ok("no seats".to_string());
    }
    Ok(records
        .into_iter()
        .map(|record| {
            let label = record.label.as_deref().unwrap_or("-");
            let session = record.last_session_id.as_deref().unwrap_or("-");
            format!("{}\tlabel={}\tlatest={}", record.id, label, session)
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub(crate) fn render_record(record: &SeatRecord) -> String {
    format!(
        "seat: {}\nlabel: {}\ncreated_at: {}\nupdated_at: {}\nlatest_session: {}\nsummary:\n{}",
        record.id,
        record.label.as_deref().unwrap_or("-"),
        record.created_at,
        record.updated_at,
        record.last_session_id.as_deref().unwrap_or("-"),
        record.summary.as_deref().unwrap_or("-")
    )
}

pub(crate) fn render_show(root: &Path, id: &str) -> Result<String> {
    let record = load(root, id)?.with_context(|| format!("seat '{id}' does not exist"))?;
    Ok(render_record(&record))
}
