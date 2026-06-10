// Phase 1: Git-native recovery checkpoints.
//
// Before Dext performs an approved workspace mutation, create a local Git
// recovery point under refs/dext/checkpoints/... Add /undo and CLI support
// to preview and restore the latest checkpoint.

use std::path::{Path, PathBuf};

const CHECKPOINTS_DIR: &str = ".dext/checkpoints";
const REF_PREFIX: &str = "refs/dext/checkpoints";
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
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(root)
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
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git spawn: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_git_env(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().map_err(|e| format!("git spawn: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn sanitize_ref_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
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

fn sidecar_dir(git_root: &Path, id: &str) -> PathBuf {
    git_root.join(CHECKPOINTS_DIR).join(id)
}

fn is_dirty(git_root: &Path) -> Result<bool, String> {
    let out = run_git(git_root, &["status", "--porcelain"])?;
    Ok(!out.trim().is_empty())
}

fn head_oid(git_root: &Path) -> Result<String, String> {
    let out = run_git(git_root, &["rev-parse", "HEAD"])?;
    Ok(out.trim().to_string())
}

/// Save untracked file content as a sidecar before direct file tools
/// overwrite it. Returns the sidecar directory path on success.
pub(crate) fn save_untracked_sidecar(
    git_root: &Path,
    id: &str,
    abs_path: &Path,
    git_root_relative: &Path,
) -> Result<(), String> {
    let dir = sidecar_dir(git_root, id);
    let dest = dir.join(git_root_relative);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("sidecar mkdir: {e}"))?;
    }
    let content = std::fs::read(abs_path).map_err(|e| format!("sidecar read: {e}"))?;
    std::fs::write(&dest, &content).map_err(|e| format!("sidecar write: {e}"))?;
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

fn resolve_existing_repo_path(
    root: &Path,
    git_root: &Path,
    user_path: &str,
) -> Option<(PathBuf, PathBuf)> {
    let candidate = if Path::new(user_path).is_absolute() {
        PathBuf::from(user_path)
    } else {
        root.join(user_path)
    };
    let abs = std::fs::canonicalize(candidate).ok()?;
    let rel = abs.strip_prefix(git_root).ok()?.to_path_buf();
    Some((abs, rel))
}

fn repo_relative_hint(git_root: &Path, path_str: &str) -> PathBuf {
    let path = Path::new(path_str);
    if path.is_absolute() {
        path.strip_prefix(git_root).unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    }
}

fn tree_has_path(git_root: &Path, oid: &str, rel: &str) -> bool {
    run_git(git_root, &["cat-file", "-e", &format!("{oid}:{rel}")]).is_ok()
}

fn remove_worktree_path(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            std::fs::remove_dir_all(path).map_err(|e| format!("remove dir: {e}"))?;
            Ok(true)
        }
        Ok(_) => {
            std::fs::remove_file(path).map_err(|e| format!("remove file: {e}"))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("stat path: {e}")),
    }
}

/// Create a recovery checkpoint before a workspace mutation.
/// Returns None if not in a Git repo. Returns error only on unexpected
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
    create_checkpoint_in_repo(root, &git_root, tool, paths_hint, ordinal).map(Some)
}

pub(crate) fn create_checkpoint_in_repo(
    root: &Path,
    git_root: &Path,
    tool: &str,
    paths_hint: &[String],
    ordinal: usize,
) -> Result<Checkpoint, String> {
    let ts = now_ms();
    let sess = session_tag();
    let tool_sanitized = sanitize_ref_component(tool);
    let id = format!("{ts}-{ordinal}-{tool_sanitized}");
    let ref_name = format!("{REF_PREFIX}/{sess}/{id}");

    let head = head_oid(git_root)?;
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

    // Handle untracked sidecar for direct file tools.
    let mut includes_untracked_sidecar = false;
    let file_tools = ["write_file", "edit_file", "multi_edit"];
    if file_tools.contains(&tool) {
        for path_str in paths_hint {
            if let Some((abs, rel)) = resolve_existing_repo_path(root, git_root, path_str)
                && !is_tracked(git_root, &rel)
                && save_untracked_sidecar(git_root, &id, &abs, &rel).is_ok()
            {
                includes_untracked_sidecar = true;
            }
        }
    }

    let cp = Checkpoint {
        id,
        ref_name,
        oid,
        tool_name: tool.to_string(),
        created_at_ms: ts,
        head,
        paths_hint: paths_hint.to_vec(),
        includes_untracked_sidecar,
        untracked_snapshot: untracked_files(git_root),
    };

    append_manifest(git_root, &cp)?;

    Ok(cp)
}

fn append_manifest(git_root: &Path, cp: &Checkpoint) -> Result<(), String> {
    let dir = checkpoints_manifest_dir(git_root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("manifest mkdir: {e}"))?;
    let line = format_manifest_line(cp);
    let manifest_path = dir.join("manifest.txt");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest_path)
        .map_err(|e| format!("manifest open: {e}"))?;
    use std::io::Write as _;
    writeln!(file, "{line}").map_err(|e| format!("manifest write: {e}"))?;
    Ok(())
}

fn format_manifest_line(cp: &Checkpoint) -> String {
    let paths = cp.paths_hint.join(",");
    // Untracked paths may contain commas, so join them with a unit separator
    // that cannot appear in a path. The field stays within its tab column.
    let untracked = cp.untracked_snapshot.join("\u{1f}");
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

fn parse_manifest_line(line: &str) -> Option<Checkpoint> {
    let parts: Vec<&str> = line.splitn(9, '\t').collect();
    if parts.len() < 7 {
        return None;
    }
    let paths_hint = parts
        .get(7)
        .map(|s| {
            s.split(',')
                .map(String::from)
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // Field 8 is absent in manifests written before untracked snapshots existed.
    let untracked_snapshot = parts
        .get(8)
        .map(|s| {
            s.split('\u{1f}')
                .map(String::from)
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(Checkpoint {
        id: parts[0].to_string(),
        ref_name: parts[1].to_string(),
        oid: parts[2].to_string(),
        tool_name: parts[3].to_string(),
        created_at_ms: parts[4].parse().ok()?,
        head: parts[5].to_string(),
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
    let content = std::fs::read_to_string(&manifest_path).unwrap_or_default();
    let mut cps: Vec<Checkpoint> = content
        .lines()
        .filter_map(|l| parse_manifest_line(l.trim()))
        .collect();
    // Newest first
    cps.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
    cps.truncate(limit);
    Ok(cps)
}

pub(crate) fn preview_restore(root: &Path, cp: &Checkpoint) -> Result<String, String> {
    let Some(git_root) = repo_root(root)? else {
        return Err("not a git repository".to_string());
    };

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

    // Sidecar note
    let sdir = sidecar_dir(&git_root, &cp.id);
    if sdir.is_dir() {
        out.push_str("\nUntracked sidecar files present; ");
        out.push_str("restore will copy them back.\n");
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

pub(crate) fn restore_worktree(
    root: &Path,
    cp: &Checkpoint,
    mode: RestoreMode,
) -> Result<String, String> {
    let Some(git_root) = repo_root(root)? else {
        return Err("not a git repository".to_string());
    };

    if mode == RestoreMode::Preview {
        return preview_restore(root, cp);
    }

    if mode == RestoreMode::ResetHead {
        return reset_head(&git_root, cp);
    }

    // Worktree restore: checkout paths from checkpoint OID
    let mut restored: Vec<String> = Vec::new();

    if !cp.paths_hint.is_empty() {
        for path_str in &cp.paths_hint {
            let rel = repo_relative_hint(&git_root, path_str);
            let rel_str = rel.to_string_lossy().to_string();
            let result = if tree_has_path(&git_root, &cp.oid, &rel_str) {
                run_git(&git_root, &["checkout", &cp.oid, "--", &rel_str]).map(|_| {
                    restored.push(rel_str.clone());
                })
            } else {
                remove_worktree_path(&git_root.join(&rel)).map(|removed| {
                    if removed {
                        restored.push(format!("removed {rel_str}"));
                    }
                })
            };
            if let Err(e) = result {
                eprintln!("warning: could not restore {path_str}: {e}");
            }
        }
    }

    // If no specific paths or mode is WorktreeAndIndex, full checkout
    if mode == RestoreMode::WorktreeAndIndex || cp.paths_hint.is_empty() {
        run_git(&git_root, &["checkout", &cp.oid, "--", "."])?;
        restored.push("(all worktree files)".to_string());
    }

    // Restore sidecar untracked files
    let sdir = sidecar_dir(&git_root, &cp.id);
    if sdir.is_dir()
        && let Ok(entries) = walk_dir(&sdir)
    {
        for entry in entries {
            if let Ok(rel) = entry.strip_prefix(&sdir) {
                let dest = git_root.join(rel);
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::copy(&entry, &dest) {
                    Ok(_) => restored.push(rel.display().to_string()),
                    Err(e) => eprintln!("warning: sidecar restore failed: {e}"),
                }
            }
        }
    }

    Ok(format!(
        "Restored from checkpoint {}:\n  {}\nRef preserved for further inspection.",
        cp.id,
        restored.join("\n  "),
    ))
}

fn reset_head(git_root: &Path, cp: &Checkpoint) -> Result<String, String> {
    run_git(git_root, &["reset", "--hard", &cp.head])?;
    Ok(format!(
        "Reset HEAD to {} (from checkpoint {}).\nWorking tree and index restored to checkpoint state.",
        cp.head, cp.id,
    ))
}

fn walk_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir_entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            let mut sub = walk_dir(&path)?;
            result.append(&mut sub);
        } else {
            result.push(path);
        }
    }
    Ok(result)
}

pub(crate) fn prune(
    root: &Path,
    keep: Option<usize>,
    max_age_hours: Option<u64>,
) -> Result<String, String> {
    let Some(git_root) = repo_root(root)? else {
        return Ok("not a git repository, nothing to prune".to_string());
    };

    let keep = keep.unwrap_or(DEFAULT_PRUNE_KEEP);
    let max_age = max_age_hours.unwrap_or(DEFAULT_PRUNE_MAX_AGE_HOURS);
    let now = now_ms();
    let max_age_ms = (max_age as u128) * 3_600_000;

    let cps = list_checkpoints(root, usize::MAX)?;
    let mut removed = 0usize;

    // Keep newest `keep` entries; remove expired ones
    for (i, cp) in cps.iter().enumerate() {
        if i < keep {
            continue;
        }
        let age = now.saturating_sub(cp.created_at_ms);
        if age > max_age_ms {
            let _ = run_git(&git_root, &["update-ref", "-d", &cp.ref_name]);
            let _ = std::fs::remove_dir_all(sidecar_dir(&git_root, &cp.id));
            removed += 1;
        }
    }

    // Rebuild manifest with remaining entries
    let remaining: Vec<Checkpoint> = cps
        .into_iter()
        .filter(|cp| run_git(&git_root, &["rev-parse", "--verify", &cp.ref_name]).is_ok())
        .collect();

    let dir = checkpoints_manifest_dir(&git_root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("manifest mkdir: {e}"))?;
    let manifest_path = dir.join("manifest.txt");
    let content: String = remaining
        .iter()
        .rev() // oldest first in file
        .map(format_manifest_line)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        &manifest_path,
        if content.is_empty() {
            String::new()
        } else {
            format!("{content}\n")
        },
    )
    .map_err(|e| format!("manifest write: {e}"))?;

    Ok(format!(
        "pruned {removed} checkpoint(s), {} remaining",
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
