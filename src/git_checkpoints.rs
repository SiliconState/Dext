// Phase 1: Git-native recovery checkpoints.
//
// Before Dext performs an approved workspace mutation, create a local Git
// recovery point under refs/dext/checkpoints/... Add /undo and CLI support
// to preview and restore the latest checkpoint.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

const CHECKPOINTS_DIR: &str = ".dext/checkpoints";
const REF_PREFIX: &str = "refs/dext/checkpoints";
const REF_SCAN_PREFIX: &str = "refs/dext/checkpoints/";
const DEFAULT_PRUNE_KEEP: usize = 20;
const DEFAULT_PRUNE_MAX_AGE_HOURS: u64 = 168; // 7 days
const CHECKPOINT_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Debug)]
pub(crate) struct Checkpoint {
    pub id: String,
    pub ref_name: String,
    pub oid: String,
    pub tool_name: String,
    pub created_at_ms: u128,
    pub head: String,
    pub paths_hint: Vec<String>,
    pub includes_untracked_sidecar: bool,
    /// Untracked (not-ignored) repo paths present when the checkpoint was
    /// taken. Lets undo/preview name untracked files a write-risk command
    /// created or removed afterwards, even though `git stash create` does not
    /// capture untracked content.
    pub untracked_snapshot: Vec<String>,
}

const UNTRACKED_SNAPSHOT_CAP: usize = 500;

fn random_checkpoint_nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("generate checkpoint nonce: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn git_command(cwd: &Path, args: &[&str]) -> Result<crate::InternalCommandOutput, String> {
    crate::run_internal_git_command(cwd, args)
}

/// List untracked, not-ignored repo paths via porcelain status.
fn untracked_files(git_root: &Path) -> Vec<String> {
    let Ok(out) = run_git(
        git_root,
        &["status", "--porcelain", "--untracked-files=all"],
    ) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| line.strip_prefix("?? "))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .take(UNTRACKED_SNAPSHOT_CAP)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreMode {
    Preview,
    Worktree,
    WorktreeAndIndex,
    ResetHead,
}

pub(crate) fn repo_root(root: &Path) -> Result<Option<PathBuf>, String> {
    let output = git_command(root, &["rev-parse", "--show-toplevel"])?;
    if output.success() {
        let trimmed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!trimmed.is_empty()).then(|| PathBuf::from(trimmed)));
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("not a git repository") || stderr.contains("outside repository") {
        return Ok(None);
    }
    Err(format!(
        "git rev-parse --show-toplevel: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_command(cwd, args)?;
    if !output.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_path_from_bytes(raw: &[u8]) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec())))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(raw.to_vec())
            .map(PathBuf::from)
            .map_err(|_| "Git returned a non-UTF-8 worktree path".to_string())
    }
}

type GitPathModes = std::collections::BTreeMap<PathBuf, String>;

fn run_git_mode_path_list(cwd: &Path, args: &[&str]) -> Result<GitPathModes, String> {
    let output = git_command(cwd, args)?;
    if !output.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut entries = GitPathModes::new();
    for raw in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let tab = raw
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| format!("git {} returned a malformed path record", args.join(" ")))?;
        let header = &raw[..tab];
        let mode = header
            .split(|byte| byte.is_ascii_whitespace())
            .next()
            .filter(|mode| mode.len() == 6 && mode.iter().all(|byte| matches!(byte, b'0'..=b'7')))
            .ok_or_else(|| format!("git {} returned an invalid file mode", args.join(" ")))?;
        let path = git_path_from_bytes(&raw[tab + 1..])?;
        match entries.get_mut(&path) {
            Some(existing) if existing.as_bytes() != mode => {
                *existing = "conflict".to_string();
            }
            Some(_) => {}
            None => {
                entries.insert(path, String::from_utf8_lossy(mode).into_owned());
            }
        }
    }
    Ok(entries)
}

fn sanitize_ref_component(s: &str) -> String {
    let mut sanitized = s
        .chars()
        .take(80)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        sanitized.push_str("session");
    }
    sanitized
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn session_tag() -> String {
    let session =
        std::env::var("DEXT_SESSION_TAG").unwrap_or_else(|_| format!("{}", std::process::id()));
    sanitize_ref_component(&session)
}

fn checkpoints_manifest_dir(git_root: &Path) -> PathBuf {
    git_root.join(CHECKPOINTS_DIR)
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "private directory path is not a real directory: {}",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder
                .create(path)
                .map_err(|e| format!("private directory {}: {e}", path.display()))?;
        }
        Err(error) => {
            return Err(format!(
                "private directory metadata {}: {error}",
                path.display()
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod private directory {}: {e}", path.display()))?;
    }
    Ok(())
}

fn ensure_checkpoint_storage(git_root: &Path) -> Result<(), String> {
    let dext_dir = git_root.join(".dext");
    match std::fs::symlink_metadata(&dext_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!(
                "checkpoint storage parent is not a real directory: {}",
                dext_dir.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_private_dir(&dext_dir)?;
        }
        Err(error) => {
            return Err(format!(
                "checkpoint storage parent metadata {}: {error}",
                dext_dir.display()
            ));
        }
    }
    ensure_private_dir(&checkpoints_manifest_dir(git_root))?;
    let tracked = run_git(git_root, &["ls-files", "--", CHECKPOINTS_DIR])?;
    if !tracked.trim().is_empty() {
        return Err(format!(
            "checkpoint storage is tracked by Git; remove {} from the index before using recovery checkpoints",
            CHECKPOINTS_DIR
        ));
    }
    Ok(())
}

fn checkpoint_storage_exists(git_root: &Path) -> Result<bool, String> {
    for path in [git_root.join(".dext"), checkpoints_manifest_dir(git_root)] {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "checkpoint storage path is not a real directory: {}",
                    path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "checkpoint storage metadata {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(true)
}

fn safe_single_link_file_metadata(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return false;
        }
    }
    true
}

fn safe_private_file_metadata(metadata: &std::fs::Metadata) -> bool {
    if !safe_single_link_file_metadata(metadata) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return false;
        }
    }
    true
}

fn safe_restore_destination_metadata(metadata: &std::fs::Metadata) -> bool {
    safe_single_link_file_metadata(metadata)
}

fn checkpoint_process_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

struct CheckpointOperationLock {
    _process_guard: std::sync::MutexGuard<'static, ()>,
    _file: std::fs::File,
}

impl CheckpointOperationLock {
    fn acquire(git_root: &Path) -> Result<Self, String> {
        let process_guard = checkpoint_process_lock()
            .lock()
            .map_err(|_| "checkpoint operation lock poisoned".to_string())?;
        ensure_checkpoint_storage(git_root)?;
        let path = checkpoints_manifest_dir(git_root).join("operation.lock");
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if !safe_private_file_metadata(&metadata) => {
                return Err(format!(
                    "checkpoint operation lock is not a safe regular file: {}",
                    path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("checkpoint operation lock metadata: {error}")),
        }

        let deadline = std::time::Instant::now() + CHECKPOINT_LOCK_WAIT;
        let file = loop {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt as _;
                options.share_mode(0);
            }
            match options.open(&path) {
                Ok(file) => break file,
                Err(error)
                    if cfg!(windows)
                        && std::time::Instant::now() < deadline
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                        ) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(format!(
                        "open checkpoint operation lock {}: {error}",
                        path.display()
                    ));
                }
            }
        };
        if !safe_private_file_metadata(
            &file
                .metadata()
                .map_err(|error| format!("checkpoint operation lock metadata: {error}"))?,
        ) {
            return Err("checkpoint operation lock is not a safe regular file".to_string());
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            loop {
                let result =
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if result == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if std::time::Instant::now() >= deadline
                    || error.kind() != std::io::ErrorKind::WouldBlock
                {
                    return Err(format!(
                        "lock checkpoint operations {}: {error}",
                        path.display()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("chmod checkpoint operation lock: {error}"))?;
        }
        Ok(Self {
            _process_guard: process_guard,
            _file: file,
        })
    }
}

#[cfg(unix)]
impl Drop for CheckpointOperationLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        unsafe {
            let _ = libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(not(unix))]
impl Drop for CheckpointOperationLock {
    fn drop(&mut self) {}
}

fn ensure_private_dir_tree(base: &Path, target: &Path) -> Result<(), String> {
    ensure_private_dir(base)?;
    let relative = target.strip_prefix(base).map_err(|_| {
        format!(
            "private directory {} escapes {}",
            target.display(),
            base.display()
        )
    })?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!(
                "unsafe private directory component in {}",
                target.display()
            ));
        };
        current.push(name);
        ensure_private_dir(&current)?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> Result<std::fs::File, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(format!(
                "private file path already exists: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("private file metadata {}: {error}", path.display())),
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|e| format!("private file open {}: {e}", path.display()))?;
    if !safe_private_file_metadata(
        &file
            .metadata()
            .map_err(|error| format!("private file metadata {}: {error}", path.display()))?,
    ) {
        return Err(format!(
            "private file path is not a safe regular file: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod private file {}: {e}", path.display()))?;
    }
    Ok(file)
}

fn read_private_file_with_limit(path: &Path, max_bytes: Option<u64>) -> Result<String, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("private file open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("private file metadata {}: {error}", path.display()))?;
    if !safe_private_file_metadata(&metadata) {
        return Err(format!(
            "private file path is not a safe regular file: {}",
            path.display()
        ));
    }
    if let Some(max_bytes) = max_bytes
        && metadata.len() > max_bytes
    {
        return Err(format!(
            "private file exceeds the {max_bytes}-byte inspection bound: {}",
            path.display()
        ));
    }
    let mut content = String::new();
    match max_bytes {
        Some(max_bytes) => {
            file.take(max_bytes + 1)
                .read_to_string(&mut content)
                .map_err(|error| format!("private file read {}: {error}", path.display()))?;
            if content.len() as u64 > max_bytes {
                return Err(format!(
                    "private file exceeds the {max_bytes}-byte inspection bound: {}",
                    path.display()
                ));
            }
        }
        None => {
            file.read_to_string(&mut content)
                .map_err(|error| format!("private file read {}: {error}", path.display()))?;
        }
    }
    Ok(content)
}

fn read_private_file(path: &Path) -> Result<String, String> {
    read_private_file_with_limit(path, None)
}

fn write_private_file(path: &Path, content: &[u8]) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !safe_private_file_metadata(&metadata) => {
            return Err(format!(
                "private file path is not a safe regular file: {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("private file metadata {}: {error}", path.display())),
    }
    crate::session::atomic_write_secret(path, content)
        .map_err(|error| format!("private file write {}: {error}", path.display()))
}

fn canonical_git_metadata_roots(git_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for args in [
        ["rev-parse", "--absolute-git-dir"].as_slice(),
        ["rev-parse", "--git-common-dir"].as_slice(),
    ] {
        let raw = run_git(git_root, args)?;
        let path = PathBuf::from(raw.trim());
        let path = if path.is_absolute() {
            path
        } else {
            git_root.join(path)
        };
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            format!("canonicalize Git metadata path {}: {error}", path.display())
        })?;
        roots.push(canonical);
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn validate_git_internal_parent(git_root: &Path, path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Git internal path has no parent: {}", path.display()))?;
    let resolved = canonicalize_with_missing_ancestors(parent)
        .ok_or_else(|| format!("resolve Git internal path parent: {}", parent.display()))?;
    if canonical_git_metadata_roots(git_root)?
        .iter()
        .any(|root| resolved.starts_with(root))
    {
        Ok(())
    } else {
        Err(format!(
            "Git internal path escapes repository metadata: {}",
            path.display()
        ))
    }
}

fn ensure_checkpoint_git_exclude(git_root: &Path) -> Result<(), String> {
    let git_path = run_git(git_root, &["rev-parse", "--git-path", "info/exclude"])?;
    let raw = PathBuf::from(git_path.trim());
    let exclude = if raw.is_absolute() {
        raw
    } else {
        git_root.join(raw)
    };
    validate_git_internal_parent(git_root, &exclude)?;
    match std::fs::symlink_metadata(&exclude) {
        Ok(metadata) if !safe_single_link_file_metadata(&metadata) => {
            return Err(format!(
                "Git exclude path is not a real file with a single link: {}",
                exclude.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("git exclude metadata: {error}")),
    }
    let existing = match std::fs::read_to_string(&exclude) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("git exclude read: {error}")),
    };
    if existing
        .lines()
        .map(str::trim)
        .any(|line| matches!(line, ".dext/" | "/.dext/"))
    {
        return Ok(());
    }
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("git exclude mkdir: {e}"))?;
    }
    validate_git_internal_parent(git_root, &exclude)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&exclude)
        .map_err(|e| format!("git exclude open: {e}"))?;
    if !safe_single_link_file_metadata(
        &file
            .metadata()
            .map_err(|error| format!("git exclude metadata: {error}"))?,
    ) {
        return Err(format!(
            "Git exclude path is not a real file with a single link: {}",
            exclude.display()
        ));
    }
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file).map_err(|e| format!("git exclude write: {e}"))?;
    }
    writeln!(file, "/.dext/").map_err(|e| format!("git exclude write: {e}"))
}

fn safe_checkpoint_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sidecar_dir(git_root: &Path, id: &str) -> PathBuf {
    debug_assert!(safe_checkpoint_id(id));
    git_root.join(CHECKPOINTS_DIR).join(id)
}

fn is_dirty(git_root: &Path) -> Result<bool, String> {
    let out = run_git(git_root, &["status", "--porcelain"])?;
    Ok(!out.trim().is_empty())
}

fn head_oid(git_root: &Path) -> Result<Option<String>, String> {
    let output = git_command(git_root, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    if output.success() {
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!oid.is_empty()).then_some(oid));
    }
    if output.stderr.is_empty() {
        return Ok(None);
    }
    Err(format!(
        "git rev-parse --verify --quiet HEAD: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Save untracked file content as a sidecar before direct file tools
/// overwrite it. Returns the sidecar directory path on success.
pub(crate) fn save_untracked_sidecar(
    git_root: &Path,
    id: &str,
    abs_path: &Path,
    git_root_relative: &Path,
) -> Result<(), String> {
    if !safe_checkpoint_id(id) || !safe_repo_relative_path(git_root_relative) {
        return Err("unsafe checkpoint sidecar path".to_string());
    }
    let dir = sidecar_dir(git_root, id);
    let dest = dir.join(git_root_relative);
    if let Some(parent) = dest.parent() {
        ensure_private_dir_tree(&checkpoints_manifest_dir(git_root), parent)
            .map_err(|e| format!("sidecar mkdir: {e}"))?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut source = options
        .open(abs_path)
        .map_err(|e| format!("sidecar read open: {e}"))?;
    let source_metadata = source
        .metadata()
        .map_err(|e| format!("sidecar source metadata: {e}"))?;
    if !source_metadata.is_file() {
        return Err("checkpoint sidecar source is not a regular file".to_string());
    }
    let mut destination = create_private_file(&dest).map_err(|e| format!("sidecar write: {e}"))?;
    if let Err(error) = std::io::copy(&mut source, &mut destination) {
        drop(destination);
        let _ = std::fs::remove_file(&dest);
        return Err(format!("sidecar copy: {error}"));
    }
    Ok(())
}

/// Check if a path is tracked by Git.
fn is_tracked(git_root: &Path, rel: &Path) -> bool {
    run_git(
        git_root,
        &["ls-files", "--error-unmatch", rel.to_str().unwrap_or("")],
    )
    .is_ok()
}

fn canonicalize_with_missing_ancestors(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        missing.push(current.file_name()?.to_os_string());
        current = current.parent()?;
        if let Ok(mut canonical) = std::fs::canonicalize(current) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
    }
}

fn safe_repo_relative_path(path: &Path) -> bool {
    let mut names = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        names.push(name);
    }
    if names.is_empty()
        || names
            .iter()
            .any(|name| name.to_string_lossy().eq_ignore_ascii_case(".git"))
    {
        return false;
    }
    !(names
        .first()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(".dext"))
        && names
            .get(1)
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("checkpoints")))
}

fn resolve_user_repo_path(root: &Path, git_root: &Path, user_path: &str) -> Option<PathBuf> {
    let candidate = if Path::new(user_path).is_absolute() {
        PathBuf::from(user_path)
    } else {
        root.join(user_path)
    };
    let canonical_root = std::fs::canonicalize(git_root).ok()?;
    let resolved = canonicalize_with_missing_ancestors(&candidate)?;
    let relative = resolved.strip_prefix(canonical_root).ok()?.to_path_buf();
    safe_repo_relative_path(&relative).then_some(relative)
}

fn manifest_repo_relative_path(root: &Path, git_root: &Path, path: &str) -> Option<PathBuf> {
    if Path::new(path).is_absolute() {
        resolve_user_repo_path(root, git_root, path)
    } else {
        let relative = PathBuf::from(path);
        safe_repo_relative_path(&relative).then_some(relative)
    }
}

fn safe_worktree_target(git_root: &Path, relative: &Path) -> Result<PathBuf, String> {
    if !safe_repo_relative_path(relative) {
        return Err(format!("unsafe checkpoint path: {}", relative.display()));
    }
    let canonical_root = std::fs::canonicalize(git_root)
        .map_err(|error| format!("canonicalize Git root: {error}"))?;
    let parent_relative = relative
        .parent()
        .ok_or_else(|| format!("checkpoint path has no parent: {}", relative.display()))?;
    let mut parent = canonical_root.clone();
    for component in parent_relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!("unsafe checkpoint path: {}", relative.display()));
        };
        parent.push(name);
        match std::fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "checkpoint restore parent is not a real directory: {}",
                    parent.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "checkpoint restore parent metadata {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    Ok(canonical_root.join(relative))
}

fn tree_has_path(git_root: &Path, oid: &str, rel: &str) -> bool {
    run_git(git_root, &["cat-file", "-e", &format!("{oid}:{rel}")]).is_ok()
}

fn remove_worktree_path(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Err(format!(
            "refusing to recursively remove directory during checkpoint restore: {}",
            path.display()
        )),
        Ok(_) => {
            std::fs::remove_file(path).map_err(|e| format!("remove file: {e}"))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("stat path: {e}")),
    }
}

#[cfg(test)]
pub(crate) fn create_checkpoint_ref(
    git_root: &Path,
    ref_name: &str,
    oid: &str,
) -> Result<(), String> {
    if !valid_object_id(oid) || !ref_name.starts_with(REF_SCAN_PREFIX) {
        return Err("invalid checkpoint ref creation request".to_string());
    }
    let zero_oid = "0".repeat(oid.len());
    run_git(git_root, &["update-ref", ref_name, oid, &zero_oid]).map(|_| ())
}

#[cfg(not(test))]
fn create_checkpoint_ref(git_root: &Path, ref_name: &str, oid: &str) -> Result<(), String> {
    let zero_oid = "0".repeat(oid.len());
    run_git(git_root, &["update-ref", ref_name, oid, &zero_oid]).map(|_| ())
}

/// Create a recovery checkpoint before a workspace mutation.
/// Returns None if not in a Git repo or if HEAD is unborn. Returns error only on unexpected
/// failures; checkpoint errors should warn but not block tool execution.
#[cfg(test)]
pub(crate) fn create_checkpoint(
    root: &Path,
    tool: &str,
    paths_hint: &[String],
    ordinal: usize,
) -> Result<Option<Checkpoint>, String> {
    let Some(git_root) = repo_root(root)? else {
        return Ok(None);
    };
    create_checkpoint_in_repo(root, &git_root, tool, paths_hint, ordinal)
}

pub(crate) fn create_checkpoint_in_repo(
    root: &Path,
    git_root: &Path,
    tool: &str,
    paths_hint: &[String],
    ordinal: usize,
) -> Result<Option<Checkpoint>, String> {
    let Some(_) = head_oid(git_root)? else {
        return Ok(None);
    };
    let _operation_lock = CheckpointOperationLock::acquire(git_root)?;
    let Some(head) = head_oid(git_root)? else {
        return Ok(None);
    };

    let ts = now_ms();
    let sess = session_tag();
    let tool_sanitized = sanitize_ref_component(tool);
    let nonce = random_checkpoint_nonce()?;
    let id = format!("{ts}-{ordinal}-{tool_sanitized}-{nonce}");
    let ref_name = format!("{REF_PREFIX}/{sess}/{id}");
    let mut normalized_paths = paths_hint
        .iter()
        .filter_map(|path| resolve_user_repo_path(root, git_root, path))
        .collect::<Vec<_>>();
    normalized_paths.sort();
    normalized_paths.dedup();
    if !paths_hint.is_empty() && normalized_paths.is_empty() {
        return Ok(None);
    }
    let Some(normalized_path_strings) = normalized_paths
        .iter()
        .map(|path| path.to_str().map(String::from))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };

    ensure_checkpoint_git_exclude(git_root)?;
    let dirty = is_dirty(git_root)?;

    // For dirty state, use git stash create to capture a snapshot
    // without touching the working tree or reflog.
    let oid = if dirty {
        let stash_out = run_git(git_root, &["stash", "create"])?;
        let stash_oid = stash_out.trim().to_string();
        if stash_oid.is_empty() {
            // Clean after all (race); use HEAD
            head.clone()
        } else {
            stash_oid
        }
    } else {
        head.clone()
    };

    // Create the ref only if it does not already exist. This compare-and-swap
    // guard prevents an ID collision from overwriting another process's recovery
    // point even if a future entropy source or caller regresses.
    create_checkpoint_ref(git_root, &ref_name, &oid)?;

    // Handle untracked sidecar for direct file tools. A checkpoint that cannot
    // preserve an existing untracked target is not a usable recovery point.
    let mut includes_untracked_sidecar = false;
    let file_tools = ["write_file", "edit_file", "multi_edit"];
    if file_tools.contains(&tool) {
        for rel in &normalized_paths {
            let abs = git_root.join(rel);
            if abs.is_file() && !is_tracked(git_root, rel) {
                if let Err(error) = save_untracked_sidecar(git_root, &id, &abs, rel) {
                    let _ = run_git(git_root, &["update-ref", "-d", &ref_name]);
                    let _ = std::fs::remove_dir_all(sidecar_dir(git_root, &id));
                    return Err(format!(
                        "preserve untracked checkpoint target {}: {error}",
                        rel.display()
                    ));
                }
                includes_untracked_sidecar = true;
            }
        }
    }

    // Direct file tools already capture their exact target via the sidecar
    // above, so skip the broad (and per-call costly) untracked scan for them;
    // it is the value-add for arbitrary commands like bash.
    let untracked_snapshot = if file_tools.contains(&tool) {
        Vec::new()
    } else {
        untracked_files(git_root)
    };

    let cp = Checkpoint {
        id,
        ref_name,
        oid,
        tool_name: tool.to_string(),
        created_at_ms: ts,
        head,
        paths_hint: normalized_path_strings,
        includes_untracked_sidecar,
        untracked_snapshot,
    };

    if let Err(error) = append_manifest(git_root, &cp) {
        let _ = run_git(git_root, &["update-ref", "-d", &cp.ref_name]);
        let _ = std::fs::remove_dir_all(sidecar_dir(git_root, &cp.id));
        return Err(error);
    }
    if let Err(error) =
        prune_checkpoint_refs(git_root, DEFAULT_PRUNE_KEEP, DEFAULT_PRUNE_MAX_AGE_HOURS)
    {
        eprintln!("[checkpoint] retention warning: {error}");
    }

    Ok(Some(cp))
}

fn append_manifest(git_root: &Path, cp: &Checkpoint) -> Result<(), String> {
    ensure_checkpoint_storage(git_root)?;
    let dir = checkpoints_manifest_dir(git_root);
    let manifest_path = dir.join("manifest.txt");
    let mut content = match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if !safe_private_file_metadata(&metadata) => {
            return Err(format!(
                "checkpoint manifest is not a safe regular file: {}",
                manifest_path.display()
            ));
        }
        Ok(_) => {
            read_private_file(&manifest_path).map_err(|error| format!("manifest read: {error}"))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("checkpoint manifest metadata: {error}")),
    };
    if !content.is_empty() && !content.ends_with('\n') {
        return Err("checkpoint manifest has an incomplete final line".to_string());
    }
    let mut ids = std::collections::HashSet::new();
    let mut refs = std::collections::HashSet::new();
    for (line_index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let existing = parse_manifest_line(line.trim()).ok_or_else(|| {
            format!(
                "invalid checkpoint manifest entry at line {}",
                line_index + 1
            )
        })?;
        if !ids.insert(existing.id) || !refs.insert(existing.ref_name) {
            return Err(format!(
                "duplicate checkpoint metadata in manifest at line {}",
                line_index + 1
            ));
        }
    }
    if ids.contains(&cp.id) || refs.contains(&cp.ref_name) {
        return Err(format!("checkpoint already exists in manifest: {}", cp.id));
    }
    content.push_str(&format_manifest_line(cp));
    content.push('\n');
    write_private_file(&manifest_path, content.as_bytes())
        .map_err(|error| format!("manifest write: {error}"))
}

fn format_manifest_line(cp: &Checkpoint) -> String {
    let paths = serde_json::to_string(&cp.paths_hint).unwrap_or_else(|_| "[]".to_string());
    let untracked =
        serde_json::to_string(&cp.untracked_snapshot).unwrap_or_else(|_| "[]".to_string());
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        cp.id,
        cp.ref_name,
        cp.oid,
        cp.tool_name,
        cp.created_at_ms,
        cp.head,
        cp.includes_untracked_sidecar,
        paths,
        untracked,
    )
}

fn write_checkpoint_manifest(
    git_root: &Path,
    checkpoints_newest_first: &[Checkpoint],
) -> Result<(), String> {
    let content = checkpoints_newest_first
        .iter()
        .rev()
        .map(format_manifest_line)
        .collect::<Vec<_>>()
        .join("\n");
    let manifest_path = checkpoints_manifest_dir(git_root).join("manifest.txt");
    write_private_file(
        &manifest_path,
        if content.is_empty() {
            String::new()
        } else {
            format!("{content}\n")
        }
        .as_bytes(),
    )
    .map_err(|error| format!("manifest write: {error}"))
}

fn checkpoint_ref_valid(ref_name: &str, id: &str) -> bool {
    let Some(suffix) = ref_name.strip_prefix(&format!("{REF_PREFIX}/")) else {
        return false;
    };
    let mut parts = suffix.split('/');
    let (Some(session), Some(ref_id), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    safe_checkpoint_id(session) && ref_id == id
}

fn valid_checkpoint_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn parse_manifest_line(line: &str) -> Option<Checkpoint> {
    let parts: Vec<&str> = line.splitn(9, '\t').collect();
    if parts.len() != 9 {
        return None;
    }
    let paths_hint = serde_json::from_str::<Vec<String>>(parts[7]).ok()?;
    let untracked_snapshot = serde_json::from_str::<Vec<String>>(parts[8]).ok()?;
    let id = parts[0];
    let ref_name = parts[1];
    let oid = parts[2];
    let head = parts[5];
    if !safe_checkpoint_id(id)
        || !checkpoint_ref_valid(ref_name, id)
        || !valid_object_id(oid)
        || !valid_object_id(head)
        || !valid_checkpoint_tool_name(parts[3])
        || !matches!(parts[6], "true" | "false")
        || !paths_hint
            .iter()
            .all(|path| safe_repo_relative_path(Path::new(path)))
        || !untracked_snapshot
            .iter()
            .all(|path| safe_repo_relative_path(Path::new(path)))
        || (parts[6] == "true" && paths_hint.is_empty())
    {
        return None;
    }
    Some(Checkpoint {
        id: id.to_string(),
        ref_name: ref_name.to_string(),
        oid: oid.to_string(),
        tool_name: parts[3].to_string(),
        created_at_ms: parts[4].parse().ok()?,
        head: head.to_string(),
        paths_hint,
        includes_untracked_sidecar: parts[6] == "true",
        untracked_snapshot,
    })
}

fn list_checkpoints_in_repo(
    git_root: &Path,
    limit: usize,
    tolerate_missing_refs: bool,
    manifest_max_bytes: Option<u64>,
) -> Result<Vec<Checkpoint>, String> {
    let manifest_path = checkpoints_manifest_dir(git_root).join("manifest.txt");
    let content = match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if !safe_private_file_metadata(&metadata) => {
            return Err(format!(
                "checkpoint manifest is not a safe regular file: {}",
                manifest_path.display()
            ));
        }
        Ok(_) => read_private_file_with_limit(&manifest_path, manifest_max_bytes)
            .map_err(|error| format!("read checkpoint manifest: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("checkpoint manifest metadata: {error}")),
    };
    if !content.is_empty() && !content.ends_with('\n') {
        return Err("checkpoint manifest has an incomplete final line".to_string());
    }
    let refs = run_git(
        git_root,
        &[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)",
            REF_SCAN_PREFIX,
        ],
    )?;
    let mut existing_refs = std::collections::HashMap::new();
    for (line_index, line) in refs.lines().enumerate() {
        let Some((ref_name, oid)) = line.split_once('\t') else {
            return Err(format!(
                "malformed checkpoint ref listing at line {}",
                line_index + 1
            ));
        };
        if !valid_object_id(oid) {
            return Err(format!("invalid checkpoint ref object ID: {ref_name}"));
        }
        existing_refs.insert(ref_name.to_string(), oid.to_string());
    }

    let mut cps = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let mut manifest_refs = std::collections::HashSet::new();
    for (line_index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let checkpoint = parse_manifest_line(line.trim()).ok_or_else(|| {
            format!(
                "invalid checkpoint manifest entry at line {}",
                line_index + 1
            )
        })?;
        if !ids.insert(checkpoint.id.clone()) {
            return Err(format!(
                "duplicate checkpoint id in manifest: {}",
                checkpoint.id
            ));
        }
        if !manifest_refs.insert(checkpoint.ref_name.clone()) {
            return Err(format!(
                "duplicate checkpoint ref in manifest: {}",
                checkpoint.ref_name
            ));
        }
        if let Some(ref_oid) = existing_refs.get(&checkpoint.ref_name) {
            if ref_oid != &checkpoint.oid {
                return Err(format!(
                    "checkpoint ref no longer matches manifest OID: {}",
                    checkpoint.ref_name
                ));
            }
            cps.push((line_index, checkpoint));
        } else if !tolerate_missing_refs {
            return Err(format!(
                "checkpoint manifest references a missing ref: {}",
                checkpoint.ref_name
            ));
        }
    }
    // Newest first. Manifest append order breaks ties when several checkpoints
    // share the same millisecond timestamp.
    cps.sort_by_key(|(line_index, checkpoint)| {
        std::cmp::Reverse((checkpoint.created_at_ms, *line_index))
    });
    let mut cps = cps
        .into_iter()
        .map(|(_, checkpoint)| checkpoint)
        .collect::<Vec<_>>();
    cps.truncate(limit);
    Ok(cps)
}

pub(crate) fn inspect_checkpoints(root: &Path, limit: usize) -> Result<Vec<Checkpoint>, String> {
    let Some(git_root) = repo_root(root)? else {
        return Ok(Vec::new());
    };
    if !checkpoint_storage_exists(&git_root)? {
        return Ok(Vec::new());
    }
    let manifest = checkpoints_manifest_dir(&git_root).join("manifest.txt");
    if let Ok(metadata) = std::fs::symlink_metadata(&manifest)
        && metadata.len() > 256 * 1024
    {
        return Err("checkpoint manifest exceeds the doctor inspection bound".to_string());
    }
    list_checkpoints_in_repo(&git_root, limit, false, Some(256 * 1024))
}

pub(crate) fn list_checkpoints(root: &Path, limit: usize) -> Result<Vec<Checkpoint>, String> {
    let Some(git_root) = repo_root(root)? else {
        return Ok(Vec::new());
    };
    if !checkpoint_storage_exists(&git_root)? {
        return Ok(Vec::new());
    }
    let _operation_lock = CheckpointOperationLock::acquire(&git_root)?;
    list_checkpoints_in_repo(&git_root, limit, false, None)
}

fn validate_checkpoint(cp: &Checkpoint) -> Result<(), String> {
    if !safe_checkpoint_id(&cp.id)
        || !checkpoint_ref_valid(&cp.ref_name, &cp.id)
        || !valid_object_id(&cp.oid)
        || !valid_object_id(&cp.head)
        || !valid_checkpoint_tool_name(&cp.tool_name)
    {
        return Err("checkpoint metadata failed validation".to_string());
    }
    if let Some(path) = cp
        .paths_hint
        .iter()
        .find(|path| !safe_repo_relative_path(Path::new(path)))
    {
        return Err(format!("unsafe checkpoint path: {path}"));
    }
    if let Some(path) = cp
        .untracked_snapshot
        .iter()
        .find(|path| !safe_repo_relative_path(Path::new(path)))
    {
        return Err(format!("unsafe checkpoint untracked path: {path}"));
    }
    if cp.includes_untracked_sidecar && cp.paths_hint.is_empty() {
        return Err("checkpoint sidecar has no declared restore paths".to_string());
    }
    Ok(())
}

fn validate_checkpoint_ref(git_root: &Path, cp: &Checkpoint) -> Result<(), String> {
    validate_checkpoint(cp)?;
    let ref_oid = run_git(git_root, &["rev-parse", "--verify", &cp.ref_name])?;
    if ref_oid.trim() != cp.oid {
        return Err("checkpoint ref no longer matches its manifest OID".to_string());
    }
    let commit_expr = format!("{}^{{commit}}", cp.oid);
    if run_git(git_root, &["rev-parse", "--verify", &commit_expr])?.trim() != cp.oid {
        return Err("checkpoint OID is not a commit".to_string());
    }
    if cp.oid != cp.head {
        let parent_expr = format!("{}^1", cp.oid);
        if run_git(git_root, &["rev-parse", "--verify", &parent_expr])?.trim() != cp.head {
            return Err("checkpoint HEAD does not match the snapshot parent".to_string());
        }
    }
    Ok(())
}

fn preview_restore_locked(git_root: &Path, cp: &Checkpoint) -> Result<String, String> {
    validate_checkpoint_ref(git_root, cp)?;

    let mut out = String::new();
    out.push_str(&format!("Checkpoint: {}\n", cp.id));
    out.push_str(&format!("Tool: {}\n", cp.tool_name));
    out.push_str(&format!("Ref: {}\n", cp.ref_name));
    out.push_str(&format!("OID: {}\n", cp.oid));
    out.push_str(&format!("HEAD at time: {}\n", cp.head));
    if !cp.paths_hint.is_empty() {
        out.push_str(&format!("Paths: {}\n", cp.paths_hint.join(", ")));
    }
    if cp.includes_untracked_sidecar {
        out.push_str("Includes untracked file sidecar(s)\n");
    }

    // Show diff of checkpoint restore target vs current worktree.
    let diff = run_git(
        git_root,
        &["diff", "--no-ext-diff", "--no-textconv", "--stat", &cp.oid],
    )
    .unwrap_or_else(|e| format!("(diff unavailable: {e})"));
    if !diff.trim().is_empty() {
        out.push_str("\nRestore diff vs current worktree:\n");
        out.push_str(&diff);
    }

    let full_diff = run_git(
        git_root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            &cp.oid,
        ],
    )
    .unwrap_or_default();
    let capped = cap_diff(&full_diff, 4000);
    if !capped.is_empty() {
        out.push_str("\nUnified diff (capped):\n");
        out.push_str(&capped);
    }

    if cp.includes_untracked_sidecar {
        let sdir = sidecar_dir(git_root, &cp.id);
        if sdir.is_dir() {
            out.push_str("\nUntracked sidecar files present; ");
            out.push_str("restore will copy them back.\n");
        } else {
            out.push_str("\nWARNING: expected untracked sidecar files are unavailable; apply will fail closed.\n");
        }
    }

    // Untracked-file delta since the checkpoint. `git stash create` does not
    // capture untracked content, so name what changed instead of restoring it.
    let before: std::collections::HashSet<&str> =
        cp.untracked_snapshot.iter().map(String::as_str).collect();
    let now = untracked_files(git_root);
    let now_set: std::collections::HashSet<&str> = now.iter().map(String::as_str).collect();
    let created: Vec<&str> = now
        .iter()
        .map(String::as_str)
        .filter(|p| !before.contains(p))
        .collect();
    let removed: Vec<&str> = cp
        .untracked_snapshot
        .iter()
        .map(String::as_str)
        .filter(|p| !now_set.contains(p))
        .collect();
    if !created.is_empty() {
        out.push_str(
            "\nUntracked files created since checkpoint (restore will NOT remove them):\n",
        );
        for p in created.iter().take(50) {
            out.push_str(&format!("  + {p}\n"));
        }
    }
    if !removed.is_empty() {
        out.push_str(
            "\nUntracked files present at checkpoint but gone now (content not recoverable):\n",
        );
        for p in removed.iter().take(50) {
            out.push_str(&format!("  - {p}\n"));
        }
    }

    out.push_str("\nUse --apply or /undo --apply to restore.\n");
    Ok(out)
}

pub(crate) fn preview_restore(root: &Path, cp: &Checkpoint) -> Result<String, String> {
    let Some(git_root) = repo_root(root)? else {
        return Err("not a git repository".to_string());
    };
    let _operation_lock = CheckpointOperationLock::acquire(&git_root)?;
    preview_restore_locked(&git_root, cp)
}

fn cap_diff(diff: &str, max_bytes: usize) -> String {
    if diff.len() <= max_bytes {
        return diff.to_string();
    }
    let mut result = String::with_capacity(max_bytes + 100);
    for line in diff.lines() {
        if result.len() + line.len() + 1 > max_bytes {
            result.push_str("\n... (diff truncated)");
            break;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

fn preflight_restore_destination(
    target: &Path,
    expected_mode: Option<&str>,
) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Some(metadata)),
        Ok(metadata) if metadata.is_file() && !safe_restore_destination_metadata(&metadata) => {
            Err(format!(
                "checkpoint restore destination is multiply linked: {}",
                target.display()
            ))
        }
        Ok(metadata)
            if metadata.is_dir() && !matches!(expected_mode, Some("040000" | "160000")) =>
        {
            Err(format!(
                "refusing to replace directory during checkpoint restore: {}",
                target.display()
            ))
        }
        Ok(metadata) if !metadata.is_file() && !metadata.is_dir() => Err(format!(
            "checkpoint restore destination is not a regular file or directory: {}",
            target.display()
        )),
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("checkpoint restore target metadata: {error}")),
    }
}

fn git_restore_path_modes(
    git_root: &Path,
    source_oid: &str,
) -> Result<(GitPathModes, GitPathModes), String> {
    let source = run_git_mode_path_list(git_root, &["ls-tree", "-rz", "-r", source_oid])?;
    let index = run_git_mode_path_list(git_root, &["ls-files", "-s", "-z"])?;
    Ok((source, index))
}

fn preflight_git_restore_destinations(
    git_root: &Path,
    source_oid: &str,
    restore_paths: &[(PathBuf, PathBuf)],
    full_restore: bool,
) -> Result<(), String> {
    let (source_modes, index_modes) = git_restore_path_modes(git_root, source_oid)?;
    let mut tracked_paths = source_modes
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    tracked_paths.extend(index_modes.keys().cloned());

    for relative in tracked_paths {
        if !full_restore
            && !restore_paths
                .iter()
                .any(|(hint, _)| relative == *hint || relative.starts_with(hint))
        {
            continue;
        }
        if !safe_repo_relative_path(&relative) {
            return Err(format!(
                "unsafe tracked checkpoint restore path: {}",
                relative.display()
            ));
        }
        let target = safe_worktree_target(git_root, &relative)?;
        let _ = preflight_restore_destination(
            &target,
            source_modes.get(&relative).map(String::as_str),
        )?;
    }
    for (relative, target) in restore_paths {
        let expected_mode = source_modes.get(relative).map(String::as_str).or_else(|| {
            source_modes
                .keys()
                .any(|source| source.starts_with(relative))
                .then_some("040000")
        });
        let _ = preflight_restore_destination(target, expected_mode)?;
    }
    Ok(())
}

fn preflight_restore_paths(
    root: &Path,
    git_root: &Path,
    cp: &Checkpoint,
    full_restore: bool,
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let restore_paths = cp
        .paths_hint
        .iter()
        .map(|path| {
            let relative = manifest_repo_relative_path(root, git_root, path)
                .ok_or_else(|| format!("unsafe checkpoint path: {path}"))?;
            let target = safe_worktree_target(git_root, &relative)?;
            Ok((relative, target))
        })
        .collect::<Result<Vec<_>, String>>()?;
    preflight_git_restore_destinations(git_root, &cp.oid, &restore_paths, full_restore)?;
    Ok(restore_paths)
}

fn preflight_sidecar_restore(
    git_root: &Path,
    cp: &Checkpoint,
    restore_paths: &[(PathBuf, PathBuf)],
) -> Result<Vec<PathBuf>, String> {
    let sdir = sidecar_dir(git_root, &cp.id);
    let metadata = match std::fs::symlink_metadata(&sdir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if cp.includes_untracked_sidecar {
                Err("required untracked checkpoint sidecar is missing".to_string())
            } else {
                Ok(Vec::new())
            };
        }
        Err(error) => return Err(format!("checkpoint sidecar metadata: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("checkpoint sidecar directory is unsafe".to_string());
    }
    let entries = walk_dir(&sdir)?;
    if entries.is_empty() {
        return if cp.includes_untracked_sidecar {
            Err("required untracked checkpoint sidecar is empty".to_string())
        } else {
            Ok(Vec::new())
        };
    }
    if !cp.includes_untracked_sidecar {
        return Err("checkpoint has unexpected untracked sidecar files".to_string());
    }
    let allowed = restore_paths
        .iter()
        .map(|(relative, _)| relative.as_path())
        .collect::<std::collections::HashSet<_>>();
    for entry in &entries {
        let relative = entry
            .strip_prefix(&sdir)
            .map_err(|_| "checkpoint sidecar escapes its storage directory".to_string())?;
        if !safe_repo_relative_path(relative) || !allowed.contains(relative) {
            return Err(format!(
                "checkpoint sidecar targets undeclared path: {}",
                relative.display()
            ));
        }
        let target = safe_worktree_target(git_root, relative)?;
        match std::fs::symlink_metadata(&target) {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    || !safe_restore_destination_metadata(&metadata) =>
            {
                return Err(format!(
                    "sidecar restore destination is unsafe or multiply linked: {}",
                    target.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("sidecar restore destination metadata: {error}")),
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(entry)
            .map_err(|error| format!("open checkpoint sidecar {}: {error}", entry.display()))?;
        if !safe_private_file_metadata(
            &file
                .metadata()
                .map_err(|error| format!("checkpoint sidecar metadata: {error}"))?,
        ) {
            return Err(format!(
                "checkpoint sidecar is not a safe private file: {}",
                entry.display()
            ));
        }
    }
    Ok(entries)
}

pub(crate) fn restore_worktree(
    root: &Path,
    cp: &Checkpoint,
    mode: RestoreMode,
) -> Result<String, String> {
    let Some(git_root) = repo_root(root)? else {
        return Err("not a git repository".to_string());
    };
    let _operation_lock = CheckpointOperationLock::acquire(&git_root)?;
    validate_checkpoint_ref(&git_root, cp)?;

    if mode == RestoreMode::Preview {
        return preview_restore_locked(&git_root, cp);
    }

    if mode == RestoreMode::ResetHead {
        preflight_git_restore_destinations(&git_root, &cp.head, &[], true)?;
        return reset_head(&git_root, cp);
    }

    let full_restore = mode == RestoreMode::WorktreeAndIndex || cp.paths_hint.is_empty();
    let restore_paths = preflight_restore_paths(root, &git_root, cp, full_restore)?;
    let sidecar_entries = preflight_sidecar_restore(&git_root, cp, &restore_paths)?;
    let sdir = sidecar_dir(&git_root, &cp.id);
    let sidecar_paths = sidecar_entries
        .iter()
        .filter_map(|entry| entry.strip_prefix(&sdir).ok())
        .collect::<std::collections::HashSet<_>>();

    // Worktree restore: checkout paths from checkpoint OID
    let mut restored: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if !restore_paths.is_empty() {
        for (rel, target) in &restore_paths {
            let rel_str = rel.to_string_lossy().to_string();
            let result = if tree_has_path(&git_root, &cp.oid, &rel_str) {
                run_git(
                    &git_root,
                    &["restore", "--source", &cp.oid, "--worktree", "--", &rel_str],
                )
                .map(|_| {
                    restored.push(rel_str.clone());
                })
            } else if sidecar_paths.contains(rel.as_path()) {
                Ok(())
            } else {
                remove_worktree_path(target).map(|removed| {
                    if removed {
                        restored.push(format!("removed {rel_str}"));
                    }
                })
            };
            if let Err(error) = result {
                warnings.push(format!("could not restore {rel_str}: {error}"));
            }
        }
    }

    // If no specific paths, restore all worktree paths. WorktreeAndIndex is an
    // internal explicit mode that also restores the index from the snapshot.
    if mode == RestoreMode::WorktreeAndIndex || cp.paths_hint.is_empty() {
        let mut args = vec!["restore", "--source", cp.oid.as_str()];
        if mode == RestoreMode::WorktreeAndIndex {
            args.push("--staged");
        }
        args.extend(["--worktree", "--", "."]);
        run_git(&git_root, &args)?;
        restored.push("(all worktree files)".to_string());
    }

    // Restore sidecar untracked files that were fully validated before any
    // worktree mutation above.
    for entry in sidecar_entries {
        let rel = entry
            .strip_prefix(&sdir)
            .expect("preflighted sidecar remains below checkpoint directory");
        match safe_worktree_target(&git_root, rel)
            .and_then(|_| copy_sidecar_file(&entry, &git_root, rel))
        {
            Ok(()) => restored.push(rel.display().to_string()),
            Err(error) => warnings.push(format!("sidecar restore failed: {error}")),
        }
    }

    if !warnings.is_empty() {
        return Err(format!(
            "checkpoint restore was incomplete after restoring {}:\n  {}",
            if restored.is_empty() {
                "no paths".to_string()
            } else {
                restored.join("\n  ")
            },
            warnings.join("\n  ")
        ));
    }
    Ok(format!(
        "Restored from checkpoint {}:\n  {}\nRef preserved for further inspection.",
        cp.id,
        restored.join("\n  "),
    ))
}

fn reset_head(git_root: &Path, cp: &Checkpoint) -> Result<String, String> {
    validate_checkpoint(cp)?;
    run_git(git_root, &["reset", "--hard", &cp.head])?;
    Ok(format!(
        "Reset HEAD to {} (from checkpoint {}).\nWorking tree and index now match that commit. The checkpoint ref remains available for restoring its captured uncommitted snapshot.",
        cp.head, cp.id,
    ))
}

fn ensure_worktree_parent_tree(git_root: &Path, relative: &Path) -> Result<PathBuf, String> {
    if !safe_repo_relative_path(relative) {
        return Err(format!("unsafe checkpoint path: {}", relative.display()));
    }
    let canonical_root = std::fs::canonicalize(git_root)
        .map_err(|error| format!("canonicalize Git root: {error}"))?;
    let parent_relative = relative
        .parent()
        .ok_or_else(|| format!("restore destination has no parent: {}", relative.display()))?;
    let mut parent = canonical_root.clone();
    for component in parent_relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!("unsafe checkpoint path: {}", relative.display()));
        };
        parent.push(name);
        match std::fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "restore parent is not a real directory: {}",
                    parent.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&parent).map_err(|error| {
                    format!("create restore parent {}: {error}", parent.display())
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "restore parent metadata {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    let canonical_parent = std::fs::canonicalize(&parent)
        .map_err(|error| format!("canonicalize restore parent {}: {error}", parent.display()))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(format!(
            "checkpoint path escapes repository through a symlink: {}",
            relative.display()
        ));
    }
    Ok(canonical_parent.join(relative.file_name().ok_or_else(|| {
        format!(
            "restore destination has no file name: {}",
            relative.display()
        )
    })?))
}

fn copy_sidecar_file(source: &Path, git_root: &Path, relative: &Path) -> Result<(), String> {
    let destination = ensure_worktree_parent_tree(git_root, relative)?;
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("sidecar metadata {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "sidecar source is not a real file: {}",
            source.display()
        ));
    }
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !safe_restore_destination_metadata(&metadata) =>
        {
            return Err(format!(
                "restore destination is not a safe single-link file: {}",
                destination.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("restore destination metadata: {error}")),
    }
    let mut source_options = OpenOptions::new();
    source_options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        source_options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut input = source_options
        .open(source)
        .map_err(|error| format!("open sidecar {}: {error}", source.display()))?;
    if !safe_private_file_metadata(
        &input
            .metadata()
            .map_err(|error| format!("sidecar metadata {}: {error}", source.display()))?,
    ) {
        return Err(format!(
            "sidecar source is not a safe private file: {}",
            source.display()
        ));
    }

    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("restore");
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "restore destination has no parent: {}",
            destination.display()
        )
    })?;
    let mut temp_path = None;
    let mut output = None;
    for _ in 0..16 {
        let nonce = random_checkpoint_nonce()?;
        let candidate = parent.join(format!(".{file_name}.dext-restore-{nonce}"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temp_path = Some(candidate);
                output = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create sidecar restore temp in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    let temp_path = temp_path.ok_or_else(|| {
        format!(
            "could not allocate sidecar restore temp in {}",
            parent.display()
        )
    })?;
    let mut output = output.expect("temp path and file are created together");
    let result = (|| -> Result<(), String> {
        std::io::copy(&mut input, &mut output)
            .map_err(|error| format!("copy sidecar to temp: {error}"))?;
        output
            .sync_all()
            .map_err(|error| format!("sync sidecar restore temp: {error}"))?;
        drop(output);
        std::fs::rename(&temp_path, &destination).map_err(|error| {
            format!(
                "replace restore destination {}: {error}",
                destination.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn walk_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata =
        std::fs::symlink_metadata(dir).map_err(|e| format!("directory metadata: {e}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("unsafe sidecar directory: {}", dir.display()));
    }
    let mut result = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir_entry: {e}"))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| format!("metadata: {e}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("unsafe sidecar symlink: {}", path.display()));
        }
        if metadata.is_dir() {
            let mut sub = walk_dir(&path)?;
            result.append(&mut sub);
        } else if safe_private_file_metadata(&metadata) {
            result.push(path);
        } else {
            return Err(format!("unsafe sidecar entry: {}", path.display()));
        }
    }
    Ok(result)
}

struct PruneOutcome {
    checkpoints_removed: usize,
    orphan_sidecars_removed: usize,
}

fn prune_checkpoint_refs(
    git_root: &Path,
    keep: usize,
    max_age_hours: u64,
) -> Result<PruneOutcome, String> {
    ensure_checkpoint_storage(git_root)?;
    let now = now_ms();
    let max_age_ms = (max_age_hours as u128) * 3_600_000;
    let cps = list_checkpoints_in_repo(git_root, usize::MAX, true, None)?;
    let mut remaining = Vec::with_capacity(cps.len());
    let mut expired = Vec::new();
    for (i, cp) in cps.into_iter().enumerate() {
        let age = now.saturating_sub(cp.created_at_ms);
        if i >= keep || age > max_age_ms {
            expired.push(cp);
        } else {
            remaining.push(cp);
        }
    }
    // Delete refs before compacting the manifest. If a ref deletion fails, the
    // original manifest still names every live checkpoint ref. A later manifest
    // write failure can leave stale entries for already-deleted refs, which list
    // and the next prune safely ignore; it cannot leave an unmanifested live ref.
    for cp in &expired {
        run_git(git_root, &["update-ref", "-d", &cp.ref_name])?;
    }
    write_checkpoint_manifest(git_root, &remaining)?;
    let orphan_sidecars_removed = prune_orphan_sidecars(git_root, &remaining)?;
    Ok(PruneOutcome {
        checkpoints_removed: expired.len(),
        orphan_sidecars_removed,
    })
}

fn prune_orphan_sidecars(git_root: &Path, remaining: &[Checkpoint]) -> Result<usize, String> {
    let dir = checkpoints_manifest_dir(git_root);
    let remaining_ids = remaining
        .iter()
        .map(|checkpoint| checkpoint.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&dir).map_err(|error| format!("checkpoint dir read: {error}"))? {
        let entry = entry.map_err(|error| format!("checkpoint dir entry: {error}"))?;
        let name = entry.file_name();
        let Some(id) = name.to_str() else {
            continue;
        };
        if !safe_checkpoint_id(id) || remaining_ids.contains(id) {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("checkpoint sidecar metadata: {error}"))?;
        if metadata.file_type().is_symlink() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("remove orphan checkpoint symlink: {error}"))?;
            removed += 1;
        } else if metadata.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|error| format!("remove orphan checkpoint sidecar: {error}"))?;
            removed += 1;
        } else {
            return Err(format!(
                "unexpected checkpoint sidecar entry: {}",
                path.display()
            ));
        }
    }
    Ok(removed)
}

pub(crate) fn prune(
    root: &Path,
    keep: Option<usize>,
    max_age_hours: Option<u64>,
) -> Result<String, String> {
    let Some(git_root) = repo_root(root)? else {
        return Ok("not a git repository, nothing to prune".to_string());
    };

    let _operation_lock = CheckpointOperationLock::acquire(&git_root)?;
    ensure_checkpoint_git_exclude(&git_root)?;
    let keep = keep.unwrap_or(DEFAULT_PRUNE_KEEP);
    let max_age = max_age_hours.unwrap_or(DEFAULT_PRUNE_MAX_AGE_HOURS);
    let outcome = prune_checkpoint_refs(&git_root, keep, max_age)?;
    let mut removed = outcome.checkpoints_removed;
    let mut orphan_sidecars_removed = outcome.orphan_sidecars_removed;
    let cps = list_checkpoints_in_repo(&git_root, usize::MAX, true, None)?;

    // Reconcile refs created outside the private manifest, then rewrite the
    // manifest after all namespace cleanup while the repository lock is held.
    let remaining: Vec<Checkpoint> = cps
        .into_iter()
        .filter(|cp| run_git(&git_root, &["rev-parse", "--verify", &cp.ref_name]).is_ok())
        .collect();
    let remaining_refs: std::collections::HashSet<&str> =
        remaining.iter().map(|cp| cp.ref_name.as_str()).collect();
    let refs = run_git(
        &git_root,
        &["for-each-ref", "--format=%(refname)", REF_SCAN_PREFIX],
    )?;
    for ref_name in refs.lines().map(str::trim).filter(|name| !name.is_empty()) {
        if !remaining_refs.contains(ref_name) {
            run_git(&git_root, &["update-ref", "-d", ref_name])?;
            removed += 1;
        }
    }

    orphan_sidecars_removed += prune_orphan_sidecars(&git_root, &remaining)?;

    write_checkpoint_manifest(&git_root, &remaining)?;

    Ok(format!(
        "pruned {removed} checkpoint(s) and {orphan_sidecars_removed} orphan sidecar entr{}, {} remaining",
        if orphan_sidecars_removed == 1 {
            "y"
        } else {
            "ies"
        },
        remaining.len()
    ))
}

pub(crate) fn find_checkpoint(root: &Path, id_or_ref: &str) -> Result<Option<Checkpoint>, String> {
    let selector = id_or_ref.trim();
    if selector.is_empty() {
        return Err("checkpoint selector cannot be empty".to_string());
    }
    let cps = list_checkpoints(root, usize::MAX)?;
    if let Some(cp) = cps
        .iter()
        .find(|cp| cp.id == selector || cp.ref_name == selector)
    {
        return Ok(Some(cp.clone()));
    }
    let mut matches = cps.iter().filter(|cp| cp.id.starts_with(selector));
    let Some(checkpoint) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(format!(
            "checkpoint selector is ambiguous; use a longer ID: {selector}"
        ));
    }
    Ok(Some(checkpoint.clone()))
}

pub(crate) fn latest_checkpoint(root: &Path) -> Result<Option<Checkpoint>, String> {
    let cps = list_checkpoints(root, 1)?;
    Ok(cps.into_iter().next())
}

/// Determine if a tool needs a checkpoint based on command risk.
pub(crate) fn tool_needs_checkpoint(name: &str, input: &serde_json::Value) -> bool {
    let always = [
        "write_file",
        "edit_file",
        "multi_edit",
        "todo_write",
        "git_commit",
    ];
    if always.contains(&name) {
        return true;
    }
    // For bash/awk/csvkit, checkpoint only if write-risk
    let risk = crate::tool_policy::classify_command_risk(name, input);
    risk != crate::tool_policy::CommandRisk::Read
}

/// Cached repo root lookup for the session.
pub(crate) struct RepoRootCache {
    cached: Option<Option<PathBuf>>,
}

impl RepoRootCache {
    pub(crate) fn new() -> Self {
        Self { cached: None }
    }

    pub(crate) fn get(&mut self, root: &Path) -> Result<Option<PathBuf>, String> {
        if self.cached.is_none() {
            self.cached = Some(repo_root(root)?);
        }
        Ok(self.cached.as_ref().unwrap().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fixture_entries(content: &str) -> Result<Vec<Checkpoint>, String> {
        if !content.is_empty() && !content.ends_with('\n') {
            return Err("checkpoint manifest has an incomplete final line".to_string());
        }
        let mut ids = std::collections::HashSet::new();
        let mut refs = std::collections::HashSet::new();
        content
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                let checkpoint = parse_manifest_line(line.trim()).ok_or_else(|| {
                    format!("invalid checkpoint manifest entry at line {}", index + 1)
                })?;
                if !ids.insert(checkpoint.id.clone()) {
                    return Err(format!(
                        "duplicate checkpoint id in manifest: {}",
                        checkpoint.id
                    ));
                }
                if !refs.insert(checkpoint.ref_name.clone()) {
                    return Err(format!(
                        "duplicate checkpoint ref in manifest: {}",
                        checkpoint.ref_name
                    ));
                }
                Ok(checkpoint)
            })
            .collect()
    }

    #[test]
    fn checked_in_manifest_fixtures_cover_valid_and_corrupt_shapes() {
        let valid = parse_fixture_entries(include_str!(
            "../tests/fixtures/state/checkpoints/valid.manifest"
        ))
        .expect("valid checkpoint fixture");
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].paths_hint, vec!["src/lib.rs"]);

        let sidecar = parse_fixture_entries(include_str!(
            "../tests/fixtures/state/checkpoints/sidecar.manifest"
        ))
        .expect("sidecar checkpoint fixture");
        assert!(sidecar[0].includes_untracked_sidecar);
        assert_eq!(sidecar[0].paths_hint, vec!["note.txt"]);

        let incomplete = parse_fixture_entries(include_str!(
            "../tests/fixtures/state/checkpoints/incomplete.manifest"
        ))
        .unwrap_err();
        assert!(incomplete.contains("incomplete final line"), "{incomplete}");

        let duplicate = parse_fixture_entries(include_str!(
            "../tests/fixtures/state/checkpoints/duplicate-id.manifest"
        ))
        .unwrap_err();
        assert!(duplicate.contains("duplicate checkpoint id"), "{duplicate}");

        let unsafe_path = parse_fixture_entries(include_str!(
            "../tests/fixtures/state/checkpoints/unsafe-path.manifest"
        ))
        .unwrap_err();
        assert!(
            unsafe_path.contains("invalid checkpoint manifest entry"),
            "{unsafe_path}"
        );

        let missing_ref = parse_fixture_entries(include_str!(
            "../tests/fixtures/state/checkpoints/missing-ref.manifest"
        ))
        .expect("missing-ref shape is syntactically valid");
        assert_eq!(
            missing_ref[0].ref_name,
            "refs/dext/checkpoints/session/fixture-missing"
        );
    }
}
