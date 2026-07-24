// Phase 1: Git-native recovery checkpoints.
//
// Before Dext performs an approved workspace mutation, create a local Git
// recovery point under refs/dext/checkpoints/... Add /undo and CLI support
// to preview and restore the latest checkpoint.

use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

const CHECKPOINTS_DIR: &str = ".dext/checkpoints";
const REF_PREFIX: &str = "refs/dext/checkpoints";
const REF_SCAN_PREFIX: &str = "refs/dext/checkpoints/";
const DEFAULT_PRUNE_KEEP: usize = 20;
const DEFAULT_PRUNE_MAX_AGE_HOURS: u64 = 168; // 7 days
const CHECKPOINT_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const CHECKPOINT_MANIFEST_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum UntrackedSidecar {
    File {
        path: String,
        digest: String,
        size: u64,
        executable: bool,
    },
    Symlink {
        path: String,
        target: String,
        #[serde(default)]
        target_is_dir: bool,
    },
}

impl UntrackedSidecar {
    fn path(&self) -> &str {
        match self {
            Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointManifestEncoding {
    Current,
    PreJsonEightFields,
    PreJsonNineFields,
}

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
    /// taken. Arbitrary-command checkpoints also preserve bounded regular-file
    /// content for these paths in the checkpoint sidecar.
    pub untracked_snapshot: Vec<String>,
    pub manifest_encoding: CheckpointManifestEncoding,
    pub legacy_sidecar_paths: Option<Vec<String>>,
    pub untracked_sidecars: Vec<UntrackedSidecar>,
    pub untracked_capture_warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UntrackedSourceVersion {
    modified_ns: u128,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_secs: i64,
    #[cfg(unix)]
    changed_ns: i64,
}

#[derive(Clone, Debug)]
enum UntrackedCandidate {
    File {
        path: String,
        size: u64,
        source_version: UntrackedSourceVersion,
        executable: bool,
    },
    Symlink {
        path: String,
        target: String,
        target_is_dir: bool,
    },
}

#[derive(Debug)]
struct UntrackedCapturePlan {
    snapshot: Vec<String>,
    candidates: Vec<UntrackedCandidate>,
    warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UntrackedBlobFingerprint {
    size: u64,
    source_version: UntrackedSourceVersion,
    blob_version: UntrackedSourceVersion,
    executable: bool,
    digest: String,
}

#[derive(Default)]
pub(crate) struct UntrackedBlobCache {
    entries: BTreeMap<String, UntrackedBlobFingerprint>,
}

const UNTRACKED_SNAPSHOT_CAP: usize = 500;
const BASH_UNTRACKED_SIDECAR_FILE_CAP: u64 = 8 * 1024 * 1024;
const BASH_UNTRACKED_SIDECAR_TOTAL_CAP: u64 = 32 * 1024 * 1024;
const SYMLINK_TARGET_CAP: usize = 16 * 1024;
const BLOBS_DIR: &str = "blobs";

fn random_checkpoint_nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("generate checkpoint nonce: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn git_command(cwd: &Path, args: &[&str]) -> Result<crate::InternalCommandOutput, String> {
    crate::run_internal_git_command(cwd, args)
}

struct UntrackedFiles {
    paths: Vec<String>,
    truncated: bool,
    unsupported_paths: usize,
}

/// List untracked, not-ignored repo paths via porcelain status.
fn untracked_files(git_root: &Path) -> Result<UntrackedFiles, String> {
    let output = git_command(
        git_root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;
    if !output.success() {
        return Err(format!(
            "git status --porcelain -z --untracked-files=all: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut paths = Vec::new();
    let mut inventoried = 0usize;
    let mut truncated = false;
    let mut unsupported_paths = 0usize;
    for record in output.stdout.split(|byte| *byte == 0) {
        let Some(raw_path) = record.strip_prefix(b"?? ") else {
            continue;
        };
        let path = match git_path_from_bytes(raw_path) {
            Ok(path) if path.starts_with(CHECKPOINTS_DIR) => continue,
            Ok(path) => path,
            Err(_) => {
                inventoried = inventoried.saturating_add(1);
                if inventoried > UNTRACKED_SNAPSHOT_CAP {
                    truncated = true;
                    break;
                }
                unsupported_paths = unsupported_paths.saturating_add(1);
                continue;
            }
        };
        inventoried = inventoried.saturating_add(1);
        if inventoried > UNTRACKED_SNAPSHOT_CAP {
            truncated = true;
            break;
        }
        match path.into_os_string().into_string() {
            Ok(path) => paths.push(path),
            Err(_) => unsupported_paths = unsupported_paths.saturating_add(1),
        }
    }
    Ok(UntrackedFiles {
        paths,
        truncated,
        unsupported_paths,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreMode {
    Preview,
    Worktree,
    ResetHead,
}

fn git_marker_in_ancestry(root: &Path) -> Result<bool, String> {
    let mut current = root;
    loop {
        match std::fs::symlink_metadata(current.join(".git")) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect Git marker under {}: {error}",
                    current.display()
                ));
            }
        }
        let Some(parent) = current.parent() else {
            return Ok(false);
        };
        if parent == current {
            return Ok(false);
        }
        current = parent;
    }
}

pub(crate) fn repo_root(root: &Path) -> Result<Option<PathBuf>, String> {
    if !git_marker_in_ancestry(root)? {
        let routed = ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"]
            .into_iter()
            .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
            .collect::<Vec<_>>();
        if !routed.is_empty() {
            return Err(format!(
                "no .git marker found, but ambient {} repository routing is set; Dext-owned Git commands intentionally ignore routing variables, so recovery checkpoints cannot safely identify this repository",
                routed.join(", ")
            ));
        }
        return Ok(None);
    }
    let output = git_command(root, &["rev-parse", "--show-toplevel"])?;
    if output.success() {
        let trimmed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!trimmed.is_empty()).then(|| PathBuf::from(trimmed)));
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

fn private_dir_owned_by_current_user(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return false;
        }
    }
    true
}

fn safe_checkpoint_storage_parent_metadata(metadata: &std::fs::Metadata) -> bool {
    if !private_dir_owned_by_current_user(metadata) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o022 != 0 {
            return false;
        }
    }
    true
}

fn safe_private_dir_metadata(metadata: &std::fs::Metadata) -> bool {
    if !private_dir_owned_by_current_user(metadata) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return false;
        }
    }
    true
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !private_dir_owned_by_current_user(&metadata) {
                return Err(format!(
                    "private directory path is not a real directory owned by the current user: {}",
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
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("private directory metadata {}: {error}", path.display()))?;
    if !safe_private_dir_metadata(&metadata) {
        return Err(format!(
            "private directory path is not owner-private: {}",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_checkpoint_storage(git_root: &Path) -> Result<(), String> {
    let dext_dir = git_root.join(".dext");
    match std::fs::symlink_metadata(&dext_dir) {
        Ok(metadata) if !safe_checkpoint_storage_parent_metadata(&metadata) => {
            return Err(format!(
                "checkpoint storage parent is not a safe current-user-owned directory: {}",
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
    let paths = [
        (git_root.join(".dext"), false),
        (checkpoints_manifest_dir(git_root), true),
    ];
    for (path, must_be_private) in paths {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata)
                if if must_be_private {
                    !safe_private_dir_metadata(&metadata)
                } else {
                    !safe_checkpoint_storage_parent_metadata(&metadata)
                } =>
            {
                return Err(format!(
                    "checkpoint storage path is not owner-safe: {}",
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

/// Save untracked file content as a private checkpoint sidecar.
fn save_untracked_sidecar(
    git_root: &Path,
    id: &str,
    abs_path: &Path,
    git_root_relative: &Path,
    max_bytes: Option<u64>,
) -> Result<u64, String> {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if source_metadata.permissions().mode() & 0o100 != 0 {
            0o700
        } else {
            0o600
        };
        destination
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("sidecar permissions: {error}"))?;
    }
    let copy_result = match max_bytes {
        Some(limit) => std::io::copy(
            &mut (&mut source).take(limit.saturating_add(1)),
            &mut destination,
        ),
        None => std::io::copy(&mut source, &mut destination),
    };
    let copied = match copy_result {
        Ok(copied) if max_bytes.is_some_and(|limit| copied > limit) => {
            drop(destination);
            let _ = std::fs::remove_file(&dest);
            return Err(
                "checkpoint sidecar source exceeded its byte limit while copying".to_string(),
            );
        }
        Ok(copied) => copied,
        Err(error) => {
            drop(destination);
            let _ = std::fs::remove_file(&dest);
            return Err(format!("sidecar copy: {error}"));
        }
    };
    Ok(copied)
}

fn untracked_source_version(metadata: &std::fs::Metadata) -> UntrackedSourceVersion {
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        UntrackedSourceVersion {
            modified_ns,
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_secs: metadata.ctime(),
            changed_ns: metadata.ctime_nsec(),
        }
    }
    #[cfg(not(unix))]
    {
        UntrackedSourceVersion { modified_ns }
    }
}

fn untracked_capture_warning(details: &[String]) -> Option<String> {
    (!details.is_empty()).then(|| {
        crate::cap_bytes_with_hint(
            format!("untracked recovery is partial: {}", details.join("; ")),
            4_096,
            "additional recovery-gap details omitted.",
        )
    })
}

fn plan_untracked_capture(git_root: &Path) -> Result<UntrackedCapturePlan, String> {
    let files = untracked_files(git_root)?;
    let mut warnings = Vec::new();
    if files.truncated {
        warnings.push(format!(
            "more than {UNTRACKED_SNAPSHOT_CAP} untracked paths exist; only the first {UNTRACKED_SNAPSHOT_CAP} can be inventoried"
        ));
    }
    if files.unsupported_paths > 0 {
        warnings.push(format!(
            "{} untracked path(s) are not valid UTF-8 and cannot be inventoried or recovered",
            files.unsupported_paths
        ));
    }
    let mut total = 0u64;
    let mut candidates = Vec::new();
    for path in &files.paths {
        let relative = Path::new(path);
        if !safe_repo_relative_path(relative) {
            return Err(format!("unsafe untracked checkpoint path: {path}"));
        }
        let absolute = git_root.join(relative);
        let metadata = std::fs::symlink_metadata(&absolute)
            .map_err(|error| format!("inspect untracked checkpoint path {path}: {error}"))?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute)
                .map_err(|error| format!("read untracked symlink {path}: {error}"))?;
            let target = match target.into_os_string().into_string() {
                Ok(target) if target.len() <= SYMLINK_TARGET_CAP => target,
                Ok(_) => {
                    if warnings.len() < 8 {
                        warnings.push(format!(
                            "symlink target exceeds {SYMLINK_TARGET_CAP} bytes: {path}"
                        ));
                    }
                    continue;
                }
                Err(_) => {
                    if warnings.len() < 8 {
                        warnings.push(format!("symlink target is not valid UTF-8: {path}"));
                    }
                    continue;
                }
            };
            #[cfg(windows)]
            let target_is_dir = {
                use std::os::windows::fs::FileTypeExt as _;
                metadata.file_type().is_symlink_dir()
            };
            #[cfg(not(windows))]
            let target_is_dir = false;
            candidates.push(UntrackedCandidate::Symlink {
                path: path.clone(),
                target,
                target_is_dir,
            });
            continue;
        }
        if !metadata.is_file() {
            if warnings.len() < 8 {
                warnings.push(format!("unsupported non-regular untracked path: {path}"));
            }
            continue;
        }
        if metadata.len() > BASH_UNTRACKED_SIDECAR_FILE_CAP {
            if warnings.len() < 8 {
                warnings.push(format!(
                    "file exceeds the {} MiB per-file limit: {path}",
                    BASH_UNTRACKED_SIDECAR_FILE_CAP / 1024 / 1024
                ));
            }
            continue;
        }
        if total.saturating_add(metadata.len()) > BASH_UNTRACKED_SIDECAR_TOTAL_CAP {
            if warnings.len() < 8 {
                warnings.push(format!(
                    "captured regular-file content would exceed the {} MiB total limit at {path}",
                    BASH_UNTRACKED_SIDECAR_TOTAL_CAP / 1024 / 1024
                ));
            }
            continue;
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt as _;
            metadata.permissions().mode() & 0o100 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        total = total.saturating_add(metadata.len());
        candidates.push(UntrackedCandidate::File {
            path: path.clone(),
            size: metadata.len(),
            source_version: untracked_source_version(&metadata),
            executable,
        });
    }
    Ok(UntrackedCapturePlan {
        snapshot: files.paths,
        candidates,
        warning: untracked_capture_warning(&warnings),
    })
}

fn checkpoint_blob_dir(git_root: &Path) -> PathBuf {
    checkpoints_manifest_dir(git_root).join(BLOBS_DIR)
}

fn blob_path(git_root: &Path, digest: &str) -> PathBuf {
    checkpoint_blob_dir(git_root).join(digest)
}

fn validate_private_blob_dir(git_root: &Path) -> Result<PathBuf, String> {
    let dir = checkpoint_blob_dir(git_root);
    let metadata = std::fs::symlink_metadata(&dir)
        .map_err(|error| format!("checkpoint blob directory metadata: {error}"))?;
    if !safe_private_dir_metadata(&metadata) {
        return Err(format!(
            "checkpoint blob directory is not owner-private: {}",
            dir.display()
        ));
    }
    Ok(dir)
}

fn private_blob_metadata_version(
    git_root: &Path,
    digest: &str,
    expected_size: u64,
) -> Option<UntrackedSourceVersion> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let dir = validate_private_blob_dir(git_root).ok()?;
    let metadata = std::fs::symlink_metadata(dir.join(digest)).ok()?;
    (safe_private_file_metadata(&metadata) && metadata.len() == expected_size)
        .then(|| untracked_source_version(&metadata))
}

fn remove_new_checkpoint_blobs(git_root: &Path, digests: &[String]) {
    for digest in digests {
        let _ = std::fs::remove_file(blob_path(git_root, digest));
    }
}

fn save_untracked_blob(
    git_root: &Path,
    relative: &str,
    absolute: &Path,
    expected_size: u64,
    source_version: &UntrackedSourceVersion,
    executable: bool,
    cache: &mut UntrackedBlobCache,
) -> Result<(String, bool), String> {
    use std::io::{Seek as _, SeekFrom};

    let blobs = checkpoint_blob_dir(git_root);
    ensure_private_dir(&blobs)?;

    #[cfg(unix)]
    if let Some(cached) = cache.entries.get(relative)
        && cached.size == expected_size
        && cached.source_version == *source_version
        && cached.executable == executable
        && private_blob_metadata_version(git_root, &cached.digest, expected_size)
            .is_some_and(|version| version == cached.blob_version)
    {
        return Ok((cached.digest.clone(), false));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut source = options
        .open(absolute)
        .map_err(|error| format!("open untracked checkpoint file: {error}"))?;
    let metadata = source
        .metadata()
        .map_err(|error| format!("untracked checkpoint file metadata: {error}"))?;
    if !metadata.is_file()
        || metadata.len() != expected_size
        || untracked_source_version(&metadata) != *source_version
    {
        return Err(format!(
            "untracked checkpoint file changed while capturing: {}",
            absolute.display()
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut hashed = 0u64;
    while hashed <= BASH_UNTRACKED_SIDECAR_FILE_CAP {
        let read = source
            .read(&mut buffer)
            .map_err(|error| format!("hash untracked checkpoint file: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed = hashed.saturating_add(read as u64);
    }
    if hashed != expected_size || hashed > BASH_UNTRACKED_SIDECAR_FILE_CAP {
        return Err(format!(
            "untracked checkpoint file changed or exceeded its byte limit: {}",
            absolute.display()
        ));
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let destination = blob_path(git_root, &digest);
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata)
            if safe_private_file_metadata(&metadata) && metadata.len() == expected_size =>
        {
            validate_private_blob(git_root, &digest, expected_size)?;
            let blob_version = private_blob_metadata_version(git_root, &digest, expected_size)
                .ok_or_else(|| format!("checkpoint blob is unsafe or corrupt: {digest}"))?;
            cache.entries.insert(
                relative.to_string(),
                UntrackedBlobFingerprint {
                    size: expected_size,
                    source_version: source_version.clone(),
                    blob_version,
                    executable,
                    digest: digest.clone(),
                },
            );
            return Ok((digest, false));
        }
        Ok(_) => {
            return Err(format!(
                "checkpoint blob is unsafe or corrupt: {}",
                destination.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("checkpoint blob metadata: {error}")),
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("rewind untracked checkpoint file: {error}"))?;
    let mut output = create_private_file(&destination)?;
    let copied = match std::io::copy(&mut source, &mut output) {
        Ok(copied) => copied,
        Err(error) => {
            drop(output);
            let _ = std::fs::remove_file(&destination);
            return Err(format!("write checkpoint blob: {error}"));
        }
    };
    if copied != expected_size {
        drop(output);
        let _ = std::fs::remove_file(&destination);
        return Err(format!(
            "untracked checkpoint file changed while copying: {}",
            absolute.display()
        ));
    }
    if let Err(error) = output.sync_all() {
        drop(output);
        let _ = std::fs::remove_file(&destination);
        return Err(format!("sync checkpoint blob: {error}"));
    }
    drop(output);
    if let Err(error) = validate_private_blob(git_root, &digest, expected_size) {
        let _ = std::fs::remove_file(&destination);
        return Err(error);
    }
    let blob_version = private_blob_metadata_version(git_root, &digest, expected_size)
        .ok_or_else(|| format!("checkpoint blob is unsafe or corrupt: {digest}"))?;
    cache.entries.insert(
        relative.to_string(),
        UntrackedBlobFingerprint {
            size: expected_size,
            source_version: source_version.clone(),
            blob_version,
            executable,
            digest: digest.clone(),
        },
    );
    Ok((digest, true))
}

fn save_bash_untracked_sidecars(
    git_root: &Path,
    plan: &UntrackedCapturePlan,
    cache: &mut UntrackedBlobCache,
) -> Result<(Vec<UntrackedSidecar>, Vec<String>), String> {
    let mut sidecars = Vec::with_capacity(plan.candidates.len());
    let mut new_blobs = Vec::new();
    for candidate in &plan.candidates {
        match candidate {
            UntrackedCandidate::File {
                path,
                size,
                source_version,
                executable,
            } => {
                let (digest, created) = match save_untracked_blob(
                    git_root,
                    path,
                    &git_root.join(path),
                    *size,
                    source_version,
                    *executable,
                    cache,
                ) {
                    Ok(saved) => saved,
                    Err(error) => {
                        remove_new_checkpoint_blobs(git_root, &new_blobs);
                        return Err(error);
                    }
                };
                if created {
                    new_blobs.push(digest.clone());
                }
                sidecars.push(UntrackedSidecar::File {
                    path: path.clone(),
                    digest,
                    size: *size,
                    executable: *executable,
                });
            }
            UntrackedCandidate::Symlink {
                path,
                target,
                target_is_dir,
            } => {
                sidecars.push(UntrackedSidecar::Symlink {
                    path: path.clone(),
                    target: target.clone(),
                    target_is_dir: *target_is_dir,
                });
            }
        }
    }
    Ok((sidecars, new_blobs))
}

fn literal_git_pathspec(relative: &str) -> String {
    format!(":(literal){relative}")
}

/// Check if a path is tracked by Git.
fn is_tracked(git_root: &Path, rel: &Path) -> Result<bool, String> {
    let relative = rel
        .to_str()
        .ok_or_else(|| format!("Git path is not valid UTF-8: {}", rel.display()))?;
    let pathspec = literal_git_pathspec(relative);
    let output = git_command(git_root, &["ls-files", "-z", "--", &pathspec])?;
    if !output.success() {
        return Err(format!(
            "git ls-files -z -- {relative}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!output.stdout.is_empty())
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
        || path.to_string_lossy().contains('\0')
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
    let user_path = Path::new(user_path);
    let candidate = if user_path.is_absolute() {
        user_path.to_path_buf()
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

fn legacy_hint_candidate(path: &Path, hint: &Path) -> Option<PathBuf> {
    let path_components = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let hint_components = hint
        .components()
        .map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!hint_components.is_empty() && path_components.ends_with(&hint_components))
        .then(|| path_components.iter().collect())
}

fn legacy_checkpoint_sidecar_paths(
    git_root: &Path,
    cp: &Checkpoint,
) -> Result<std::collections::BTreeSet<PathBuf>, String> {
    let mut paths = std::collections::BTreeSet::new();
    if !cp.includes_untracked_sidecar {
        return Ok(paths);
    }

    let sdir = sidecar_dir(git_root, &cp.id);
    match std::fs::symlink_metadata(&sdir) {
        Ok(metadata) if safe_private_dir_metadata(&metadata) => {
            for entry in walk_dir(&sdir)? {
                let relative = entry
                    .strip_prefix(&sdir)
                    .map_err(|_| "checkpoint sidecar escapes its storage directory".to_string())?;
                paths.insert(relative.to_path_buf());
            }
        }
        Ok(_) => return Err("checkpoint sidecar directory is not owner-private".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("checkpoint sidecar metadata: {error}")),
    }
    Ok(paths)
}

fn checkpoint_hint_repo_relative_path(
    root: &Path,
    git_root: &Path,
    cp: &Checkpoint,
    path: &str,
    legacy_sidecar_paths: Option<&std::collections::BTreeSet<PathBuf>>,
) -> Result<PathBuf, String> {
    let unsafe_path = || format!("unsafe checkpoint path: {path}");
    if cp.manifest_encoding == CheckpointManifestEncoding::Current {
        return manifest_repo_relative_path(root, git_root, path).ok_or_else(unsafe_path);
    }
    if Path::new(path).is_absolute() {
        return resolve_user_repo_path(root, git_root, path).ok_or_else(unsafe_path);
    }

    let hint = PathBuf::from(path);
    let legacy_sidecar_paths = legacy_sidecar_paths
        .ok_or_else(|| "legacy checkpoint sidecar path index is unavailable".to_string())?;
    if let Some(active_relative) = resolve_user_repo_path(root, git_root, path)
        && legacy_sidecar_paths.contains(&active_relative)
    {
        return Ok(active_relative);
    }
    let mut candidates = legacy_sidecar_paths
        .iter()
        .filter_map(|source| legacy_hint_candidate(source, &hint))
        .collect::<std::collections::BTreeSet<_>>();
    if candidates.len() == 1 {
        return candidates
            .pop_first()
            .ok_or_else(|| "legacy checkpoint candidate disappeared".to_string());
    }
    if candidates.is_empty() {
        Err(format!(
            "pre-JSON checkpoint relative path has no sidecar-backed exact target: {path}"
        ))
    } else {
        Err(format!(
            "ambiguous pre-JSON checkpoint path matches multiple prior targets: {path}"
        ))
    }
}

fn checkpoint_restore_paths(
    root: &Path,
    git_root: &Path,
    cp: &Checkpoint,
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let needs_legacy_sidecar_index = cp.manifest_encoding != CheckpointManifestEncoding::Current
        && cp
            .paths_hint
            .iter()
            .any(|path| !Path::new(path).is_absolute());
    let legacy_sidecar_paths = needs_legacy_sidecar_index
        .then(|| legacy_checkpoint_sidecar_paths(git_root, cp))
        .transpose()?;
    cp.paths_hint
        .iter()
        .map(|path| {
            let relative = checkpoint_hint_repo_relative_path(
                root,
                git_root,
                cp,
                path,
                legacy_sidecar_paths.as_ref(),
            )?;
            let target = safe_worktree_target(git_root, &relative)?;
            Ok((relative, target))
        })
        .collect()
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

fn tree_has_path(git_root: &Path, oid: &str, rel: &str) -> Result<bool, String> {
    let pathspec = literal_git_pathspec(rel);
    let output = git_command(git_root, &["ls-tree", "-z", oid, "--", &pathspec])?;
    if !output.success() {
        return Err(format!(
            "git ls-tree -z {oid} -- {rel}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!output.stdout.is_empty())
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

fn path_has_prior_state(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "inspect checkpoint target {}: {error}",
            path.display()
        )),
    }
}

/// Create a recovery checkpoint before a workspace mutation.
/// Returns None if not in a Git repo or if there is no prior state to preserve.
/// Unexpected failures are returned so callers can apply tool-specific policy.
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
    let mut blob_cache = UntrackedBlobCache::default();
    create_checkpoint_in_repo(
        root,
        &git_root,
        tool,
        paths_hint,
        ordinal,
        false,
        &mut blob_cache,
    )
}

pub(crate) fn create_checkpoint_in_repo(
    root: &Path,
    git_root: &Path,
    tool: &str,
    paths_hint: &[String],
    ordinal: usize,
    allow_partial_untracked: bool,
    blob_cache: &mut UntrackedBlobCache,
) -> Result<Option<Checkpoint>, String> {
    if !valid_checkpoint_tool_name(tool) {
        return Err("invalid checkpoint tool name".to_string());
    }
    if paths_hint.len() > UNTRACKED_SNAPSHOT_CAP {
        return Err(format!(
            "checkpoint path hints exceed the {UNTRACKED_SNAPSHOT_CAP}-path limit"
        ));
    }
    let _operation_lock = CheckpointOperationLock::acquire(git_root)?;
    let file_tools = ["write_file", "edit_file", "multi_edit"];
    let arbitrary_command = matches!(tool, "bash" | "awk" | "csvkit");
    let Some(head) = head_oid(git_root)? else {
        ensure_checkpoint_git_exclude(git_root)?;
        if arbitrary_command && is_dirty(git_root)? {
            return Err(
                "repository has no initial commit and contains worktree or index state; commit or remove it before running an arbitrary write-risk command"
                    .to_string(),
            );
        }
        if file_tools.contains(&tool) {
            for path in paths_hint {
                let Some(relative) = resolve_user_repo_path(root, git_root, path) else {
                    continue;
                };
                if path_has_prior_state(&git_root.join(relative))? {
                    return Err(
                        "repository has no initial commit; commit the existing target before mutating it"
                            .to_string(),
                    );
                }
            }
        }
        return Ok(None);
    };

    let untracked_plan = if arbitrary_command {
        let plan = plan_untracked_capture(git_root)?;
        if let Some(warning) = &plan.warning
            && !allow_partial_untracked
        {
            return Err(warning.clone());
        }
        Some(plan)
    } else {
        None
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

    let mut includes_untracked_sidecar = false;
    let mut legacy_sidecar_paths = Vec::new();
    if file_tools.contains(&tool) {
        for (rel, rel_string) in normalized_paths.iter().zip(&normalized_path_strings) {
            let abs = git_root.join(rel);
            let tracked = match is_tracked(git_root, rel) {
                Ok(tracked) => tracked,
                Err(error) => {
                    let _ = run_git(git_root, &["update-ref", "-d", &ref_name]);
                    let _ = std::fs::remove_dir_all(sidecar_dir(git_root, &id));
                    return Err(error);
                }
            };
            if abs.is_file() && !tracked {
                if let Err(error) = save_untracked_sidecar(git_root, &id, &abs, rel, None) {
                    let _ = run_git(git_root, &["update-ref", "-d", &ref_name]);
                    let _ = std::fs::remove_dir_all(sidecar_dir(git_root, &id));
                    return Err(format!(
                        "preserve untracked checkpoint target {}: {error}",
                        rel.display()
                    ));
                }
                includes_untracked_sidecar = true;
                legacy_sidecar_paths.push(rel_string.clone());
            }
        }
    }

    let mut untracked_sidecars = Vec::new();
    let mut new_untracked_blobs = Vec::new();
    let mut untracked_capture_warning = None;
    let untracked_snapshot = if let Some(plan) = untracked_plan {
        match save_bash_untracked_sidecars(git_root, &plan, blob_cache) {
            Ok((saved, new_blobs)) => {
                includes_untracked_sidecar = !saved.is_empty();
                untracked_sidecars = saved;
                new_untracked_blobs = new_blobs;
                untracked_capture_warning = plan.warning;
            }
            Err(error) => {
                let _ = run_git(git_root, &["update-ref", "-d", &ref_name]);
                let _ = std::fs::remove_dir_all(sidecar_dir(git_root, &id));
                return Err(format!("preserve untracked checkpoint files: {error}"));
            }
        }
        plan.snapshot
    } else {
        Vec::new()
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
        manifest_encoding: CheckpointManifestEncoding::Current,
        legacy_sidecar_paths: Some(legacy_sidecar_paths),
        untracked_sidecars,
        untracked_capture_warning,
    };

    if let Err(error) = append_manifest(git_root, &cp) {
        let _ = run_git(git_root, &["update-ref", "-d", &cp.ref_name]);
        let _ = std::fs::remove_dir_all(sidecar_dir(git_root, &cp.id));
        remove_new_checkpoint_blobs(git_root, &new_untracked_blobs);
        return Err(error);
    }
    match prune_checkpoint_refs(git_root, DEFAULT_PRUNE_KEEP, DEFAULT_PRUNE_MAX_AGE_HOURS) {
        Ok(outcome) => {
            for warning in outcome.warnings {
                eprintln!("[checkpoint] retention warning: {warning}");
            }
        }
        Err(error) => eprintln!("[checkpoint] retention warning: {error}"),
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
        Ok(_) => read_private_file_with_limit(&manifest_path, Some(CHECKPOINT_MANIFEST_MAX_BYTES))
            .map_err(|error| format!("manifest read: {error}"))?,
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
        let existing =
            parse_manifest_line(line.strip_suffix('\r').unwrap_or(line)).ok_or_else(|| {
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
    if content.len() as u64 > CHECKPOINT_MANIFEST_MAX_BYTES {
        return Err(format!(
            "checkpoint manifest exceeds the {CHECKPOINT_MANIFEST_MAX_BYTES}-byte runtime bound"
        ));
    }
    write_private_file(&manifest_path, content.as_bytes())
        .map_err(|error| format!("manifest write: {error}"))
}

fn format_manifest_line(cp: &Checkpoint) -> String {
    if cp.manifest_encoding == CheckpointManifestEncoding::PreJsonEightFields {
        return format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            cp.id,
            cp.ref_name,
            cp.oid,
            cp.tool_name,
            cp.created_at_ms,
            cp.head,
            cp.includes_untracked_sidecar,
            cp.paths_hint.join(","),
        );
    }
    if cp.manifest_encoding == CheckpointManifestEncoding::PreJsonNineFields {
        return format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            cp.id,
            cp.ref_name,
            cp.oid,
            cp.tool_name,
            cp.created_at_ms,
            cp.head,
            cp.includes_untracked_sidecar,
            cp.paths_hint.join(","),
            cp.untracked_snapshot.join("\u{1f}"),
        );
    }
    let paths = serde_json::to_string(&cp.paths_hint).unwrap_or_else(|_| "[]".to_string());
    let untracked =
        serde_json::to_string(&cp.untracked_snapshot).unwrap_or_else(|_| "[]".to_string());
    let sidecars =
        serde_json::to_string(&cp.untracked_sidecars).unwrap_or_else(|_| "[]".to_string());
    let warning =
        serde_json::to_string(&cp.untracked_capture_warning).unwrap_or_else(|_| "null".to_string());
    let legacy_sidecars = cp
        .legacy_sidecar_paths
        .as_ref()
        .map(|paths| serde_json::to_string(paths).unwrap_or_else(|_| "[]".to_string()));
    let mut line = format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        cp.id,
        cp.ref_name,
        cp.oid,
        cp.tool_name,
        cp.created_at_ms,
        cp.head,
        cp.includes_untracked_sidecar,
        paths,
        untracked,
        sidecars,
        warning,
    );
    if let Some(legacy_sidecars) = legacy_sidecars {
        line.push('\t');
        line.push_str(&legacy_sidecars);
    }
    line
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
    let content = if content.is_empty() {
        String::new()
    } else {
        format!("{content}\n")
    };
    if content.len() as u64 > CHECKPOINT_MANIFEST_MAX_BYTES {
        return Err(format!(
            "checkpoint manifest exceeds the {CHECKPOINT_MANIFEST_MAX_BYTES}-byte runtime bound"
        ));
    }
    write_private_file(&manifest_path, content.as_bytes())
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

fn safe_pre_json_manifest_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
        return false;
    }
    let mut has_name = false;
    for component in path.components() {
        if let Component::Normal(name) = component {
            if name.to_string_lossy().eq_ignore_ascii_case(".git") {
                return false;
            }
            has_name = true;
        }
    }
    has_name
}

fn safe_pre_json_untracked_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
        return false;
    }
    let mut has_name = false;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        if name.to_string_lossy().eq_ignore_ascii_case(".git") {
            return false;
        }
        has_name = true;
    }
    has_name
}

fn looks_like_json_array_field(value: &str) -> bool {
    value.trim_start().starts_with('[')
}

fn parse_manifest_line(line: &str) -> Option<Checkpoint> {
    let parts: Vec<&str> = line.splitn(13, '\t').collect();
    if !matches!(parts.len(), 8 | 9 | 11 | 12) {
        return None;
    }
    let (paths_hint, untracked_snapshot, manifest_encoding) = match parts.len() {
        8 => (
            (!parts[7].is_empty())
                .then(|| parts[7].to_string())
                .into_iter()
                .collect(),
            Vec::new(),
            CheckpointManifestEncoding::PreJsonEightFields,
        ),
        9 => {
            let paths_json = serde_json::from_str::<Vec<String>>(parts[7]);
            let untracked_json = serde_json::from_str::<Vec<String>>(parts[8]);
            match (paths_json, untracked_json) {
                (Ok(paths), Ok(untracked)) => {
                    (paths, untracked, CheckpointManifestEncoding::Current)
                }
                (paths, untracked)
                    if paths.is_ok()
                        || untracked.is_ok()
                        || looks_like_json_array_field(parts[7])
                        || looks_like_json_array_field(parts[8]) =>
                {
                    return None;
                }
                _ => (
                    (!parts[7].is_empty())
                        .then(|| parts[7].to_string())
                        .into_iter()
                        .collect(),
                    parts[8]
                        .split('\u{1f}')
                        .filter(|path| !path.is_empty())
                        .map(String::from)
                        .collect(),
                    CheckpointManifestEncoding::PreJsonNineFields,
                ),
            }
        }
        11 | 12 => (
            serde_json::from_str::<Vec<String>>(parts[7]).ok()?,
            serde_json::from_str::<Vec<String>>(parts[8]).ok()?,
            CheckpointManifestEncoding::Current,
        ),
        _ => return None,
    };
    let untracked_sidecars = if parts.len() >= 11 {
        serde_json::from_str::<Vec<UntrackedSidecar>>(parts[9]).ok()?
    } else {
        Vec::new()
    };
    let untracked_capture_warning = if parts.len() >= 11 {
        serde_json::from_str::<Option<String>>(parts[10]).ok()?
    } else {
        None
    };
    let legacy_sidecar_paths = if parts.len() == 12 {
        Some(serde_json::from_str::<Vec<String>>(parts[11]).ok()?)
    } else {
        None
    };
    let id = parts[0];
    let ref_name = parts[1];
    let oid = parts[2];
    let head = parts[5];
    let pre_json_manifest = manifest_encoding != CheckpointManifestEncoding::Current;
    let valid_hint = |path: &String| {
        safe_repo_relative_path(Path::new(path))
            || (pre_json_manifest && safe_pre_json_manifest_path(Path::new(path)))
    };
    let valid_snapshot = |path: &String| {
        safe_repo_relative_path(Path::new(path))
            || (pre_json_manifest && safe_pre_json_untracked_path(Path::new(path)))
    };
    if !safe_checkpoint_id(id)
        || !checkpoint_ref_valid(ref_name, id)
        || !valid_object_id(oid)
        || !valid_object_id(head)
        || !valid_checkpoint_tool_name(parts[3])
        || !matches!(parts[6], "true" | "false")
        || !paths_hint.iter().all(valid_hint)
        || !untracked_snapshot.iter().all(valid_snapshot)
        || !legacy_sidecar_paths.as_deref().is_none_or(|paths| {
            paths
                .iter()
                .all(|path| safe_repo_relative_path(Path::new(path)))
        })
        || !untracked_sidecars.iter().all(|sidecar| {
            safe_repo_relative_path(Path::new(sidecar.path()))
                && match sidecar {
                    UntrackedSidecar::File { digest, size, .. } => {
                        digest.len() == 64
                            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                            && *size <= BASH_UNTRACKED_SIDECAR_FILE_CAP
                    }
                    UntrackedSidecar::Symlink { target, .. } => target.len() <= SYMLINK_TARGET_CAP,
                }
        })
        || untracked_capture_warning
            .as_ref()
            .is_some_and(|warning| warning.len() > 4_096)
        || (parts[6] == "true" && paths_hint.is_empty() && untracked_snapshot.is_empty())
    {
        return None;
    }
    let checkpoint = Checkpoint {
        id: id.to_string(),
        ref_name: ref_name.to_string(),
        oid: oid.to_string(),
        tool_name: parts[3].to_string(),
        created_at_ms: parts[4].parse().ok()?,
        head: head.to_string(),
        paths_hint,
        includes_untracked_sidecar: parts[6] == "true",
        untracked_snapshot,
        manifest_encoding,
        legacy_sidecar_paths,
        untracked_sidecars,
        untracked_capture_warning,
    };
    validate_checkpoint(&checkpoint).ok()?;
    Some(checkpoint)
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
        Ok(_) => read_private_file_with_limit(
            &manifest_path,
            Some(
                manifest_max_bytes
                    .unwrap_or(CHECKPOINT_MANIFEST_MAX_BYTES)
                    .min(CHECKPOINT_MANIFEST_MAX_BYTES),
            ),
        )
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
        let checkpoint =
            parse_manifest_line(line.strip_suffix('\r').unwrap_or(line)).ok_or_else(|| {
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
    if cp.paths_hint.len() > UNTRACKED_SNAPSHOT_CAP
        || cp.untracked_snapshot.len() > UNTRACKED_SNAPSHOT_CAP
        || cp
            .legacy_sidecar_paths
            .as_deref()
            .is_some_and(|paths| paths.len() > UNTRACKED_SNAPSHOT_CAP)
        || cp.untracked_sidecars.len() > UNTRACKED_SNAPSHOT_CAP
    {
        return Err("checkpoint metadata exceeds its path-count limit".to_string());
    }
    let unique_paths = |paths: &[String]| {
        paths.iter().collect::<std::collections::HashSet<_>>().len() == paths.len()
    };
    if !unique_paths(&cp.paths_hint)
        || !unique_paths(&cp.untracked_snapshot)
        || cp
            .legacy_sidecar_paths
            .as_deref()
            .is_some_and(|paths| !unique_paths(paths))
    {
        return Err("checkpoint metadata contains duplicate paths".to_string());
    }
    let pre_json_manifest = cp.manifest_encoding != CheckpointManifestEncoding::Current;
    let direct_file_tool = matches!(
        cp.tool_name.as_str(),
        "write_file" | "edit_file" | "multi_edit"
    );
    if (direct_file_tool && cp.paths_hint.is_empty())
        || (pre_json_manifest && cp.paths_hint.len() > 1)
        || (cp.manifest_encoding == CheckpointManifestEncoding::PreJsonEightFields
            && !cp.untracked_snapshot.is_empty())
        || (pre_json_manifest
            && (cp.legacy_sidecar_paths.is_some()
                || !cp.untracked_sidecars.is_empty()
                || cp.untracked_capture_warning.is_some()))
    {
        return Err("checkpoint metadata does not match its manifest encoding".to_string());
    }
    let valid_hint = |path: &&String| {
        safe_repo_relative_path(Path::new(path))
            || (pre_json_manifest && safe_pre_json_manifest_path(Path::new(path)))
    };
    let valid_snapshot = |path: &&String| {
        safe_repo_relative_path(Path::new(path))
            || (pre_json_manifest && safe_pre_json_untracked_path(Path::new(path)))
    };
    if let Some(path) = cp.paths_hint.iter().find(|path| !valid_hint(path)) {
        return Err(format!("unsafe checkpoint path: {path}"));
    }
    if let Some(path) = cp
        .untracked_snapshot
        .iter()
        .find(|path| !valid_snapshot(path))
    {
        return Err(format!("unsafe checkpoint untracked path: {path}"));
    }
    if let Some(path) = cp.legacy_sidecar_paths.as_deref().and_then(|paths| {
        paths.iter().find(|path| {
            !safe_repo_relative_path(Path::new(path))
                || !cp.paths_hint.iter().any(|hint| hint == *path)
        })
    }) {
        return Err(format!(
            "unsafe or undeclared legacy checkpoint sidecar path: {path}"
        ));
    }
    let mut sidecar_paths = std::collections::HashSet::new();
    let mut regular_file_bytes = 0u64;
    for sidecar in &cp.untracked_sidecars {
        let path = sidecar.path();
        if !sidecar_paths.insert(path) {
            return Err(format!("duplicate checkpoint sidecar path: {path}"));
        }
        if !safe_repo_relative_path(Path::new(path))
            || !cp
                .untracked_snapshot
                .iter()
                .any(|snapshot| snapshot == path)
        {
            return Err(format!(
                "unsafe or undeclared checkpoint sidecar path: {path}"
            ));
        }
        match sidecar {
            UntrackedSidecar::File { digest, size, .. }
                if digest.len() != 64
                    || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || *size > BASH_UNTRACKED_SIDECAR_FILE_CAP =>
            {
                return Err(format!("invalid checkpoint blob digest for {path}"));
            }
            UntrackedSidecar::File { size, .. } => {
                regular_file_bytes = regular_file_bytes.saturating_add(*size);
                if regular_file_bytes > BASH_UNTRACKED_SIDECAR_TOTAL_CAP {
                    return Err("checkpoint sidecars exceed their total byte limit".to_string());
                }
            }
            UntrackedSidecar::Symlink { target, .. }
                if target.len() > SYMLINK_TARGET_CAP || target.contains('\0') =>
            {
                return Err(format!("checkpoint symlink target is invalid: {path}"));
            }
            _ => {}
        }
    }
    if !cp.includes_untracked_sidecar
        && (cp
            .legacy_sidecar_paths
            .as_deref()
            .is_some_and(|paths| !paths.is_empty())
            || !cp.untracked_sidecars.is_empty())
    {
        return Err("checkpoint declares sidecars without enabling restore".to_string());
    }
    if cp
        .legacy_sidecar_paths
        .as_deref()
        .is_some_and(|paths| !paths.is_empty())
        && !cp.untracked_sidecars.is_empty()
    {
        return Err("checkpoint mixes legacy and content-addressed sidecars".to_string());
    }
    if cp.includes_untracked_sidecar && cp.paths_hint.is_empty() && cp.untracked_snapshot.is_empty()
    {
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

fn preview_restore_locked(root: &Path, git_root: &Path, cp: &Checkpoint) -> Result<String, String> {
    validate_checkpoint_ref(git_root, cp)?;
    let preview_restore_paths = checkpoint_restore_paths(root, git_root, cp)?;

    let mut preview_untracked = Vec::new();
    let mut skipped_untracked = 0usize;
    for path in &cp.untracked_snapshot {
        let Some(relative) = manifest_repo_relative_path(root, git_root, path) else {
            skipped_untracked += 1;
            continue;
        };
        let Ok(target) = safe_worktree_target(git_root, &relative) else {
            skipped_untracked += 1;
            continue;
        };
        let Some(relative_string) = relative.to_str().map(String::from) else {
            skipped_untracked += 1;
            continue;
        };
        preview_untracked.push((relative_string, target));
    }

    let mut out = String::new();
    out.push_str(&format!("Checkpoint: {}\n", cp.id));
    out.push_str(&format!("Tool: {}\n", cp.tool_name));
    out.push_str(&format!("Ref: {}\n", cp.ref_name));
    out.push_str(&format!("OID: {}\n", cp.oid));
    out.push_str(&format!("HEAD at time: {}\n", cp.head));
    if !preview_restore_paths.is_empty() {
        out.push_str(&format!(
            "Paths: {}\n",
            preview_restore_paths
                .iter()
                .map(|(relative, _)| relative.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if cp.includes_untracked_sidecar {
        out.push_str("Includes untracked file sidecar(s)\n");
    }
    if let Some(warning) = &cp.untracked_capture_warning {
        out.push_str(&format!("WARNING: {warning}\n"));
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
        if preflight_sidecar_restore(git_root, cp, &preview_restore_paths, false).is_ok() {
            out.push_str("\nUntracked sidecar content present; restore will recreate it.\n");
        } else {
            out.push_str("\nWARNING: expected untracked sidecar content is unavailable; apply will fail closed.\n");
        }
    }

    // Untracked-file delta since the checkpoint. Older manifests may only
    // identify removed paths; current arbitrary-command checkpoints preserve
    // bounded regular-file content in sidecars.
    let before: std::collections::HashSet<&str> = preview_untracked
        .iter()
        .map(|(relative, _)| relative.as_str())
        .collect();
    let now = untracked_files(git_root)?;
    let created: Vec<&str> = now
        .paths
        .iter()
        .map(String::as_str)
        .filter(|p| !before.contains(p))
        .collect();
    let captured: std::collections::HashSet<&str> = cp
        .untracked_sidecars
        .iter()
        .map(UntrackedSidecar::path)
        .chain(
            cp.legacy_sidecar_paths
                .as_deref()
                .into_iter()
                .flatten()
                .map(String::as_str),
        )
        .chain(
            preview_restore_paths
                .iter()
                .filter(|_| {
                    cp.legacy_sidecar_paths.is_none()
                        && cp.includes_untracked_sidecar
                        && cp.untracked_sidecars.is_empty()
                })
                .filter_map(|(relative, _)| relative.to_str()),
        )
        .collect();
    let removed: Vec<&str> = preview_untracked
        .iter()
        .filter(|(_, target)| {
            std::fs::symlink_metadata(target)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        })
        .map(|(relative, _)| relative.as_str())
        .collect();
    let (removed_captured, removed_uncaptured): (Vec<_>, Vec<_>) = removed
        .into_iter()
        .partition(|path| captured.contains(path));
    if skipped_untracked > 0 {
        out.push_str(&format!(
            "\nSkipped {skipped_untracked} checkpoint untracked path(s) outside safe repository targets.\n"
        ));
    }
    if now.truncated {
        out.push_str(&format!(
            "\nCurrent untracked-file scan capped at {UNTRACKED_SNAPSHOT_CAP} paths; listed deltas may be incomplete.\n"
        ));
    }
    if now.unsupported_paths > 0 {
        out.push_str(&format!(
            "\nCurrent untracked-file scan omitted {} non-UTF-8 path(s); listed deltas may be incomplete.\n",
            now.unsupported_paths
        ));
    }
    if !created.is_empty() {
        out.push_str(
            "\nUntracked files created since checkpoint (restore will NOT remove them):\n",
        );
        for p in created.iter().take(50) {
            out.push_str(&format!("  + {p}\n"));
        }
    }
    if !removed_captured.is_empty() {
        out.push_str(
            "\nUntracked files present at checkpoint but gone now (sidecar content will be restored):\n",
        );
        for path in removed_captured.iter().take(50) {
            out.push_str(&format!("  - {path}\n"));
        }
    }
    if !removed_uncaptured.is_empty() {
        out.push_str(
            "\nUntracked files present at checkpoint but gone now (content not recoverable):\n",
        );
        for path in removed_uncaptured.iter().take(50) {
            out.push_str(&format!("  - {path}\n"));
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
    preview_restore_locked(root, &git_root, cp)
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
    let restore_paths = checkpoint_restore_paths(root, git_root, cp)?;
    preflight_git_restore_destinations(git_root, &cp.oid, &restore_paths, full_restore)?;
    Ok(restore_paths)
}

#[derive(Clone, Debug)]
enum PreparedSidecarRestore {
    File {
        source: PathBuf,
        relative: PathBuf,
        executable: Option<bool>,
        digest: Option<String>,
        size: Option<u64>,
    },
    Symlink {
        relative: PathBuf,
        target: String,
        target_is_dir: bool,
    },
}

impl PreparedSidecarRestore {
    fn relative(&self) -> &Path {
        match self {
            Self::File { relative, .. } | Self::Symlink { relative, .. } => relative,
        }
    }
}

fn validate_private_blob(
    git_root: &Path,
    digest: &str,
    expected_size: u64,
) -> Result<PathBuf, String> {
    let dir = validate_private_blob_dir(git_root)?;
    let path = dir.join(digest);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("open checkpoint blob {digest}: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("checkpoint blob metadata: {error}"))?;
    if !safe_private_file_metadata(&metadata) || metadata.len() != expected_size {
        return Err(format!("checkpoint blob is unsafe or corrupt: {digest}"));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read_total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read checkpoint blob: {error}"))?;
        if read == 0 {
            break;
        }
        read_total = read_total.saturating_add(read as u64);
        if read_total > BASH_UNTRACKED_SIDECAR_FILE_CAP {
            return Err(format!("checkpoint blob exceeds its byte limit: {digest}"));
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if read_total != expected_size || actual != digest {
        return Err(format!("checkpoint blob digest mismatch: {digest}"));
    }
    Ok(path)
}

fn preflight_legacy_sidecar_restore(
    git_root: &Path,
    cp: &Checkpoint,
    restore_paths: &[(PathBuf, PathBuf)],
    validate_destinations: bool,
) -> Result<Vec<PreparedSidecarRestore>, String> {
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
    if !safe_private_dir_metadata(&metadata) {
        return Err("checkpoint sidecar directory is not owner-private".to_string());
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
    let selected = if restore_paths.is_empty() {
        cp.untracked_snapshot
            .iter()
            .map(Path::new)
            .collect::<std::collections::HashSet<_>>()
    } else {
        restore_paths
            .iter()
            .map(|(relative, _)| relative.as_path())
            .collect::<std::collections::HashSet<_>>()
    };
    let allowed = if let Some(declared) = cp.legacy_sidecar_paths.as_deref() {
        declared
            .iter()
            .map(String::as_str)
            .map(Path::new)
            .filter(|path| selected.contains(path))
            .collect::<std::collections::HashSet<_>>()
    } else {
        selected
    };
    let present = entries
        .iter()
        .map(|entry| {
            entry
                .strip_prefix(&sdir)
                .map_err(|_| "checkpoint sidecar escapes its storage directory".to_string())
        })
        .collect::<Result<std::collections::HashSet<_>, String>>()?;
    if cp.legacy_sidecar_paths.is_some() {
        if let Some(missing) = allowed.iter().find(|path| !present.contains(*path)) {
            return Err(format!(
                "required untracked checkpoint sidecar is missing: {}",
                missing.display()
            ));
        }
    } else {
        for missing in allowed.difference(&present) {
            let relative = missing.to_str().ok_or_else(|| {
                format!(
                    "legacy checkpoint path is not valid UTF-8: {}",
                    missing.display()
                )
            })?;
            if restore_paths.is_empty() || !tree_has_path(git_root, &cp.oid, relative)? {
                return Err(format!(
                    "required legacy checkpoint sidecar is missing or was not recorded: {}",
                    missing.display()
                ));
            }
        }
    }
    let mut prepared = Vec::with_capacity(entries.len());
    for entry in entries {
        let relative = entry
            .strip_prefix(&sdir)
            .map_err(|_| "checkpoint sidecar escapes its storage directory".to_string())?
            .to_path_buf();
        if !safe_repo_relative_path(&relative) || !allowed.contains(relative.as_path()) {
            return Err(format!(
                "checkpoint sidecar targets undeclared path: {}",
                relative.display()
            ));
        }
        if validate_destinations {
            let target = safe_worktree_target(git_root, &relative)?;
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
                Err(error) => {
                    return Err(format!("sidecar restore destination metadata: {error}"));
                }
            }
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&entry)
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
        prepared.push(PreparedSidecarRestore::File {
            source: entry,
            relative,
            executable: None,
            digest: None,
            size: None,
        });
    }
    Ok(prepared)
}

fn preflight_sidecar_destination(
    destination: &Path,
    restoring_symlink: bool,
) -> Result<(), String> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => Err(format!(
            "refusing to replace directory during sidecar restore: {}",
            destination.display()
        )),
        Ok(metadata) if metadata.file_type().is_symlink() && !restoring_symlink => Err(format!(
            "refusing to replace symlink with checkpoint file: {}",
            destination.display()
        )),
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(()),
        Ok(metadata) if metadata.is_file() && !safe_restore_destination_metadata(&metadata) => {
            Err(format!(
                "sidecar restore destination is multiply linked: {}",
                destination.display()
            ))
        }
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(format!(
            "sidecar restore destination is not a regular file or symlink: {}",
            destination.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("sidecar restore destination metadata: {error}")),
    }
}

fn preflight_sidecar_restore(
    git_root: &Path,
    cp: &Checkpoint,
    restore_paths: &[(PathBuf, PathBuf)],
    validate_destinations: bool,
) -> Result<Vec<PreparedSidecarRestore>, String> {
    if cp.untracked_sidecars.is_empty() {
        return preflight_legacy_sidecar_restore(
            git_root,
            cp,
            restore_paths,
            validate_destinations,
        );
    }
    if !cp.includes_untracked_sidecar {
        return Err("checkpoint declares sidecars without enabling restore".to_string());
    }
    let allowed = if restore_paths.is_empty() {
        cp.untracked_snapshot
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>()
    } else {
        restore_paths
            .iter()
            .filter_map(|(relative, _)| relative.to_str())
            .collect::<std::collections::HashSet<_>>()
    };
    let mut prepared = Vec::with_capacity(cp.untracked_sidecars.len());
    for sidecar in &cp.untracked_sidecars {
        if !allowed.contains(sidecar.path()) {
            return Err(format!(
                "checkpoint sidecar targets undeclared path: {}",
                sidecar.path()
            ));
        }
        let relative = PathBuf::from(sidecar.path());
        if validate_destinations {
            let destination = safe_worktree_target(git_root, &relative)?;
            let restoring_symlink = matches!(sidecar, UntrackedSidecar::Symlink { .. });
            preflight_sidecar_destination(&destination, restoring_symlink)?;
        }
        match sidecar {
            UntrackedSidecar::File {
                digest,
                size,
                executable,
                ..
            } => prepared.push(PreparedSidecarRestore::File {
                source: validate_private_blob(git_root, digest, *size)?,
                relative,
                executable: Some(*executable),
                digest: Some(digest.clone()),
                size: Some(*size),
            }),
            UntrackedSidecar::Symlink {
                target,
                target_is_dir,
                ..
            } => prepared.push(PreparedSidecarRestore::Symlink {
                relative,
                target: target.clone(),
                target_is_dir: *target_is_dir,
            }),
        }
    }
    Ok(prepared)
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
        return preview_restore_locked(root, &git_root, cp);
    }

    if mode == RestoreMode::ResetHead {
        preflight_git_restore_destinations(&git_root, &cp.head, &[], true)?;
        return reset_head(&git_root, cp);
    }

    let full_restore = cp.paths_hint.is_empty();
    let restore_paths = preflight_restore_paths(root, &git_root, cp, full_restore)?;
    let sidecar_entries = preflight_sidecar_restore(&git_root, cp, &restore_paths, true)?;
    let restore_path_tree_presence = restore_paths
        .iter()
        .map(|(relative, _)| {
            let relative = relative.to_str().ok_or_else(|| {
                format!(
                    "checkpoint restore path is not valid UTF-8: {}",
                    relative.display()
                )
            })?;
            tree_has_path(&git_root, &cp.oid, relative)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let sidecar_paths = sidecar_entries
        .iter()
        .map(PreparedSidecarRestore::relative)
        .collect::<std::collections::HashSet<_>>();

    // Worktree restore: checkout paths from checkpoint OID
    let mut restored: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if !restore_paths.is_empty() {
        for ((rel, target), tree_has_path) in restore_paths.iter().zip(restore_path_tree_presence) {
            let rel_str = rel.to_string_lossy().to_string();
            let result = if tree_has_path {
                let pathspec = literal_git_pathspec(&rel_str);
                run_git(
                    &git_root,
                    &[
                        "restore",
                        "--source",
                        &cp.oid,
                        "--worktree",
                        "--",
                        &pathspec,
                    ],
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

    // If no specific paths, restore all worktree paths.
    if cp.paths_hint.is_empty() {
        let args = vec![
            "restore",
            "--source",
            cp.oid.as_str(),
            "--worktree",
            "--",
            ".",
        ];
        run_git(&git_root, &args)?;
        restored.push("(all worktree files)".to_string());
    }

    // Restore sidecar untracked files that were fully validated before any
    // worktree mutation above.
    for sidecar in sidecar_entries {
        let relative = sidecar.relative().to_path_buf();
        let result = match sidecar {
            PreparedSidecarRestore::File {
                source,
                executable,
                digest,
                size,
                ..
            } => copy_sidecar_file(
                &source,
                &git_root,
                &relative,
                executable,
                digest.as_deref().zip(size),
            ),
            PreparedSidecarRestore::Symlink {
                target,
                target_is_dir,
                ..
            } => restore_sidecar_symlink(&git_root, &relative, &target, target_is_dir),
        };
        match result {
            Ok(()) => restored.push(relative.display().to_string()),
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

fn create_platform_symlink(
    link_target: &str,
    destination: &Path,
    target_is_dir: bool,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let _ = target_is_dir;
        std::os::unix::fs::symlink(link_target, destination)
    }
    #[cfg(windows)]
    {
        if target_is_dir {
            std::os::windows::fs::symlink_dir(link_target, destination)
        } else {
            std::os::windows::fs::symlink_file(link_target, destination)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (link_target, target_is_dir, destination);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlink restore is unsupported on this platform",
        ))
    }
}

fn restore_sidecar_symlink(
    git_root: &Path,
    relative: &Path,
    link_target: &str,
    target_is_dir: bool,
) -> Result<(), String> {
    let destination = ensure_worktree_parent_tree(git_root, relative)?;
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "checkpoint symlink destination has no parent: {}",
            destination.display()
        )
    })?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("restore");
    let mut temp_path = None;
    for _ in 0..16 {
        let nonce = random_checkpoint_nonce()?;
        let candidate = parent.join(format!(".{file_name}.dext-restore-{nonce}"));
        match create_platform_symlink(link_target, &candidate, target_is_dir) {
            Ok(()) => {
                temp_path = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("restore checkpoint symlink: {error}")),
        }
    }
    let temp_path = temp_path.ok_or_else(|| {
        format!(
            "could not allocate checkpoint symlink restore temp in {}",
            parent.display()
        )
    })?;
    if let Err(error) = preflight_sidecar_destination(&destination, true) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    let result = crate::session::replace_file_atomically(&temp_path, &destination)
        .map_err(|error| format!("atomically replace checkpoint symlink: {error}"));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn copy_sidecar_file(
    source: &Path,
    git_root: &Path,
    relative: &Path,
    executable: Option<bool>,
    expected_integrity: Option<(&str, u64)>,
) -> Result<(), String> {
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
    let input_metadata = input
        .metadata()
        .map_err(|error| format!("sidecar metadata {}: {error}", source.display()))?;
    if !safe_private_file_metadata(&input_metadata) {
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
    #[cfg(unix)]
    let restore_mode = executable.map_or_else(
        || {
            use std::os::unix::fs::PermissionsExt as _;
            if input_metadata.permissions().mode() & 0o100 != 0 {
                0o700
            } else {
                0o600
            }
        },
        |executable| if executable { 0o700 } else { 0o600 },
    );
    for _ in 0..16 {
        let nonce = random_checkpoint_nonce()?;
        let candidate = parent.join(format!(".{file_name}.dext-restore-{nonce}"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(restore_mode).custom_flags(libc::O_NOFOLLOW);
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
    let copy_result = (|| -> Result<(), String> {
        let mut copied = 0u64;
        let mut hasher = expected_integrity.map(|_| Sha256::new());
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|error| format!("read sidecar for restore: {error}"))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("copy sidecar to temp: {error}"))?;
            if let Some(hasher) = hasher.as_mut() {
                hasher.update(&buffer[..read]);
            }
            copied = copied.saturating_add(read as u64);
        }
        if let Some((expected_digest, expected_size)) = expected_integrity {
            let actual_digest = hasher
                .expect("integrity request creates a hasher")
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if copied != expected_size || actual_digest != expected_digest {
                return Err(format!(
                    "checkpoint blob changed during restore: {expected_digest}"
                ));
            }
        }
        output
            .sync_all()
            .map_err(|error| format!("sync sidecar restore temp: {error}"))?;
        Ok(())
    })();
    drop(output);
    let result = copy_result.and_then(|()| {
        preflight_sidecar_destination(&destination, false)?;
        crate::session::replace_file_atomically(&temp_path, &destination).map_err(|error| {
            format!(
                "replace restore destination {}: {error}",
                destination.display()
            )
        })
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

const SIDECAR_TREE_ENTRY_CAP: usize = 65_536;

fn walk_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(current) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| format!("directory metadata {}: {error}", current.display()))?;
        if !safe_private_dir_metadata(&metadata) {
            return Err(format!(
                "unsafe non-private sidecar directory: {}",
                current.display()
            ));
        }
        let entries = std::fs::read_dir(&current)
            .map_err(|error| format!("read sidecar directory {}: {error}", current.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("read sidecar directory entry: {error}"))?;
            visited = visited.saturating_add(1);
            if visited > SIDECAR_TREE_ENTRY_CAP {
                return Err(format!(
                    "checkpoint sidecar tree exceeds the {SIDECAR_TREE_ENTRY_CAP}-entry limit: {}",
                    dir.display()
                ));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("sidecar metadata {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() {
                return Err(format!("unsafe sidecar symlink: {}", path.display()));
            }
            if metadata.is_dir() {
                if !safe_private_dir_metadata(&metadata) {
                    return Err(format!(
                        "unsafe non-private sidecar directory: {}",
                        path.display()
                    ));
                }
                pending.push(path);
            } else if safe_private_file_metadata(&metadata) {
                result.push(path);
            } else {
                return Err(format!("unsafe sidecar entry: {}", path.display()));
            }
        }
    }
    Ok(result)
}

struct PruneOutcome {
    checkpoints_removed: usize,
    orphan_sidecars_removed: usize,
    warnings: Vec<String>,
}

#[derive(Default)]
struct BlobPruneOutcome {
    removed: usize,
    warnings: Vec<String>,
}

const PRUNE_WARNING_CAP: usize = 8;

fn record_prune_warning(warnings: &mut Vec<String>, warning: String) {
    if warnings.len() < PRUNE_WARNING_CAP {
        warnings.push(warning);
    } else if warnings.len() == PRUNE_WARNING_CAP {
        warnings.push("additional checkpoint prune warnings omitted".to_string());
    }
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
    let sidecar_outcome = prune_orphan_sidecars(git_root, &remaining)?;
    Ok(PruneOutcome {
        checkpoints_removed: expired.len(),
        orphan_sidecars_removed: sidecar_outcome.removed,
        warnings: sidecar_outcome.warnings,
    })
}

fn prune_orphan_blobs(
    git_root: &Path,
    remaining: &[Checkpoint],
) -> Result<BlobPruneOutcome, String> {
    let dir = checkpoint_blob_dir(git_root);
    let metadata = match std::fs::symlink_metadata(&dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BlobPruneOutcome::default());
        }
        Err(error) => return Err(format!("checkpoint blob directory metadata: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        let mut outcome = BlobPruneOutcome::default();
        record_prune_warning(
            &mut outcome.warnings,
            format!("skip unsafe checkpoint blob path: {}", dir.display()),
        );
        return Ok(outcome);
    }
    if !safe_private_dir_metadata(&metadata) {
        let mut outcome = BlobPruneOutcome::default();
        record_prune_warning(
            &mut outcome.warnings,
            format!("skip unsafe checkpoint blob directory: {}", dir.display()),
        );
        return Ok(outcome);
    }
    let referenced = remaining
        .iter()
        .flat_map(|checkpoint| checkpoint.untracked_sidecars.iter())
        .filter_map(|sidecar| match sidecar {
            UntrackedSidecar::File { digest, .. } => Some(digest.as_str()),
            UntrackedSidecar::Symlink { .. } => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let mut outcome = BlobPruneOutcome::default();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => {
            record_prune_warning(
                &mut outcome.warnings,
                format!(
                    "skip unreadable checkpoint blob directory {}: {error}",
                    dir.display()
                ),
            );
            return Ok(outcome);
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record_prune_warning(
                    &mut outcome.warnings,
                    format!("skip unreadable checkpoint blob directory entry: {error}"),
                );
                continue;
            }
        };
        let name = entry.file_name();
        let path = entry.path();
        let Some(digest) = name.to_str() else {
            record_prune_warning(
                &mut outcome.warnings,
                format!(
                    "skip checkpoint blob with non-UTF-8 name: {}",
                    path.display()
                ),
            );
            continue;
        };
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                record_prune_warning(
                    &mut outcome.warnings,
                    format!(
                        "skip unreadable checkpoint blob {}: {error}",
                        path.display()
                    ),
                );
                continue;
            }
        };
        if digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !safe_private_file_metadata(&metadata)
        {
            record_prune_warning(
                &mut outcome.warnings,
                format!("skip unsafe checkpoint blob entry: {}", path.display()),
            );
            continue;
        }
        if !referenced.contains(digest) {
            match std::fs::remove_file(&path) {
                Ok(()) => outcome.removed += 1,
                Err(error) => record_prune_warning(
                    &mut outcome.warnings,
                    format!(
                        "could not remove orphan checkpoint blob {}: {error}",
                        path.display()
                    ),
                ),
            }
        }
    }
    Ok(outcome)
}

fn prune_orphan_sidecars(
    git_root: &Path,
    remaining: &[Checkpoint],
) -> Result<BlobPruneOutcome, String> {
    let dir = checkpoints_manifest_dir(git_root);
    let remaining_ids = remaining
        .iter()
        .map(|checkpoint| checkpoint.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut outcome = prune_orphan_blobs(git_root, remaining)?;
    for entry in std::fs::read_dir(&dir).map_err(|error| format!("checkpoint dir read: {error}"))? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record_prune_warning(
                    &mut outcome.warnings,
                    format!("skip unreadable checkpoint sidecar directory entry: {error}"),
                );
                continue;
            }
        };
        let name = entry.file_name();
        let Some(id) = name.to_str() else {
            record_prune_warning(
                &mut outcome.warnings,
                format!(
                    "skip checkpoint sidecar with non-UTF-8 name: {}",
                    entry.path().display()
                ),
            );
            continue;
        };
        if matches!(id, BLOBS_DIR | "manifest.txt" | "operation.lock") {
            continue;
        }
        let path = entry.path();
        if !safe_checkpoint_id(id) {
            record_prune_warning(
                &mut outcome.warnings,
                format!(
                    "skip malformed checkpoint sidecar entry: {}",
                    path.display()
                ),
            );
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                record_prune_warning(
                    &mut outcome.warnings,
                    format!(
                        "skip unreadable checkpoint sidecar {}: {error}",
                        path.display()
                    ),
                );
                continue;
            }
        };
        if remaining_ids.contains(id) {
            if !safe_private_dir_metadata(&metadata) {
                record_prune_warning(
                    &mut outcome.warnings,
                    format!(
                        "skip unsafe retained checkpoint sidecar: {}",
                        path.display()
                    ),
                );
            } else if let Err(error) = walk_dir(&path) {
                record_prune_warning(
                    &mut outcome.warnings,
                    format!(
                        "skip unsafe retained checkpoint sidecar {}: {error}",
                        path.display()
                    ),
                );
            }
            continue;
        }
        let removal = if metadata.file_type().is_symlink() {
            std::fs::remove_file(&path)
        } else if safe_private_dir_metadata(&metadata) {
            if let Err(error) = walk_dir(&path) {
                record_prune_warning(
                    &mut outcome.warnings,
                    format!(
                        "skip unsafe orphan checkpoint sidecar {}: {error}",
                        path.display()
                    ),
                );
                continue;
            }
            std::fs::remove_dir_all(&path)
        } else if metadata.is_dir() {
            record_prune_warning(
                &mut outcome.warnings,
                format!("skip unsafe orphan checkpoint sidecar: {}", path.display()),
            );
            continue;
        } else {
            record_prune_warning(
                &mut outcome.warnings,
                format!(
                    "skip unexpected checkpoint sidecar entry: {}",
                    path.display()
                ),
            );
            continue;
        };
        match removal {
            Ok(()) => outcome.removed += 1,
            Err(error) => record_prune_warning(
                &mut outcome.warnings,
                format!(
                    "could not remove orphan checkpoint sidecar {}: {error}",
                    path.display()
                ),
            ),
        }
    }
    Ok(outcome)
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
    let PruneOutcome {
        checkpoints_removed: mut removed,
        mut orphan_sidecars_removed,
        mut warnings,
    } = prune_checkpoint_refs(&git_root, keep, max_age)?;
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

    let sidecar_outcome = prune_orphan_sidecars(&git_root, &remaining)?;
    orphan_sidecars_removed += sidecar_outcome.removed;
    for warning in sidecar_outcome.warnings {
        if !warnings.contains(&warning) {
            record_prune_warning(&mut warnings, warning);
        }
    }

    write_checkpoint_manifest(&git_root, &remaining)?;

    let mut result = format!(
        "pruned {removed} checkpoint(s) and {orphan_sidecars_removed} orphan sidecar entr{}, {} remaining",
        if orphan_sidecars_removed == 1 {
            "y"
        } else {
            "ies"
        },
        remaining.len()
    );
    if !warnings.is_empty() {
        result.push_str("\nwarnings:");
        for warning in warnings {
            result.push_str("\n  ");
            result.push_str(&warning);
        }
    }
    Ok(result)
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

pub(crate) fn is_partial_untracked_recovery_error(error: &str) -> bool {
    error.starts_with("untracked recovery is partial:")
}

pub(crate) fn checkpoint_failure_blocks_tool(name: &str) -> bool {
    matches!(
        name,
        "bash"
            | "awk"
            | "csvkit"
            | "write_file"
            | "edit_file"
            | "multi_edit"
            | "todo_write"
            | "git_commit"
    )
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
    // Arbitrary command tools need a checkpoint when they can write. Other
    // non-read tools keep the existing checkpoint policy without broad
    // untracked-content capture.
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
        if let Some(Some(cached)) = &self.cached {
            return Ok(Some(cached.clone()));
        }
        let discovered = repo_root(root)?;
        if discovered.is_some() {
            self.cached = Some(discovered.clone());
        }
        Ok(discovered)
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
                let checkpoint = parse_manifest_line(line.strip_suffix('\r').unwrap_or(line))
                    .ok_or_else(|| {
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

    #[test]
    fn manifest_schema_distinguishes_legacy_and_exact_sidecar_membership() {
        let oid = "a".repeat(40);
        let digest = "b".repeat(64);
        let pre_json_eight_line = format!(
            "fixture-eight\trefs/dext/checkpoints/session/fixture-eight\t{oid}\twrite_file\t0\t{oid}\tfalse\t/home/legacy/outside.txt"
        );
        let pre_json_nine_line = format!(
            "fixture-pre-json\trefs/dext/checkpoints/session/fixture-pre-json\t{oid}\tbash\t1\t{oid}\tfalse\t\t.dext/checkpoints/manifest.txt\u{1f}notes.txt"
        );
        let comma_pre_json_line = format!(
            "fixture-comma\trefs/dext/checkpoints/session/fixture-comma\t{oid}\twrite_file\t2\t{oid}\tfalse\treports/a,b.txt"
        );
        let legacy_nine = format!(
            "fixture-nine\trefs/dext/checkpoints/session/fixture-nine\t{oid}\twrite_file\t1\t{oid}\ttrue\t[\"note.txt\"]\t[]"
        );
        let legacy_eleven = format!(
            "fixture-eleven\trefs/dext/checkpoints/session/fixture-eleven\t{oid}\tbash\t2\t{oid}\ttrue\t[]\t[\"note.txt\"]\t[{{\"kind\":\"file\",\"path\":\"note.txt\",\"digest\":\"{digest}\",\"size\":0,\"executable\":false}}]\tnull"
        );
        let current_direct = format!(
            "fixture-direct\trefs/dext/checkpoints/session/fixture-direct\t{oid}\twrite_file\t3\t{oid}\ttrue\t[\"note.txt\"]\t[]\t[]\tnull\t[\"note.txt\"]"
        );
        let current_without_direct_sidecars = format!(
            "fixture-current\trefs/dext/checkpoints/session/fixture-current\t{oid}\twrite_file\t4\t{oid}\tfalse\t[\"tracked.txt\"]\t[]\t[]\tnull\t[]"
        );

        let pre_json_eight =
            parse_manifest_line(&pre_json_eight_line).expect("parse 8-field pre-JSON manifest");
        assert_eq!(
            pre_json_eight.manifest_encoding,
            CheckpointManifestEncoding::PreJsonEightFields
        );
        assert_eq!(
            pre_json_eight.paths_hint,
            ["/home/legacy/outside.txt".to_string()]
        );
        assert_eq!(format_manifest_line(&pre_json_eight), pre_json_eight_line);

        let pre_json_nine =
            parse_manifest_line(&pre_json_nine_line).expect("parse 9-field pre-JSON manifest");
        assert_eq!(
            pre_json_nine.manifest_encoding,
            CheckpointManifestEncoding::PreJsonNineFields
        );
        assert_eq!(
            pre_json_nine.untracked_snapshot,
            [
                ".dext/checkpoints/manifest.txt".to_string(),
                "notes.txt".to_string()
            ]
        );
        assert_eq!(format_manifest_line(&pre_json_nine), pre_json_nine_line);

        let comma_pre_json =
            parse_manifest_line(&comma_pre_json_line).expect("parse comma-bearing pre-JSON hint");
        assert_eq!(
            comma_pre_json.paths_hint,
            ["reports/a,b.txt".to_string()],
            "the runtime only ever emitted one direct path hint; commas belong to that path"
        );
        assert_eq!(format_manifest_line(&comma_pre_json), comma_pre_json_line);

        let legacy_nine = parse_manifest_line(&legacy_nine).expect("parse 9-field manifest");
        assert_eq!(
            legacy_nine.manifest_encoding,
            CheckpointManifestEncoding::Current
        );
        assert_eq!(legacy_nine.legacy_sidecar_paths, None);
        assert_eq!(format_manifest_line(&legacy_nine).split('\t').count(), 11);

        let legacy_eleven = parse_manifest_line(&legacy_eleven).expect("parse 11-field manifest");
        assert_eq!(legacy_eleven.legacy_sidecar_paths, None);
        assert_eq!(legacy_eleven.untracked_sidecars.len(), 1);
        assert_eq!(format_manifest_line(&legacy_eleven).split('\t').count(), 11);

        let current_direct =
            parse_manifest_line(&current_direct).expect("parse 12-field direct-sidecar manifest");
        assert_eq!(
            current_direct.legacy_sidecar_paths.as_deref(),
            Some(["note.txt".to_string()].as_slice())
        );
        assert_eq!(
            format_manifest_line(&current_direct).split('\t').count(),
            12
        );

        let current_without_direct_sidecars = parse_manifest_line(&current_without_direct_sidecars)
            .expect("parse 12-field manifest with exact empty membership");
        assert_eq!(
            current_without_direct_sidecars
                .legacy_sidecar_paths
                .as_deref(),
            Some([].as_slice())
        );
        assert_eq!(
            format_manifest_line(&current_without_direct_sidecars)
                .split('\t')
                .count(),
            12
        );

        let malformed_current = format!(
            "fixture-malformed\trefs/dext/checkpoints/session/fixture-malformed\t{oid}\tbash\t5\t{oid}\tfalse\t[\"note.txt\"\t[]"
        );
        assert!(
            parse_manifest_line(&malformed_current).is_none(),
            "JSON-shaped current rows must not fall back to the pre-JSON parser"
        );
        let truncated_pre_json = format!(
            "fixture-truncated\trefs/dext/checkpoints/session/fixture-truncated\t{oid}\tbash\t7\t{oid}\tfalse"
        );
        assert!(
            parse_manifest_line(&truncated_pre_json).is_none(),
            "no Dext writer emitted 7-field rows; a missing trailing field is truncation"
        );
        let empty_direct_current = format!(
            "fixture-empty-direct\trefs/dext/checkpoints/session/fixture-empty-direct\t{oid}\twrite_file\t9\t{oid}\tfalse\t[]\t[]"
        );
        assert!(
            parse_manifest_line(&empty_direct_current).is_none(),
            "Dext never writes a direct-file checkpoint without one path hint"
        );
    }
}
