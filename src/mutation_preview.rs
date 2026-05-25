// Phase 3: In-memory mutation previews for direct file tools.
//
// Shows capped unified diffs before applying mutations when permission
// is being requested. Computes proposed content in memory without
// touching disk.

use std::path::{Path, PathBuf};

const PREVIEW_DIFF_CAP: usize = 4096;

pub(crate) struct MutationPreview {
    pub path: PathBuf,
    pub is_new_file: bool,
    pub added: usize,
    pub removed: usize,
    pub diff: String,
    pub truncated: bool,
}

pub(crate) fn preview_write_file(
    root: &Path,
    path_str: &str,
    content: &str,
) -> Result<MutationPreview, String> {
    let path = canonical_within(root, path_str)?;
    let before = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("failed to read existing file: {e}")),
    };
    let is_new_file = before.is_empty() && !path.exists();
    compute_preview(path, root, &before, content, is_new_file)
}

pub(crate) fn preview_edit_file(
    root: &Path,
    path_str: &str,
    old: &str,
    new: &str,
) -> Result<MutationPreview, String> {
    let path = canonical_within(root, path_str)?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let count = content.matches(old).count();
    if count == 0 {
        return Err(format!("old_string not found in {}", path.display()));
    }
    if count > 1 {
        return Err(format!(
            "old_string found {} times in {}; must be unique",
            count,
            path.display()
        ));
    }
    let updated = content.replacen(old, new, 1);
    compute_preview(path, root, &content, &updated, false)
}

pub(crate) struct MultiEdit {
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

pub(crate) fn preview_multi_edit(
    root: &Path,
    path_str: &str,
    edits: &[MultiEdit],
) -> Result<MutationPreview, String> {
    let path = canonical_within(root, path_str)?;
    let before = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut content = before.clone();
    for (i, edit) in edits.iter().enumerate() {
        if edit.replace_all {
            if !content.contains(&edit.old_string) {
                return Err(format!("edit[{i}]: old_string not found"));
            }
            content = content.replace(&edit.old_string, &edit.new_string);
        } else {
            let count = content.matches(&edit.old_string).count();
            if count == 0 {
                return Err(format!("edit[{i}]: old_string not found"));
            }
            if count > 1 {
                return Err(format!(
                    "edit[{i}]: old_string found {} times; must be unique",
                    count
                ));
            }
            content = content.replacen(&edit.old_string, &edit.new_string, 1);
        }
    }
    compute_preview(path, root, &before, &content, false)
}

fn compute_preview(
    path: PathBuf,
    _root: &Path,
    before: &str,
    after: &str,
    is_new_file: bool,
) -> Result<MutationPreview, String> {
    if before == after && !is_new_file {
        return Ok(MutationPreview {
            path,
            is_new_file: false,
            added: 0,
            removed: 0,
            diff: "(no changes)".to_string(),
            truncated: false,
        });
    }

    let diff = compute_simple_diff(before, after);
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }

    let truncated = diff.len() > PREVIEW_DIFF_CAP;
    let capped_diff = cap_string(&diff, PREVIEW_DIFF_CAP);

    Ok(MutationPreview {
        path,
        is_new_file,
        added,
        removed,
        diff: capped_diff,
        truncated,
    })
}

fn compute_simple_diff(before: &str, after: &str) -> String {
    let mut result = String::new();
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();

    // Simple line-level diff: show removed then added
    // For a real unified diff, use the existing git_unified_diff
    // via a temp dir, but this simple version avoids spawning git.
    let max = before_lines.len().max(after_lines.len());
    let mut in_hunk = false;
    let context = 1;

    for i in 0..max {
        let b = before_lines.get(i);
        let a = after_lines.get(i);
        let changed = match (b, a) {
            (Some(b), Some(a)) => b != a,
            (None, Some(_)) => true,
            (Some(_), None) => true,
            (None, None) => false,
        };

        if changed {
            if !in_hunk {
                let start = i.saturating_sub(context);
                for j in start..i {
                    if let Some(line) = before_lines.get(j) {
                        result.push_str(&format!(" {line}\n"));
                    }
                }
                in_hunk = true;
            }
            if let Some(line) = b {
                result.push_str(&format!("-{line}\n"));
            }
            if let Some(line) = a {
                result.push_str(&format!("+{line}\n"));
            }
        } else if in_hunk {
            // Show context after
            if let Some(line) = b.or(a) {
                result.push_str(&format!(" {line}\n"));
            }
            in_hunk = false;
        }
    }

    result
}

fn cap_string(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut result = String::with_capacity(max + 50);
    for line in s.lines() {
        if result.len() + line.len() + 1 > max {
            result.push_str("\n... (preview truncated)");
            break;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

fn canonical_within(root: &Path, path_str: &str) -> Result<PathBuf, String> {
    let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let candidate = if Path::new(path_str).is_absolute() {
        PathBuf::from(path_str)
    } else {
        root_canon.join(path_str)
    };
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(_) => {
            let parent = candidate.parent().ok_or("path has no parent")?;
            let name = candidate.file_name().ok_or("path has no filename")?;
            let parent = std::fs::canonicalize(parent)
                .map_err(|e| format!("parent dir does not exist or is not accessible: {e}"))?;
            parent.join(name)
        }
    };
    if !canonical.starts_with(&root_canon) {
        return Err(format!(
            "path outside sandbox ({}): {}",
            root_canon.display(),
            canonical.display()
        ));
    }
    Ok(canonical)
}
