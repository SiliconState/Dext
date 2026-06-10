// Phase 2: Section-aware merge driver for MEMORY.md and recall.md.
//
// Protects Dext's durable and prompt-facing memory from bad text merges.
// Conservative: never silently drops conflicting memory.

use std::path::{Path, PathBuf};

pub(crate) struct ParsedMemory {
    pub preamble: String,
    pub sections: Vec<Section>,
}

#[derive(Clone)]
pub(crate) struct Section {
    pub level: usize,
    pub heading_path: Vec<String>,
    pub heading_line: String,
    pub body: String,
}

pub(crate) struct MergeOutcome {
    pub content: String,
    pub clean: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisterMode {
    Memory,
    Recall,
}

#[derive(Debug)]
pub(crate) struct MemoryMergeStatus {
    pub memory_registered: bool,
    pub recall_registered: bool,
    pub gitattributes_local: bool,
    pub gitattributes_versioned: bool,
}

const MERGE_DRIVER_MEMORY: &str = "dext-memory";
const MERGE_DRIVER_RECALL: &str = "dext-recall";

pub(crate) fn parse_memory(input: &str) -> ParsedMemory {
    let mut preamble = String::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut current_level = 0usize;
    let mut current_heading_path: Vec<String> = Vec::new();
    let mut current_heading_line = String::new();
    let mut current_body = String::new();
    let mut stack: Vec<String> = Vec::new();
    let mut in_preamble = true;

    for line in input.lines() {
        if let Some((level, title)) = parse_heading(line) {
            if in_preamble {
                in_preamble = false;
            } else if !current_heading_line.is_empty() {
                sections.push(Section {
                    level: current_level,
                    heading_path: current_heading_path.clone(),
                    heading_line: current_heading_line.clone(),
                    body: current_body.trim_end().to_string(),
                });
                current_body.clear();
            }
            stack.truncate(level.saturating_sub(1));
            stack.push(title.to_string());
            current_level = level;
            current_heading_path = stack.clone();
            current_heading_line = line.trim_end().to_string();
            continue;
        }

        if in_preamble {
            if !preamble.is_empty() || !line.trim().is_empty() {
                preamble.push_str(line);
                preamble.push('\n');
            }
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    if !current_heading_line.is_empty() {
        sections.push(Section {
            level: current_level,
            heading_path: current_heading_path,
            heading_line: current_heading_line,
            body: current_body.trim_end().to_string(),
        });
    }

    ParsedMemory {
        preamble: preamble.trim_end().to_string(),
        sections,
    }
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_end();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after_hashes = trimmed.get(hashes..)?;
    if !after_hashes.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = after_hashes.trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((hashes, rest.trim_end_matches('#').trim_end()))
}

fn section_key(s: &Section) -> String {
    s.heading_path
        .iter()
        .map(|p| p.trim().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn merge_list_body(base: &str, ours: &str, theirs: &str) -> Option<String> {
    fn list_like(body: &str) -> bool {
        body.lines().all(|line| {
            let trimmed = line.trim();
            trimmed.is_empty()
                || trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed.starts_with("+ ")
        })
    }
    if !list_like(base) || !list_like(ours) || !list_like(theirs) {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in ours.lines().chain(theirs.lines()) {
        let norm = line.trim().to_ascii_lowercase();
        if norm.is_empty() {
            continue;
        }
        if seen.insert(norm) {
            out.push(line.to_string());
        }
    }
    Some(out.join("\n"))
}

fn render_parsed(pm: &ParsedMemory) -> String {
    let mut out = String::new();
    if !pm.preamble.is_empty() {
        out.push_str(&pm.preamble);
        out.push('\n');
    }
    for section in &pm.sections {
        out.push('\n');
        out.push_str(&section.heading_line);
        out.push('\n');
        if !section.body.is_empty() {
            out.push_str(&section.body);
            out.push('\n');
        }
    }
    out
}

pub(crate) fn merge_memory(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    let base_parsed = parse_memory(base);
    let ours_parsed = parse_memory(ours);
    let theirs_parsed = parse_memory(theirs);

    let mut warnings = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut clean = true;

    // Index sections by key
    let base_map: std::collections::HashMap<String, &Section> = base_parsed
        .sections
        .iter()
        .map(|s| (section_key(s), s))
        .collect();
    let ours_map: std::collections::HashMap<String, &Section> = ours_parsed
        .sections
        .iter()
        .map(|s| (section_key(s), s))
        .collect();
    let theirs_map: std::collections::HashMap<String, &Section> = theirs_parsed
        .sections
        .iter()
        .map(|s| (section_key(s), s))
        .collect();

    // Track all known section keys in stable order
    let mut seen_keys = std::collections::HashSet::new();
    let mut all_keys: Vec<String> = Vec::new();
    for s in base_parsed
        .sections
        .iter()
        .chain(ours_parsed.sections.iter())
        .chain(theirs_parsed.sections.iter())
    {
        let key = section_key(s);
        if seen_keys.insert(key.clone()) {
            all_keys.push(key);
        }
    }

    for key in &all_keys {
        let base_sec = base_map.get(key);
        let ours_sec = ours_map.get(key);
        let theirs_sec = theirs_map.get(key);

        match (base_sec, ours_sec, theirs_sec) {
            // Same in all — keep
            (Some(b), Some(o), Some(t)) if o.body == b.body && t.body == b.body => {
                sections.push((*o).clone());
            }
            // Only ours changed from base
            (Some(b), Some(o), Some(t)) if t.body == b.body => {
                sections.push((*o).clone());
            }
            // Only theirs changed from base
            (Some(b), Some(o), Some(t)) if o.body == b.body => {
                sections.push((*t).clone());
            }
            // Both changed identically
            (Some(_), Some(o), Some(t)) if o.body == t.body => {
                sections.push((*o).clone());
            }
            // Both changed differently: union pure list sections, otherwise conflict.
            (Some(b), Some(o), Some(t)) => {
                if let Some(body) = merge_list_body(&b.body, &o.body, &t.body) {
                    sections.push(Section {
                        level: o.level,
                        heading_path: o.heading_path.clone(),
                        heading_line: o.heading_line.clone(),
                        body,
                    });
                    continue;
                }
                clean = false;
                warnings.push(format!("conflict in section '{}'", key));
                let mut body = String::new();
                body.push_str("<<<<<<< ours\n");
                body.push_str(&o.body);
                body.push('\n');
                body.push_str("=======\n");
                body.push_str(&t.body);
                body.push('\n');
                body.push_str(">>>>>>> theirs\n");
                sections.push(Section {
                    level: o.level,
                    heading_path: o.heading_path.clone(),
                    heading_line: o.heading_line.clone(),
                    body,
                });
            }
            // New in ours only
            (None, Some(o), None) => {
                sections.push((*o).clone());
            }
            // New in theirs only
            (None, None, Some(t)) => {
                sections.push((*t).clone());
            }
            // New in both: take identical additions, otherwise mark an explicit conflict.
            (None, Some(o), Some(t)) => {
                if o.body == t.body {
                    sections.push((*o).clone());
                } else {
                    clean = false;
                    warnings.push(format!("conflict in new section '{}'", key));
                    let mut body = String::new();
                    body.push_str("<<<<<<< ours\n");
                    body.push_str(&o.body);
                    body.push('\n');
                    body.push_str("=======\n");
                    body.push_str(&t.body);
                    body.push('\n');
                    body.push_str(">>>>>>> theirs\n");
                    sections.push(Section {
                        level: o.level,
                        heading_path: o.heading_path.clone(),
                        heading_line: o.heading_line.clone(),
                        body,
                    });
                }
            }
            // Removed in one side
            (Some(_), None, Some(t)) => {
                // Ours removed, theirs kept or changed
                sections.push((*t).clone());
                warnings.push(format!("section '{}' removed in ours, kept theirs", key));
            }
            (Some(_), Some(o), None) => {
                sections.push((*o).clone());
                warnings.push(format!("section '{}' removed in theirs, kept ours", key));
            }
            (Some(_), None, None) => {
                // Both removed — skip
            }
            (None, None, None) => {}
        }
    }

    let content = render_parsed(&ParsedMemory {
        preamble: ours_parsed.preamble.clone(),
        sections,
    });

    MergeOutcome {
        content,
        clean,
        warnings,
    }
}

pub(crate) fn merge_recall(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    let base_set: std::collections::HashSet<String> = base
        .lines()
        .map(normalize_recall_line)
        .filter(|s| !s.is_empty())
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut result: Vec<String> = Vec::new();

    for line in ours.lines() {
        let norm = normalize_recall_line(line);
        if norm.is_empty() || seen.insert(norm) {
            result.push(line.to_string());
        }
    }

    for line in theirs.lines() {
        let norm = normalize_recall_line(line);
        if norm.is_empty() || base_set.contains(&norm) || !seen.insert(norm) {
            continue;
        }
        result.push(line.to_string());
    }

    let mut content = result.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }

    MergeOutcome {
        content,
        clean: true,
        warnings: Vec::new(),
    }
}

fn normalize_recall_line(line: &str) -> String {
    line.trim()
        .trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim_start_matches("+ ")
        .trim_start_matches("- ")
        .trim()
        .to_ascii_lowercase()
}

/// Result of deterministically distilling recall.md against MEMORY.md.
pub(crate) struct RecallDistill {
    /// Proposed recall.md content (exact-duplicate bullets removed; structure
    /// and ordering otherwise preserved).
    pub content: String,
    /// Bullets dropped because an earlier bullet normalizes identically.
    pub removed_duplicates: Vec<String>,
    /// Kept bullets that are near-duplicates of an earlier kept bullet
    /// (flagged for human review, not removed automatically).
    pub near_duplicates: Vec<String>,
    /// Kept bullets whose content is not reflected in MEMORY.md — possibly
    /// stale, or worth promoting into durable memory.
    pub unbacked: Vec<String>,
    pub original_bullets: usize,
    pub kept_bullets: usize,
}

fn significant_tokens(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 3)
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union <= 0.0 { 0.0 } else { inter / union }
}

fn is_bullet(trimmed: &str) -> bool {
    trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ")
}

/// Deterministically distil recall.md: drop exact-duplicate bullets, flag
/// near-duplicates, and flag bullets not reflected in MEMORY.md. Never reorders
/// or rewrites surviving content, so it is safe to apply.
pub(crate) fn distill_recall(memory: &str, recall: &str) -> RecallDistill {
    let memory_lower = memory.to_ascii_lowercase();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept_token_sets: Vec<std::collections::HashSet<String>> = Vec::new();
    let mut kept_lines: Vec<String> = Vec::new();
    let mut removed_duplicates = Vec::new();
    let mut near_duplicates = Vec::new();
    let mut unbacked = Vec::new();
    let mut original_bullets = 0usize;

    for line in recall.lines() {
        if !is_bullet(line.trim_start()) {
            kept_lines.push(line.to_string());
            continue;
        }
        original_bullets += 1;
        let norm = normalize_recall_line(line);
        if norm.is_empty() {
            kept_lines.push(line.to_string());
            continue;
        }
        if !seen.insert(norm.clone()) {
            removed_duplicates.push(line.trim().to_string());
            continue;
        }
        let tokens = significant_tokens(&norm);
        if kept_token_sets
            .iter()
            .any(|prev| jaccard(prev, &tokens) >= 0.85)
        {
            near_duplicates.push(line.trim().to_string());
        }
        // "Backed" = at least half of the bullet's significant tokens appear in
        // MEMORY.md. Bullets with no significant tokens are treated as backed.
        let backed = tokens.is_empty() || {
            let present = tokens.iter().filter(|t| memory_lower.contains(*t)).count();
            present * 2 >= tokens.len()
        };
        if !backed {
            unbacked.push(line.trim().to_string());
        }
        kept_token_sets.push(tokens);
        kept_lines.push(line.to_string());
    }

    let mut content = kept_lines.join("\n");
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }

    RecallDistill {
        content,
        removed_duplicates,
        near_duplicates,
        unbacked,
        original_bullets,
        kept_bullets: kept_token_sets.len(),
    }
}

pub(crate) fn register(
    repo: &Path,
    mode: RegisterMode,
    versioned_attributes: bool,
) -> Result<(), String> {
    let repo_root = find_repo_root(repo)?;
    let git_dir = find_git_dir(repo)?;
    let driver_name = match mode {
        RegisterMode::Memory => MERGE_DRIVER_MEMORY,
        RegisterMode::Recall => MERGE_DRIVER_RECALL,
    };
    let file_pattern = match mode {
        RegisterMode::Memory => "MEMORY.md",
        RegisterMode::Recall => "recall.md",
    };

    // Write git config (local)
    run_git(
        repo,
        &[
            "config",
            &format!("merge.{driver_name}.name"),
            "Dext section-aware memory merge",
        ],
    )?;
    let driver_command = match mode {
        RegisterMode::Memory => "dext memory merge %O %A %B %L %P",
        RegisterMode::Recall => "dext memory merge --recall %O %A %B %L %P",
    };
    run_git(
        repo,
        &[
            "config",
            &format!("merge.{driver_name}.driver"),
            driver_command,
        ],
    )?;

    // Write gitattributes
    let attr_path = if versioned_attributes {
        repo_root.join(".gitattributes")
    } else {
        git_dir.join("info").join("attributes")
    };
    if let Some(parent) = attr_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let entry = format!("{file_pattern} merge={driver_name}\n");
    let mut existing = std::fs::read_to_string(&attr_path).unwrap_or_default();
    // Remove old entry for this pattern
    let pattern_prefix = format!("{file_pattern} merge=");
    existing = existing
        .lines()
        .filter(|l| !l.starts_with(&pattern_prefix))
        .collect::<Vec<_>>()
        .join("\n");
    if !existing.ends_with('\n') && !existing.is_empty() {
        existing.push('\n');
    }
    existing.push_str(&entry);
    std::fs::write(&attr_path, existing).map_err(|e| format!("write attributes: {e}"))?;

    Ok(())
}

pub(crate) fn unregister(repo: &Path) -> Result<(), String> {
    let git_dir = find_git_dir(repo)?;

    // Remove both merge drivers from config
    for driver in [MERGE_DRIVER_MEMORY, MERGE_DRIVER_RECALL] {
        let _ = run_git(
            repo,
            &["config", "--unset", &format!("merge.{driver}.name")],
        );
        let _ = run_git(
            repo,
            &["config", "--unset", &format!("merge.{driver}.driver")],
        );
    }

    // Remove entries from local attributes
    let attr_path = git_dir.join("info").join("attributes");
    if attr_path.exists() {
        let content = std::fs::read_to_string(&attr_path).unwrap_or_default();
        let filtered: String = content
            .lines()
            .filter(|l| !l.contains("merge=dext-memory") && !l.contains("merge=dext-recall"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&attr_path, format!("{filtered}\n"))
            .map_err(|e| format!("write attributes: {e}"))?;
    }

    Ok(())
}

pub(crate) fn check(repo: &Path) -> Result<MemoryMergeStatus, String> {
    let repo_root = find_repo_root(repo).unwrap_or_else(|_| repo.to_path_buf());
    let git_dir_result = find_git_dir(repo);

    let git_dir = match git_dir_result {
        Ok(d) => d,
        Err(_) => {
            return Ok(MemoryMergeStatus {
                memory_registered: false,
                recall_registered: false,
                gitattributes_local: false,
                gitattributes_versioned: false,
            });
        }
    };

    let memory_config = run_git(
        repo,
        &[
            "config",
            "--local",
            &format!("merge.{MERGE_DRIVER_MEMORY}.driver"),
        ],
    )
    .is_ok();
    let recall_config = run_git(
        repo,
        &[
            "config",
            "--local",
            &format!("merge.{MERGE_DRIVER_RECALL}.driver"),
        ],
    )
    .is_ok();

    let local_attr = git_dir.join("info").join("attributes");
    let versioned_attr = repo_root.join(".gitattributes");

    let local_content = std::fs::read_to_string(&local_attr).unwrap_or_default();
    let versioned_content = std::fs::read_to_string(&versioned_attr).unwrap_or_default();

    let local_has_memory = local_content.contains("merge=dext-memory");
    let local_has_recall = local_content.contains("merge=dext-recall");
    let versioned_has_memory = versioned_content.contains("merge=dext-memory");
    let versioned_has_recall = versioned_content.contains("merge=dext-recall");

    Ok(MemoryMergeStatus {
        memory_registered: memory_config && (local_has_memory || versioned_has_memory),
        recall_registered: recall_config && (local_has_recall || versioned_has_recall),
        gitattributes_local: local_has_memory || local_has_recall,
        gitattributes_versioned: versioned_has_memory || versioned_has_recall,
    })
}

fn find_repo_root(repo: &Path) -> Result<PathBuf, String> {
    let out = run_git(repo, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

fn find_git_dir(repo: &Path) -> Result<PathBuf, String> {
    let out = run_git(repo, &["rev-parse", "--absolute-git-dir"])?;
    let git_dir = PathBuf::from(out.trim());
    if git_dir.is_dir() {
        Ok(git_dir)
    } else {
        Err("not a git repository".to_string())
    }
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
