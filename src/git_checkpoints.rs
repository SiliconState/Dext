// Phase 1: Git-native recovery checkpoints.
//
// Before Dext performs an approved workspace mutation, create a local Git
// recovery point under refs/dext/checkpoints/... Add /undo and CLI support
// to preview and restore the latest checkpoint.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const CHECKPOINTS_DIR: &str = ".dext/checkpoints";
const REF_PREFIX: &str = "refs/dext/checkpoints";
const REF_SCAN_PREFIX: &str = "refs/dext/checkpoints/";
const DEFAULT_PRUNE_KEEP: usize = 20;
const DEFAULT_PRUNE_MAX_AGE_HOURS: u64 = 168; // 7 days

#[derive(Clone)]
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

fn git_command(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::deny_interactive_prompt_env_std(&mut cmd);
    crate::scrub_tool_credentials_from_std_command(&mut cmd);
    cmd
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
    let output = git_command(root, &["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("git spawn: {e}"))?;
    if output.status.success() {
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
    let output = git_command(cwd, args)
        .output()
        .map_err(|e| format!("git spawn: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_git_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = git_command(cwd, args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    crate::scrub_tool_credentials_from_std_command(&mut cmd);
    let output = cmd.output().map_err(|e| format!("git spawn: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
    ensure_private_dir(&checkpoints_manifest_dir(git_root))
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

fn open_private_file(path: &Path, append: bool) -> Result<std::fs::File, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!(
                "private file path is not a real file: {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("private file metadata {}: {error}", path.display())),
    }
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|e| format!("private file open {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod private file {}: {e}", path.display()))?;
    }
    Ok(file)
}

fn write_private_file(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = open_private_file(path, false)?;
    file.write_all(content)
        .map_err(|e| format!("private file write {}: {e}", path.display()))
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
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!(
                "Git exclude path is not a real file: {}",
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
    let output = git_command(git_root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map_err(|e| format!("git spawn: {e}"))?;
    if output.status.success() {
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
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut source = options
        .open(abs_path)
        .map_err(|e| format!("sidecar read open: {e}"))?;
    let mut destination =
        open_private_file(&dest, false).map_err(|e| format!("sidecar write: {e}"))?;
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
    let target = canonical_root.join(relative);
    let parent = target
        .parent()
        .ok_or_else(|| format!("checkpoint path has no parent: {}", relative.display()))?;
    let resolved_parent = canonicalize_with_missing_ancestors(parent)
        .ok_or_else(|| format!("resolve checkpoint path parent: {}", parent.display()))?;
    if !resolved_parent.starts_with(&canonical_root) {
        return Err(format!(
            "checkpoint path escapes repository through a symlink: {}",
            relative.display()
        ));
    }
    Ok(resolved_parent.join(
        target
            .file_name()
            .ok_or_else(|| format!("checkpoint path has no file name: {}", relative.display()))?,
    ))
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
    let ts = now_ms();
    let sess = session_tag();
    let tool_sanitized = sanitize_ref_component(tool);
    let id = format!("{ts}-{ordinal}-{tool_sanitized}");
    let ref_name = format!("{REF_PREFIX}/{sess}/{id}");

    let normalized_paths = paths_hint
        .iter()
        .filter_map(|path| resolve_user_repo_path(root, git_root, path))
        .collect::<Vec<_>>();
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

    let Some(head) = head_oid(git_root)? else {
        return Ok(None);
    };
    ensure_checkpoint_storage(git_root)?;
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

    // Store the ref
    run_git(git_root, &["update-ref", &ref_name, &oid])?;

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
    let _ = prune_checkpoint_refs(
        root,
        git_root,
        DEFAULT_PRUNE_KEEP,
        DEFAULT_PRUNE_MAX_AGE_HOURS,
    );

    Ok(Some(cp))
}

fn append_manifest(git_root: &Path, cp: &Checkpoint) -> Result<(), String> {
    ensure_checkpoint_storage(git_root)?;
    let dir = checkpoints_manifest_dir(git_root);
    let line = format!("{}\n", format_manifest_line(cp));
    let manifest_path = dir.join("manifest.txt");
    let mut file =
        open_private_file(&manifest_path, true).map_err(|e| format!("manifest open: {e}"))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("manifest write: {e}"))?;
    Ok(())
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

fn parse_manifest_line(line: &str) -> Option<Checkpoint> {
    let parts: Vec<&str> = line.splitn(9, '\t').collect();
    if parts.len() < 7 {
        return None;
    }
    let paths_hint = parts
        .get(7)
        .map(|value| {
            serde_json::from_str::<Vec<String>>(value).unwrap_or_else(|_| {
                value
                    .split(',')
                    .map(String::from)
                    .filter(|path| !path.is_empty())
                    .collect()
            })
        })
        .unwrap_or_default();
    let untracked_snapshot = parts
        .get(8)
        .map(|value| {
            serde_json::from_str::<Vec<String>>(value).unwrap_or_else(|_| {
                value
                    .split('\u{1f}')
                    .map(String::from)
                    .filter(|path| !path.is_empty())
                    .collect()
            })
        })
        .unwrap_or_default();
    let id = parts[0];
    let ref_name = parts[1];
    let oid = parts[2];
    let head = parts[5];
    if !safe_checkpoint_id(id)
        || !checkpoint_ref_valid(ref_name, id)
        || !valid_object_id(oid)
        || !valid_object_id(head)
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

pub(crate) fn list_checkpoints(root: &Path, limit: usize) -> Result<Vec<Checkpoint>, String> {
    let Some(git_root) = repo_root(root)? else {
        return Ok(Vec::new());
    };
    let manifest_path = checkpoints_manifest_dir(&git_root).join("manifest.txt");
    let content = match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!(
                "checkpoint manifest is not a real file: {}",
                manifest_path.display()
            ));
        }
        Ok(_) => std::fs::read_to_string(&manifest_path)
            .map_err(|error| format!("read checkpoint manifest: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("checkpoint manifest metadata: {error}")),
    };
    let existing_refs = run_git(
        &git_root,
        &["for-each-ref", "--format=%(refname)", REF_SCAN_PREFIX],
    )
    .ok()
    .map(|refs| {
        refs.lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect::<std::collections::HashSet<_>>()
    });
    let mut cps: Vec<(usize, Checkpoint)> = content
        .lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            parse_manifest_line(line.trim()).map(|checkpoint| (line_index, checkpoint))
        })
        .filter(|(_, cp)| {
            existing_refs
                .as_ref()
                .is_none_or(|refs| refs.contains(&cp.ref_name))
        })
        .collect();
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

fn validate_checkpoint(cp: &Checkpoint) -> Result<(), String> {
    if safe_checkpoint_id(&cp.id)
        && checkpoint_ref_valid(&cp.ref_name, &cp.id)
        && valid_object_id(&cp.oid)
        && valid_object_id(&cp.head)
    {
        Ok(())
    } else {
        Err("checkpoint metadata failed validation".to_string())
    }
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

pub(crate) fn preview_restore(root: &Path, cp: &Checkpoint) -> Result<String, String> {
    let Some(git_root) = repo_root(root)? else {
        return Err("not a git repository".to_string());
    };
    validate_checkpoint_ref(&git_root, cp)?;

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
    let diff = run_git(&git_root, &["diff", "--stat", &cp.oid])
        .unwrap_or_else(|e| format!("(diff unavailable: {e})"));
    if !diff.trim().is_empty() {
        out.push_str("\nRestore diff vs current worktree:\n");
        out.push_str(&diff);
    }

    let full_diff =
        run_git_env(&git_root, &["diff", "--no-color", &cp.oid], &[]).unwrap_or_default();
    let capped = cap_diff(&full_diff, 4000);
    if !capped.is_empty() {
        out.push_str("\nUnified diff (capped):\n");
        out.push_str(&capped);
    }

    if cp.includes_untracked_sidecar {
        let sdir = sidecar_dir(&git_root, &cp.id);
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
    let now = untracked_files(&git_root);
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

fn preflight_restore_paths(
    root: &Path,
    git_root: &Path,
    cp: &Checkpoint,
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    cp.paths_hint
        .iter()
        .map(|path| {
            let relative = manifest_repo_relative_path(root, git_root, path)
                .ok_or_else(|| format!("unsafe checkpoint path: {path}"))?;
            let target = safe_worktree_target(git_root, &relative)?;
            let relative_string = relative.to_string_lossy();
            if !tree_has_path(git_root, &cp.oid, &relative_string) {
                match std::fs::symlink_metadata(&target) {
                    Ok(metadata) if metadata.is_dir() => {
                        return Err(format!(
                            "refusing to recursively remove directory during checkpoint restore: {}",
                            target.display()
                        ));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("checkpoint restore target metadata: {error}"));
                    }
                }
            }
            Ok((relative, target))
        })
        .collect()
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
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(format!(
                    "sidecar restore destination is unsafe: {}",
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
        options
            .open(entry)
            .map_err(|error| format!("open checkpoint sidecar {}: {error}", entry.display()))?;
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
    validate_checkpoint_ref(&git_root, cp)?;

    if mode == RestoreMode::Preview {
        return preview_restore(root, cp);
    }

    if mode == RestoreMode::ResetHead {
        return reset_head(&git_root, cp);
    }

    let restore_paths = preflight_restore_paths(root, &git_root, cp)?;
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
                run_git(&git_root, &["checkout", &cp.oid, "--", &rel_str]).map(|_| {
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

    // If no specific paths or mode is WorktreeAndIndex, full checkout
    if mode == RestoreMode::WorktreeAndIndex || cp.paths_hint.is_empty() {
        run_git(&git_root, &["checkout", &cp.oid, "--", "."])?;
        restored.push("(all worktree files)".to_string());
    }

    // Restore sidecar untracked files that were fully validated before any
    // worktree mutation above.
    for entry in sidecar_entries {
        let rel = entry
            .strip_prefix(&sdir)
            .expect("preflighted sidecar remains below checkpoint directory");
        match safe_worktree_target(&git_root, rel).and_then(|dest| copy_sidecar_file(&entry, &dest))
        {
            Ok(()) => restored.push(rel.display().to_string()),
            Err(error) => warnings.push(format!("sidecar restore failed: {error}")),
        }
    }

    let warning_text = if warnings.is_empty() {
        String::new()
    } else {
        format!("\nWarnings:\n  {}", warnings.join("\n  "))
    };
    Ok(format!(
        "Restored from checkpoint {}:\n  {}\nRef preserved for further inspection.{}",
        cp.id,
        restored.join("\n  "),
        warning_text,
    ))
}

fn reset_head(git_root: &Path, cp: &Checkpoint) -> Result<String, String> {
    validate_checkpoint(cp)?;
    run_git(git_root, &["reset", "--hard", &cp.head])?;
    Ok(format!(
        "Reset HEAD to {} (from checkpoint {}).\nWorking tree and index restored to checkpoint state.",
        cp.head, cp.id,
    ))
}

fn copy_sidecar_file(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "restore destination has no parent: {}",
            destination.display()
        )
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create restore parent {}: {error}", parent.display()))?;
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("sidecar metadata {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "sidecar source is not a real file: {}",
            source.display()
        ));
    }
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!(
                "restore destination is not a real file: {}",
                destination.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("restore destination metadata: {error}")),
    }
    let mut source_options = OpenOptions::new();
    source_options.read(true);
    let mut destination_options = OpenOptions::new();
    destination_options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        source_options.custom_flags(libc::O_NOFOLLOW);
        destination_options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut input = source_options
        .open(source)
        .map_err(|error| format!("open sidecar {}: {error}", source.display()))?;
    let mut output = destination_options.open(destination).map_err(|error| {
        format!(
            "open restore destination {}: {error}",
            destination.display()
        )
    })?;
    std::io::copy(&mut input, &mut output)
        .map_err(|error| format!("copy sidecar to {}: {error}", destination.display()))?;
    Ok(())
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
        } else if metadata.is_file() {
            result.push(path);
        } else {
            return Err(format!("unsafe sidecar entry: {}", path.display()));
        }
    }
    Ok(result)
}

fn prune_checkpoint_refs(
    root: &Path,
    git_root: &Path,
    keep: usize,
    max_age_hours: u64,
) -> Result<usize, String> {
    ensure_checkpoint_storage(git_root)?;
    let now = now_ms();
    let max_age_ms = (max_age_hours as u128) * 3_600_000;
    let cps = list_checkpoints(root, usize::MAX)?;
    let mut removed = 0usize;
    for (i, cp) in cps.iter().enumerate() {
        let age = now.saturating_sub(cp.created_at_ms);
        if i >= keep || age > max_age_ms {
            if run_git(git_root, &["update-ref", "-d", &cp.ref_name]).is_ok() {
                let _ = std::fs::remove_dir_all(sidecar_dir(git_root, &cp.id));
                removed += 1;
            }
        }
    }
    Ok(removed)
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
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&path)
                .map_err(|error| format!("remove orphan checkpoint sidecar: {error}"))?;
            removed += 1;
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

    ensure_checkpoint_storage(&git_root).map_err(|e| format!("manifest mkdir: {e}"))?;
    ensure_checkpoint_git_exclude(&git_root)?;
    let keep = keep.unwrap_or(DEFAULT_PRUNE_KEEP);
    let max_age = max_age_hours.unwrap_or(DEFAULT_PRUNE_MAX_AGE_HOURS);
    let mut removed = prune_checkpoint_refs(root, &git_root, keep, max_age)?;
    let cps = list_checkpoints(root, usize::MAX)?;

    // Rebuild the private manifest after manual pruning. Automatic retention
    // only deletes refs/sidecars so concurrent checkpoint appends are never
    // overwritten by a background compaction.
    let remaining: Vec<Checkpoint> = cps
        .into_iter()
        .filter(|cp| run_git(&git_root, &["rev-parse", "--verify", &cp.ref_name]).is_ok())
        .collect();
    let remaining_refs: std::collections::HashSet<&str> =
        remaining.iter().map(|cp| cp.ref_name.as_str()).collect();
    if let Ok(refs) = run_git(
        &git_root,
        &["for-each-ref", "--format=%(refname)", REF_SCAN_PREFIX],
    ) {
        for ref_name in refs.lines().map(str::trim).filter(|name| !name.is_empty()) {
            if !remaining_refs.contains(ref_name)
                && run_git(&git_root, &["update-ref", "-d", ref_name]).is_ok()
            {
                removed += 1;
            }
        }
    }

    let orphan_sidecars_removed = prune_orphan_sidecars(&git_root, &remaining)?;

    let dir = checkpoints_manifest_dir(&git_root);
    let manifest_path = dir.join("manifest.txt");
    let content: String = remaining
        .iter()
        .rev() // oldest first in file
        .map(format_manifest_line)
        .collect::<Vec<_>>()
        .join("\n");
    write_private_file(
        &manifest_path,
        if content.is_empty() {
            String::new()
        } else {
            format!("{content}\n")
        }
        .as_bytes(),
    )
    .map_err(|e| format!("manifest write: {e}"))?;

    Ok(format!(
        "pruned {removed} checkpoint(s) and {orphan_sidecars_removed} orphan sidecar director{}, {} remaining",
        if orphan_sidecars_removed == 1 {
            "y"
        } else {
            "ies"
        },
        remaining.len()
    ))
}

pub(crate) fn find_checkpoint(root: &Path, id_or_ref: &str) -> Result<Option<Checkpoint>, String> {
    let cps = list_checkpoints(root, usize::MAX)?;
    // Try exact id match
    if let Some(cp) = cps.iter().find(|cp| cp.id == id_or_ref) {
        return Ok(Some(cp.clone()));
    }
    // Try ref suffix match
    if let Some(cp) = cps.iter().find(|cp| cp.ref_name.ends_with(id_or_ref)) {
        return Ok(Some(cp.clone()));
    }
    // Try partial id match
    if let Some(cp) = cps.iter().find(|cp| cp.id.starts_with(id_or_ref)) {
        return Ok(Some(cp.clone()));
    }
    Ok(None)
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
